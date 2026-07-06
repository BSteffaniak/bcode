pub fn generated_value() -> &'static str {
    include_str!("../generated/value.txt").trim()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uses_source_not_generated() {
        assert_eq!(generated_value(), "source-owned");
    }
}
