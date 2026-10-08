//! Monotonic execution deadlines independent of transport and activity contracts.
//!
//! The caller supplies only substantive public activity counters. Snapshot polling,
//! heartbeats and running status must not increment those counters. Complete subtree
//! coverage is required to conclude inactivity; this module does not discover sessions.
use std::collections::BTreeMap;
use std::num::NonZeroUsize;
use std::time::Duration;

/// A substantive activity counter within one comparable epoch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProgressMark<E> {
    pub epoch: E,
    pub count: u64,
}

/// Why an execution allowance expired.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimeoutReason {
    Idle,
    Wall,
}

/// An observation that cannot safely establish inactivity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClockError {
    TimeWentBackwards,
    MissingBaseline,
    EpochChanged,
    CacheFull,
    #[cfg(test)]
    CoverageUnavailable,
}

/// A bounded progress cache and injectable monotonic execution clock.
///
/// `idle_interval: None` selects a wall-only clock. An idle clock has no implicit
/// wall limit: `wall_limit` must be supplied separately. Baseline registration is
/// not progress, even when a newly discovered descendant has a nonzero counter.
#[derive(Debug)]
pub struct ExecutionClock<E> {
    start: Duration,
    last_now: Duration,
    last_progress: Duration,
    idle_interval: Option<Duration>,
    wall_limit: Option<Duration>,
    max_sessions: NonZeroUsize,
    progress: BTreeMap<String, ProgressMark<E>>,
    terminal: Option<TimeoutReason>,
}

impl<E: Eq> ExecutionClock<E> {
    #[must_use]
    pub const fn new(
        now: Duration,
        idle_interval: Option<Duration>,
        wall_limit: Option<Duration>,
        max_sessions: NonZeroUsize,
    ) -> Self {
        Self {
            start: now,
            last_now: now,
            last_progress: now,
            idle_interval,
            wall_limit,
            max_sessions,
            progress: BTreeMap::new(),
            terminal: None,
        }
    }

    /// Whether a session already has a retained comparable baseline.
    #[must_use]
    pub fn contains(&self, session: &str) -> bool {
        self.progress.contains_key(session)
    }

    /// Retire a marker only after authoritative own-session activity verifies inactivity.
    ///
    /// Retirement is not progress. A later activation must establish a fresh baseline;
    /// neither replay of the old marker nor rediscovery extends the idle interval.
    #[cfg(test)]
    pub fn retire(&mut self, session: &str) {
        self.progress.remove(session);
    }

    /// Retain only sessions certified active by a complete authoritative inventory.
    pub fn retain_active(&mut self, active: &std::collections::BTreeSet<String>) {
        self.progress.retain(|session, _| active.contains(session));
    }

    /// Register the first trustworthy mark without extending any deadline.
    ///
    /// Existing entries are compared as observations, never overwritten as a new
    /// baseline. Entries are not evicted: eviction could make replay look fresh.
    ///
    /// # Errors
    /// Returns an error on capacity exhaustion or an incomparable epoch.
    pub fn baseline(&mut self, session: String, mark: ProgressMark<E>) -> Result<(), ClockError> {
        if let Some(previous) = self.progress.get(&session) {
            if previous.epoch != mark.epoch {
                return Err(ClockError::EpochChanged);
            }
            return Ok(());
        }
        if self.progress.len() >= self.max_sessions.get() {
            return Err(ClockError::CacheFull);
        }
        self.progress.insert(session, mark);
        Ok(())
    }

    /// Compare one substantive activity observation to its retained baseline.
    ///
    /// Duplicate and lower counters are stale and never reset idle. Epoch changes
    /// are not assumed to mean progress. Once expired, the clock cannot be reopened.
    ///
    /// # Errors
    /// Returns an error for backwards time, absent baselines or changed epochs.
    pub fn observe(
        &mut self,
        now: Duration,
        session: &str,
        mark: ProgressMark<E>,
    ) -> Result<bool, ClockError> {
        self.advance(now)?;
        let previous = self
            .progress
            .get_mut(session)
            .ok_or(ClockError::MissingBaseline)?;
        if previous.epoch != mark.epoch {
            return Err(ClockError::EpochChanged);
        }
        if mark.count <= previous.count || self.terminal.is_some() {
            return Ok(false);
        }
        *previous = mark;
        self.last_progress = now;
        Ok(true)
    }

    /// Apply a verified substantive event time without treating observation as progress.
    ///
    /// # Errors
    /// Rejects backwards observation time or an event outside the execution interval.
    pub fn substantive_at(&mut self, now: Duration, event: Duration) -> Result<(), ClockError> {
        self.advance(now)?;
        if event > now || event < self.start {
            return Err(ClockError::TimeWentBackwards);
        }
        if self.terminal.is_none() {
            self.last_progress = self.last_progress.max(event);
        }
        Ok(())
    }

    /// Evaluate inactivity as of an inventory start, while wall limits use current time.
    ///
    /// # Errors
    /// Rejects backwards observation time or an as-of time later than observation.
    pub fn expired_as_of(
        &mut self,
        now: Duration,
        as_of: Duration,
    ) -> Result<Option<TimeoutReason>, ClockError> {
        self.advance(now)?;
        if as_of > now {
            return Err(ClockError::TimeWentBackwards);
        }
        if self.terminal.is_some() {
            return Ok(self.terminal);
        }
        if self
            .wall_limit
            .is_some_and(|limit| now.saturating_sub(self.start) >= limit)
        {
            self.terminal = Some(TimeoutReason::Wall);
        } else if self.last_progress <= as_of
            && self
                .idle_interval
                .is_some_and(|limit| as_of.saturating_sub(self.last_progress) >= limit)
        {
            self.terminal = Some(TimeoutReason::Idle);
        }
        Ok(self.terminal)
    }

    /// Decide expiration after observing activity from a bounded subtree scan.
    ///
    /// # Errors
    /// Returns an error for backwards time or unavailable/incomplete idle coverage.
    /// Explicit wall expiration remains decidable without activity coverage.
    #[cfg(test)]
    pub fn expired(
        &mut self,
        now: Duration,
        coverage_complete: bool,
    ) -> Result<Option<TimeoutReason>, ClockError> {
        self.advance(now)?;
        if self.terminal.is_some() {
            return Ok(self.terminal);
        }
        if self
            .wall_limit
            .is_some_and(|limit| now.saturating_sub(self.start) >= limit)
        {
            self.terminal = Some(TimeoutReason::Wall);
        } else if let Some(interval) = self.idle_interval {
            if !coverage_complete {
                return Err(ClockError::CoverageUnavailable);
            }
            if now.saturating_sub(self.last_progress) >= interval {
                self.terminal = Some(TimeoutReason::Idle);
            }
        }
        Ok(self.terminal)
    }

    /// Remaining execution allowance, not an IPC/setup/capture allowance.
    ///
    /// # Errors
    /// Returns the same observation errors as [`Self::expired`].
    #[cfg(test)]
    pub fn remaining(
        &mut self,
        now: Duration,
        coverage_complete: bool,
    ) -> Result<Option<Duration>, ClockError> {
        if self.expired(now, coverage_complete)?.is_some() {
            return Ok(Some(Duration::ZERO));
        }
        let wall = self
            .wall_limit
            .map(|limit| limit.saturating_sub(now.saturating_sub(self.start)));
        let idle = self
            .idle_interval
            .map(|limit| limit.saturating_sub(now.saturating_sub(self.last_progress)));
        Ok(match (wall, idle) {
            (Some(wall), Some(idle)) => Some(wall.min(idle)),
            (wall, idle) => wall.or(idle),
        })
    }

    fn advance(&mut self, now: Duration) -> Result<(), ClockError> {
        if now < self.last_now {
            return Err(ClockError::TimeWentBackwards);
        }
        self.last_now = now;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seconds(value: u64) -> Duration {
        Duration::from_secs(value)
    }

    fn mark(epoch: u64, count: u64) -> ProgressMark<u64> {
        ProgressMark { epoch, count }
    }

    fn clock(idle: bool, wall: Option<u64>) -> ExecutionClock<u64> {
        ExecutionClock::new(
            Duration::ZERO,
            idle.then(|| seconds(180)),
            wall.map(seconds),
            NonZeroUsize::new(2).unwrap(),
        )
    }

    #[test]
    fn substantive_descendant_activity_continues_past_180_total_seconds() {
        let mut clock = clock(true, None);
        clock.baseline("root".into(), mark(1, 0)).unwrap();
        clock.baseline("child".into(), mark(2, 10)).unwrap();
        for count in 1..=10 {
            let now = seconds(count * 100);
            assert!(clock.observe(now, "child", mark(2, 10 + count)).unwrap());
            assert_eq!(clock.expired(now, true), Ok(None));
            assert_eq!(clock.remaining(now, true), Ok(Some(seconds(180))));
        }
        assert_eq!(
            clock.expired(seconds(1180), true),
            Ok(Some(TimeoutReason::Idle))
        );
    }

    #[test]
    fn polling_heartbeat_and_stale_counters_do_not_extend_idle() {
        let mut clock = clock(true, None);
        clock.baseline("root".into(), mark(1, 7)).unwrap();
        for now in [60, 120, 179] {
            assert!(!clock.observe(seconds(now), "root", mark(1, 7)).unwrap());
            assert_eq!(clock.expired(seconds(now), true), Ok(None));
        }
        assert!(!clock.observe(seconds(180), "root", mark(1, 6)).unwrap());
        assert_eq!(
            clock.expired(seconds(180), true),
            Ok(Some(TimeoutReason::Idle))
        );
        assert!(!clock.observe(seconds(181), "root", mark(1, 8)).unwrap());
        assert_eq!(
            clock.expired(seconds(181), true),
            Ok(Some(TimeoutReason::Idle))
        );
    }

    #[test]
    fn optional_wall_limit_is_independent_of_activity() {
        let mut clock = clock(true, Some(250));
        clock.baseline("root".into(), mark(1, 0)).unwrap();
        clock.observe(seconds(170), "root", mark(1, 1)).unwrap();
        assert_eq!(clock.remaining(seconds(200), true), Ok(Some(seconds(50))));
        assert_eq!(
            clock.expired(seconds(250), false),
            Ok(Some(TimeoutReason::Wall))
        );
    }

    #[test]
    fn legacy_wall_clock_needs_no_activity_coverage() {
        let mut clock = clock(false, Some(180));
        assert_eq!(clock.expired(seconds(179), false), Ok(None));
        assert_eq!(
            clock.expired(seconds(180), false),
            Ok(Some(TimeoutReason::Wall))
        );
    }

    #[test]
    fn missing_coverage_cannot_be_classified_as_idle() {
        let mut clock = clock(true, None);
        assert_eq!(
            clock.expired(seconds(180), false),
            Err(ClockError::CoverageUnavailable)
        );
        assert_eq!(
            clock.remaining(seconds(181), false),
            Err(ClockError::CoverageUnavailable)
        );
    }

    #[test]
    fn missing_cache_and_epoch_change_are_errors_not_progress() {
        let mut clock = clock(true, None);
        assert_eq!(
            clock.observe(seconds(1), "root", mark(1, 1)),
            Err(ClockError::MissingBaseline)
        );
        clock.baseline("root".into(), mark(1, 3)).unwrap();
        assert_eq!(
            clock.observe(seconds(100), "root", mark(2, 4)),
            Err(ClockError::EpochChanged)
        );
        assert_eq!(
            clock.baseline("root".into(), mark(2, 4)),
            Err(ClockError::EpochChanged)
        );
        assert_eq!(
            clock.expired(seconds(180), true),
            Ok(Some(TimeoutReason::Idle))
        );
    }

    #[test]
    fn retirement_bounds_active_cache_without_limiting_lifetime_sessions() {
        let mut clock = ExecutionClock::new(
            Duration::ZERO,
            Some(seconds(180)),
            None,
            NonZeroUsize::new(1).unwrap(),
        );
        for index in 0..5000 {
            let session = index.to_string();
            clock.baseline(session.clone(), mark(1, 10)).unwrap();
            clock.retire(&session);
            assert!(!clock.contains(&session));
        }
        clock.baseline("0".into(), mark(1, 10)).unwrap();
        assert!(!clock.observe(seconds(179), "0", mark(1, 10)).unwrap());
        assert_eq!(
            clock.expired(seconds(180), true),
            Ok(Some(TimeoutReason::Idle))
        );
    }

    #[test]
    fn asof_idle_does_not_charge_page_latency_or_ignore_later_progress() {
        let budget = NonZeroUsize::new(1).unwrap();
        let mut clock: ExecutionClock<u64> =
            ExecutionClock::new(Duration::ZERO, Some(seconds(180)), None, budget);
        assert_eq!(
            clock.expired_as_of(seconds(190), seconds(179)).unwrap(),
            None
        );
        clock.substantive_at(seconds(191), seconds(185)).unwrap();
        assert_eq!(
            clock.expired_as_of(seconds(192), seconds(180)).unwrap(),
            None
        );
        assert_eq!(
            clock.expired_as_of(seconds(365), seconds(365)).unwrap(),
            Some(TimeoutReason::Idle)
        );
    }

    #[test]
    fn first_seen_verified_event_uses_event_time_not_page_time() {
        let mut clock: ExecutionClock<u64> = clock(true, None);
        clock.substantive_at(seconds(190), seconds(170)).unwrap();
        assert_eq!(
            clock.expired_as_of(seconds(350), seconds(349)).unwrap(),
            None
        );
        assert_eq!(
            clock.expired_as_of(seconds(351), seconds(350)).unwrap(),
            Some(TimeoutReason::Idle)
        );
    }

    #[test]
    fn cache_capacity_fails_without_eviction() {
        let mut clock = clock(true, None);
        clock.baseline("root".into(), mark(1, 0)).unwrap();
        clock.baseline("child".into(), mark(1, 0)).unwrap();
        assert_eq!(
            clock.baseline("other".into(), mark(1, 0)),
            Err(ClockError::CacheFull)
        );
        assert!(clock.observe(seconds(10), "root", mark(1, 1)).unwrap());
    }

    #[test]
    fn baseline_registration_and_replay_do_not_reset_idle() {
        let mut clock = clock(true, None);
        assert_eq!(clock.expired(seconds(100), true), Ok(None));
        clock.baseline("root".into(), mark(1, 10)).unwrap();
        clock.baseline("root".into(), mark(1, 100)).unwrap();
        assert_eq!(
            clock.expired(seconds(180), true),
            Ok(Some(TimeoutReason::Idle))
        );
    }

    #[test]
    fn injected_clock_rejects_backwards_time_and_supports_nonzero_origin() {
        let mut clock: ExecutionClock<u64> = ExecutionClock::new(
            seconds(1000),
            Some(seconds(180)),
            None,
            NonZeroUsize::new(1).unwrap(),
        );
        assert_eq!(clock.remaining(seconds(1100), true), Ok(Some(seconds(80))));
        assert_eq!(
            clock.expired(seconds(1099), true),
            Err(ClockError::TimeWentBackwards)
        );
        assert_eq!(
            clock.expired(seconds(1180), true),
            Ok(Some(TimeoutReason::Idle))
        );
    }
}
