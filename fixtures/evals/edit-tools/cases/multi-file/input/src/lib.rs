pub mod math;
pub mod report;

pub fn summary(values: &[i32]) -> String {
    report::format_total(math::sum(values))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uses_total_label() {
        assert_eq!(summary(&[2, 3]), "total=5");
    }
}
