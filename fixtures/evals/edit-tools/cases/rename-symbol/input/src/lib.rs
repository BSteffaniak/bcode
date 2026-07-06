pub fn old_name(value: i32) -> i32 {
    value + 1
}

pub fn compute(values: &[i32]) -> i32 {
    values.iter().map(|value| old_name(*value)).sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn computes_values() {
        assert_eq!(compute(&[1, 2, 3]), 9);
        assert_eq!(old_name(4), 5);
    }
}
