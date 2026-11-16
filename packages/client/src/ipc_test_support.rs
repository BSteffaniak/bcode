//! Private socket directories for client transport fixtures only.

/// Create one fresh fixture directory, private from its first filesystem entry.
/// Never repair permissions on an existing path: it might not belong to a test.
pub fn create_socket_directory(path: &std::path::Path) -> std::io::Result<()> {
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt as _;
        builder.mode(0o700);
    }
    builder.create(path)
}

#[cfg(unix)]
pub struct SocketListener {
    listener: Option<bcode_ipc::LocalIpcListener>,
    path: std::path::PathBuf,
    identity: (u64, u64),
}

#[cfg(unix)]
impl SocketListener {
    /// Bind only a fixture-owned socket. Drop closes the listener before unlinking
    /// the same inode; a replaced path is never cleanup authority.
    pub fn bind(endpoint: &bcode_ipc::IpcEndpoint) -> Result<Self, bcode_ipc::IpcTransportError> {
        use std::os::unix::fs::MetadataExt as _;
        let listener = bcode_ipc::LocalIpcListener::bind(endpoint)?;
        let bcode_ipc::IpcEndpoint::UnixSocket(path) = endpoint else {
            unreachable!("Unix fixture requires a socket endpoint");
        };
        let metadata = std::fs::symlink_metadata(path)?;
        Ok(Self {
            listener: Some(listener),
            path: path.clone(),
            identity: (metadata.dev(), metadata.ino()),
        })
    }

    pub async fn accept(&self) -> Result<bcode_ipc::LocalIpcStream, bcode_ipc::IpcTransportError> {
        self.listener
            .as_ref()
            .expect("fixture listener retained")
            .accept()
            .await
    }
}

#[cfg(unix)]
impl Drop for SocketListener {
    fn drop(&mut self) {
        use std::os::unix::fs::{FileTypeExt as _, MetadataExt as _};
        drop(self.listener.take());
        let metadata = std::fs::symlink_metadata(&self.path).expect("owned fixture socket exists");
        assert!(
            metadata.file_type().is_socket(),
            "fixture path replaced by non-socket"
        );
        assert_eq!(
            (metadata.dev(), metadata.ino()),
            self.identity,
            "fixture socket identity changed; refusing cleanup"
        );
        std::fs::remove_file(&self.path).expect("remove closed fixture-owned socket");
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::create_socket_directory;
    use std::os::unix::fs::{DirBuilderExt as _, PermissionsExt as _};

    #[tokio::test]
    async fn owned_listener_removes_only_its_closed_socket() {
        let parent = tempfile::tempdir().unwrap();
        let directory = parent.path().join("ipc");
        create_socket_directory(&directory).unwrap();
        let socket = directory.join("owned.sock");
        let endpoint = bcode_ipc::IpcEndpoint::unix_socket(&socket);
        let listener = super::SocketListener::bind(&endpoint).unwrap();
        let client = bcode_ipc::LocalIpcStream::connect(&endpoint).await.unwrap();
        let peer = listener.accept().await.unwrap();
        assert!(socket.exists());
        drop(client);
        drop(peer);
        drop(listener);
        assert!(!socket.exists());
        assert!(bcode_ipc::LocalIpcStream::connect(&endpoint).await.is_err());
        std::fs::remove_dir(directory).unwrap();
    }

    #[test]
    fn socket_fixture_is_private_at_creation() {
        let parent = tempfile::tempdir().unwrap();
        let directory = parent.path().join("socket-fixture");
        create_socket_directory(&directory).unwrap();
        assert_eq!(
            std::fs::metadata(directory).unwrap().permissions().mode() & 0o777,
            0o700
        );
    }

    #[test]
    fn socket_fixture_refuses_to_repair_an_existing_directory() {
        let parent = tempfile::tempdir().unwrap();
        let directory = parent.path().join("existing");
        std::fs::DirBuilder::new()
            .mode(0o755)
            .create(&directory)
            .unwrap();
        let before = std::fs::metadata(&directory).unwrap().permissions().mode();
        assert_eq!(
            create_socket_directory(&directory).unwrap_err().kind(),
            std::io::ErrorKind::AlreadyExists
        );
        assert_eq!(
            std::fs::metadata(directory).unwrap().permissions().mode(),
            before
        );
    }
}
