//! The `.series` files: every series of [`crate::series`] the `.seconds` files don't keep,
//! recorded by the history service.
//!
//! `YYYY-MM-DD.series` has each series' value every second, and is kept for a day or so;
//! `YYYY-MM-DD.series-minutes` each minute's average and peak, kept as long as the rest of the
//! history (a minute apart is plenty for a graph of a day or a week). Values are half floats, in
//! the series' [`Unit::scale`].
//!
//! After a 16-byte header a file is frames: a tag byte, a two-byte length and that many bytes.
//! A name frame numbers the next series: its unit's code, then its id. A sample frame is a Unix
//! time, then each numbered series' value in number order (NaN when it had none); in a minutes
//! file, its average then its peak. A sample may stop short of the last numbers: series named
//! after it. A frame cut short (being written right now, or by a crash) ends the file for
//! readers and is trimmed off by the writer.

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use crate::archive::day_name;
use crate::series::{Unit, is_id};

const SECONDS_MAGIC: [u8; 4] = *b"FTSS";
const MINUTES_MAGIC: [u8; 4] = *b"FTSM";
const VERSION: u32 = 1;
const HEADER_SIZE: usize = 16;
const NAME: u8 = 1;
const SAMPLE: u8 = 2;
/// The most series a file numbers.
pub const MAX_SERIES: usize = 1024;
pub const SECONDS_EXTENSION: &str = "series";
pub const MINUTES_EXTENSION: &str = "series-minutes";
const NAN16: u16 = 0x7e00;

// ----- Half floats -----

/// IEEE 754 half precision (rounded to nearest, ties to even): 11 significant bits, up to 65504.
pub fn f16_from_f32(value: f32) -> u16 {
    let bits = value.to_bits();
    let sign = ((bits >> 16) & 0x8000) as u16;
    let exponent = ((bits >> 23) & 0xff) as i32;
    let mantissa = bits & 0x7f_ffff;
    if exponent == 0xff {
        return sign | if mantissa == 0 { 0x7c00 } else { NAN16 };
    }
    let half_exponent = exponent - 112;
    if half_exponent >= 0x1f {
        return sign | 0x7c00;
    }
    let round = |value: u32, shift: u32| {
        let kept = value >> shift;
        let rest = value & ((1 << shift) - 1);
        let halfway = 1 << (shift - 1);
        if rest > halfway || (rest == halfway && kept & 1 == 1) { kept + 1 } else { kept }
    };
    if half_exponent <= 0 {
        // Too small for a normal half: a subnormal, or zero.
        if half_exponent < -10 {
            return sign;
        }
        return sign | round(mantissa | 0x80_0000, (14 - half_exponent) as u32) as u16;
    }
    // A carry out of the mantissa steps the exponent up, which is what rounding up should do.
    sign | round(((half_exponent as u32) << 23) | mantissa, 13) as u16
}

pub fn f32_from_f16(half: u16) -> f32 {
    let sign = if half & 0x8000 == 0 { 1.0 } else { -1.0 };
    let exponent = i32::from((half >> 10) & 0x1f);
    let mantissa = f32::from(half & 0x3ff);
    match exponent {
        0 => sign * mantissa * 2f32.powi(-24),
        0x1f if mantissa == 0.0 => sign * f32::INFINITY,
        0x1f => f32::NAN,
        _ => sign * (1.0 + mantissa / 1024.0) * 2f32.powi(exponent - 15),
    }
}

fn encode(unit: Unit, value: f64) -> u16 {
    if value.is_finite() { f16_from_f32((value * unit.scale()) as f32) } else { NAN16 }
}

fn decode(unit: Unit, half: u16) -> Option<f64> {
    let value = f64::from(f32_from_f16(half)) / unit.scale();
    value.is_finite().then_some(value)
}

// ----- Reading -----

fn header(magic: [u8; 4]) -> [u8; HEADER_SIZE] {
    let mut bytes = [0u8; HEADER_SIZE];
    bytes[..4].copy_from_slice(&magic);
    bytes[4..8].copy_from_slice(&VERSION.to_le_bytes());
    bytes
}

/// Calls `frame` with each whole frame's tag and contents; returns where the whole frames end,
/// or `None` if the file isn't ours.
fn walk(bytes: &[u8], magic: [u8; 4], mut frame: impl FnMut(u8, &[u8])) -> Option<usize> {
    if bytes.get(..4)? != magic || bytes.get(4..8)? != VERSION.to_le_bytes() {
        return None;
    }
    let mut at = HEADER_SIZE;
    while let Some(head) = bytes.get(at..at + 3) {
        let length = usize::from(u16::from_le_bytes([head[1], head[2]]));
        let Some(contents) = bytes.get(at + 3..at + 3 + length) else { break };
        if !matches!(head[0], NAME | SAMPLE) {
            break;
        }
        frame(head[0], contents);
        at += 3 + length;
    }
    Some(at)
}

/// A name frame's series, or `None` for one this version doesn't understand (it still takes a
/// number).
fn parse_name(contents: &[u8]) -> Option<(String, Unit)> {
    let (&code, id) = contents.split_first()?;
    let id = std::str::from_utf8(id).ok().filter(|id| is_id(id))?;
    Some((id.to_owned(), Unit::from_code(code)?))
}

/// One series' recorded points: Unix time, value and peak (in a seconds file, the value again).
pub type Points = Vec<(u32, f64, f64)>;

/// Adds the points of `ids` between `from` and `to` in one file's `bytes` to `out` (one list per
/// id), and notes each one's unit in `units`.
pub fn parse(
    bytes: &[u8],
    minutes: bool,
    ids: &[String],
    (from, to): (u64, u64),
    out: &mut [Points],
    units: &mut [Option<Unit>],
) {
    let magic = if minutes { MINUTES_MAGIC } else { SECONDS_MAGIC };
    let width = if minutes { 4 } else { 2 };
    // For each numbered series, which of `ids` it is and its unit.
    let mut numbers: Vec<Option<(usize, Unit)>> = Vec::new();
    walk(bytes, magic, |tag, contents| match tag {
        NAME if numbers.len() < MAX_SERIES => {
            let wanted = parse_name(contents)
                .and_then(|(id, unit)| ids.iter().position(|wanted| *wanted == id).map(|i| (i, unit)));
            if let Some((index, unit)) = wanted
                && let Some(slot) = units.get_mut(index)
            {
                *slot = Some(unit);
            }
            numbers.push(wanted);
        }
        SAMPLE => {
            let Some(time) = contents.get(..4).map(|t| u32::from_le_bytes([t[0], t[1], t[2], t[3]])) else { return };
            if !(from..=to).contains(&u64::from(time)) {
                return;
            }
            for (number, wanted) in numbers.iter().enumerate() {
                let Some((index, unit)) = *wanted else { continue };
                let at = 4 + number * width;
                let half = |at: usize| contents.get(at..at + 2).map(|h| u16::from_le_bytes([h[0], h[1]]));
                let Some(value) = half(at).and_then(|h| decode(unit, h)) else { continue };
                let peak = if minutes { half(at + 2).and_then(|h| decode(unit, h)).unwrap_or(value) } else { value };
                if let Some(points) = out.get_mut(index) {
                    points.push((time, value, peak.max(value)));
                }
            }
        }
        _ => {}
    });
}

/// The points of `ids` between `from` and `to` (Unix times), oldest first, from the
/// every-second files or the every-minute ones; and each one's unit, where a file names it.
pub fn read(dir: &Path, ids: &[String], from: u64, to: u64, minutes: bool) -> (Vec<Points>, Vec<Option<Unit>>) {
    let extension = if minutes { MINUTES_EXTENSION } else { SECONDS_EXTENSION };
    let mut out = vec![Vec::new(); ids.len()];
    let mut units = vec![None; ids.len()];
    let mut day = from / 86_400 * 86_400;
    while day <= to {
        if let Ok(bytes) = std::fs::read(dir.join(format!("{}.{extension}", day_name(day)))) {
            parse(&bytes, minutes, ids, (from, to), &mut out, &mut units);
        }
        day += 86_400;
    }
    (out, units)
}

// ----- Writing (the service) -----

/// One day file being appended to.
#[derive(Debug)]
struct Log {
    file: File,
    /// Numbered series' ids (empty for one this version doesn't understand).
    names: Vec<String>,
    numbers: HashMap<String, usize>,
}

impl Log {
    /// Opens a day file: writes the header to a new one, learns an existing one's numbered
    /// series, and trims a partly written last frame (or replaces a file that isn't ours).
    fn open(path: &Path, magic: [u8; 4]) -> io::Result<Self> {
        let mut file = OpenOptions::new().read(true).write(true).create(true).truncate(false).open(path)?;
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)?;
        let mut names = Vec::new();
        let end = walk(&bytes, magic, |tag, contents| {
            if tag == NAME {
                names.push(parse_name(contents).map(|(id, _)| id).unwrap_or_default());
            }
        });
        match end {
            Some(end) => file.set_len(end as u64)?,
            None => {
                names.clear();
                file.set_len(0)?;
                file.rewind()?;
                file.write_all(&header(magic))?;
            }
        }
        file.seek(SeekFrom::End(0))?;
        let numbers =
            names.iter().enumerate().filter(|(_, id)| !id.is_empty()).map(|(n, id)| (id.clone(), n)).collect();
        Ok(Self { file, names, numbers })
    }

    /// Appends one sample of `values` (id, unit, value, peak), numbering new series first. With
    /// `peaks`, each series gets its peak after its value.
    fn append(&mut self, time: u32, values: &[(&str, Unit, f64, f64)], peaks: bool) -> io::Result<()> {
        let mut buffer = Vec::new();
        let frame = |buffer: &mut Vec<u8>, tag: u8, contents: &[u8]| {
            buffer.push(tag);
            buffer.extend_from_slice(&(contents.len() as u16).to_le_bytes());
            buffer.extend_from_slice(contents);
        };
        for (id, unit, ..) in values {
            if !self.numbers.contains_key(*id) && self.names.len() < MAX_SERIES && is_id(id) {
                let mut contents = vec![unit.code()];
                contents.extend_from_slice(id.as_bytes());
                frame(&mut buffer, NAME, &contents);
                self.numbers.insert((*id).to_owned(), self.names.len());
                self.names.push((*id).to_owned());
            }
        }
        let width = if peaks { 2 } else { 1 };
        let mut halves = vec![NAN16; self.names.len() * width];
        for (id, unit, value, peak) in values {
            if let Some(&number) = self.numbers.get(*id) {
                halves[number * width] = encode(*unit, *value);
                if peaks {
                    halves[number * width + 1] = encode(*unit, *peak);
                }
            }
        }
        let mut contents = time.to_le_bytes().to_vec();
        for half in halves {
            contents.extend_from_slice(&half.to_le_bytes());
        }
        frame(&mut buffer, SAMPLE, &contents);
        self.file.write_all(&buffer)
    }
}

/// Appends samples to the day's `.series` and `.series-minutes` files, starting new ones at
/// midnight UTC.
#[derive(Debug)]
pub struct SeriesWriter {
    dir: PathBuf,
    day: String,
    seconds: Option<Log>,
    minutes: Option<Log>,
}

impl SeriesWriter {
    pub fn new(dir: &Path) -> Self {
        Self { dir: dir.to_path_buf(), day: String::new(), seconds: None, minutes: None }
    }

    /// One second's values: `(id, unit, value)`, NaN for none.
    pub fn write_second(&mut self, time: u64, values: &[(&str, Unit, f64)]) -> io::Result<()> {
        let values: Vec<(&str, Unit, f64, f64)> = values.iter().map(|&(id, unit, v)| (id, unit, v, v)).collect();
        self.write(time, &values, false)
    }

    /// One minute's averages and peaks: `(id, unit, average, peak)`.
    pub fn write_minute(&mut self, time: u64, values: &[(&str, Unit, f64, f64)]) -> io::Result<()> {
        self.write(time, values, true)
    }

    fn write(&mut self, time: u64, values: &[(&str, Unit, f64, f64)], minutes: bool) -> io::Result<()> {
        let day = day_name(time);
        if day != self.day {
            self.seconds = None;
            self.minutes = None;
            self.day = day;
        }
        let (slot, magic, extension) = if minutes {
            (&mut self.minutes, MINUTES_MAGIC, MINUTES_EXTENSION)
        } else {
            (&mut self.seconds, SECONDS_MAGIC, SECONDS_EXTENSION)
        };
        if slot.is_none() {
            *slot = Some(Log::open(&self.dir.join(format!("{}.{extension}", self.day)), magic)?);
        }
        let Some(log) = slot.as_mut() else { return Ok(()) };
        let result = log.append(time as u32, values, minutes);
        if result.is_err() {
            // Part of a frame may have been written: open the file again (trimming it) next time,
            // so what's numbered matches what's on disk.
            *slot = None;
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn half_floats() {
        assert_eq!(f16_from_f32(1.0), 0x3c00);
        assert_eq!(f16_from_f32(-2.0), 0xc000);
        assert_eq!(f16_from_f32(65504.0), 0x7bff);
        assert_eq!(f16_from_f32(1e6), 0x7c00);
        assert_eq!(f16_from_f32(6.103_515_6e-5), 0x0400);
        assert_eq!(f16_from_f32(0.0), 0);
        assert_eq!(f16_from_f32(1e-9), 0);
        assert!(f32_from_f16(f16_from_f32(f32::NAN)).is_nan());
        // Ties round to even; 1 + 2^-11 is halfway between 1 and the next half.
        assert_eq!(f16_from_f32(1.0 + 2f32.powi(-11)), 0x3c00);
        assert_eq!(f16_from_f32(1.0 + 3.0 * 2f32.powi(-11)), 0x3c02);
        for value in [0.0f32, 0.5, 1.0, 71.25, 0.123, 1234.5, 3.0e-6, 60000.0] {
            let back = f32_from_f16(f16_from_f32(value));
            assert!((back - value).abs() <= value * 1.0e-3 + 1.0e-7, "{value} came back as {back}");
        }
    }

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("ferrotop-series-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        dir
    }

    #[test]
    fn series_round_trip_and_grow() {
        let dir = temp_dir("round-trip");
        let day = 20_000 * 86_400;
        let mut writer = SeriesWriter::new(&dir);
        writer.write_second(day + 1, &[("gpu.0.temp", Unit::Celsius, 61.0)]).expect("write");
        // A series that appears later, and one that's missing a value.
        let later =
            [("gpu.0.temp", Unit::Celsius, 62.5), ("gpu.0.dedicated", Unit::Bytes, 3.0 * f64::from(1u32 << 30))];
        writer.write_second(day + 2, &later).expect("write");
        writer.write_second(day + 3, &[("gpu.0.temp", Unit::Celsius, f64::NAN)]).expect("write");
        drop(writer);

        // Reopened (the service restarted): carries on with the same numbers.
        let mut writer = SeriesWriter::new(&dir);
        writer
            .write_second(day + 4, &[("gpu.0.dedicated", Unit::Bytes, 1.0), ("cpu.speed", Unit::Megahertz, 4501.0)])
            .expect("write");
        writer.write_minute(day, &[("gpu.0.temp", Unit::Celsius, 61.5, 62.5)]).expect("write");

        let ids = ["gpu.0.temp".to_owned(), "gpu.0.dedicated".to_owned(), "cpu.speed".to_owned(), "nope".to_owned()];
        let (points, units) = read(&dir, &ids, day, day + 10, false);
        assert_eq!(units, [Some(Unit::Celsius), Some(Unit::Bytes), Some(Unit::Megahertz), None]);
        assert_eq!(points[0], vec![(day as u32 + 1, 61.0, 61.0), (day as u32 + 2, 62.5, 62.5)]);
        assert_eq!(points[1].len(), 2);
        assert!((points[1][0].1 / f64::from(1u32 << 30) - 3.0).abs() < 1e-3);
        assert_eq!(points[2], vec![(day as u32 + 4, 4500.0, 4500.0)], "half floats keep 11 bits");
        assert!(points[3].is_empty());
        let (minutes, _) = read(&dir, &ids, day, day + 10, true);
        assert_eq!(minutes[0], vec![(day as u32, 61.5, 62.5)]);
        // Outside the range.
        assert!(read(&dir, &ids, day + 5, day + 10, false).0[0].is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_cut_short_frame_is_ignored_then_trimmed() {
        let dir = temp_dir("trim");
        let day = 20_000 * 86_400;
        let mut writer = SeriesWriter::new(&dir);
        writer.write_second(day + 1, &[("cpu.speed", Unit::Megahertz, 3000.0)]).expect("write");
        drop(writer);
        let path = dir.join(format!("{}.series", day_name(day)));
        let mut bytes = std::fs::read(&path).expect("read");
        bytes.extend_from_slice(&[SAMPLE, 50, 0, 1, 2]);
        std::fs::write(&path, &bytes).expect("write");
        let ids = ["cpu.speed".to_owned()];
        assert_eq!(read(&dir, &ids, day, day + 10, false).0[0].len(), 1);

        let mut writer = SeriesWriter::new(&dir);
        writer.write_second(day + 2, &[("cpu.speed", Unit::Megahertz, 3100.0)]).expect("write");
        assert_eq!(read(&dir, &ids, day, day + 10, false).0[0].len(), 2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn never_panics_on_damaged_files() {
        let ids = ["a".to_owned()];
        let mut out = vec![Vec::new()];
        let mut units = vec![None];
        let mut file = header(SECONDS_MAGIC).to_vec();
        file.extend_from_slice(&[
            NAME, 2, 0, 0, b'a', SAMPLE, 2, 0, 9, 9, NAME, 0, 0, SAMPLE, 6, 0, 1, 0, 0, 0, 0xff, 0xff,
        ]);
        let mut state = 0x1234_5678u32;
        for length in 0..file.len() {
            parse(&file[..length], false, &ids, (0, u64::MAX), &mut out, &mut units);
            parse(&file[..length], true, &ids, (0, u64::MAX), &mut out, &mut units);
            let mut noisy = file[..length].to_vec();
            for byte in noisy.iter_mut().skip(4) {
                state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                if state >> 28 == 0 {
                    *byte = (state >> 8) as u8;
                }
            }
            parse(&noisy, false, &ids, (0, u64::MAX), &mut out, &mut units);
        }
    }
}
