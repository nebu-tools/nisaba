//! Sync protocol version 1.
//!
//! Each WebSocket binary message is one frame: a `u8` type followed by its
//! fields. A blob is a big-endian `u32` length and that many bytes.
//!
//! | Type | Frame | Direction | Fields |
//! |---|---|---|---|
//! | 1 | Hello | client to server | protocol version `u8`, version vector blob |
//! | 2 | Welcome | server to client | catch-up blob, version vector blob |
//! | 3 | Update | both | Loro update blob |
//! | 4 | Ack | server to client | version vector blob |
//! | 5 | Presence | both | Loro ephemeral-store blob |
//! | 6 | Heartbeat | both | none |

use crate::error::FrameError;

pub const PROTOCOL_VERSION: u8 = 1;

/// WebSocket close codes the server sends.
pub mod close {
    pub const NORMAL: u16 = 1000;
    pub const MALFORMED: u16 = 4000;
    pub const TOO_SLOW: u16 = 4002;
    pub const FORBIDDEN: u16 = 4003;
    /// The project does not exist or the user is not a member.
    pub const NOT_FOUND: u16 = 4004;
    pub const FULL: u16 = 4029;
    pub const TOO_LARGE: u16 = 4130;
    pub const VERSION: u16 = 4400;
    /// The session cookie is missing or expired; sign in again.
    pub const SIGNED_OUT: u16 = 4401;
    pub const SERVER_ERROR: u16 = 4500;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Frame {
    /// Opens a session. `version` is the client's encoded version vector,
    /// empty for a client that holds nothing.
    Hello {
        protocol: u8,
        version: Vec<u8>,
    },
    /// Answers Hello. `catch_up` holds every operation the client lacks and is
    /// empty when it lacks none. `version` is what the server holds; all of it
    /// is durable.
    Welcome {
        catch_up: Vec<u8>,
        version: Vec<u8>,
    },
    Update(Vec<u8>),
    /// The server's version vector after an update became durable.
    Ack(Vec<u8>),
    Presence(Vec<u8>),
    Heartbeat,
}

const HELLO: u8 = 1;
const WELCOME: u8 = 2;
const UPDATE: u8 = 3;
const ACK: u8 = 4;
const PRESENCE: u8 = 5;
const HEARTBEAT: u8 = 6;

impl Frame {
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        match self {
            Self::Hello { protocol, version } => {
                out.extend([HELLO, *protocol]);
                blob(&mut out, version);
            }
            Self::Welcome { catch_up, version } => {
                out.push(WELCOME);
                blob(&mut out, catch_up);
                blob(&mut out, version);
            }
            Self::Update(b) => {
                out.push(UPDATE);
                blob(&mut out, b);
            }
            Self::Ack(b) => {
                out.push(ACK);
                blob(&mut out, b);
            }
            Self::Presence(b) => {
                out.push(PRESENCE);
                blob(&mut out, b);
            }
            Self::Heartbeat => out.push(HEARTBEAT),
        }
        out
    }

    /// Decodes one frame. A blob longer than `max_blob` is refused before any
    /// allocation.
    pub fn decode(buf: &[u8], max_blob: usize) -> Result<Self, FrameError> {
        let mut r = Reader { buf, max_blob };
        let frame = match r.u8()? {
            HELLO => Self::Hello {
                protocol: r.u8()?,
                version: r.blob()?,
            },
            WELCOME => Self::Welcome {
                catch_up: r.blob()?,
                version: r.blob()?,
            },
            UPDATE => Self::Update(r.blob()?),
            ACK => Self::Ack(r.blob()?),
            PRESENCE => Self::Presence(r.blob()?),
            HEARTBEAT => Self::Heartbeat,
            other => return Err(FrameError::UnknownType(other)),
        };
        if !r.buf.is_empty() {
            return Err(FrameError::Trailing(r.buf.len()));
        }
        Ok(frame)
    }
}

fn blob(out: &mut Vec<u8>, b: &[u8]) {
    let len = u32::try_from(b.len()).expect("blob shorter than 4 GiB");
    out.extend(len.to_be_bytes());
    out.extend(b);
}

struct Reader<'a> {
    buf: &'a [u8],
    max_blob: usize,
}

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], FrameError> {
        if self.buf.len() < n {
            return Err(FrameError::Truncated);
        }
        let (head, rest) = self.buf.split_at(n);
        self.buf = rest;
        Ok(head)
    }

    fn u8(&mut self) -> Result<u8, FrameError> {
        Ok(self.take(1)?[0])
    }

    fn blob(&mut self) -> Result<Vec<u8>, FrameError> {
        let len = u32::from_be_bytes(self.take(4)?.try_into().expect("4 bytes")) as usize;
        if len > self.max_blob {
            return Err(FrameError::TooLarge(len));
        }
        Ok(self.take(len)?.to_vec())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    const MAX: usize = 1 << 16;

    fn bytes() -> impl Strategy<Value = Vec<u8>> {
        proptest::collection::vec(any::<u8>(), 0..64)
    }

    fn frame() -> impl Strategy<Value = Frame> {
        prop_oneof![
            (any::<u8>(), bytes())
                .prop_map(|(protocol, version)| Frame::Hello { protocol, version }),
            (bytes(), bytes()).prop_map(|(catch_up, version)| Frame::Welcome { catch_up, version }),
            bytes().prop_map(Frame::Update),
            bytes().prop_map(Frame::Ack),
            bytes().prop_map(Frame::Presence),
            Just(Frame::Heartbeat),
        ]
    }

    proptest! {
        #[test]
        fn every_frame_round_trips(f in frame()) {
            prop_assert_eq!(Frame::decode(&f.encode(), MAX), Ok(f));
        }

        #[test]
        fn every_cut_frame_is_truncated(f in frame(), cut in any::<prop::sample::Index>()) {
            let bytes = f.encode();
            let at = cut.index(bytes.len());
            prop_assert_eq!(Frame::decode(&bytes[..at], MAX), Err(FrameError::Truncated));
        }

        #[test]
        fn extra_bytes_are_refused(f in frame(), extra in proptest::collection::vec(any::<u8>(), 1..8)) {
            let mut bytes = f.encode();
            bytes.extend(&extra);
            prop_assert_eq!(Frame::decode(&bytes, MAX), Err(FrameError::Trailing(extra.len())));
        }

        #[test]
        fn unknown_types_are_refused(tag in 7u8.., rest in bytes()) {
            let mut bytes = vec![tag];
            bytes.extend(rest);
            prop_assert_eq!(Frame::decode(&bytes, MAX), Err(FrameError::UnknownType(tag)));
        }

        #[test]
        fn decode_never_panics(bytes in proptest::collection::vec(any::<u8>(), 0..128)) {
            let _ = Frame::decode(&bytes, MAX);
        }
    }

    #[test]
    fn type_zero_is_unknown() {
        assert_eq!(Frame::decode(&[0], MAX), Err(FrameError::UnknownType(0)));
    }

    #[test]
    fn a_long_blob_is_refused_before_reading_it() {
        let bytes = Frame::Update(vec![0; 100]).encode();
        assert_eq!(Frame::decode(&bytes, 99), Err(FrameError::TooLarge(100)));
    }
}
