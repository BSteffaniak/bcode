//! Thread confinement for the single-threaded V2 engine inside the `Send` root model.
//!
//! The V2 engine retains BMUX components (`Element` erases `Box<dyn Component>`; producers
//! share parts through `Rc`) and is driven from one thread by design (X10). The root runtime
//! model, however, must be `Send`: the CLI drives it through a boxed `Send` future (it is
//! polled with `block_on` on the terminal thread, so it never actually changes threads).
//! [`ThreadBound`] bridges the two: it is `Send`, but the value is only reachable, and only
//! dropped, on the thread that created it.

use std::mem::ManuallyDrop;
use std::thread::{self, ThreadId};

/// A value confined to the thread that created it.
///
/// Moving a `ThreadBound` to another thread is allowed; touching the value there is not.
/// [`Self::get`] and [`Self::get_mut`] panic off the owning thread, and dropping off the
/// owning thread leaks the value instead of running its destructor there.
pub struct ThreadBound<T> {
    value: ManuallyDrop<T>,
    owner: ThreadId,
}

// SAFETY: the wrapped value is never accessed (borrowed, mutated or dropped) on any thread but
// `owner`: `get`/`get_mut` check the current thread before handing out a reference, and `Drop`
// only drops the value on `owner` (it leaks it elsewhere). A bitwise move of `T` between
// threads touches no shared state, so every non-`Send` part of `T` (`Rc` counts, thread-local
// caches it refers to) is only ever used on the owning thread. `ThreadBound` is not `Sync`
// unless `T` is, so shared references cannot reach other threads either.
#[expect(
    clippy::non_send_fields_in_send_ty,
    reason = "the non-Send value is confined to its owning thread (see SAFETY above)"
)]
unsafe impl<T> Send for ThreadBound<T> {}

impl<T> ThreadBound<T> {
    /// Confine `value` to the current thread.
    #[must_use]
    pub fn new(value: T) -> Self {
        Self {
            value: ManuallyDrop::new(value),
            owner: thread::current().id(),
        }
    }

    /// Whether the current thread owns the value.
    #[must_use]
    pub fn is_owner(&self) -> bool {
        thread::current().id() == self.owner
    }

    /// Borrow the value.
    ///
    /// # Panics
    ///
    /// Panics when called off the owning thread.
    #[must_use]
    pub fn get(&self) -> &T {
        self.assert_owner();
        &self.value
    }

    /// Borrow the value mutably.
    ///
    /// # Panics
    ///
    /// Panics when called off the owning thread.
    pub fn get_mut(&mut self) -> &mut T {
        self.assert_owner();
        &mut self.value
    }

    fn assert_owner(&self) {
        assert!(
            self.is_owner(),
            "the V2 transcript engine is confined to the thread that created it"
        );
    }
}

impl<T> Drop for ThreadBound<T> {
    fn drop(&mut self) {
        if self.is_owner() {
            // SAFETY: dropped exactly once, here, on the owning thread; the value is not used
            // afterwards.
            unsafe { ManuallyDrop::drop(&mut self.value) };
        }
    }
}

impl<T: std::fmt::Debug> std::fmt::Debug for ThreadBound<T> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.is_owner() {
            formatter
                .debug_tuple("ThreadBound")
                .field(&*self.value)
                .finish()
        } else {
            formatter.write_str("ThreadBound(<other thread>)")
        }
    }
}

#[cfg(test)]
mod tests {
    use std::rc::Rc;

    use super::ThreadBound;

    fn assert_send<T: Send>(_: &T) {}

    #[test]
    fn non_send_values_are_usable_on_the_owning_thread() {
        let shared = Rc::new(5);
        let mut bound = ThreadBound::new(Rc::clone(&shared));
        assert_send(&bound);
        assert!(bound.is_owner());
        assert_eq!(**bound.get(), 5);
        *bound.get_mut() = Rc::new(6);
        assert_eq!(**bound.get(), 6);
        drop(bound);
        assert_eq!(Rc::strong_count(&shared), 1, "dropped on the owner thread");
    }

    #[test]
    fn other_threads_cannot_reach_the_value_and_do_not_drop_it() {
        let bound = ThreadBound::new(String::from("engine"));
        let outcome = std::thread::spawn(move || {
            let reached =
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| bound.get().len()));
            let owner = bound.is_owner();
            drop(bound);
            (reached.is_err(), owner)
        })
        .join()
        .expect("thread completes");
        assert_eq!(outcome, (true, false));
    }
}
