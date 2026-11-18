//! Sync protocol version 2.
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
//! | 7 | Begin | both | total length `u32` of the frame that follows in parts |
//! | 8 | Part | both | part number `u32`, bytes blob |
//!
//! A frame whose encoding is longer than [`PART`] bytes travels in parts:
//! Begin declares its length, then Parts 0, 1, 2, ... carry its bytes in
//! order. [`split`] makes the messages and an [`Assembler`] puts them back
//! together. Both ends use these two, for every direction.

use crate::error::{FrameError, SyncError};

pub const PROTOCOL_VERSION: u8 = 2;

/// The longest message either end sends. A longer frame goes in parts.
pub const PART: usize = 1 << 20;

/// The bytes an Update frame adds to its update: the type and the length.
pub const UPDATE_HEADER: usize = 5;

/// WebSocket close codes the server sends.
pub mod close {
    pub const NORMAL: u16 = 1000;
    /// The server holds as many unfinished updates as it may; send the
    /// update again after a delay.
    pub const TRY_AGAIN: u16 = 1013;
    pub const MALFORMED: u16 = 4000;
    pub const TOO_SLOW: u16 = 4002;
    /// A multi-part update did not arrive by its deadline. Nothing of it was
    /// kept; send it again after reconnecting.
    pub const TIMED_OUT: u16 = 4008;
    pub const FORBIDDEN: u16 = 4003;
    /// The project does not exist or the user is not a member, or no longer
    /// one.
    pub const NOT_FOUND: u16 = 4004;
    pub const FULL: u16 = 4029;
    /// The update does not fit in what the project has left.
    pub const TOO_LARGE: u16 = 4130;
    pub const VERSION: u16 = 4400;
    /// The session is missing, expired or signed out; sign in again.
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
    /// The next `total` bytes of Parts form one frame.
    Begin {
        total: u32,
    },
    Part {
        index: u32,
        bytes: Vec<u8>,
    },
}

const HELLO: u8 = 1;
const WELCOME: u8 = 2;
const UPDATE: u8 = 3;
const ACK: u8 = 4;
const PRESENCE: u8 = 5;
const HEARTBEAT: u8 = 6;
const BEGIN: u8 = 7;
const PART_TYPE: u8 = 8;

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
            Self::Begin { total } => {
                out.push(BEGIN);
                out.extend(total.to_be_bytes());
            }
            Self::Part { index, bytes } => {
                out.push(PART_TYPE);
                out.extend(index.to_be_bytes());
                blob(&mut out, bytes);
            }
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
            BEGIN => Self::Begin { total: r.u32()? },
            PART_TYPE => Self::Part {
                index: r.u32()?,
                bytes: r.blob()?,
            },
            other => return Err(FrameError::UnknownType(other)),
        };
        if !r.buf.is_empty() {
            return Err(FrameError::Trailing(r.buf.len()));
        }
        Ok(frame)
    }
}

/// The messages that carry `frame`: the frame itself when its encoding is at
/// most `part` bytes long, else a Begin and Parts of at most `part` bytes.
#[must_use]
pub fn split(frame: &Frame, part: usize) -> Vec<Vec<u8>> {
    let bytes = frame.encode();
    if bytes.len() <= part {
        return vec![bytes];
    }
    let total = u32::try_from(bytes.len()).expect("frame shorter than 4 GiB");
    let mut out = vec![Frame::Begin { total }.encode()];
    for (index, chunk) in bytes.chunks(part).enumerate() {
        out.push(
            Frame::Part {
                index: u32::try_from(index).expect("fewer than 2^32 parts"),
                bytes: chunk.to_vec(),
            }
            .encode(),
        );
    }
    out
}

/// Puts frames sent in parts back together, for one connection.
///
/// A message is either a whole frame, or a Begin followed by Parts numbered
/// from 0 whose bytes add up to the declared length. The buffer never holds
/// more than that length. Nothing comes out until the last part arrives, so
/// a frame is used whole or not at all. Any other order is an error: a part
/// missing, repeated or out of order, more bytes than declared, a Part
/// without a Begin, or another frame in the middle.
#[derive(Debug)]
pub struct Assembler {
    part: usize,
    open: Option<Open>,
}

#[derive(Debug)]
struct Open {
    total: usize,
    next: u32,
    buf: Vec<u8>,
}

impl Assembler {
    /// Accepts messages of at most `part` bytes of payload.
    #[must_use]
    pub fn new(part: usize) -> Self {
        Self { part, open: None }
    }

    /// Takes one message. Returns the frame it completes, if any. `allow`
    /// sees the declared length of a Begin before any part is kept, and may
    /// refuse it.
    pub fn push(
        &mut self,
        message: &[u8],
        allow: impl FnOnce(usize) -> Result<(), SyncError>,
    ) -> Result<Option<Frame>, SyncError> {
        let frame = Frame::decode(message, self.part)?;
        let Some(open) = &mut self.open else {
            return match frame {
                Frame::Begin { total } => {
                    let total = total as usize;
                    if total == 0 {
                        return Err(FrameError::PartsOrder("a Begin of 0 bytes").into());
                    }
                    allow(total)?;
                    self.open = Some(Open {
                        total,
                        next: 0,
                        buf: Vec::new(),
                    });
                    Ok(None)
                }
                Frame::Part { .. } => Err(FrameError::PartsOrder("a Part without a Begin").into()),
                frame => Ok(Some(frame)),
            };
        };
        let Frame::Part { index, bytes } = frame else {
            self.open = None;
            return Err(FrameError::PartsOrder("another frame among the parts").into());
        };
        if index != open.next || bytes.is_empty() {
            let expected = open.next;
            self.open = None;
            return Err(FrameError::PartNumber {
                expected,
                got: index,
            }
            .into());
        }
        if open.buf.len() + bytes.len() > open.total {
            self.open = None;
            return Err(FrameError::PartsOrder("more bytes than the Begin declared").into());
        }
        open.buf.extend_from_slice(&bytes);
        open.next += 1;
        if open.buf.len() < open.total {
            return Ok(None);
        }
        let open = self.open.take().expect("open");
        match Frame::decode(&open.buf, open.total)? {
            Frame::Begin { .. } | Frame::Part { .. } => {
                Err(FrameError::PartsOrder("parts inside parts").into())
            }
            frame => Ok(Some(frame)),
        }
    }

    /// Bytes held for an unfinished frame.
    #[must_use]
    pub fn held(&self) -> usize {
        self.open.as_ref().map_or(0, |o| o.buf.len())
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

    fn u32(&mut self) -> Result<u32, FrameError> {
        Ok(u32::from_be_bytes(
            self.take(4)?.try_into().expect("4 bytes"),
        ))
    }

    fn blob(&mut self) -> Result<Vec<u8>, FrameError> {
        let len = self.u32()? as usize;
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
            any::<u32>().prop_map(|total| Frame::Begin { total }),
            (any::<u32>(), bytes()).prop_map(|(index, bytes)| Frame::Part { index, bytes }),
        ]
    }

    /// A frame that may be sent whole or in parts.
    fn payload() -> impl Strategy<Value = Frame> {
        let big = || proptest::collection::vec(any::<u8>(), 0..400);
        prop_oneof![
            big().prop_map(Frame::Update),
            (big(), bytes()).prop_map(|(catch_up, version)| Frame::Welcome { catch_up, version }),
            bytes().prop_map(Frame::Ack),
            Just(Frame::Heartbeat),
        ]
    }

    #[expect(
        clippy::unnecessary_wraps,
        reason = "the shape of the Assembler's check"
    )]
    fn allow_all(_: usize) -> Result<(), SyncError> {
        Ok(())
    }

    /// Feeds every message; returns the frames that came out, or the error.
    fn feed(a: &mut Assembler, messages: &[Vec<u8>]) -> Result<Vec<Frame>, String> {
        let mut out = Vec::new();
        for m in messages {
            if let Some(f) = a.push(m, allow_all).map_err(|e| e.to_string())? {
                out.push(f);
            }
        }
        Ok(out)
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
        fn unknown_types_are_refused(tag in 9u8.., rest in bytes()) {
            let mut bytes = vec![tag];
            bytes.extend(rest);
            prop_assert_eq!(Frame::decode(&bytes, MAX), Err(FrameError::UnknownType(tag)));
        }

        #[test]
        fn decode_never_panics(bytes in proptest::collection::vec(any::<u8>(), 0..128)) {
            let _ = Frame::decode(&bytes, MAX);
        }

        #[test]
        fn split_frames_come_back_whole(frames in proptest::collection::vec(payload(), 1..6), part in 16usize..200) {
            let messages: Vec<_> = frames.iter().flat_map(|f| split(f, part)).collect();
            prop_assert!(messages.iter().all(|m| m.len() <= part + 9));
            let mut a = Assembler::new(part);
            prop_assert_eq!(feed(&mut a, &messages), Ok(frames));
            prop_assert_eq!(a.held(), 0);
        }

        #[test]
        fn nothing_comes_out_before_the_last_part(len in 100usize..400, part in 16usize..64) {
            let messages = split(&Frame::Update(vec![7; len]), part);
            let mut a = Assembler::new(part);
            let (last, rest) = messages.split_last().expect("parts");
            prop_assert_eq!(feed(&mut a, rest), Ok(vec![]));
            prop_assert!(a.held() <= len + UPDATE_HEADER);
            prop_assert_eq!(feed(&mut a, std::slice::from_ref(last)), Ok(vec![Frame::Update(vec![7; len])]));
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

    fn parts_of(len: usize) -> Vec<Vec<u8>> {
        let m = split(&Frame::Update(vec![1; len]), 10);
        assert!(m.len() > 3, "{} messages", m.len());
        m
    }

    fn refused(messages: &[Vec<u8>]) -> String {
        let mut a = Assembler::new(10);
        let err = feed(&mut a, messages).expect_err("refused");
        assert_eq!(a.held(), 0, "the buffer is dropped with the error");
        err
    }

    #[test]
    fn a_missing_part_is_refused() {
        let mut m = parts_of(40);
        m.remove(2);
        assert!(refused(&m).contains("part 1, got 2"), "{}", refused(&m));
    }

    #[test]
    fn a_repeated_part_is_refused() {
        let mut m = parts_of(40);
        m.insert(2, m[1].clone());
        assert!(refused(&m).contains("part 1, got 0"));
    }

    #[test]
    fn parts_out_of_order_are_refused() {
        let mut m = parts_of(40);
        m.swap(1, 2);
        assert!(refused(&m).contains("part 0, got 1"));
    }

    #[test]
    fn parts_longer_than_declared_are_refused() {
        let mut m = parts_of(40);
        m[0] = Frame::Begin { total: 35 }.encode();
        assert!(refused(&m).contains("more bytes than the Begin declared"));
    }

    #[test]
    fn a_part_without_a_begin_is_refused() {
        let m = parts_of(40);
        assert!(refused(&m[1..]).contains("without a Begin"));
    }

    #[test]
    fn another_frame_among_the_parts_is_refused() {
        let mut m = parts_of(40);
        m.insert(2, Frame::Heartbeat.encode());
        assert!(refused(&m).contains("another frame"));
    }

    #[test]
    fn parts_that_hold_parts_are_refused() {
        let inner = Frame::Begin { total: 9 }.encode();
        let mut m = vec![
            Frame::Begin {
                total: u32::try_from(inner.len()).unwrap(),
            }
            .encode(),
        ];
        m.push(
            Frame::Part {
                index: 0,
                bytes: inner,
            }
            .encode(),
        );
        assert!(refused(&m).contains("parts inside parts"));
    }

    #[test]
    fn a_begin_can_be_refused_before_any_part() {
        let m = parts_of(40);
        let mut a = Assembler::new(10);
        let err = a
            .push(&m[0], |total| Err(SyncError::TooLarge(total)))
            .unwrap_err();
        assert!(matches!(err, SyncError::TooLarge(45)), "{err:?}");
        assert_eq!(a.held(), 0);
    }

    #[test]
    fn a_whole_message_longer_than_a_part_is_refused() {
        let mut a = Assembler::new(10);
        let err = a
            .push(&Frame::Update(vec![0; 11]).encode(), allow_all)
            .unwrap_err();
        assert!(matches!(err, SyncError::Frame(FrameError::TooLarge(11))));
    }
}
