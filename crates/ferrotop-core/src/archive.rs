//! The history the Ferrotop service records, and reading it back for the History page.
//!
//! Each UTC day gets two append-only files in the service's folder: `YYYY-MM-DD.seconds`, one
//! fixed-size record of system-wide figures per second, and `YYYY-MM-DD.minutes`, the busiest
//! processes of each minute. A record cut short (by a crash, or because it's being written
//! right now) is ignored by readers and trimmed off by the writer before it appends.

use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

/// How many days of history are kept.
pub const KEEP_DAYS: u64 = 7;
/// The every-second `.series` files are big (a value for every sensor), so only today's and
/// yesterday's are kept; older days have their every-minute files.
pub const KEEP_SERIES_SECONDS_DAYS: u64 = 1;
const SECONDS_MAGIC: [u8; 4] = *b"FTHS";
const MINUTES_MAGIC: [u8; 4] = *b"FTHM";
const VERSION: u32 = 1;
const HEADER_SIZE: usize = 16;
const SECOND_SIZE: usize = 48;
/// The most processes kept per minute, and the size of each one's entry.
pub const MAX_MINUTE_ENTRIES: usize = 12;
const ENTRY_SIZE: usize = 64;
const NAME_SIZE: usize = 40;
const MINUTE_SIZE: usize = 8 + MAX_MINUTE_ENTRIES * ENTRY_SIZE;
const DAY: u64 = 86_400;

/// System-wide figures for one second.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct SecondRecord {
    /// Unix time.
    pub time: u32,
    /// Fraction of all CPUs busy, `0..=1`.
    pub cpu: f32,
    pub memory_used: u64,
    pub commit: u64,
    /// Bytes per second, all disks and all network adapters.
    pub disk_read: f32,
    pub disk_write: f32,
    pub sent: f32,
    pub received: f32,
    /// The busiest GPU engine, `0..=1`.
    pub gpu: f32,
    /// °C; NaN when unknown.
    pub cpu_temp: f32,
}

/// One process's figures over a minute.
#[derive(Debug, Clone, PartialEq)]
pub struct MinuteEntry {
    pub pid: u32,
    pub name: String,
    /// Average share of all CPUs, `0..=1`.
    pub cpu: f32,
    /// The most private memory it held.
    pub private_bytes: u64,
    /// Average bytes per second.
    pub disk: f32,
    pub network: f32,
}

/// The busiest processes of one minute.
#[derive(Debug, Clone, PartialEq)]
pub struct MinuteRecord {
    /// Unix time of the minute's start.
    pub time: u32,
    pub entries: Vec<MinuteEntry>,
}

// ----- Encoding -----

fn header(magic: [u8; 4], record_size: usize) -> [u8; HEADER_SIZE] {
    let mut bytes = [0u8; HEADER_SIZE];
    bytes[..4].copy_from_slice(&magic);
    bytes[4..8].copy_from_slice(&VERSION.to_le_bytes());
    bytes[8..12].copy_from_slice(&(record_size as u32).to_le_bytes());
    bytes
}

pub(crate) fn encode_second(r: &SecondRecord) -> [u8; SECOND_SIZE] {
    let mut out = [0u8; SECOND_SIZE];
    let mut at = 0;
    let mut put = |bytes: &[u8]| {
        out[at..at + bytes.len()].copy_from_slice(bytes);
        at += bytes.len();
    };
    put(&r.time.to_le_bytes());
    put(&r.cpu.to_le_bytes());
    put(&r.memory_used.to_le_bytes());
    put(&r.commit.to_le_bytes());
    for value in [r.disk_read, r.disk_write, r.sent, r.received, r.gpu, r.cpu_temp] {
        put(&value.to_le_bytes());
    }
    out
}

pub(crate) fn encode_minute(r: &MinuteRecord) -> Vec<u8> {
    let mut out = vec![0u8; MINUTE_SIZE];
    out[..4].copy_from_slice(&r.time.to_le_bytes());
    let entries = &r.entries[..r.entries.len().min(MAX_MINUTE_ENTRIES)];
    out[4..8].copy_from_slice(&(entries.len() as u32).to_le_bytes());
    for (i, entry) in entries.iter().enumerate() {
        let at = 8 + i * ENTRY_SIZE;
        out[at..at + 4].copy_from_slice(&entry.pid.to_le_bytes());
        out[at + 4..at + 8].copy_from_slice(&entry.cpu.to_le_bytes());
        out[at + 8..at + 16].copy_from_slice(&entry.private_bytes.to_le_bytes());
        out[at + 16..at + 20].copy_from_slice(&entry.disk.to_le_bytes());
        out[at + 20..at + 24].copy_from_slice(&entry.network.to_le_bytes());
        // The name, cut at a character boundary to fit.
        let mut end = entry.name.len().min(NAME_SIZE);
        while !entry.name.is_char_boundary(end) {
            end -= 1;
        }
        out[at + 24..at + 24 + end].copy_from_slice(&entry.name.as_bytes()[..end]);
    }
    out
}

// ----- Decoding (untrusted input: never panics) -----

fn u32_at(bytes: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(bytes.get(at..at + 4)?.try_into().ok()?))
}

fn u64_at(bytes: &[u8], at: usize) -> Option<u64> {
    Some(u64::from_le_bytes(bytes.get(at..at + 8)?.try_into().ok()?))
}

fn f32_at(bytes: &[u8], at: usize) -> Option<f32> {
    u32_at(bytes, at).map(f32::from_bits)
}

/// A fraction, rate or temperature kept only if it's a sensible number.
fn sane(value: f32, max: f32) -> f32 {
    if value.is_finite() && (0.0..=max).contains(&value) { value } else { 0.0 }
}

/// The record size a file declares, if its header is ours.
fn record_size(bytes: &[u8], magic: [u8; 4], minimum: usize) -> Option<usize> {
    if bytes.get(..4)? != magic || u32_at(bytes, 4)? != VERSION {
        return None;
    }
    // A later version may add fields to the end of each record.
    Some(u32_at(bytes, 8)? as usize).filter(|&size| (minimum..=4096).contains(&size))
}

/// Every whole record in a `.seconds` file.
pub fn parse_seconds(bytes: &[u8]) -> Vec<SecondRecord> {
    let Some(size) = record_size(bytes, SECONDS_MAGIC, SECOND_SIZE) else { return Vec::new() };
    let Some(body) = bytes.get(HEADER_SIZE..) else { return Vec::new() };
    body.chunks_exact(size)
        .filter_map(|r| {
            Some(SecondRecord {
                time: u32_at(r, 0)?,
                cpu: sane(f32_at(r, 4)?, 1.0),
                memory_used: u64_at(r, 8)?,
                commit: u64_at(r, 16)?,
                disk_read: sane(f32_at(r, 24)?, f32::MAX),
                disk_write: sane(f32_at(r, 28)?, f32::MAX),
                sent: sane(f32_at(r, 32)?, f32::MAX),
                received: sane(f32_at(r, 36)?, f32::MAX),
                gpu: sane(f32_at(r, 40)?, 1.0),
                cpu_temp: Some(f32_at(r, 44)?)
                    .filter(|t| t.is_finite() && (-50.0..=200.0).contains(t))
                    .unwrap_or(f32::NAN),
            })
        })
        .collect()
}

/// Every whole record in a `.minutes` file.
pub fn parse_minutes(bytes: &[u8]) -> Vec<MinuteRecord> {
    let Some(size) = record_size(bytes, MINUTES_MAGIC, MINUTE_SIZE) else { return Vec::new() };
    let Some(body) = bytes.get(HEADER_SIZE..) else { return Vec::new() };
    body.chunks_exact(size)
        .filter_map(|r| {
            let count = (u32_at(r, 4)? as usize).min(MAX_MINUTE_ENTRIES);
            let entries = (0..count)
                .filter_map(|i| {
                    let at = 8 + i * ENTRY_SIZE;
                    let name = r.get(at + 24..at + 24 + NAME_SIZE)?;
                    let name = &name[..name.iter().position(|&b| b == 0).unwrap_or(NAME_SIZE)];
                    Some(MinuteEntry {
                        pid: u32_at(r, at)?,
                        name: String::from_utf8_lossy(name).into_owned(),
                        cpu: sane(f32_at(r, at + 4)?, 1.0),
                        private_bytes: u64_at(r, at + 8)?,
                        disk: sane(f32_at(r, at + 16)?, f32::MAX),
                        network: sane(f32_at(r, at + 20)?, f32::MAX),
                    })
                })
                .collect();
            Some(MinuteRecord { time: u32_at(r, 0)?, entries })
        })
        .collect()
}

// ----- Dates -----

/// `YYYY-MM-DD` (UTC) for a Unix time.
pub fn day_name(unix: u64) -> String {
    let (year, month, day) = civil_from_days((unix / DAY) as i64);
    format!("{year:04}-{month:02}-{day:02}")
}

/// Days since 1970-01-01 to a (year, month, day) date (Howard Hinnant's algorithm).
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let year = yoe + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

pub fn now_unix() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

// ----- Writing (the service) -----

/// Appends records to the day's files, starting new ones at midnight UTC.
#[derive(Debug)]
pub struct ArchiveWriter {
    dir: PathBuf,
    day: String,
    seconds: File,
    minutes: File,
}

impl ArchiveWriter {
    pub fn open(dir: &Path, now: u64) -> io::Result<Self> {
        let day = day_name(now);
        Ok(Self {
            seconds: open_for_append(&dir.join(format!("{day}.seconds")), SECONDS_MAGIC, SECOND_SIZE)?,
            minutes: open_for_append(&dir.join(format!("{day}.minutes")), MINUTES_MAGIC, MINUTE_SIZE)?,
            dir: dir.to_path_buf(),
            day,
        })
    }

    fn roll(&mut self, time: u64) -> io::Result<()> {
        if day_name(time) != self.day {
            *self = Self::open(&self.dir, time)?;
        }
        Ok(())
    }

    pub fn write_second(&mut self, record: &SecondRecord) -> io::Result<()> {
        self.roll(u64::from(record.time))?;
        self.seconds.write_all(&encode_second(record))
    }

    pub fn write_minute(&mut self, record: &MinuteRecord) -> io::Result<()> {
        self.roll(u64::from(record.time))?;
        self.minutes.write_all(&encode_minute(record))
    }
}

/// Opens a day file for appending: writes the header to a new one, and trims a partly written
/// last record (or replaces a file that isn't ours) so new records stay aligned.
fn open_for_append(path: &Path, magic: [u8; 4], size: usize) -> io::Result<File> {
    let mut file = OpenOptions::new().read(true).write(true).create(true).truncate(false).open(path)?;
    let mut head = [0u8; HEADER_SIZE];
    let len = file.metadata()?.len();
    let valid = len >= HEADER_SIZE as u64
        && std::io::Read::read_exact(&mut file, &mut head).is_ok()
        && record_size(&head, magic, size) == Some(size);
    if valid {
        let whole = (len - HEADER_SIZE as u64) / size as u64 * size as u64 + HEADER_SIZE as u64;
        file.set_len(whole)?;
    } else {
        file.set_len(0)?;
        std::io::Seek::rewind(&mut file)?;
        file.write_all(&header(magic, size))?;
    }
    std::io::Seek::seek(&mut file, std::io::SeekFrom::End(0))?;
    Ok(file)
}

/// Deletes day files older than [`KEEP_DAYS`] (every-second `.series` files sooner).
pub fn prune(dir: &Path, now: u64) {
    let oldest = day_name(now.saturating_sub(KEEP_DAYS * DAY));
    let oldest_series = day_name(now.saturating_sub(KEEP_SERIES_SECONDS_DAYS * DAY));
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let Some((day, extension)) = name.split_once('.') else { continue };
        let keep_from = match extension {
            "seconds" | "minutes" | "series-minutes" => &oldest,
            "series" => &oldest_series,
            _ => continue,
        };
        if day.len() == 10 && day < keep_from.as_str() {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

// ----- Reading (the History page) -----

/// The day files that may hold records between `from` and `to`.
fn day_files(dir: &Path, from: u64, to: u64, extension: &str) -> Vec<PathBuf> {
    let mut days = Vec::new();
    let mut day = from / DAY * DAY;
    while day <= to && days.len() < 64 {
        days.push(dir.join(format!("{}.{extension}", day_name(day))));
        day += DAY;
    }
    days
}

/// The per-second records between `from` and `to` (Unix times), oldest first.
pub fn read_seconds(dir: &Path, from: u64, to: u64) -> Vec<SecondRecord> {
    day_files(dir, from, to, "seconds")
        .iter()
        .filter_map(|path| std::fs::read(path).ok())
        .flat_map(|bytes| parse_seconds(&bytes))
        .filter(|r| (from..=to).contains(&u64::from(r.time)))
        .collect()
}

/// The per-minute records between `from` and `to`, oldest first.
pub fn read_minutes(dir: &Path, from: u64, to: u64) -> Vec<MinuteRecord> {
    day_files(dir, from, to, "minutes")
        .iter()
        .filter_map(|path| std::fs::read(path).ok())
        .flat_map(|bytes| parse_minutes(&bytes))
        .filter(|r| (from..=to).contains(&u64::from(r.time)))
        .collect()
}

/// Averages `records` into `points` equal slices of `from..to`, for a graph; `None` where
/// nothing was recorded (the PC was off, or the service stopped).
pub fn buckets(records: &[SecondRecord], from: u64, to: u64, points: usize) -> Vec<Option<SecondRecord>> {
    let span = to.saturating_sub(from).max(1);
    let mut sums: Vec<(SecondRecord, f64, f64, u32)> = vec![(SecondRecord::default(), 0.0, 0.0, 0); points.max(1)];
    for r in records {
        let Some(offset) = u64::from(r.time).checked_sub(from).filter(|o| *o < span) else { continue };
        let slot = &mut sums[(offset as u128 * points as u128 / span as u128) as usize];
        let (sum, memory, commit, count) = slot;
        sum.cpu += r.cpu;
        *memory += r.memory_used as f64;
        *commit += r.commit as f64;
        sum.disk_read += r.disk_read;
        sum.disk_write += r.disk_write;
        sum.sent += r.sent;
        sum.received += r.received;
        sum.gpu += r.gpu;
        if r.cpu_temp.is_finite() {
            // Temperatures are averaged separately below; keep the hottest instead, which is
            // what matters over a long slice.
            sum.cpu_temp = if sum.cpu_temp.is_finite() { sum.cpu_temp.max(r.cpu_temp) } else { r.cpu_temp };
        } else if *count == 0 {
            sum.cpu_temp = f32::NAN;
        }
        *count += 1;
    }
    sums.into_iter()
        .enumerate()
        .map(|(i, (sum, memory, commit, count))| {
            (count > 0).then(|| {
                let n = count as f32;
                SecondRecord {
                    time: (from + i as u64 * span / points.max(1) as u64) as u32,
                    cpu: sum.cpu / n,
                    memory_used: (memory / f64::from(count)) as u64,
                    commit: (commit / f64::from(count)) as u64,
                    disk_read: sum.disk_read / n,
                    disk_write: sum.disk_write / n,
                    sent: sum.sent / n,
                    received: sum.received / n,
                    gpu: sum.gpu / n,
                    cpu_temp: sum.cpu_temp,
                }
            })
        })
        .collect()
}

/// One program's activity over a stretch of history.
#[derive(Debug, Clone, PartialEq)]
pub struct ProgramSummary {
    pub name: String,
    /// Average share of all CPUs over the minutes it was among the busiest.
    pub cpu: f32,
    pub peak_cpu: f32,
    pub peak_private_bytes: u64,
    /// Total bytes over the stretch.
    pub disk_bytes: f64,
    pub network_bytes: f64,
    /// Minutes it was among the busiest.
    pub minutes: u32,
}

/// Sums the busiest-process minutes by program name (PIDs change from run to run), busiest by
/// CPU first.
pub fn busiest(minutes: &[MinuteRecord]) -> Vec<ProgramSummary> {
    let mut by_name: std::collections::HashMap<&str, ProgramSummary> = std::collections::HashMap::new();
    for minute in minutes {
        // Several processes of one program (browsers) in a minute add up.
        let mut this_minute: std::collections::HashMap<&str, (f32, u64, f64, f64)> = std::collections::HashMap::new();
        for entry in &minute.entries {
            let slot = this_minute.entry(entry.name.as_str()).or_default();
            slot.0 += entry.cpu;
            slot.1 = slot.1.max(entry.private_bytes);
            slot.2 += f64::from(entry.disk) * 60.0;
            slot.3 += f64::from(entry.network) * 60.0;
        }
        for (name, (cpu, private, disk, network)) in this_minute {
            let summary = by_name.entry(name).or_insert_with(|| ProgramSummary {
                name: name.to_owned(),
                cpu: 0.0,
                peak_cpu: 0.0,
                peak_private_bytes: 0,
                disk_bytes: 0.0,
                network_bytes: 0.0,
                minutes: 0,
            });
            summary.cpu += cpu;
            summary.peak_cpu = summary.peak_cpu.max(cpu);
            summary.peak_private_bytes = summary.peak_private_bytes.max(private);
            summary.disk_bytes += disk;
            summary.network_bytes += network;
            summary.minutes += 1;
        }
    }
    let total_minutes = minutes.len().max(1) as f32;
    let mut summaries: Vec<ProgramSummary> = by_name
        .into_values()
        .map(|mut s| {
            // Averaged over the whole stretch, so a brief spike doesn't outrank steady use.
            s.cpu /= total_minutes;
            s
        })
        .collect();
    summaries.sort_by(|a, b| b.cpu.total_cmp(&a.cpu).then_with(|| a.name.cmp(&b.name)));
    summaries
}

#[cfg(test)]
mod tests {
    use super::*;

    fn second(time: u32, cpu: f32) -> SecondRecord {
        SecondRecord {
            time,
            cpu,
            memory_used: 1 << 30,
            commit: 2 << 30,
            disk_read: 100.0,
            cpu_temp: 50.0,
            ..SecondRecord::default()
        }
    }

    fn entry(name: &str, cpu: f32) -> MinuteEntry {
        MinuteEntry { pid: 1, name: name.into(), cpu, private_bytes: 1 << 20, disk: 10.0, network: 0.0 }
    }

    #[test]
    fn day_names_are_utc_dates() {
        assert_eq!(day_name(0), "1970-01-01");
        assert_eq!(day_name(951_782_400), "2000-02-29");
        assert_eq!(day_name(1_792_972_799), "2026-10-25");
        assert_eq!(day_name(1_792_972_800), "2026-10-26");
    }

    #[test]
    fn records_round_trip_and_bad_files_read_as_empty() {
        let records = [second(10, 0.25), second(11, 0.5)];
        let mut file = header(SECONDS_MAGIC, SECOND_SIZE).to_vec();
        for r in &records {
            file.extend_from_slice(&encode_second(r));
        }
        file.extend_from_slice(&[1, 2, 3]); // a record cut short
        assert_eq!(parse_seconds(&file), records);
        assert!(parse_seconds(b"junk").is_empty());
        assert!(parse_seconds(&header(SECONDS_MAGIC, SECOND_SIZE)[..13]).is_empty(), "a header cut short");
        assert!(parse_seconds(&header(MINUTES_MAGIC, SECOND_SIZE)).is_empty(), "wrong kind of file");

        let long_name = "a-very-long-process-name-that-runs-past-forty-bytes.exe";
        let minute = MinuteRecord { time: 60, entries: vec![entry("chrome.exe", 0.1), entry(long_name, 0.02)] };
        let mut file = header(MINUTES_MAGIC, MINUTE_SIZE).to_vec();
        file.extend_from_slice(&encode_minute(&minute));
        let parsed = parse_minutes(&file);
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].entries[0], minute.entries[0]);
        assert_eq!(parsed[0].entries[1].name, &long_name[..40]);
    }

    #[test]
    fn writer_appends_trims_and_rolls_over_days() {
        let dir = std::env::temp_dir().join(format!("ferrotop-archive-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("dir");
        let midnight = 1_792_886_400u64;
        let mut writer = ArchiveWriter::open(&dir, midnight - 2).expect("open");
        writer.write_second(&second((midnight - 2) as u32, 0.1)).expect("write");
        writer.write_second(&second((midnight - 1) as u32, 0.2)).expect("write");
        writer.write_second(&second(midnight as u32, 0.3)).expect("write");
        writer
            .write_minute(&MinuteRecord { time: (midnight - 60) as u32, entries: vec![entry("a.exe", 0.5)] })
            .expect("minute");
        drop(writer);
        // Simulate a crash mid-record, then reopen: the fragment must not misalign what follows.
        let yesterday = dir.join(format!("{}.seconds", day_name(midnight - 1)));
        std::fs::OpenOptions::new()
            .append(true)
            .open(&yesterday)
            .and_then(|mut f| f.write_all(&[9; 20]))
            .expect("garble");
        let mut writer = ArchiveWriter::open(&dir, midnight - 1).expect("reopen");
        writer.write_second(&second((midnight - 1) as u32, 0.4)).expect("write");
        drop(writer);

        let all = read_seconds(&dir, midnight - 10, midnight + 10);
        assert_eq!(all.iter().map(|r| r.cpu).collect::<Vec<_>>(), [0.1, 0.2, 0.4, 0.3]);
        assert_eq!(read_minutes(&dir, midnight - 3600, midnight).len(), 1);

        // Old days are pruned, recent ones kept.
        prune(&dir, midnight + KEEP_DAYS * DAY + 10);
        assert!(read_seconds(&dir, midnight - 10, midnight - 1).is_empty());
        assert_eq!(read_seconds(&dir, midnight, midnight + 10).len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn never_panics_on_damaged_files() {
        let mut seconds = header(SECONDS_MAGIC, SECOND_SIZE).to_vec();
        seconds.extend_from_slice(&encode_second(&second(1, 0.5)));
        crate::fuzz::mutate(&seconds, 5_000, |bytes| {
            for record in parse_seconds(bytes) {
                assert!((0.0..=1.0).contains(&record.cpu) && record.disk_read >= 0.0);
            }
        });
        let mut minutes = header(MINUTES_MAGIC, MINUTE_SIZE).to_vec();
        minutes.extend_from_slice(&encode_minute(&MinuteRecord { time: 0, entries: vec![entry("x.exe", 0.5)] }));
        crate::fuzz::mutate(&minutes, 5_000, |bytes| {
            for record in parse_minutes(bytes) {
                assert!(record.entries.len() <= MAX_MINUTE_ENTRIES);
            }
        });
    }

    #[test]
    fn buckets_average_and_leave_gaps() {
        let records = [second(0, 0.2), second(1, 0.4), second(9, 1.0)];
        let points = buckets(&records, 0, 10, 5);
        assert_eq!(points.len(), 5);
        assert!((points[0].expect("first").cpu - 0.3).abs() < 1e-6);
        assert!(points[1].is_none() && points[2].is_none() && points[3].is_none());
        assert_eq!(points[4].expect("last").cpu, 1.0);
        assert_eq!(points[0].expect("first").cpu_temp, 50.0);
    }

    #[test]
    fn busiest_groups_by_program() {
        let minutes = [
            MinuteRecord {
                time: 0,
                entries: vec![entry("chrome.exe", 0.2), entry("chrome.exe", 0.1), entry("code.exe", 0.05)],
            },
            MinuteRecord { time: 60, entries: vec![entry("code.exe", 0.4)] },
        ];
        let summary = busiest(&minutes);
        assert_eq!(summary[0].name, "code.exe");
        assert!((summary[0].cpu - 0.225).abs() < 1e-6);
        assert!((summary[0].peak_cpu - 0.4).abs() < 1e-6);
        assert_eq!(summary[1].name, "chrome.exe");
        assert!((summary[1].peak_cpu - 0.3).abs() < 1e-6, "processes of one program add up");
        assert_eq!(summary[1].minutes, 1);
    }
}
