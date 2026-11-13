//! Automatic navigation policy (N4, N5, N9, N10).
//!
//! Submission acceptance reveal (T2/T27), assistant segment reveal (T3, re-evaluated by T24)
//! and overflow handoff (T4, T6) are decided only here, from `Content`, `SubmissionAccepted`
//! and grace-expiry inputs. The decisions are returned as [`AutoAction`]s that
//! [`super::reduce`] applies to the same state machine; this module never mutates state.
//! Latest-bar activity code never emits navigation.
//!
//! Admission (N10) is by stable identity, once, among resident units: a unit is admitted when
//! it is first resident with at least one visible row. Units that arrive at the tail after
//! every admitted unit are *arrivals* and may trigger automatic movement; units that become
//! resident elsewhere (history pages, window replacements) are admitted silently and other
//! insertions above the last admitted unit are never arrivals. While grace is
//! active and the reader is following or holding, assistant segments stay unadmitted (recorded
//! as deferred) so the newest one can be revealed once when grace ends (T24).
//!
//! A content input inspects only the units after the last admitted unit and the deferred
//! segments, never the whole resident window (D11).

use web_time::Instant;

use super::{FollowPolicy, NavInput, NavMode, NavState, NavTimer, NavView, UnitRole};
use crate::units::UnitKey;
use crate::window::WindowMode;

/// One automatic decision, applied in order by the reducer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AutoAction {
    /// Record admission of a unit (N10).
    Admit(UnitKey),
    /// Hold a unit's top on the viewport top via motion `ItemTop` (T2: not sticky; T3: sticky).
    Reveal {
        /// Unit to reveal.
        item: UnitKey,
        /// Whether the hold survives overflow until the segment ends (C13).
        sticky: bool,
    },
    /// An accepted submission is not resident yet; reveal it when it becomes visible (T2).
    QueueReveal {
        /// Accepted user-message unit.
        item: UnitKey,
    },
    /// An accepted submission in a detached window: load the tail window first (T27).
    LoadTailForReveal {
        /// Accepted user-message unit.
        item: UnitKey,
    },
    /// A sticky hold becomes non-sticky because a non-assistant unit arrived (T6).
    Unstick,
    /// A non-sticky hold overflowed the viewport: follow the latest bottom (T4).
    Handoff,
    /// Grace defers admission of an arrived assistant segment so the newest one can be
    /// revealed once when grace ends (T24, N5).
    Defer(UnitKey),
}

/// Automatic actions implied by an input, evaluated against the current state and geometry.
///
/// Returns no actions for inputs other than `Content`, `SubmissionAccepted` and
/// `Timer(GraceExpired)`. The state is read as it is before the input is applied; for a grace
/// expiry `state.grace_until` is still set.
#[must_use]
pub fn automatic_actions(
    state: &NavState,
    input: &NavInput,
    view: &NavView<'_>,
) -> Vec<AutoAction> {
    match input {
        NavInput::SubmissionAccepted { item, .. } => submission(item, view),
        NavInput::Content { at, .. }
        | NavInput::Timer {
            key: NavTimer::GraceExpired,
            at,
        } => content(state, view, *at),
        _ => Vec::new(),
    }
}

/// T2 / T27 for a newly accepted submission.
fn submission(item: &UnitKey, view: &NavView<'_>) -> Vec<AutoAction> {
    let item = item.clone();
    if matches!(view.window_mode(), WindowMode::Detached) {
        vec![AutoAction::LoadTailForReveal { item }]
    } else if view.is_visible(&item) {
        vec![AutoAction::Admit(item.clone()), reveal(item, false, view)]
    } else {
        vec![AutoAction::QueueReveal { item }]
    }
}

/// Decisions so far and the mode they leave.
struct Plan {
    mode: NavMode,
    actions: Vec<AutoAction>,
}

impl Plan {
    fn push(&mut self, action: AutoAction) {
        match &action {
            AutoAction::Reveal { item, sticky } => {
                self.mode = NavMode::Holding {
                    item: item.clone(),
                    sticky: *sticky,
                };
            }
            AutoAction::Unstick => {
                if let NavMode::Holding { sticky, .. } = &mut self.mode {
                    *sticky = false;
                }
            }
            AutoAction::Handoff => self.mode = NavMode::Following,
            AutoAction::Admit(_)
            | AutoAction::Defer(_)
            | AutoAction::QueueReveal { .. }
            | AutoAction::LoadTailForReveal { .. } => {}
        }
        self.actions.push(action);
    }

    fn admitted(&self, key: &UnitKey) -> bool {
        self.actions
            .iter()
            .any(|action| matches!(action, AutoAction::Admit(admitted) if admitted == key))
    }

    const fn attached(&self) -> bool {
        matches!(self.mode, NavMode::Following | NavMode::Holding { .. })
    }
}

/// Inputs shared by the content-policy steps.
struct Context<'s, 'v, 'a> {
    state: &'s NavState,
    view: &'v NavView<'a>,
    in_grace: bool,
    grace_ended: bool,
}

impl<'v> Context<'_, 'v, '_> {
    /// Resident, visible, neither a boundary nor a local row (LR6) and not admitted before
    /// this input.
    fn is_candidate(&self, key: &UnitKey) -> bool {
        !key.is_boundary()
            && !key.is_local()
            && !self.state.admitted.contains(key)
            && self.view.is_visible(key)
    }

    fn is_assistant(&self, key: &UnitKey) -> bool {
        self.view.facts.role(key) == UnitRole::AssistantSegment
    }

    fn position(&self, key: &UnitKey) -> Option<usize> {
        self.view.index.index_of(key)
    }

    /// Unit key at a document position.
    fn key_at(&self, position: usize) -> Option<&'v UnitKey> {
        self.view.index.item(position).map(|item| &item.key)
    }
}

/// T2 (queued reveals), T3, T6, T4 and admission after content changed or grace expired.
///
/// Work is bounded by the units after the last admitted unit plus the segments grace deferred
/// (D11): a revision of a resident unit never rescans the resident window.
fn content(state: &NavState, view: &NavView<'_>, at: Instant) -> Vec<AutoAction> {
    let mut plan = Plan {
        mode: state.mode.clone(),
        actions: Vec::new(),
    };
    let cx = Context {
        state,
        view,
        in_grace: state.in_grace(at),
        grace_ended: state.grace_until.is_some_and(|until| until <= at),
    };
    queued_reveal(&cx, &mut plan);

    // N12: nothing admitted yet (empty document or fresh engine): admit silently, never infer
    // arrivals from growth.
    if state.admitted.is_empty() {
        for key in view.unit_keys() {
            if cx.is_candidate(key) && !plan.admitted(key) {
                plan.push(AutoAction::Admit(key.clone()));
            }
        }
        return plan.actions;
    }

    let arrivals = arrivals(&cx, &plan);
    reveal_newest_segment(&cx, &arrivals, &mut plan);
    admit(&cx, &arrivals, &mut plan);
    overflow_handoff(&cx, &mut plan);
    plan.actions
}

/// T2: a queued submission reveal whose unit became visible. Never suppressed (N5).
fn queued_reveal(cx: &Context<'_, '_, '_>, plan: &mut Plan) {
    if !cx.view.is_live() {
        return;
    }
    let newest = cx
        .state
        .pending_reveals
        .iter()
        .filter(|(item, _)| cx.view.is_visible(item))
        .max_by_key(|(_, request)| request.requested);
    if let Some((item, _)) = newest {
        plan.push(AutoAction::Admit(item.clone()));
        plan.push(reveal(item.clone(), false, cx.view));
    }
}

/// Bring a submission into view under the follow policy: hold its top (T2), or follow the
/// latest bottom.
fn reveal(item: UnitKey, sticky: bool, view: &NavView<'_>) -> AutoAction {
    match view.follow {
        FollowPolicy::Reply => AutoAction::Reveal { item, sticky },
        FollowPolicy::Latest => AutoAction::Handoff,
    }
}

/// Candidates after the last admitted unit, in document order with their positions. The scan
/// walks back from the newest unit and stops at the first admitted one; local rows are never
/// admitted and never stop it (LR6), so canonical units arriving before a pending submission
/// row are still arrivals.
fn arrivals<'v>(cx: &Context<'_, 'v, '_>, plan: &Plan) -> Vec<(usize, &'v UnitKey)> {
    let mut arrivals = Vec::new();
    let mut position = cx.view.index.len();
    while position > 0 {
        position -= 1;
        let Some(key) = cx.key_at(position) else {
            break;
        };
        if key.is_local() {
            continue;
        }
        if cx.state.admitted.contains(key) {
            break;
        }
        if cx.is_candidate(key) && !plan.admitted(key) {
            arrivals.push((position, key));
        }
    }
    arrivals.reverse();
    arrivals
}

/// Segments grace deferred (T24) that are still candidates, in document order.
fn deferred<'s>(cx: &Context<'s, '_, '_>, plan: &Plan) -> Vec<(usize, &'s UnitKey)> {
    let mut deferred: Vec<(usize, &UnitKey)> = cx
        .state
        .deferred
        .iter()
        .filter(|key| cx.is_candidate(key) && !plan.admitted(key))
        .filter_map(|key| Some((cx.position(key)?, key)))
        .collect();
    deferred.sort_by_key(|(position, _)| *position);
    deferred
}

/// T3 (also re-evaluated by T24): the newest assistant segment, once, if not yet admitted.
///
/// Candidates are arrivals and, once grace ended, the segments grace deferred. Admission keeps
/// every assistant segment after a deferred one deferred too (or admits the deferred ones), so
/// the newest candidate is the newest unadmitted segment in the document.
fn reveal_newest_segment(
    cx: &Context<'_, '_, '_>,
    arrivals: &[(usize, &UnitKey)],
    plan: &mut Plan,
) {
    if !plan.attached() || cx.in_grace {
        return;
    }
    let newest_arrival = arrivals
        .iter()
        .rev()
        .find(|(_, key)| cx.is_assistant(key))
        .copied();
    let newest_deferred = if cx.grace_ended {
        deferred(cx, plan)
            .into_iter()
            .rev()
            .find(|(_, key)| cx.is_assistant(key))
    } else {
        None
    };
    let newest = match (newest_arrival, newest_deferred) {
        (Some(arrival), Some(deferred)) => Some(if deferred.0 > arrival.0 {
            deferred
        } else {
            arrival
        }),
        (arrival, deferred) => arrival.or(deferred),
    };
    let eligible = newest
        .map(|(_, key)| key)
        .filter(|key| cx.view.facts.is_streaming(key) && !plan.admitted(key));
    if let Some(segment) = eligible {
        let held = matches!(&plan.mode, NavMode::Holding { item, .. } if item == segment);
        plan.push(AutoAction::Admit(segment.clone()));
        // Following the latest content needs no reveal: a follower stays at the bottom and a
        // reader keeps their place.
        if !held && cx.view.follow == FollowPolicy::Reply {
            plan.push(AutoAction::Reveal {
                item: segment.clone(),
                sticky: true,
            });
        }
    }
}

/// Admission of everything else (assistant segments wait while grace defers T3, N5), plus T6.
fn admit(cx: &Context<'_, '_, '_>, arrivals: &[(usize, &UnitKey)], plan: &mut Plan) {
    let defer_assistants = cx.in_grace && plan.attached();
    if !defer_assistants {
        // Grace is over (or the reader detached): deferred segments are admitted silently.
        for (_, key) in deferred(cx, plan) {
            plan.push(AutoAction::Admit(key.clone()));
        }
    }
    let mut unstick = false;
    for (position, key) in arrivals {
        if plan.admitted(key) {
            continue;
        }
        if defer_assistants && cx.is_assistant(key) {
            if !cx.state.deferred.contains(*key) {
                plan.push(AutoAction::Defer((*key).clone()));
            }
            continue;
        }
        plan.push(AutoAction::Admit((*key).clone()));
        if cx.is_assistant(key) {
            continue;
        }
        // T6: a new non-assistant unit below a sticky hold makes it non-sticky.
        if let NavMode::Holding { item, sticky: true } = &plan.mode
            && cx.position(item).is_some_and(|held| *position > held)
        {
            unstick = true;
        }
    }
    if unstick {
        plan.push(AutoAction::Unstick);
    }
}

/// T4: a non-sticky hold whose content extends past the viewport bottom (not during grace).
fn overflow_handoff(cx: &Context<'_, '_, '_>, plan: &mut Plan) {
    if cx.in_grace {
        return;
    }
    if let NavMode::Holding {
        item,
        sticky: false,
    } = &plan.mode
        && cx
            .view
            .unit_offset(item)
            .is_some_and(|offset| offset < cx.view.max_content_top())
    {
        plan.push(AutoAction::Handoff);
    }
}
