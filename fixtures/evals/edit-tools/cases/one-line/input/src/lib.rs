pub const TIMEOUT_MS: u64 = 1000;

pub fn timeout_ms() -> u64 {
    TIMEOUT_MS
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timeout_is_updated() {
        assert_eq!(timeout_ms(), 1500);
    }
}
