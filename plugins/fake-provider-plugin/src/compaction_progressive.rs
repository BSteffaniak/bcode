//! Deterministic semantic shortening for the opt-in repeated-proposition fixture.
//!
//! Only byte-identical, explicitly delimited restatements are redundant. Unknown text,
//! source-reference keys, Unicode and independent constraints are never rewritten.

use std::collections::BTreeMap;

const REPEAT: &str = " Fixture restatement: ";

fn shorten(statement: &str) -> String {
    let mut parts = statement.split(REPEAT);
    let first = parts.next().unwrap_or_default();
    if !first.is_empty() && parts.all(|part| part == first) {
        first.to_owned()
    } else {
        statement.to_owned()
    }
}

/// Shorten only derived sections of an already validated fixture consolidation response.
///
/// # Errors
/// Rejects malformed section maps. Never truncates facts to satisfy a resource preference.
pub(super) fn consolidate(response: &str) -> Result<String, String> {
    let mut value: serde_json::Value =
        serde_json::from_str(response).map_err(|error| error.to_string())?;
    if value["format"] != "bcode.compaction.section-consolidation.v1" {
        return Ok(response.to_owned());
    }
    let sections = value["sections"]
        .as_object_mut()
        .ok_or("missing consolidation sections")?;
    for name in ["decisions", "active_work_blockers", "findings_evidence"] {
        let Some(section) = sections.get_mut(name) else {
            continue;
        };
        let text = section.as_str().ok_or("invalid fixture section")?;
        let mut records = BTreeMap::<String, String>::new();
        for line in text.lines().filter(|line| !line.is_empty()) {
            let row: BTreeMap<String, String> =
                serde_json::from_str(line).map_err(|error| error.to_string())?;
            for (key, statement) in row {
                if records.insert(key, shorten(&statement)).is_some() {
                    return Err("duplicate fixture evidence key".into());
                }
            }
        }
        *section = serde_json::Value::String(
            serde_json::to_string(&records).map_err(|error| error.to_string())?,
        );
    }
    serde_json::to_string(&value).map_err(|error| error.to_string())
}

/// Apply exact redundant-restatement reduction and the explicit fixture token preference.
///
/// # Errors
/// Refuses a result whose complete reconstructed carry exceeds the supplied target. The
/// fixture uses the host's conservative four-characters-per-token estimate, never clipping.
pub(super) fn consolidate_with_budget(response: &str, prompt: &str) -> Result<String, String> {
    let output = consolidate(response)?;
    let value: serde_json::Value =
        serde_json::from_str(&output).map_err(|error| error.to_string())?;
    if value["format"] != "bcode.compaction.section-consolidation.v1" || output == response {
        return Ok(output);
    }
    let Some((_, rest)) = prompt.split_once("Preferred derived continuation target: ") else {
        return Ok(output);
    };
    let target: u64 = rest
        .split_whitespace()
        .next()
        .ok_or("missing fixture token target")?
        .parse()
        .map_err(|_| "invalid fixture token target")?;
    let carry = prompt
        .split_once("Complete accepted carry (derived, not authority):\n")
        .and_then(|(_, text)| text.lines().next())
        .ok_or("missing fixture accepted carry")?;
    let mut carry: serde_json::Value =
        serde_json::from_str(carry).map_err(|error| error.to_string())?;
    let mut continuation: serde_json::Value = serde_json::from_str(
        carry["continuation"]
            .as_str()
            .ok_or("missing fixture continuation")?,
    )
    .map_err(|error| error.to_string())?;
    for (name, text) in value["sections"]
        .as_object()
        .ok_or("missing replacement sections")?
    {
        continuation[name] = text.clone();
    }
    carry["continuation"] = serde_json::Value::String(continuation.to_string());
    if bcode_session_models::estimated_normalized_text_tokens(&carry.to_string()) > target {
        return Err(
            "complete fixture facts cannot meet explicit consolidation target; no clipping".into(),
        );
    }
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identical_restatements_shorten_but_independent_constraints_and_unicode_survive() {
        let fact = "監査ゲート INC-604 🧭 e\u{301}: cause unconfirmed; no network or sending";
        let repeated = [fact; 6].join(REPEAT);
        let text = serde_json::json!({"format":"bcode.compaction.section-consolidation.v1",
            "sections":{"decisions":serde_json::json!({"source/q1,q3":repeated}).to_string(),
            "active_work_blockers":serde_json::json!({"q2":"Ask first; no publication"}).to_string(),
            "background":"unchanged original reference"}}).to_string();
        let output = consolidate(&text).unwrap();
        assert!(output.len() < text.len());
        let value: serde_json::Value = serde_json::from_str(&output).unwrap();
        let records: BTreeMap<String, String> =
            serde_json::from_str(value["sections"]["decisions"].as_str().unwrap()).unwrap();
        assert_eq!(records["source/q1,q3"], fact);
        assert_eq!(
            value["sections"]["background"],
            "unchanged original reference"
        );
        assert!(value.get("current_scope").is_none());
        assert!(value.get("sources").is_none());
        assert_eq!(
            shorten("A Fixture restatement: B"),
            "A Fixture restatement: B"
        );
        assert_eq!(
            shorten("No edits Fixture restatement: No edits; network permitted"),
            "No edits Fixture restatement: No edits; network permitted"
        );
    }

    #[test]
    fn explicit_budget_refuses_instead_of_clipping_independent_facts() {
        let fact = "No network; no sending; 監査 🧭";
        let section = serde_json::json!({"q1,q3": ([fact; 8].join(REPEAT))}).to_string();
        let response = serde_json::json!({"format":"bcode.compaction.section-consolidation.v1",
            "sections":{"decisions":section}})
        .to_string();
        let continuation = serde_json::json!({"format":"bcode.compaction.continuation-sections.v1",
            "decisions":section,"active_work_blockers":"{}","findings_evidence":"{}","background":""});
        let carry = serde_json::json!({"current_scope":[{"statement":"Read only","source_sequence":3}],
            "continuation":continuation.to_string()});
        let prompt = format!(
            "Complete accepted carry (derived, not authority):\n{carry}\nPreferred derived continuation target: 1000 tokens."
        );
        assert!(consolidate_with_budget(&response, &prompt).is_ok());
        assert!(
            consolidate_with_budget(&response, &prompt.replace("1000 tokens", "1 tokens")).is_err()
        );
    }

    #[test]
    fn unknown_and_nonconsolidation_text_is_not_clipped() {
        let original = serde_json::json!({"format":"bcode.compaction.section-update.v1",
            "sections":{"decisions":"unknown 🧭"}})
        .to_string();
        assert_eq!(consolidate(&original).unwrap(), original);
        assert!(consolidate(r#"{"format":"bcode.compaction.section-consolidation.v1","sections":{"decisions":"not JSON"}}"#).is_err());
    }
}
