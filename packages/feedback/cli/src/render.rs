//! Human and JSON output. Human output is for the person, who sees sensitive values.

use std::fmt::Write as _;

use bcode_feedback_models::{FeedbackItem, FeedbackPage, FeedbackStatus, TriageState};
use serde_json::json;

pub fn submitted(
    item: &FeedbackItem,
    session: Option<&str>,
    reply: Option<&str>,
    json: bool,
) -> Result<(), String> {
    if json {
        return print_json(&json!({
            "item": item,
            "intake_session_id": session,
            "reply": reply,
        }));
    }
    println!("Recorded {} ({})", item.id, item.app);
    if let Some(reply) = reply {
        println!("\n{reply}");
    } else if session.is_some() {
        println!("Sent to the intake agent for triage.");
    }
    Ok(())
}

pub fn page(page: &FeedbackPage, json: bool) -> Result<(), String> {
    if json {
        return print_json(page);
    }
    if page.items.is_empty() {
        println!("No feedback matches.");
    }
    for item in &page.items {
        println!("{}", line(item));
    }
    if let Some(before) = page.next_before {
        println!("\nMore: --before {before}");
    }
    Ok(())
}

pub fn inbox(
    resolved: &[FeedbackItem],
    questions: &[FeedbackItem],
    json: bool,
) -> Result<(), String> {
    if json {
        return print_json(&json!({ "to_verify": resolved, "open_questions": questions }));
    }
    if resolved.is_empty() && questions.is_empty() {
        println!("Nothing waiting on you.");
        return Ok(());
    }
    if !resolved.is_empty() {
        println!("Resolved by an agent; verify with `feedback verify <id>` or reopen:");
        for item in resolved {
            println!("  {}", line(item));
            if let Some(note) = item
                .status_change
                .as_ref()
                .and_then(|change| change.note.as_deref())
            {
                println!("      {note}");
            }
        }
    }
    if !questions.is_empty() {
        if !resolved.is_empty() {
            println!();
        }
        println!("Questions from triage:");
        for item in questions {
            println!("  {}", line(item));
            for question in item.triage.iter().flat_map(|triage| &triage.open_questions) {
                println!("      ? {question}");
            }
        }
    }
    Ok(())
}

pub fn item(item: &FeedbackItem, json: bool) -> Result<(), String> {
    if json {
        return print_json(item);
    }
    let mut out = format!(
        "{}  {}  {}\n{}\n",
        item.id,
        item.app,
        status(item.status),
        item.headline()
    );
    if let Some(triage) = &item.triage {
        let mut labels = Vec::new();
        if let Some(category) = &triage.category {
            labels.push(category.clone());
        }
        if let Some(severity) = triage.severity {
            labels.push(format!("{severity:?}").to_lowercase());
        }
        labels.extend(triage.tags.iter().map(|tag| format!("#{tag}")));
        if !labels.is_empty() {
            let _ = writeln!(out, "{}", labels.join("  "));
        }
        if let Some(summary) = &triage.summary {
            let _ = writeln!(out, "\n{summary}");
        }
        for question in &triage.open_questions {
            let _ = writeln!(out, "? {question}");
        }
    }
    let _ = writeln!(out, "\nFeedback:\n{}", item.text);
    if !item.facets.is_empty() {
        let _ = writeln!(out, "\nContext:");
        for facet in &item.facets {
            let marker = if facet.sensitive { " (sensitive)" } else { "" };
            let _ = writeln!(out, "  {}={}{marker}", facet.key, facet.value);
        }
    }
    for link in &item.links {
        let _ = writeln!(out, "{:?} → {}", link.kind, link.target);
    }
    if let Some(change) = &item.status_change {
        let _ = write!(out, "\nLast status change by {:?}", change.by);
        if let Some(note) = &change.note {
            let _ = write!(out, ": {note}");
        }
        if !change.commits.is_empty() {
            let _ = write!(out, " ({})", change.commits.join(", "));
        }
        out.push('\n');
    }
    print!("{out}");
    Ok(())
}

fn line(item: &FeedbackItem) -> String {
    let pending = if item.triage_state == TriageState::Pending {
        " (untriaged)"
    } else {
        ""
    };
    format!(
        "{:<7} {:<15} {:<10} {}{pending}",
        item.id.to_string(),
        status(item.status),
        item.app,
        item.headline()
    )
}

const fn status(status: FeedbackStatus) -> &'static str {
    match status {
        FeedbackStatus::Open => "open",
        FeedbackStatus::AgentResolved => "agent-resolved",
        FeedbackStatus::Verified => "verified",
        FeedbackStatus::Dismissed => "dismissed",
    }
}

fn print_json(value: &impl serde::Serialize) -> Result<(), String> {
    let text = serde_json::to_string_pretty(value).map_err(|error| error.to_string())?;
    println!("{text}");
    Ok(())
}
