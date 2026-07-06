pub fn table() -> Vec<(&'static str, i32)> {
    vec![
        ("alpha",   1),
        ("beta",   2),
        ("gamma",  3),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn updates_beta_only() {
        assert_eq!(table()[1], ("beta", 20));
    }
}
