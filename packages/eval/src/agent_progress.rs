//! Bounded public application observations for agent inactivity accounting.
use super::{EvalError, Instant};
use bcode_ipc::SessionProgressSnapshot;
use bcode_session_models::{
    SessionActivityCoverage, SessionActivityQuery, SessionActivityScope, SessionActivityState,
    SessionChildQuery, SessionId,
};
use std::collections::{BTreeMap, BTreeSet, VecDeque};

pub async fn observe(
    client: &bcode_client::BcodeClient,
    root: SessionId,
    previous: &mut BTreeMap<SessionId, SessionProgressSnapshot>,
    last_progress: &mut Instant,
) -> Result<(), EvalError> {
    let scan = async {
        let mut pending = VecDeque::from([root]);
        let mut visited = BTreeSet::new();
        while let Some(session) = pending.pop_front() {
            if !visited.insert(session) {
                return Err(EvalError::Validation(
                    "ambiguous session descendant discovery".into(),
                ));
            }
            let activity = client
                .session_activity(
                    session,
                    SessionActivityQuery {
                        scope: SessionActivityScope::Own,
                        after: None,
                        limit: 100,
                    },
                )
                .await
                .map_err(|error| EvalError::Client(error.to_string()))?;
            if activity.coverage != SessionActivityCoverage::Complete
                || !activity.issues.is_empty()
                || activity
                    .own
                    .evidence
                    .as_ref()
                    .is_none_or(|evidence| !evidence.counts_complete)
                || activity.own.states.contains(&SessionActivityState::Unknown)
            {
                return Err(EvalError::Validation(
                    "agent descendant activity coverage unavailable".into(),
                ));
            }
            if activity.own.states.contains(&SessionActivityState::Idle) {
                previous.remove(&session);
            } else {
                let snapshot = client
                    .session_progress(session)
                    .await
                    .map_err(|error| EvalError::Client(error.to_string()))?;
                if snapshot.advanced_since(&snapshot).is_none() {
                    return Err(EvalError::Validation(
                        "agent progress coverage unavailable".into(),
                    ));
                }
                if let Some(before) = previous.get(&session) {
                    match snapshot.advanced_since(before) {
                        Some(true) => *last_progress = Instant::now(),
                        Some(false) => {}
                        None => {
                            return Err(EvalError::Validation(
                                "agent progress continuity unavailable".into(),
                            ));
                        }
                    }
                }
                previous.insert(session, snapshot);
            }
            let mut after = None;
            loop {
                let page = client
                    .query_child_sessions(SessionChildQuery {
                        parent_id: session,
                        after,
                        limit: 100,
                    })
                    .await
                    .map_err(|error| EvalError::Client(error.to_string()))?;
                if !page.complete || !page.issues.is_empty() {
                    return Err(EvalError::Validation(
                        "agent child discovery incomplete".into(),
                    ));
                }
                for child in page.children {
                    if child.parent_id != Some(session) {
                        return Err(EvalError::Validation("agent child parent mismatch".into()));
                    }
                    pending.push_back(child.id);
                }
                match page.next_after {
                    Some(next) if after.is_none_or(|before| next > before) => after = Some(next),
                    None => break,
                    Some(_) => {
                        return Err(EvalError::Validation(
                            "agent child cursor did not advance".into(),
                        ));
                    }
                }
            }
        }
        previous.retain(|id, _| visited.contains(id));
        Ok(())
    };
    tokio::time::timeout(std::time::Duration::from_secs(15), scan)
        .await
        .map_err(|_| {
            EvalError::Validation("agent activity observation allowance exhausted".into())
        })?
}
