//! Frontend-independent pre-authentication navigation. No provider effects or credentials.

/// Semantic stages of an owned provider login, independent of rendering.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthenticationStep {
    /// Select an owned account, or explicitly create one.
    Account,
    /// Resolve an existing method or select a new account's method.
    Method,
    /// Preserve memberships, or request an explicit enrollment change.
    Membership,
    /// Review before any provider side effect.
    Review,
}

/// One ephemeral journey. Earlier changes invalidate all dependent selections.
#[derive(Debug, Default)]
pub struct AuthenticationJourney {
    account: Option<String>,
    method: Option<String>,
    step: Option<AuthenticationStep>,
}
impl AuthenticationJourney {
    /// Begin with no implicit authorization or selected identity.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            account: None,
            method: None,
            step: Some(AuthenticationStep::Account),
        }
    }
    /// Current stage.
    #[must_use]
    pub fn step(&self) -> AuthenticationStep {
        self.step.unwrap_or(AuthenticationStep::Account)
    }
    /// Selected account identity, never credentials.
    #[must_use]
    pub fn account(&self) -> Option<&str> {
        self.account.as_deref()
    }
    /// Selected provider-local method ID.
    #[must_use]
    pub fn method(&self) -> Option<&str> {
        self.method.as_deref()
    }
    /// Select an account and invalidate method/membership review.
    pub fn select_account(&mut self, account: Option<String>) {
        self.account = account;
        self.method = None;
        self.step = Some(AuthenticationStep::Method);
    }
    /// Select a method and invalidate membership review.
    pub fn select_method(&mut self, method: String) {
        self.method = Some(method);
        self.step = Some(AuthenticationStep::Membership);
    }
    /// Mark the membership plan ready for review; this never executes it.
    pub const fn review(&mut self) {
        self.step = Some(AuthenticationStep::Review);
    }
    /// Return to the preceding unlocked stage. False means exit this provider's journey.
    pub fn back(
        &mut self,
        account_locked: bool,
        method_locked: bool,
        membership_locked: bool,
    ) -> bool {
        self.step = match self.step() {
            AuthenticationStep::Review if !membership_locked => {
                Some(AuthenticationStep::Membership)
            }
            AuthenticationStep::Review | AuthenticationStep::Membership if !method_locked => {
                Some(AuthenticationStep::Method)
            }
            AuthenticationStep::Review
            | AuthenticationStep::Membership
            | AuthenticationStep::Method
                if !account_locked =>
            {
                Some(AuthenticationStep::Account)
            }
            _ => return false,
        };
        if self.step() == AuthenticationStep::Account {
            self.method = None;
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn back_skips_explicit_choices_and_earlier_changes_invalidate_dependents() {
        let mut journey = AuthenticationJourney::new();
        journey.select_account(Some("one".to_owned()));
        journey.select_method("browser".to_owned());
        journey.review();
        assert!(journey.back(false, true, true));
        assert_eq!(journey.step(), AuthenticationStep::Account);
        journey.select_account(Some("two".to_owned()));
        assert_eq!(journey.method(), None);
        assert!(!journey.back(true, true, true));
    }
}
