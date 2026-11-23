//! Human and JSON output. Human output is for the person, who sees sensitive values.

use std::fmt::Write as _;

use bcode_feedback_models::{
    FeedbackItem, FeedbackPage, FeedbackStatus, TriageDispatch, TriageDispatchState, TriageState,
};
use serde_json::json;

pub fn submitted(item: &FeedbackItem, reply: Option<&str>, json: bool) -> Result<(), String> {
    let session = item
        .triage_dispatch
        .as_ref()
        .and_then(|dispatch| dispatch.session_id.as_deref());
    if json {
        return print_json(&json!({
            "item": item,
            "intake_session_id": session,
            "reply": reply,
        }));
    }
    println!("Recorded {} ({})", item.id, item.app);
    println!("Triage: {}", triage_status(item));
    if let Some(reply) = reply {
        println!("\n{reply}");
    }
    Ok(())
}

pub fn triaged(items: &[FeedbackItem], json: bool) -> Result<(), String> {
    if json {
        return print_json(&items);
    }
    for item in items {
        println!("{}  triage: {}", item.id, triage_status(item));
    }
    Ok(())
}

/// Where an item's triage stands, for the person.
pub fn triage_status(item: &FeedbackItem) -> String {
    if item.triage_state == TriageState::Triaged {
        return "triaged".to_owned();
    }
    item.triage_dispatch
        .as_ref()
        .map_or_else(|| "not requested".to_owned(), dispatch_status)
}

fn dispatch_status(dispatch: &TriageDispatch) -> String {
    match dispatch.state {
        TriageDispatchState::Queued if dispatch.attempts == 0 => "queued".to_owned(),
        TriageDispatchState::Queued => {
            let mut status = format!(
                "queued, retrying after {} failed attempt{}",
                dispatch.attempts,
                if dispatch.attempts == 1 { "" } else { "s" }
            );
            if let Some(next) = dispatch.next_attempt_at_ms {
                let _ = write!(status, " (next in {})", until(next));
            }
            if let Some(error) = &dispatch.last_error {
                let _ = write!(status, ": {error}");
            }
            status
        }
        TriageDispatchState::Started => dispatch.turn_id.as_ref().map_or_else(
            || "started".to_owned(),
            |turn| format!("started (intake turn {turn})"),
        ),
        TriageDispatchState::Blocked => format!(
            "blocked: {} (ask again with `feedback triage <id>`)",
            dispatch
                .blocked_reason
                .as_deref()
                .unwrap_or("delivery stopped")
        ),
        TriageDispatchState::Skipped => "skipped at submission".to_owned(),
        TriageDispatchState::Completed => "triaged".to_owned(),
    }
}

/// Time from now until `at_ms`, coarsely.
fn until(at_ms: u64) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| {
            u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)
        });
    let seconds = at_ms.saturating_sub(now).div_ceil(1000);
    if seconds < 60 {
        format!("{seconds}s")
    } else {
        format!("{}m", seconds.div_ceil(60))
    }
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
    stalled: &[FeedbackItem],
    json: bool,
) -> Result<(), String> {
    if json {
        return print_json(&json!({
            "to_verify": resolved,
            "open_questions": questions,
            "triage_attention": stalled,
        }));
    }
    if resolved.is_empty() && questions.is_empty() && stalled.is_empty() {
        println!("Nothing waiting on you.");
        return Ok(());
    }
    if !stalled.is_empty() {
        println!("Not triaged yet; `feedback triage <id>` sends one again:");
        for item in stalled {
            println!("  {}", line(item));
            println!("      triage: {}", triage_status(item));
        }
        if !resolved.is_empty() || !questions.is_empty() {
            println!();
        }
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
    let _ = writeln!(out, "\nTriage: {}", triage_status(item));
    print!("{out}");
    Ok(())
}

fn line(item: &FeedbackItem) -> String {
    let pending = match (item.triage_state, item.triage_dispatch.as_ref()) {
        (TriageState::Triaged, _) => "",
        (TriageState::Pending, Some(dispatch)) => match dispatch.state {
            TriageDispatchState::Blocked => " (triage blocked)",
            TriageDispatchState::Queued if dispatch.attempts > 0 => " (triage retrying)",
            TriageDispatchState::Queued => " (triage queued)",
            TriageDispatchState::Started => " (triaging)",
            TriageDispatchState::Skipped | TriageDispatchState::Completed => " (untriaged)",
        },
        (TriageState::Pending, None) => " (untriaged)",
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
