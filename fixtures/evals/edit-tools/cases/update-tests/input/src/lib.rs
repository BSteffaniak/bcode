pub fn greeting(name: &str) -> String {
    format!("hello, {name}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn greets_loudly() {
        assert_eq!(greeting("bcode"), "HELLO, bcode");
    }
}
