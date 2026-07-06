pub fn parse_port(value: &str) -> u16 {
    value.parse().unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_invalid_port() {
        assert!(parse_port("nope").is_err());
    }
}
