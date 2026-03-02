//! Wire protocol for communication between Zed and `pty-host`.
//!
//! The protocol is a simple length-prefixed binary format optimised for the
//! common case (DATA messages — raw terminal I/O bytes).
//!
//! ## Frame layout
//!
//! ```text
//! ┌──────────┬────────────────┬─────────────────────┐
//! │ type: u8 │ length: u32 LE │ payload: [u8; length]│
//! └──────────┴────────────────┴─────────────────────┘
//! ```
//!
//! The `length` field covers only the payload, not the header.
//! Maximum payload size is 16 MiB (to bound memory usage on decode).

use std::io::{self, Read, Write};

/// Protocol version. Bump this whenever the wire format changes.
/// The host sends this in the READY message; the client rejects
/// connections with a mismatched version.
pub const PROTOCOL_VERSION: u32 = 4;

/// Header size in bytes: 1 (type) + 4 (length).
pub const HEADER_SIZE: usize = 5;

/// Maximum payload size (16 MiB). Anything larger is rejected as malformed.
pub const MAX_PAYLOAD_SIZE: u32 = 16 * 1024 * 1024;

// ---------------------------------------------------------------------------
// Message types
// ---------------------------------------------------------------------------

/// Messages sent from Zed → PTY host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClientMessage {
    /// Raw bytes to write to the PTY (terminal input from the user).
    Data(Vec<u8>),

    /// Resize the PTY.
    Resize(WindowSize),

    /// Graceful disconnect — keep the session alive.
    Detach,

    /// Terminate the shell and clean up the session.
    Kill,
}

/// Messages sent from PTY host → Zed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostMessage {
    /// Raw bytes read from the PTY (terminal output).
    Data(Vec<u8>),

    /// The child process exited.
    ChildExit {
        /// Raw wait status from `waitpid` on Unix.
        raw_status: Option<i32>,
    },

    /// The session is ready — sent after the PTY is created and the shell is
    /// spawned. Contains the protocol version and child PID.
    Ready { version: u32, pid: u32, cols: u16, rows: u16 },

    /// An error occurred in the host.
    Error(String),

    /// Sent after all replay DATA messages. The client can stop draining
    /// replay and switch to live mode.
    ReplayDone,

    /// Serialised terminal state snapshot (binary TermState).
    /// Sent instead of Data-based ANSI replay when the host uses serde-based
    /// serialisation. The client deserialises with `bincode::deserialize`.
    Snapshot(Vec<u8>),
}

/// Terminal window size, matching alacritty's `WindowSize`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WindowSize {
    pub cols: u16,
    pub rows: u16,
    pub cell_width: u16,
    pub cell_height: u16,
}

// ---------------------------------------------------------------------------
// Wire type tags
// ---------------------------------------------------------------------------

mod tag {
    // Client → Host
    pub const CLIENT_DATA: u8 = 0x01;
    pub const CLIENT_RESIZE: u8 = 0x02;
    pub const CLIENT_DETACH: u8 = 0x03;
    pub const CLIENT_KILL: u8 = 0x04;

    // Host → Client
    pub const HOST_DATA: u8 = 0x81;
    pub const HOST_CHILD_EXIT: u8 = 0x82;
    pub const HOST_READY: u8 = 0x83;
    pub const HOST_ERROR: u8 = 0x84;
    pub const HOST_REPLAY_DONE: u8 = 0x85;
    pub const HOST_SNAPSHOT: u8 = 0x86;
}

// ---------------------------------------------------------------------------
// Encoding
// ---------------------------------------------------------------------------

impl ClientMessage {
    /// Encode this message into `buf`, returning the number of bytes written.
    pub fn encode_into(&self, buf: &mut Vec<u8>) {
        match self {
            ClientMessage::Data(bytes) => {
                write_header(buf, tag::CLIENT_DATA, bytes.len() as u32);
                buf.extend_from_slice(bytes);
            }
            ClientMessage::Resize(size) => {
                write_header(buf, tag::CLIENT_RESIZE, 8);
                buf.extend_from_slice(&size.cols.to_le_bytes());
                buf.extend_from_slice(&size.rows.to_le_bytes());
                buf.extend_from_slice(&size.cell_width.to_le_bytes());
                buf.extend_from_slice(&size.cell_height.to_le_bytes());
            }
            ClientMessage::Detach => {
                write_header(buf, tag::CLIENT_DETACH, 0);
            }
            ClientMessage::Kill => {
                write_header(buf, tag::CLIENT_KILL, 0);
            }
        }
    }

    /// Convenience: encode into a new `Vec<u8>`.
    pub fn encode(&self) -> Vec<u8> {
        let mut buf = Vec::with_capacity(HEADER_SIZE + self.payload_len());
        self.encode_into(&mut buf);
        buf
    }

    fn payload_len(&self) -> usize {
        match self {
            ClientMessage::Data(bytes) => bytes.len(),
            ClientMessage::Resize(_) => 8,
            ClientMessage::Detach | ClientMessage::Kill => 0,
        }
    }
}

impl HostMessage {
    /// Encode this message into `buf`.
    pub fn encode_into(&self, buf: &mut Vec<u8>) {
        match self {
            HostMessage::Data(bytes) => {
                write_header(buf, tag::HOST_DATA, bytes.len() as u32);
                buf.extend_from_slice(bytes);
            }
            HostMessage::ChildExit { raw_status } => {
                // Payload: 1 byte has_status + 4 bytes status
                let status = raw_status.unwrap_or(0);
                let has = if raw_status.is_some() { 1u8 } else { 0u8 };
                write_header(buf, tag::HOST_CHILD_EXIT, 5);
                buf.push(has);
                buf.extend_from_slice(&status.to_le_bytes());
            }
            HostMessage::Ready { version, pid, cols, rows } => {
                write_header(buf, tag::HOST_READY, 12);
                buf.extend_from_slice(&version.to_le_bytes());
                buf.extend_from_slice(&pid.to_le_bytes());
                buf.extend_from_slice(&cols.to_le_bytes());
                buf.extend_from_slice(&rows.to_le_bytes());
            }
            HostMessage::Error(msg) => {
                let bytes = msg.as_bytes();
                write_header(buf, tag::HOST_ERROR, bytes.len() as u32);
                buf.extend_from_slice(bytes);
            }
            HostMessage::ReplayDone => {
                write_header(buf, tag::HOST_REPLAY_DONE, 0);
            }
            HostMessage::Snapshot(bytes) => {
                write_header(buf, tag::HOST_SNAPSHOT, bytes.len() as u32);
                buf.extend_from_slice(bytes);
            }
        }
    }

    /// Convenience: encode into a new `Vec<u8>`.
    pub fn encode(&self) -> Vec<u8> {
        let mut buf = Vec::with_capacity(HEADER_SIZE + self.payload_len());
        self.encode_into(&mut buf);
        buf
    }

    fn payload_len(&self) -> usize {
        match self {
            HostMessage::Data(bytes) => bytes.len(),
            HostMessage::ChildExit { .. } => 5,
            HostMessage::Ready { .. } => 12,
            HostMessage::Error(msg) => msg.len(),
            HostMessage::ReplayDone => 0,
            HostMessage::Snapshot(bytes) => bytes.len(),
        }
    }
}

fn write_header(buf: &mut Vec<u8>, type_tag: u8, length: u32) {
    buf.push(type_tag);
    buf.extend_from_slice(&length.to_le_bytes());
}

// ---------------------------------------------------------------------------
// Decoding
// ---------------------------------------------------------------------------

/// Decode errors.
#[derive(Debug)]
pub enum DecodeError {
    /// The stream reached EOF mid-message or before any data.
    UnexpectedEof,
    /// An I/O error occurred during reading.
    Io(io::Error),
    /// The type tag is not recognized.
    UnknownTag(u8),
    /// The payload length exceeds `MAX_PAYLOAD_SIZE`.
    PayloadTooLarge(u32),
    /// A payload could not be parsed (e.g., bad UTF-8 in an error message).
    MalformedPayload(&'static str),
}

impl std::fmt::Display for DecodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DecodeError::UnexpectedEof => write!(f, "unexpected EOF"),
            DecodeError::Io(e) => write!(f, "I/O error: {e}"),
            DecodeError::UnknownTag(t) => write!(f, "unknown message tag: 0x{t:02x}"),
            DecodeError::PayloadTooLarge(n) => write!(f, "payload too large: {n} bytes"),
            DecodeError::MalformedPayload(msg) => write!(f, "malformed payload: {msg}"),
        }
    }
}

impl std::error::Error for DecodeError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            DecodeError::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<io::Error> for DecodeError {
    fn from(err: io::Error) -> Self {
        if err.kind() == io::ErrorKind::UnexpectedEof {
            DecodeError::UnexpectedEof
        } else {
            DecodeError::Io(err)
        }
    }
}

impl ClientMessage {
    /// Read one message from a stream. Blocks until a complete message is
    /// available or EOF / error.
    pub fn decode_from(reader: &mut impl Read) -> Result<Self, DecodeError> {
        let (type_tag, payload) = read_frame(reader)?;
        match type_tag {
            tag::CLIENT_DATA => Ok(ClientMessage::Data(payload)),
            tag::CLIENT_RESIZE => {
                if payload.len() != 8 {
                    return Err(DecodeError::MalformedPayload("RESIZE expects 8 bytes"));
                }
                Ok(ClientMessage::Resize(WindowSize {
                    cols: u16::from_le_bytes([payload[0], payload[1]]),
                    rows: u16::from_le_bytes([payload[2], payload[3]]),
                    cell_width: u16::from_le_bytes([payload[4], payload[5]]),
                    cell_height: u16::from_le_bytes([payload[6], payload[7]]),
                }))
            }
            tag::CLIENT_DETACH => Ok(ClientMessage::Detach),
            tag::CLIENT_KILL => Ok(ClientMessage::Kill),
            other => Err(DecodeError::UnknownTag(other)),
        }
    }
}

impl HostMessage {
    /// Read one message from a stream. Blocks until a complete message is
    /// available or EOF / error.
    pub fn decode_from(reader: &mut impl Read) -> Result<Self, DecodeError> {
        let (type_tag, payload) = read_frame(reader)?;
        match type_tag {
            tag::HOST_DATA => Ok(HostMessage::Data(payload)),
            tag::HOST_CHILD_EXIT => {
                if payload.len() != 5 {
                    return Err(DecodeError::MalformedPayload("CHILD_EXIT expects 5 bytes"));
                }
                let has = payload[0] != 0;
                let code = i32::from_le_bytes([payload[1], payload[2], payload[3], payload[4]]);
                Ok(HostMessage::ChildExit {
                    raw_status: if has { Some(code) } else { None },
                })
            }
            tag::HOST_READY => {
                if payload.len() != 12 {
                    return Err(DecodeError::MalformedPayload("READY expects 12 bytes"));
                }
                let version =
                    u32::from_le_bytes([payload[0], payload[1], payload[2], payload[3]]);
                let pid =
                    u32::from_le_bytes([payload[4], payload[5], payload[6], payload[7]]);
                let cols =
                    u16::from_le_bytes([payload[8], payload[9]]);
                let rows =
                    u16::from_le_bytes([payload[10], payload[11]]);
                Ok(HostMessage::Ready { version, pid, cols, rows })
            }
            tag::HOST_ERROR => {
                let msg = String::from_utf8(payload)
                    .map_err(|_| DecodeError::MalformedPayload("ERROR payload is not UTF-8"))?;
                Ok(HostMessage::Error(msg))
            }
            tag::HOST_REPLAY_DONE => Ok(HostMessage::ReplayDone),
            tag::HOST_SNAPSHOT => Ok(HostMessage::Snapshot(payload)),
            other => Err(DecodeError::UnknownTag(other)),
        }
    }
}

fn read_frame(reader: &mut impl Read) -> Result<(u8, Vec<u8>), DecodeError> {
    let mut header = [0u8; HEADER_SIZE];
    reader.read_exact(&mut header)?;

    let type_tag = header[0];
    let length = u32::from_le_bytes([header[1], header[2], header[3], header[4]]);

    if length > MAX_PAYLOAD_SIZE {
        return Err(DecodeError::PayloadTooLarge(length));
    }

    let mut payload = vec![0u8; length as usize];
    if length > 0 {
        reader.read_exact(&mut payload)?;
    }

    Ok((type_tag, payload))
}

// ---------------------------------------------------------------------------
// Stream helpers
// ---------------------------------------------------------------------------

/// Write a message to a stream and flush.
pub fn send_client_message(writer: &mut impl Write, msg: &ClientMessage) -> io::Result<()> {
    let encoded = msg.encode();
    writer.write_all(&encoded)?;
    writer.flush()
}

/// Write a message to a stream and flush.
pub fn send_host_message(writer: &mut impl Write, msg: &HostMessage) -> io::Result<()> {
    let encoded = msg.encode();
    writer.write_all(&encoded)?;
    writer.flush()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn client_data_round_trips() {
        let msg = ClientMessage::Data(b"hello world".to_vec());
        let encoded = msg.encode();
        let decoded = ClientMessage::decode_from(&mut Cursor::new(&encoded)).unwrap();
        assert_eq!(msg, decoded);
    }

    #[test]
    fn client_resize_round_trips() {
        let msg = ClientMessage::Resize(WindowSize {
            cols: 80,
            rows: 24,
            cell_width: 8,
            cell_height: 16,
        });
        let encoded = msg.encode();
        let decoded = ClientMessage::decode_from(&mut Cursor::new(&encoded)).unwrap();
        assert_eq!(msg, decoded);
    }

    #[test]
    fn client_detach_round_trips() {
        let msg = ClientMessage::Detach;
        let encoded = msg.encode();
        let decoded = ClientMessage::decode_from(&mut Cursor::new(&encoded)).unwrap();
        assert_eq!(msg, decoded);
    }

    #[test]
    fn client_kill_round_trips() {
        let msg = ClientMessage::Kill;
        let encoded = msg.encode();
        let decoded = ClientMessage::decode_from(&mut Cursor::new(&encoded)).unwrap();
        assert_eq!(msg, decoded);
    }

    #[test]
    fn host_data_round_trips() {
        let msg = HostMessage::Data(b"\x1b[31mred\x1b[0m".to_vec());
        let encoded = msg.encode();
        let decoded = HostMessage::decode_from(&mut Cursor::new(&encoded)).unwrap();
        assert_eq!(msg, decoded);
    }

    #[test]
    fn host_child_exit_with_code_round_trips() {
        let msg = HostMessage::ChildExit {
            raw_status: Some(42),
        };
        let encoded = msg.encode();
        let decoded = HostMessage::decode_from(&mut Cursor::new(&encoded)).unwrap();
        assert_eq!(msg, decoded);
    }

    #[test]
    fn host_child_exit_no_code_round_trips() {
        let msg = HostMessage::ChildExit { raw_status: None };
        let encoded = msg.encode();
        let decoded = HostMessage::decode_from(&mut Cursor::new(&encoded)).unwrap();
        assert_eq!(msg, decoded);
    }

    #[test]
    fn host_ready_round_trips() {
        let msg = HostMessage::Ready { version: PROTOCOL_VERSION, pid: 12345, cols: 80, rows: 24 };
        let encoded = msg.encode();
        let decoded = HostMessage::decode_from(&mut Cursor::new(&encoded)).unwrap();
        assert_eq!(msg, decoded);
    }

    #[test]
    fn host_replay_done_round_trips() {
        let msg = HostMessage::ReplayDone;
        let encoded = msg.encode();
        let decoded = HostMessage::decode_from(&mut Cursor::new(&encoded)).unwrap();
        assert_eq!(msg, decoded);
    }

    #[test]
    fn host_error_round_trips() {
        let msg = HostMessage::Error("something went wrong".to_string());
        let encoded = msg.encode();
        let decoded = HostMessage::decode_from(&mut Cursor::new(&encoded)).unwrap();
        assert_eq!(msg, decoded);
    }

    #[test]
    fn empty_data_round_trips() {
        let msg = ClientMessage::Data(vec![]);
        let encoded = msg.encode();
        let decoded = ClientMessage::decode_from(&mut Cursor::new(&encoded)).unwrap();
        assert_eq!(msg, decoded);
    }

    #[test]
    fn multiple_messages_in_stream() {
        let messages = vec![
            ClientMessage::Data(b"first".to_vec()),
            ClientMessage::Resize(WindowSize {
                cols: 120,
                rows: 40,
                cell_width: 7,
                cell_height: 14,
            }),
            ClientMessage::Data(b"second".to_vec()),
            ClientMessage::Detach,
        ];

        let mut buf = Vec::new();
        for msg in &messages {
            msg.encode_into(&mut buf);
        }

        let mut cursor = Cursor::new(&buf);
        for expected in &messages {
            let decoded = ClientMessage::decode_from(&mut cursor).unwrap();
            assert_eq!(expected, &decoded);
        }
    }

    #[test]
    fn unknown_tag_returns_error() {
        let frame = [0xFF, 0, 0, 0, 0]; // unknown tag, zero-length payload
        let result = ClientMessage::decode_from(&mut Cursor::new(&frame));
        assert!(matches!(result, Err(DecodeError::UnknownTag(0xFF))));
    }

    #[test]
    fn truncated_header_returns_eof() {
        let frame = [0x01, 0x05]; // only 2 of 5 header bytes
        let result = ClientMessage::decode_from(&mut Cursor::new(&frame));
        assert!(matches!(result, Err(DecodeError::UnexpectedEof)));
    }

    #[test]
    fn truncated_payload_returns_eof() {
        let frame = [0x01, 0x0A, 0x00, 0x00, 0x00, b'h', b'i']; // claims 10 bytes, only 2
        let result = ClientMessage::decode_from(&mut Cursor::new(&frame));
        assert!(matches!(result, Err(DecodeError::UnexpectedEof)));
    }

    #[test]
    fn payload_too_large_returns_error() {
        let huge_len = (MAX_PAYLOAD_SIZE + 1).to_le_bytes();
        let frame = [0x01, huge_len[0], huge_len[1], huge_len[2], huge_len[3]];
        let result = ClientMessage::decode_from(&mut Cursor::new(&frame));
        assert!(matches!(result, Err(DecodeError::PayloadTooLarge(_))));
    }

    #[test]
    fn send_helpers_work() {
        let mut buf = Vec::new();
        send_client_message(&mut buf, &ClientMessage::Data(b"test".to_vec())).unwrap();
        let decoded = ClientMessage::decode_from(&mut Cursor::new(&buf)).unwrap();
        assert_eq!(decoded, ClientMessage::Data(b"test".to_vec()));

        let mut buf = Vec::new();
        send_host_message(&mut buf, &HostMessage::Ready { version: PROTOCOL_VERSION, pid: 99, cols: 80, rows: 24 }).unwrap();
        let decoded = HostMessage::decode_from(&mut Cursor::new(&buf)).unwrap();
        assert_eq!(decoded, HostMessage::Ready { version: PROTOCOL_VERSION, pid: 99, cols: 80, rows: 24 });
    }
}