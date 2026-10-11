//! Bounded, process-local leases. Bearer values are never retained or logged.
use rand::TryRngCore;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;

#[derive(Clone)]
pub struct Lease {
    pub(super) scope: String,
    pub(super) expires_at: u64,
}

pub struct Leases {
    entries: BTreeMap<[u8; 32], Lease>,
    capacity: usize,
}

fn random_id() -> Result<String, &'static str> {
    use std::fmt::Write as _;
    let mut bytes = [0_u8; 32];
    rand::rngs::OsRng
        .try_fill_bytes(&mut bytes)
        .map_err(|_| "randomness_unavailable")?;
    let mut output = String::with_capacity(64);
    for byte in bytes {
        write!(&mut output, "{byte:02x}").map_err(|_| "randomness_unavailable")?;
    }
    Ok(output)
}

fn digest(token: &str) -> [u8; 32] {
    Sha256::digest(token.as_bytes()).into()
}

impl Leases {
    pub(super) const fn new(capacity: usize) -> Self {
        Self {
            entries: BTreeMap::new(),
            capacity,
        }
    }

    pub(super) fn issue(
        &mut self,
        time: u64,
        expires_at: u64,
    ) -> Result<(String, Lease), &'static str> {
        self.entries.retain(|_, lease| lease.expires_at > time);
        if self.entries.len() >= self.capacity || expires_at <= time {
            return Err("lease_capacity_exhausted");
        }
        let token = random_id()?;
        let lease = Lease {
            scope: random_id()?,
            expires_at,
        };
        self.entries.insert(digest(&token), lease.clone());
        Ok((token, lease))
    }

    pub(super) fn resolve(&self, token: &str, time: u64) -> Option<Lease> {
        if token.len() != 64 || !token.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return None;
        }
        self.entries
            .get(&digest(token))
            .filter(|lease| lease.expires_at > time)
            .cloned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn leases_are_opaque_bounded_and_expire() {
        let mut store = Leases::new(1);
        let (token, lease) = store.issue(100, 110).unwrap();
        assert_ne!(token, lease.scope);
        assert!(store.resolve(&token, 109).is_some());
        assert!(store.resolve(&token, 110).is_none());
        assert!(store.issue(109, 120).is_err());
        assert!(store.issue(110, 120).is_ok());
        assert!(store.resolve(&"0".repeat(64), 111).is_none());
    }
}
