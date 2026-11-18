//! Project rooms and the sync wire protocol.
//!
//! A [`Room`] holds the Loro document of one project in memory. It checks each
//! update, writes it to a [`RoomStore`], imports it and sends it to the other
//! clients. It answers with an [`Frame::Ack`] only after the store reports the
//! update durable. The transport (a WebSocket in `nisaba-server`) decodes
//! frames, passes them to the room and writes the room's replies back.

mod error;
mod protocol;
mod room;
mod store;

pub use error::{FrameError, SyncError};
pub use protocol::{
    Assembler, FRAMES, FieldKind, Frame, FrameType, HEARTBEAT_SECONDS, HELLO_SECONDS, IDLE_SECONDS,
    PART, PROTOCOL_VERSION, UPDATE_HEADER, close, split,
};
pub use room::{Access, Limits, Peer, Reservation, Role, Room, always};
pub use store::{LogSize, MemoryStore, RoomStore, Stored};
