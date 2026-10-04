//! Bounded one-shot retained history reads. Never attaches, resizes, or sends input.
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::{Duration, Instant};

use pty_core::protocol::{HistoryRequest, HistoryResponse, MessageType, PacketReader, encode_packet};

use crate::ClientError;

pub fn read_in(
    root: &Path,
    name: &str,
    request: &HistoryRequest,
    timeout: Duration,
) -> Result<HistoryResponse, ClientError> {
    let deadline = Instant::now() + timeout;
    let mut socket = UnixStream::connect(root.join(format!("{name}.sock")))
        .map_err(|error| ClientError::Connection(error.to_string()))?;
    socket.set_write_timeout(Some(timeout)).map_err(|error| ClientError::Connection(error.to_string()))?;
    let payload = serde_json::to_vec(request).map_err(|error| ClientError::Connection(error.to_string()))?;
    socket.write_all(&encode_packet(MessageType::History, &payload))
        .map_err(|error| ClientError::Connection(error.to_string()))?;
    let mut reader = PacketReader::new();
    let mut buffer = [0_u8; 16_384];
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(ClientError::Connection("history read deadline expired".into()));
        }
        socket.set_read_timeout(Some(remaining)).map_err(|error| ClientError::Connection(error.to_string()))?;
        let count = socket.read(&mut buffer).map_err(|error| ClientError::Connection(error.to_string()))?;
        if count == 0 {
            return Err(ClientError::Connection("terminal closed before history response".into()));
        }
        for packet in reader.feed(&buffer[..count]).map_err(|error| ClientError::Connection(error.to_string()))? {
            if packet.type_ == MessageType::History {
                return serde_json::from_slice(&packet.payload).map_err(|error| ClientError::Connection(error.to_string()));
            }
        }
    }
}
