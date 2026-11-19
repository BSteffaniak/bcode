//! Environment variables the hosting process holds for itself.
//!
//! An application that hosts Bcode inside its own process (for example a database client
//! holding `PGPASSWORD`) registers the names of variables commands must not inherit. Shell
//! commands remove them alongside Bcode's own private variables and the configured
//! `tools.shell.remove_env`.

use std::collections::BTreeSet;
use std::sync::{PoisonError, RwLock};

static HOST_PRIVATE_ENV: RwLock<BTreeSet<String>> = RwLock::new(BTreeSet::new());

/// Keep `names` out of the environment of commands Bcode runs in this process from now on.
/// Names accumulate; empty names are ignored.
pub fn register_host_private_env(names: impl IntoIterator<Item = String>) {
    let mut registered = HOST_PRIVATE_ENV
        .write()
        .unwrap_or_else(PoisonError::into_inner);
    registered.extend(names.into_iter().filter(|name| !name.is_empty()));
}

/// The variables registered with [`register_host_private_env`], in name order.
#[must_use]
pub fn host_private_env() -> Vec<String> {
    HOST_PRIVATE_ENV
        .read()
        .unwrap_or_else(PoisonError::into_inner)
        .iter()
        .cloned()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registered_names_accumulate_without_empties_or_duplicates() {
        register_host_private_env(["PGPASSWORD".to_owned(), String::new()]);
        register_host_private_env(["DATABASE_URL".to_owned(), "PGPASSWORD".to_owned()]);
        let registered = host_private_env();
        assert!(registered.contains(&"PGPASSWORD".to_owned()));
        assert!(registered.contains(&"DATABASE_URL".to_owned()));
        assert!(!registered.contains(&String::new()));
        assert_eq!(
            registered
                .iter()
                .filter(|name| *name == "PGPASSWORD")
                .count(),
            1
        );
    }
}
