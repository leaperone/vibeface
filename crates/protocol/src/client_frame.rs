//! Wire framing shared by the Node and the native LAN client.

use std::fmt;

pub const ALPN: &[u8] = b"vibeface/client/1";
pub const HEADER_BYTES: usize = 5;
pub const MAX_PAYLOAD_BYTES: usize = 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum FrameKind {
    Binary = 0,
    Text = 1,
    Ping = 2,
    Pong = 3,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FrameError {
    PayloadTooLarge,
    UnknownTag(u8),
}

impl fmt::Display for FrameError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::PayloadTooLarge => f.write_str("client frame too large"),
            Self::UnknownTag(tag) => write!(f, "unknown client frame tag {tag}"),
        }
    }
}

impl std::error::Error for FrameError {}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FrameHeader {
    tag: u8,
    payload_len: usize,
}

impl FrameHeader {
    pub fn payload_len(self) -> usize {
        self.payload_len
    }

    /// Call this after reading the payload so a truncated frame reports a read error first.
    pub fn kind(self) -> Result<FrameKind, FrameError> {
        [
            FrameKind::Binary,
            FrameKind::Text,
            FrameKind::Ping,
            FrameKind::Pong,
        ]
        .into_iter()
        .find(|kind| *kind as u8 == self.tag)
        .ok_or(FrameError::UnknownTag(self.tag))
    }
}

pub fn encode_header(
    kind: FrameKind,
    payload_len: usize,
) -> Result<[u8; HEADER_BYTES], FrameError> {
    if payload_len > MAX_PAYLOAD_BYTES {
        return Err(FrameError::PayloadTooLarge);
    }
    let length = u32::try_from(payload_len).map_err(|_| FrameError::PayloadTooLarge)?;
    let [a, b, c, d] = length.to_be_bytes();
    Ok([kind as u8, a, b, c, d])
}

/// Validates the length before callers allocate the payload buffer.
pub fn decode_header([tag, a, b, c, d]: [u8; HEADER_BYTES]) -> Result<FrameHeader, FrameError> {
    let payload_len = usize::try_from(u32::from_be_bytes([a, b, c, d]))
        .map_err(|_| FrameError::PayloadTooLarge)?;
    if payload_len > MAX_PAYLOAD_BYTES {
        return Err(FrameError::PayloadTooLarge);
    }
    Ok(FrameHeader { tag, payload_len })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn headers_match_fixed_wire_vectors() {
        assert_eq!(ALPN, b"vibeface/client/1");
        for (kind, len, wire) in [
            (FrameKind::Binary, 0, [0, 0, 0, 0, 0]),
            (FrameKind::Text, "é".len(), [1, 0, 0, 0, 2]),
            (FrameKind::Ping, 0, [2, 0, 0, 0, 0]),
            (FrameKind::Pong, 0x01_0203, [3, 0, 1, 2, 3]),
            (FrameKind::Binary, 1_048_576, [0, 0, 16, 0, 0]),
        ] {
            assert_eq!(encode_header(kind, len), Ok(wire));
            let header = decode_header(wire).unwrap();
            assert_eq!(header.kind(), Ok(kind));
            assert_eq!(header.payload_len(), len);
        }
    }

    #[test]
    fn oversize_lengths_fail_before_tag_validation() {
        assert_eq!(
            encode_header(FrameKind::Binary, 1_048_577),
            Err(FrameError::PayloadTooLarge)
        );
        assert_eq!(
            decode_header([0, 0, 16, 0, 1]),
            Err(FrameError::PayloadTooLarge)
        );
        assert_eq!(
            decode_header([255, 255, 255, 255, 255]),
            Err(FrameError::PayloadTooLarge)
        );
        let unknown = decode_header([255, 0, 0, 0, 1]).unwrap();
        assert_eq!(unknown.payload_len(), 1);
        assert_eq!(unknown.kind(), Err(FrameError::UnknownTag(255)));
    }
}
