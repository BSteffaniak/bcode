use super::*;
use crate::contracts::{Action, Status};
use std::os::unix::fs::symlink;

#[derive(Default)]
struct Fake {
    calls: usize,
}

impl Handler for Fake {
    fn action(&mut self, _: Action) -> Result<Status> {
        self.calls += 1;
        Ok(Status::Recording { operation: 1 })
    }

    fn status(&self) -> Status {
        Status::Idle
    }
}

fn root() -> tempfile::TempDir {
    let root = tempfile::tempdir().unwrap();
    std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    root
}

#[test]
fn private_directory_creation_and_modes() {
    let root = root();
    let directory = directory_at(root.path(), true, uid()).unwrap();
    assert_eq!(directory.0.metadata().unwrap().mode() & 0o777, 0o700);
    let (_listener, endpoint) = bind(directory).unwrap();
    let path = root.path().join("vox").join(SOCKET_NAME);
    assert_eq!(
        std::fs::symlink_metadata(&path).unwrap().mode() & 0o777,
        0o600
    );
    drop(endpoint);
    assert!(!path.exists());
}

#[test]
fn directory_rejects_shared_modes_wrong_owner_and_symlinks() {
    let root = root();
    std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o750)).unwrap();
    assert!(directory_at(root.path(), true, uid()).is_err());
    std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    assert!(directory_at(root.path(), true, uid().wrapping_add(1)).is_err());
    let elsewhere = tempfile::tempdir().unwrap();
    symlink(elsewhere.path(), root.path().join("vox")).unwrap();
    assert!(directory_at(root.path(), true, uid()).is_err());
}

#[test]
fn no_symlinked_runtime_root_and_no_relative_root() {
    let root = root();
    let target = root.path().join("target");
    std::fs::create_dir(&target).unwrap();
    std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o700)).unwrap();
    let alias = root.path().join("alias");
    symlink(target, &alias).unwrap();
    assert!(directory_at(&alias, true, uid()).is_err());
    assert!(directory_at(Path::new("relative"), true, uid()).is_err());
}

#[test]
fn existing_live_and_stale_socket_are_never_deleted() {
    let root = root();
    let (listener, endpoint) = bind(directory_at(root.path(), true, uid()).unwrap()).unwrap();
    let path = root.path().join("vox").join(SOCKET_NAME);
    let inode = std::fs::symlink_metadata(&path).unwrap().ino();
    assert!(bind(directory_at(root.path(), true, uid()).unwrap()).is_err());
    assert_eq!(std::fs::symlink_metadata(&path).unwrap().ino(), inode);
    drop(listener);
    assert!(bind(directory_at(root.path(), true, uid()).unwrap()).is_err());
    assert_eq!(std::fs::symlink_metadata(&path).unwrap().ino(), inode);
    drop(endpoint);
}

#[test]
fn endpoint_cleanup_preserves_replacement_file() {
    let root = root();
    let (_listener, endpoint) = bind(directory_at(root.path(), true, uid()).unwrap()).unwrap();
    let path = root.path().join("vox").join(SOCKET_NAME);
    std::fs::remove_file(&path).unwrap();
    std::fs::write(&path, b"replacement").unwrap();
    drop(endpoint);
    assert_eq!(std::fs::read(path).unwrap(), b"replacement");
}

#[test]
fn peer_uid_is_verified_in_both_directions() {
    let (one, two) = UnixStream::pair().unwrap();
    assert!(check_peer(&one, uid()).is_ok());
    assert!(check_peer(&two, uid()).is_ok());
    assert!(check_peer(&one, uid().wrapping_add(1)).is_err());
}

#[test]
fn empty_oversized_truncated_and_invalid_json_frames_fail_closed() {
    for frame in [
        0_u32.to_be_bytes().to_vec(),
        u32::try_from(MAX_FRAME + 1).unwrap().to_be_bytes().to_vec(),
        vec![0, 0, 0, 9, b'{'],
        vec![0, 0, 0, 1, b'?'],
    ] {
        let (mut sender, mut receiver) = UnixStream::pair().unwrap();
        sender.write_all(&frame).unwrap();
        sender.shutdown(std::net::Shutdown::Write).unwrap();
        let mut dispatcher =
            Dispatcher::new(Fake::default(), "11111111111111111111111111111111".into());
        assert!(connection(&mut receiver, &mut dispatcher, uid()).is_err());
        assert_eq!(dispatcher.handler.calls, 0);
    }
}

#[test]
fn read_deadline_expires_for_incomplete_frame() {
    let (mut sender, mut receiver) = UnixStream::pair().unwrap();
    sender.write_all(&[0, 0]).unwrap();
    let result = read_frame::<Request>(&mut receiver, Instant::now() + Duration::from_millis(20));
    assert!(result.is_err());
}

#[test]
fn expired_deadline_and_oversized_output_fail_without_writing() {
    let (mut sender, mut receiver) = UnixStream::pair().unwrap();
    assert!(
        write_frame(
            &mut sender,
            &vec![0_u8; MAX_FRAME],
            Instant::now() + TIMEOUT
        )
        .is_err()
    );
    assert!(write_frame(&mut sender, &1, Instant::now()).is_err());
    receiver.set_nonblocking(true).unwrap();
    let mut byte = [0];
    assert_eq!(
        receiver.read(&mut byte).unwrap_err().kind(),
        io::ErrorKind::WouldBlock
    );
}

#[test]
fn lost_reply_is_recovered_without_repeating_action() {
    let mut dispatcher =
        Dispatcher::new(Fake::default(), "11111111111111111111111111111111".into());
    let make_request = || Request {
        version: VERSION,
        command: Command::Action {
            id: OperationId {
                instance: "11111111111111111111111111111111".into(),
                client: "22222222222222222222222222222222".into(),
                sequence: 1,
            },
            action: Action::Toggle,
        },
    };
    let (mut client, mut server) = UnixStream::pair().unwrap();
    write_frame(&mut client, &make_request(), Instant::now() + TIMEOUT).unwrap();
    drop(client);
    assert!(connection(&mut server, &mut dispatcher, uid()).is_err());
    assert_eq!(dispatcher.handler.calls, 1);

    let (mut client, mut server) = UnixStream::pair().unwrap();
    write_frame(&mut client, &make_request(), Instant::now() + TIMEOUT).unwrap();
    connection(&mut server, &mut dispatcher, uid()).unwrap();
    let reply: Response = read_frame(&mut client, Instant::now() + TIMEOUT).unwrap();
    assert_eq!(reply.result, Ok(Status::Recording { operation: 1 }));
    assert_eq!(dispatcher.handler.calls, 1);
}

#[test]
fn owned_directory_socket_can_be_connected_without_launching_processes() {
    let root = root();
    let (listener, endpoint) = bind(directory_at(root.path(), true, uid()).unwrap()).unwrap();
    let client = connect(&endpoint.directory.socket()).unwrap();
    let (server, _) = listener.accept().unwrap();
    assert!(check_peer(&client, uid()).is_ok());
    assert!(check_peer(&server, uid()).is_ok());
}
