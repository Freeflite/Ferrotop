/// A cumulative counter and how much it changed at the last update
/// (System Informer's `PH_UINT64_DELTA`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Delta {
    pub value: u64,
    pub delta: u64,
}

impl Delta {
    pub fn new(value: u64) -> Self {
        Self { value, delta: 0 }
    }

    /// Records a new reading. A counter that went backwards gives a zero delta, not a huge one.
    pub fn update(&mut self, value: u64) {
        self.delta = value.saturating_sub(self.value);
        self.value = value;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tracks_change_and_never_underflows() {
        let mut d = Delta::new(100);
        d.update(150);
        assert_eq!((d.value, d.delta), (150, 50));
        d.update(120);
        assert_eq!((d.value, d.delta), (120, 0));
    }
}
