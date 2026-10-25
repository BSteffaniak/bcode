//! Linux-only transport and filesystem security boundary.
use super::{
    Command, Dispatcher, Handler, MAX_FRAME, OperationId, Request, Response, RuntimeHandler,
    VERSION,
};
use crate::config::Config;
use crate::{Error, Result};
use serde::{Serialize, de::DeserializeOwned};
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Component, Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

const TIMEOUT: Duration = Duration::from_secs(2);
const SOCKET_NAME: &str = "control-v1.sock";
static CLIENT: Mutex<Option<(u32, String, u64)>> = Mutex::new(None);

fn invalid(message: &str) -> Error {
    Error::Invalid(message.into())
}

fn uid() -> u32 {
    // SAFETY: geteuid has no arguments or memory safety preconditions.
    unsafe { libc::geteuid() }
}

struct Directory(File);

impl Directory {
    fn path(&self) -> PathBuf {
        PathBuf::from(format!("/proc/self/fd/{}", self.0.as_raw_fd()))
    }

    fn socket(&self) -> PathBuf {
        self.path().join(SOCKET_NAME)
    }
}

fn open_directory(path: &Path) -> Result<File> {
    Ok(OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)?)
}

fn private_directory(file: &File, expected_uid: u32) -> Result<()> {
    let metadata = file.metadata()?;
    if !metadata.is_dir() || metadata.uid() != expected_uid || metadata.mode() & 0o077 != 0 {
        return Err(invalid(
            "Vox runtime directory must be owned by the current user with mode 0700",
        ));
    }
    Ok(())
}

fn directory_at(root: &Path, create: bool, expected_uid: u32) -> Result<Directory> {
    if !root.is_absolute() {
        return Err(invalid("XDG_RUNTIME_DIR must be absolute"));
    }
    // Open every component without following symlinks, keeping each parent pinned
    // by fd. /proc/self/fd is only used for descriptors opened by this process.
    let mut parent = Directory(open_directory(Path::new("/"))?);
    for component in root.components() {
        match component {
            Component::RootDir => {}
            Component::Normal(name) => {
                parent = Directory(open_directory(&parent.path().join(name))?);
            }
            _ => {
                return Err(invalid(
                    "XDG_RUNTIME_DIR must not contain relative components",
                ));
            }
        }
    }
    private_directory(&parent.0, expected_uid)?;
    let child = parent.path().join("vox");
    if create {
        let mut builder = std::fs::DirBuilder::new();
        std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
        match builder.create(&child) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error.into()),
        }
    }
    let directory = Directory(open_directory(&child)?);
    private_directory(&directory.0, expected_uid)?;
    Ok(directory)
}

fn directory(create: bool) -> Result<Directory> {
    let user = uid();
    if user == 0 {
        return Err(invalid("Vox IPC must run as a non-root user"));
    }
    let root = std::env::var_os("XDG_RUNTIME_DIR")
        .ok_or_else(|| invalid("XDG_RUNTIME_DIR is required; no temporary-directory fallback"))?;
    directory_at(Path::new(&root), create, user)
}

fn token() -> Result<String> {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut random = [0_u8; 16];
    File::open("/dev/urandom")?.read_exact(&mut random)?;
    let mut output = String::with_capacity(32);
    for byte in random {
        output.push(char::from(HEX[usize::from(byte >> 4)]));
        output.push(char::from(HEX[usize::from(byte & 15)]));
    }
    Ok(output)
}

pub(super) fn operation_id() -> Result<OperationId> {
    let response = exchange(Command::Status)?;
    response.result.map_err(super::Failure::into_error)?;
    let mut client = CLIENT
        .lock()
        .map_err(|_| invalid("IPC client identity lock failed"))?;
    let pid = std::process::id();
    if client.as_ref().is_none_or(|entry| entry.0 != pid) {
        *client = Some((pid, token()?, 0));
    }
    let (_, name, sequence) = client
        .as_mut()
        .ok_or_else(|| invalid("IPC identity unavailable"))?;
    *sequence = sequence
        .checked_add(1)
        .ok_or_else(|| invalid("IPC client sequence exhausted"))?;
    let id = OperationId {
        instance: response.instance,
        client: name.clone(),
        sequence: *sequence,
    };
    drop(client);
    Ok(id)
}

fn check_peer(stream: &UnixStream, expected_uid: u32) -> Result<()> {
    // SAFETY: ucred is a plain C integer struct; zero is valid for every field.
    let mut credentials: libc::ucred = unsafe { std::mem::zeroed() };
    let mut length = libc::socklen_t::try_from(std::mem::size_of::<libc::ucred>())
        .map_err(|_| invalid("peer credential size unsupported"))?;
    // SAFETY: credentials and length are writable, correctly sized objects and
    // stream owns a live socket descriptor for the duration of the call.
    let result = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            std::ptr::from_mut(&mut credentials).cast(),
            &raw mut length,
        )
    };
    if result != 0 {
        return Err(io::Error::last_os_error().into());
    }
    if usize::try_from(length).ok() != Some(std::mem::size_of::<libc::ucred>())
        || credentials.uid != expected_uid
    {
        return Err(invalid("Vox IPC peer ownership mismatch"));
    }
    Ok(())
}

fn remaining(deadline: Instant) -> io::Result<Duration> {
    deadline
        .checked_duration_since(Instant::now())
        .filter(|duration| !duration.is_zero())
        .ok_or_else(|| io::Error::new(io::ErrorKind::TimedOut, "Vox IPC deadline exceeded"))
}

// Local connect is nonblocking too: a full Unix listen backlog must not hang a client.
fn connect(path: &Path) -> Result<UnixStream> {
    // SAFETY: socket has no pointer arguments; descriptor ownership is transferred below.
    let raw = unsafe {
        libc::socket(
            libc::AF_UNIX,
            libc::SOCK_STREAM | libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK,
            0,
        )
    };
    if raw < 0 {
        return Err(io::Error::last_os_error().into());
    }
    // SAFETY: socket returned a fresh descriptor; OwnedFd becomes its sole owner.
    let socket = unsafe { OwnedFd::from_raw_fd(raw) };
    // SAFETY: sockaddr_un is a C struct whose integer/array fields admit zero.
    let mut address: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    address.sun_family = libc::sa_family_t::try_from(libc::AF_UNIX)
        .map_err(|_| invalid("Unix address family unsupported"))?;
    let bytes = path.as_os_str().as_bytes();
    if bytes.len() >= address.sun_path.len() || bytes.contains(&0) {
        return Err(invalid("Vox IPC socket path is too long or invalid"));
    }
    for (slot, byte) in address.sun_path.iter_mut().zip(bytes) {
        *slot = libc::c_char::from_ne_bytes([*byte]);
    }
    let length = libc::socklen_t::try_from(std::mem::size_of::<libc::sockaddr_un>())
        .map_err(|_| invalid("Unix address size unsupported"))?;
    // SAFETY: address is initialized and length describes the complete struct.
    let result = unsafe {
        libc::connect(
            socket.as_raw_fd(),
            std::ptr::from_ref(&address).cast(),
            length,
        )
    };
    if result != 0 {
        let error = io::Error::last_os_error();
        // EAGAIN for Unix-domain sockets means no connection was queued. Do not
        // retry; retrying mutations belongs exclusively to the explicit ID API.
        if error.raw_os_error() != Some(libc::EINPROGRESS) {
            return Err(error.into());
        }
        let deadline = Instant::now() + TIMEOUT;
        loop {
            let milliseconds = i32::try_from(remaining(deadline)?.as_millis().max(1))
                .map_err(|_| invalid("IPC timeout out of range"))?;
            let mut descriptor = libc::pollfd {
                fd: socket.as_raw_fd(),
                events: libc::POLLOUT,
                revents: 0,
            };
            // SAFETY: descriptor points to one initialized pollfd for this call.
            let ready = unsafe { libc::poll(&raw mut descriptor, 1, milliseconds) };
            if ready < 0 {
                let error = io::Error::last_os_error();
                if error.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(error.into());
            }
            if ready == 0 {
                continue;
            }
            break;
        }
    }
    let stream = UnixStream::from(socket);
    if let Some(error) = stream.take_error()? {
        return Err(error.into());
    }
    stream.set_nonblocking(false)?;
    Ok(stream)
}

fn read_exact(stream: &mut UnixStream, mut bytes: &mut [u8], deadline: Instant) -> Result<()> {
    while !bytes.is_empty() {
        stream.set_read_timeout(Some(remaining(deadline)?))?;
        match stream.read(bytes) {
            Ok(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "incomplete Vox IPC frame",
                )
                .into());
            }
            Ok(count) => bytes = &mut bytes[count..],
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

fn read_frame<T: DeserializeOwned>(stream: &mut UnixStream, deadline: Instant) -> Result<T> {
    let mut header = [0_u8; 4];
    read_exact(stream, &mut header, deadline)?;
    let size = usize::try_from(u32::from_be_bytes(header))
        .map_err(|_| invalid("invalid IPC frame size"))?;
    if size == 0 || size > MAX_FRAME {
        return Err(invalid("Vox IPC frame exceeds bounds"));
    }
    let mut bytes = vec![0; size];
    read_exact(stream, &mut bytes, deadline)?;
    // Never report serde's input-dependent error string across this boundary.
    serde_json::from_slice(&bytes).map_err(|_| invalid("malformed Vox IPC JSON"))
}

fn write_frame<T: Serialize>(stream: &mut UnixStream, value: &T, deadline: Instant) -> Result<()> {
    let bytes = serde_json::to_vec(value).map_err(|_| invalid("Vox IPC serialization failed"))?;
    if bytes.is_empty() || bytes.len() > MAX_FRAME {
        return Err(invalid("Vox IPC frame exceeds bounds"));
    }
    let size = u32::try_from(bytes.len()).map_err(|_| invalid("invalid IPC frame size"))?;
    let frame = [size.to_be_bytes().as_slice(), bytes.as_slice()].concat();
    let mut remaining_bytes = frame.as_slice();
    while !remaining_bytes.is_empty() {
        stream.set_write_timeout(Some(remaining(deadline)?))?;
        match stream.write(remaining_bytes) {
            Ok(0) => {
                return Err(
                    io::Error::new(io::ErrorKind::WriteZero, "Vox IPC write incomplete").into(),
                );
            }
            Ok(count) => remaining_bytes = &remaining_bytes[count..],
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

pub(super) fn exchange(command: Command) -> Result<Response> {
    let directory = directory(false)?;
    let mut stream = connect(&directory.socket())?;
    check_peer(&stream, uid())?;
    let deadline = Instant::now() + TIMEOUT;
    let expected_instance = match &command {
        Command::Action { id, .. } | Command::Shutdown { id } => Some(id.instance.clone()),
        Command::Status => None,
    };
    write_frame(
        &mut stream,
        &Request {
            version: VERSION,
            command,
        },
        deadline,
    )?;
    let response: Response = read_frame(&mut stream, deadline)?;
    if response.version != VERSION || !super::valid_token(&response.instance) {
        return Err(invalid("unsupported or malformed Vox IPC response"));
    }
    if expected_instance.is_some_and(|instance| instance != response.instance) {
        return Err(invalid(
            "Vox service restarted; old mutation was not retried",
        ));
    }
    Ok(response)
}

struct Endpoint {
    directory: Directory,
    device: u64,
    inode: u64,
}

impl Drop for Endpoint {
    fn drop(&mut self) {
        let path = self.directory.socket();
        if let Ok(metadata) = std::fs::symlink_metadata(&path)
            && metadata.file_type().is_socket()
            && metadata.dev() == self.device
            && metadata.ino() == self.inode
        {
            // Only our bound socket is eligible. Never remove a replacement,
            // symlink, or a pre-existing endpoint discovered during startup.
            let _ = std::fs::remove_file(path);
        }
    }
}

fn bind(directory: Directory) -> Result<(UnixListener, Endpoint)> {
    let path = directory.socket();
    // bind itself is the atomic singleton claim; EADDRINUSE is not repaired.
    let listener = UnixListener::bind(&path)?;
    let metadata = std::fs::symlink_metadata(&path)?;
    let endpoint = Endpoint {
        directory,
        device: metadata.dev(),
        inode: metadata.ino(),
    };
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    Ok((listener, endpoint))
}

fn connection<H: Handler>(
    stream: &mut UnixStream,
    dispatcher: &mut Dispatcher<H>,
    expected_uid: u32,
) -> Result<()> {
    check_peer(stream, expected_uid)?;
    let deadline = Instant::now() + TIMEOUT;
    let request: Request = read_frame(stream, deadline)?;
    let response = dispatcher.dispatch(&request);
    // If the reply is lost, deduplication already contains the mutation result.
    write_frame(stream, &response, deadline)
}

pub(super) fn serve(config: Config) -> Result<()> {
    config.validate()?;
    let (listener, _endpoint) = bind(directory(true)?)?;
    let handler = RuntimeHandler(crate::runtime::spawn(config)?);
    let mut dispatcher = Dispatcher::new(handler, token()?);
    while !dispatcher.stopping {
        match listener.accept() {
            Ok((mut stream, _)) => {
                // Invalid clients affect only their connection, with no input logging.
                let _ = connection(&mut stream, &mut dispatcher, uid());
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests;
