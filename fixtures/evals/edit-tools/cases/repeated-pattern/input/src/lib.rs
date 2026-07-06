pub fn normalize_status(status: &str) -> &'static str {
    match status {
        "queued" => "pending",
        "running" => "active",
        "finished" => "done",
        "failed" => "failed",
        _ => "unknown",
    }
}

pub fn visible_statuses() -> Vec<&'static str> {
    vec!["pending", "active", "done", "failed"]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_finished_to_complete() {
        assert_eq!(normalize_status("finished"), "complete");
        assert!(visible_statuses().contains(&"complete"));
    }
}
