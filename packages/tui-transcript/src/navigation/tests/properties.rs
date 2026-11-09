//! Property tests: random interleavings of content mutations and manual input never move the
//! anchored reader unless an intent asked for it (P1, N11, BUG-A1/B2).

use proptest::prelude::*;

use super::{Fixture, OLDER, unit};
use crate::navigation::{NavInput, NavMode};
use crate::units::UnitKey;
use crate::window::PageDirection;

#[derive(Debug, Clone)]
enum Op {
    /// Older page of units with these heights (zero-height units included).
    Page(Vec<u64>),
    /// Live growth at the tail.
    Append(u64),
    /// Height change of the unit at this ordinal (mod count).
    Revise(usize, u64),
    /// Removal of the unit at this ordinal (mod count), unless it is the reference unit.
    Remove(usize),
    /// Wheel or key movement.
    Scroll(i64),
}

fn op() -> impl Strategy<Value = Op> {
    prop_oneof![
        prop::collection::vec(0_u64..8, 0..4).prop_map(Op::Page),
        (0_u64..6).prop_map(Op::Append),
        (0_usize..64, 0_u64..9).prop_map(|(at, rows)| Op::Revise(at, rows)),
        (0_usize..64).prop_map(Op::Remove),
        (-25_i64..25).prop_map(Op::Scroll),
    ]
}

/// First non-boundary unit intersecting the viewport, and its screen row.
fn reference(f: &Fixture) -> Option<(UnitKey, i64)> {
    let top = f.top();
    let mut offset = 0_u64;
    for (key, rows) in &f.units {
        if !key.is_boundary() && *rows > 0 && offset + rows > top {
            let row = i64::try_from(offset).ok()? - i64::try_from(top).ok()?;
            return Some((key.clone(), row));
        }
        offset += rows;
    }
    None
}

/// Whether `key` lies after the anchor unit's start but before the reference unit (only
/// possible when the anchor is the boundary row above zero-height units).
fn between_anchor_and_reference(
    f: &Fixture,
    reference: Option<&(UnitKey, i64)>,
    key: &UnitKey,
) -> bool {
    let NavMode::Reading { anchor } = &f.state.mode else {
        return false;
    };
    let position = |target: &UnitKey| f.units.iter().position(|(unit, _)| unit == target);
    match (
        position(&anchor.unit),
        reference.and_then(|(r, _)| position(r)),
        position(key),
    ) {
        (Some(anchor), Some(reference), Some(key)) => anchor <= key && key < reference,
        _ => false,
    }
}

fn screen_row(f: &Fixture, key: &UnitKey) -> Option<i64> {
    let offset = f.with_view(|view| view.unit_offset(key))?;
    Some(i64::try_from(offset).ok()? - i64::try_from(f.top()).ok()?)
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 256,
        failure_persistence: None,
        ..ProptestConfig::default()
    })]

    #[test]
    fn content_and_input_interleavings_never_move_the_reader_uninvited(
        ops in prop::collection::vec(op(), 1..40),
    ) {
        let mut f = Fixture::uniform(10, 12, 4);
        f.window.has_older = true;
        f.admit_all();
        let _ = f.scroll(-20, 0);
        let mut page = 0_usize;
        let mut appended = 0_usize;
        for (step, op) in ops.into_iter().enumerate() {
            let ms = 1_000 + 10 * u64::try_from(step).unwrap();
            let reading = matches!(f.state.mode, NavMode::Reading { .. });
            let before = reference(&f);
            let top = f.top();
            let mut expected_shift = 0_i64;
            let mut removed_reference = false;
            match op {
                Op::Page(heights) => {
                    let names: Vec<String> =
                        heights.iter().map(|_| { page += 1; format!("p{page}") }).collect();
                    let units: Vec<(&str, u64)> =
                        names.iter().map(String::as_str).zip(heights).collect();
                    let inserted = f.prepend(&units);
                    // Pending upward movement is the only invited move: spent once, bounded by
                    // the inserted rows.
                    if let Some(pending) = f.state.pending_movement {
                        expected_shift = i64::try_from(pending.rows.min(inserted)).unwrap();
                    }
                    let generation = f.window.generation;
                    let _ = f.reduce(NavInput::PageResult {
                        generation,
                        direction: PageDirection::Older,
                        inserted_rows: inserted,
                        exhausted: false,
                    });
                }
                Op::Append(rows) => {
                    appended += 1;
                    f.append(&format!("n{appended}"), rows);
                    let _ = f.content(ms);
                }
                Op::Revise(at, rows) => {
                    let candidates: Vec<UnitKey> = f
                        .units
                        .iter()
                        .map(|(key, _)| key.clone())
                        .filter(|key| key != &OLDER)
                        .collect();
                    if candidates.is_empty() {
                        continue;
                    }
                    let key = candidates[at % candidates.len()].clone();
                    // Shrinking at or below the viewport may clamp an anchored reader at the
                    // content bottom (no blank rows while reading); such edits only grow.
                    let offset = f.with_view(|view| view.unit_offset(&key)).unwrap();
                    let current = f.units.iter().find(|(k, _)| k == &key).unwrap().1;
                    let visible_or_below = offset + current > top;
                    let rows = if visible_or_below { current.max(rows) } else { rows };
                    if before.as_ref().is_some_and(|(r, _)| r == &key) {
                        // Shrinking the reference may clamp its row offset (P2).
                        continue;
                    }
                    if between_anchor_and_reference(&f, before.as_ref(), &key) {
                        // Growth below the keyed anchor legitimately pushes later content.
                        continue;
                    }
                    if let UnitKey::Item(id) = &key {
                        f.set_height(id.get(), rows);
                    }
                    let _ = f.content(ms);
                }
                Op::Remove(at) => {
                    let candidates: Vec<UnitKey> = f
                        .units
                        .iter()
                        .map(|(key, _)| key.clone())
                        .filter(|key| key != &OLDER)
                        .collect();
                    if candidates.len() < 2 {
                        continue;
                    }
                    let key = candidates[at % candidates.len()].clone();
                    let offset = f.with_view(|view| view.unit_offset(&key)).unwrap();
                    if offset >= top {
                        // Removals at or below the viewport may clamp; P4 has its own tests.
                        continue;
                    }
                    // Removing the reference or anchor unit remaps it (P4), an allowed move.
                    removed_reference = before.as_ref().is_some_and(|(r, _)| r == &key)
                        || matches!(&f.state.mode, NavMode::Reading { anchor } if anchor.unit == key);
                    if let UnitKey::Item(id) = &key {
                        let name = id.get().to_owned();
                        f.remove(&[name.as_str()]);
                    }
                    let _ = f.content(ms);
                }
                Op::Scroll(rows) => {
                    // Live window: manual movement may reach the bounded tail space.
                    let max = crate::viewport::list_geometry(f.viewport.height)
                        .max_overscroll_top(f.total());
                    let moved = if rows < 0 {
                        -i64::try_from(rows.unsigned_abs().min(top)).unwrap()
                    } else {
                        i64::try_from(rows.unsigned_abs().min(max.saturating_sub(top))).unwrap()
                    };
                    let _ = f.scroll(rows, ms);
                    prop_assert_eq!(
                        i64::try_from(f.top()).unwrap() - i64::try_from(top).unwrap(),
                        moved,
                        "scroll by {} from top {}", rows, top
                    );
                    continue;
                }
            }
            if !reading || removed_reference {
                continue;
            }
            if let Some((key, row)) = before
                && let Some(after) = screen_row(&f, &key)
            {
                prop_assert_eq!(after, row + expected_shift, "reference {} moved", key);
            }
        }
    }

    #[test]
    fn repeated_reduction_of_unrelated_inputs_is_idempotent_for_position(
        appends in prop::collection::vec(0_u64..5, 1..20),
    ) {
        let mut f = Fixture::uniform(10, 8, 3);
        f.admit_all();
        let _ = f.scroll(-7, 0);
        let anchor = f.state.resting_position();
        for (n, rows) in appends.into_iter().enumerate() {
            f.append(&format!("n{n}"), rows);
            let _ = f.content(1_000);
            let _ = f.reduce(NavInput::PresentationCommitted {
                frame: crate::frame::FrameId(u64::try_from(n).unwrap()),
            });
            prop_assert_eq!(&f.state.resting_position(), &anchor);
        }
        prop_assert!(f.with_view(|view| view.contains(&unit("u0"))));
    }
}
