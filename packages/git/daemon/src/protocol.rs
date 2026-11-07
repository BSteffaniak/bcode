//! Messages between clients and the daemon: one JSON object per line.
//!
//! A conversation is: client [`ClientMessage::Hello`], daemon [`ServerMessage::Welcome`]
//! (or [`ServerMessage::Refused`] for another protocol version); client
//! [`ClientMessage::Subscribe`], daemon [`ServerMessage::Subscribed`] (or
//! [`ServerMessage::Failed`]); then daemon updates and client requests until either side
//! closes.

use std::io::{BufRead, Read, Write};
use std::path::PathBuf;

use bcode_git_live::source::SourceKind;
use bcode_git_live::{EngineConfig, LiveUpdate, Request};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

/// Version of this protocol; both sides must speak the same one.
pub const PROTOCOL: u32 = 1;

/// Longest message accepted, in bytes.
const MESSAGE_LIMIT: u64 = 256 * 1024 * 1024;

/// From a client.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientMessage {
    /// Opens the conversation.
    Hello {
        /// [`PROTOCOL`] of the client.
        protocol: u32,
    },
    /// Serve the worktree containing `path`.
    Subscribe {
        /// A directory inside the worktree.
        path: PathBuf,
        /// What to keep.
        config: EngineConfig,
        /// How to watch, should the repository not be served yet (`None`: without
        /// watching).
        watcher: Option<SourceKind>,
    },
    /// Work beyond reacting to changes.
    Request {
        /// The request.
        request: Request,
    },
}

/// From the daemon.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerMessage {
    /// The protocol matches.
    Welcome {
        /// [`PROTOCOL`] of the daemon.
        protocol: u32,
        /// Process id of the daemon.
        pid: u32,
    },
    /// The conversation cannot go on.
    Refused {
        /// Why.
        reason: String,
    },
    /// Subscribed; updates follow.
    Subscribed {
        /// Whether the repository's changes are watched.
        watching: bool,
    },
    /// The worktree cannot be served.
    Failed {
        /// Why.
        reason: String,
    },
    /// What changed.
    Update {
        /// The change.
        update: Box<LiveUpdate>,
    },
}

/// Write one message and flush it.
///
/// # Errors
/// * [`std::io::Error`] when writing fails.
pub fn write<T: Serialize>(writer: &mut impl Write, message: &T) -> std::io::Result<()> {
    let mut line = serde_json::to_vec(message).map_err(std::io::Error::other)?;
    line.push(b'\n');
    writer.write_all(&line)?;
    writer.flush()
}

/// Read one message; `None` at the end of the stream.
///
/// # Errors
/// * [`std::io::Error`] when reading fails, or the message is malformed or too long.
pub fn read<T: DeserializeOwned>(reader: &mut impl BufRead) -> std::io::Result<Option<T>> {
    let mut line = Vec::new();
    let read = reader.take(MESSAGE_LIMIT).read_until(b'\n', &mut line)?;
    if read == 0 {
        return Ok(None);
    }
    if line.last() != Some(&b'\n') {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "message too long or cut off",
        ));
    }
    serde_json::from_slice(&line)
        .map(Some)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn messages_round_trip_one_per_line() {
        let mut buffer = Vec::new();
        let sent = ClientMessage::Request {
            request: Request::Full,
        };
        write(&mut buffer, &sent).unwrap();
        write(&mut buffer, &ClientMessage::Hello { protocol: PROTOCOL }).unwrap();
        let mut reader = std::io::Cursor::new(buffer);
        assert_eq!(read::<ClientMessage>(&mut reader).unwrap(), Some(sent));
        assert!(matches!(
            read::<ClientMessage>(&mut reader).unwrap(),
            Some(ClientMessage::Hello { protocol: PROTOCOL })
        ));
        assert_eq!(read::<ClientMessage>(&mut reader).unwrap(), None);
    }
}
