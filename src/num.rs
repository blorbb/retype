#[derive(Debug, Clone, Copy)]
pub struct IncrementalU16(u16);

impl IncrementalU16 {
    pub fn new() -> Self {
        Self(0)
    }

    pub fn push_digit(&mut self, digit: u8) {
        self.0 = self.0.saturating_mul(10).saturating_add(u16::from(digit))
    }

    pub fn value(&self) -> u16 {
        self.0
    }

    pub fn clear(&mut self) {
        self.0 = 0
    }
}
