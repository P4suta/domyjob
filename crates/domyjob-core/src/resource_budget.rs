#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Budget {
    pub concurrent: u32,
    pub high: u64,
    pub max: u64,
    pub swap: u64,
}

impl Budget {
    #[must_use]
    pub const fn valid(self) -> bool {
        self.concurrent > 0
            && self.concurrent <= 2
            && self.high > 0
            && self.high < self.max
            && self.max <= 11 * 1024 * 1024 * 1024
            && self.swap <= 2 * 1024 * 1024 * 1024
    }
}
