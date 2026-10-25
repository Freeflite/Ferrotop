use std::collections::VecDeque;
use std::fmt::Write;

/// A fixed-size window of recent samples, oldest first. Values are raw (a fraction, bytes per
/// second, ...); graphs scale them when drawing.
#[derive(Debug, Clone)]
pub struct History {
    samples: VecDeque<f64>,
    capacity: usize,
}

impl History {
    pub fn new(capacity: usize) -> Self {
        Self { samples: VecDeque::with_capacity(capacity), capacity: capacity.max(2) }
    }

    pub fn push(&mut self, value: f64) {
        if self.samples.len() == self.capacity {
            self.samples.pop_front();
        }
        self.samples.push_back(if value.is_finite() { value.max(0.0) } else { 0.0 });
    }

    pub fn latest(&self) -> Option<f64> {
        self.samples.back().copied()
    }

    /// The largest sample in the window (0 when empty).
    pub fn peak(&self) -> f64 {
        self.samples.iter().copied().fold(0.0, f64::max)
    }

    pub fn len(&self) -> usize {
        self.samples.len()
    }

    pub fn is_empty(&self) -> bool {
        self.samples.is_empty()
    }

    /// SVG path commands for a `width` x `height` viewbox: `(line, filled area)`, with `max` at
    /// the top edge. The newest sample sits at the right edge, so the graph scrolls left as it fills.
    pub fn svg_paths(&self, width: f32, height: f32, max: f64) -> (String, String) {
        let step = width / (self.capacity - 1) as f32;
        let start = (self.capacity - self.samples.len()) as f32 * step;
        let mut line = String::new();
        for (i, value) in self.samples.iter().enumerate() {
            let fraction = if max > 0.0 { (value / max).clamp(0.0, 1.0) as f32 } else { 0.0 };
            let x = start + i as f32 * step;
            let y = height - fraction * height;
            let _ = write!(line, "{}{x:.2} {y:.2} ", if i == 0 { "M " } else { "L " });
        }
        if line.is_empty() {
            let baseline = format!("M 0 {height:.2} L {width:.2} {height:.2}");
            return (baseline.clone(), format!("{baseline} Z"));
        }
        let fill = format!("{line}L {width:.2} {height:.2} L {start:.2} {height:.2} Z");
        (line.trim_end().to_owned(), fill)
    }
}

/// A "nice" graph ceiling at or above `value`: 1, 2 or 5 times a power of ten.
pub fn nice_ceiling(value: f64) -> f64 {
    if !(value > 0.0 && value.is_finite()) {
        return 1.0;
    }
    // Start a decade low (powi is exact, log10 may not be) and step up to the first fit.
    let mut magnitude = 10f64.powi(value.log10().floor() as i32 - 1);
    loop {
        if let Some(ceiling) = [1.0, 2.0, 5.0].into_iter().map(|m| m * magnitude).find(|&c| c >= value) {
            return ceiling;
        }
        magnitude *= 10.0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_the_newest_samples() {
        let mut h = History::new(3);
        for v in [0.1, 0.2, 0.3, 0.4] {
            h.push(v);
        }
        assert_eq!(h.len(), 3);
        assert_eq!(h.latest(), Some(0.4));
        assert_eq!(h.peak(), 0.4);
    }

    #[test]
    fn paths_end_at_the_right_edge_and_scale_to_max() {
        let mut h = History::new(5);
        h.push(0.0);
        h.push(500.0);
        let (line, fill) = h.svg_paths(200.0, 50.0, 1000.0);
        assert_eq!(line, "M 150.00 50.00 L 200.00 25.00");
        assert!(fill.ends_with("L 200.00 50.00 L 150.00 50.00 Z"));
    }

    #[test]
    fn nice_ceilings() {
        assert_eq!(nice_ceiling(0.0), 1.0);
        assert_eq!(nice_ceiling(3.0), 5.0);
        assert_eq!(nice_ceiling(120.0), 200.0);
        assert_eq!(nice_ceiling(1_000.0), 1_000.0);
        assert_eq!(nice_ceiling(6_500_000.0), 10_000_000.0);
    }
}
