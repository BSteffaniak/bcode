//! Query-independent path normalization and derived-directory cache.
use super::{SearchPathKind, matcher};
use std::collections::BTreeSet;
use std::sync::{Arc, Mutex, TryLockError};
use std::time::Duration;

pub(super) struct Entry {
    pub path: String,
    pub lower: String,
    pub mask: u64,
    pub kind: SearchPathKind,
    pub depth: usize,
    pub hidden: bool,
}
pub(super) struct Prepared {
    source: Arc<BTreeSet<String>>,
    pub entries: Vec<Entry>,
}
static TRACKED: Mutex<Option<Arc<Prepared>>> = Mutex::new(None);
static WORKSPACE: Mutex<Option<Arc<Prepared>>> = Mutex::new(None);

pub(super) fn load(
    source: &Arc<BTreeSet<String>>,
    workspace: bool,
    tick: &mut dyn FnMut() -> Result<(), &'static str>,
) -> Result<Arc<Prepared>, &'static str> {
    let slot = if workspace { &WORKSPACE } else { &TRACKED };
    let mut cache = loop {
        tick()?;
        match slot.try_lock() {
            Ok(cache) => break cache,
            Err(TryLockError::Poisoned(_)) => return Err("inventory_unavailable"),
            Err(TryLockError::WouldBlock) => std::thread::sleep(Duration::from_millis(1)),
        }
    };
    if let Some(prepared) = cache
        .as_ref()
        .filter(|cached| Arc::ptr_eq(&cached.source, source))
    {
        return Ok(Arc::clone(prepared));
    }
    let mut directories = BTreeSet::new();
    let mut entries = Vec::new();
    let mut bytes = 0_usize;
    for (index, path) in source.iter().enumerate() {
        if index % 128 == 0 {
            tick()?;
        }
        if let Some(directory) = path.strip_suffix('/') {
            insert_directory(&mut directories, directory, &mut bytes)?;
        } else {
            push(&mut entries, path, SearchPathKind::File, &mut bytes)?;
        }
        for (offset, _) in path.match_indices('/') {
            if offset > 0 {
                insert_directory(&mut directories, &path[..offset], &mut bytes)?;
            }
        }
        if directories.len() > 250_000 {
            return Err("inventory_entry_limit");
        }
    }
    for (index, directory) in directories.into_iter().enumerate() {
        if index % 128 == 0 {
            tick()?;
        }
        push(
            &mut entries,
            &directory,
            SearchPathKind::Directory,
            &mut bytes,
        )?;
    }
    tick()?;
    let prepared = Arc::new(Prepared {
        source: Arc::clone(source),
        entries,
    });
    *cache = Some(Arc::clone(&prepared));
    drop(cache);
    Ok(prepared)
}
fn push(
    entries: &mut Vec<Entry>,
    path: &str,
    kind: SearchPathKind,
    bytes: &mut usize,
) -> Result<(), &'static str> {
    let lower = path.to_lowercase();
    *bytes = bytes.saturating_add(path.len() + lower.len() + 128);
    if *bytes > 64 * 1024 * 1024 {
        return Err("inventory_byte_limit");
    }
    entries.push(Entry {
        mask: matcher::mask(&lower),
        lower,
        path: path.into(),
        kind,
        depth: path.bytes().filter(|byte| *byte == b'/').count() + 1,
        hidden: path.split('/').any(|part| part.starts_with('.')),
    });
    Ok(())
}

fn insert_directory(
    directories: &mut BTreeSet<String>,
    path: &str,
    bytes: &mut usize,
) -> Result<(), &'static str> {
    if !directories.contains(path) {
        *bytes = bytes.saturating_add(path.len() + 64);
        if *bytes > 64 * 1024 * 1024 {
            return Err("inventory_byte_limit");
        }
        directories.insert(path.to_owned());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn normalization_and_directories_are_reused_per_source_identity() {
        let source = Arc::new(BTreeSet::from(["src/İ中.txt".into(), "empty/".into()]));
        let first = load(&source, false, &mut || Ok(())).unwrap();
        let second = load(&source, false, &mut || Ok(())).unwrap();
        assert!(Arc::ptr_eq(&first, &second));
        assert!(
            first
                .entries
                .iter()
                .any(|entry| entry.path == "empty" && entry.kind == SearchPathKind::Directory)
        );
        assert!(first.entries.iter().any(|entry| entry.path == "src"));
        assert!(
            first
                .entries
                .iter()
                .any(|entry| entry.lower == "src/i\u{307}中.txt")
        );
        assert!(load(&source, false, &mut || Err("cancelled")).is_err());
    }
}
