//! Mutable working notes, separate from finalized artifact compression and event authority.
use bcode_session_models::{
    MAX_WORKING_DOCUMENT_BYTES, SessionWorkingDocument, SessionWorkingDocumentRequest,
};
use std::{io, path::Path};

/// Prepare or read one bounded working document under the owning session root.
/// Caller must hold session ownership and authorize explicit creation before calling.
///
/// # Errors
/// Rejects unsupported versions, noncanonical UUIDs, oversized content, symlinks,
/// unknown file representations and unavailable storage. Existing notes are never overwritten.
pub fn access(
    root: &Path,
    request: &SessionWorkingDocumentRequest,
) -> io::Result<Option<SessionWorkingDocument>> {
    let invalid = || {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid working document request or storage",
        )
    };
    if request.version != 1
        || uuid::Uuid::parse_str(&request.scope_id)
            .map_err(|_| invalid())?
            .to_string()
            != request.scope_id
        || request
            .initial_text
            .as_ref()
            .is_some_and(|s| s.len() > MAX_WORKING_DOCUMENT_BYTES)
    {
        return Err(invalid());
    }
    let relative = Path::new("session-artifacts")
        .join(request.session_id.to_string())
        .join("working-documents")
        .join(&request.scope_id);
    #[cfg(unix)]
    {
        use crate::artifact_storage::confined;
        use std::{ffi::CString, fs::File, io::Read as _, os::fd::AsRawFd as _};
        let root = root.canonicalize()?;
        let mut parent = File::open(&root)?;
        for component in relative.components() {
            let name =
                CString::new(component.as_os_str().as_encoded_bytes()).map_err(|_| invalid())?;
            if request.initial_text.is_some() {
                // SAFETY: parent is a live directory descriptor and name is NUL terminated.
                let result = unsafe { libc::mkdirat(parent.as_raw_fd(), name.as_ptr(), 0o700) };
                if result != 0 && io::Error::last_os_error().kind() != io::ErrorKind::AlreadyExists
                {
                    return Err(io::Error::last_os_error());
                }
            }
            match confined::open_child(&parent, &name, true) {
                Ok(next) => parent = next,
                Err(error)
                    if error.kind() == io::ErrorKind::NotFound
                        && request.initial_text.is_none() =>
                {
                    return Ok(None);
                }
                Err(error) => return Err(error),
            }
        }
        if let Some(text) = &request.initial_text {
            publish_initial(&parent, text)?;
        }
        let file = match confined::open_child(&parent, c"progress.md", false) {
            Ok(file) => file,
            Err(error)
                if error.kind() == io::ErrorKind::NotFound && request.initial_text.is_none() =>
            {
                return Ok(None);
            }
            Err(error) => return Err(error),
        };
        if !file.metadata()?.is_file() {
            return Err(invalid());
        }
        let mut bytes = Vec::new();
        file.take((MAX_WORKING_DOCUMENT_BYTES + 1) as u64)
            .read_to_end(&mut bytes)?;
        if bytes.len() > MAX_WORKING_DOCUMENT_BYTES {
            return Err(invalid());
        }
        let text = String::from_utf8(bytes).map_err(|_| invalid())?;
        Ok(Some(SessionWorkingDocument {
            session_id: request.session_id,
            scope_id: request.scope_id.clone(),
            path: root
                .join(relative)
                .join("progress.md")
                .to_string_lossy()
                .into_owned(),
            text,
        }))
    }
    #[cfg(not(unix))]
    {
        let _ = (root, relative);
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "confined working documents unavailable on this platform",
        ))
    }
}

#[cfg(unix)]
fn publish_initial(parent: &std::fs::File, text: &str) -> io::Result<()> {
    use std::{
        ffi::CString,
        fs::File,
        io::Write as _,
        os::fd::{AsRawFd as _, FromRawFd as _},
    };
    let temporary =
        CString::new(format!(".prepare-{}", uuid::Uuid::new_v4())).expect("UUID contains no NUL");
    // SAFETY: parent and NUL-terminated name remain live throughout the syscall.
    let fd = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            temporary.as_ptr(),
            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            0o600,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: transfer freshly created descriptor exactly once.
    let mut file = unsafe { File::from_raw_fd(fd) };
    let result = (|| {
        file.write_all(text.as_bytes())?;
        file.sync_all()?;
        // SAFETY: both names and the parent descriptor are live. linkat never replaces a destination.
        if unsafe {
            libc::linkat(
                parent.as_raw_fd(),
                temporary.as_ptr(),
                parent.as_raw_fd(),
                c"progress.md".as_ptr(),
                0,
            )
        } != 0
            && io::Error::last_os_error().kind() != io::ErrorKind::AlreadyExists
        {
            return Err(io::Error::last_os_error());
        }
        parent.sync_all()
    })();
    // SAFETY: remove only the uniquely created staging name, never the document.
    unsafe { libc::unlinkat(parent.as_raw_fd(), temporary.as_ptr(), 0) };
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn documents_are_scoped_bounded_and_never_overwritten() {
        let root = tempfile::tempdir().unwrap();
        let mut request = SessionWorkingDocumentRequest {
            version: 1,
            session_id: bcode_session_models::SessionId::new(),
            scope_id: uuid::Uuid::new_v4().to_string(),
            initial_text: None,
        };
        assert!(access(root.path(), &request).unwrap().is_none());
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0);
        request.initial_text = Some("- [ ] Work".into());
        let first = access(root.path(), &request).unwrap().unwrap();
        std::fs::write(&first.path, "- [x] Work\n- [ ] New finding").unwrap();
        assert!(
            access(root.path(), &request)
                .unwrap()
                .unwrap()
                .text
                .contains("New finding")
        );
        request.scope_id = uuid::Uuid::new_v4().to_string();
        assert_ne!(
            access(root.path(), &request).unwrap().unwrap().path,
            first.path
        );
        request.scope_id = "../escape".into();
        assert!(access(root.path(), &request).is_err());
        request.scope_id = uuid::Uuid::new_v4().to_string();
        request.initial_text = Some("x".repeat(MAX_WORKING_DOCUMENT_BYTES + 1));
        assert!(access(root.path(), &request).is_err());
        request.version = 2;
        assert!(access(root.path(), &request).is_err());
    }
    #[cfg(unix)]
    #[test]
    fn refuses_symlink_leaf_and_preserves_other_state_roots() {
        let root = tempfile::tempdir().unwrap();
        let other = tempfile::tempdir().unwrap();
        let request = SessionWorkingDocumentRequest {
            version: 1,
            session_id: bcode_session_models::SessionId::new(),
            scope_id: uuid::Uuid::new_v4().to_string(),
            initial_text: Some("initial".into()),
        };
        let doc = access(root.path(), &request).unwrap().unwrap();
        let other_doc = access(other.path(), &request).unwrap().unwrap();
        std::fs::remove_file(&doc.path).unwrap();
        std::os::unix::fs::symlink(&other_doc.path, &doc.path).unwrap();
        assert!(access(root.path(), &request).is_err());
        assert_eq!(std::fs::read_to_string(other_doc.path).unwrap(), "initial");
        let mut read = request;
        read.initial_text = None;
        assert!(access(root.path(), &read).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symlinked_parents_and_documents() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(outside.path(), root.path().join("session-artifacts")).unwrap();
        let request = SessionWorkingDocumentRequest {
            version: 1,
            session_id: bcode_session_models::SessionId::new(),
            scope_id: uuid::Uuid::new_v4().to_string(),
            initial_text: Some("notes".into()),
        };
        assert!(access(root.path(), &request).is_err());
        assert_eq!(std::fs::read_dir(outside.path()).unwrap().count(), 0);
    }
}
