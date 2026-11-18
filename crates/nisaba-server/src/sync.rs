//! The sync endpoint and the open project rooms.
//!
//! A room is the old sync service's `DocRoom`, unchanged: it imports an
//! update, appends it to the log, and only then fans it out. What this module
//! adds is the acknowledgement. After the room returns, the server sends the
//! sender an `Ack` frame carrying the room's version vector. A client counts an
//! edit as saved only when an acknowledged version vector includes it, and the
//! log row was committed before that frame was written, so an acknowledged edit
//! cannot be lost by a crash.
//!
//! Wire frames are the old protocol's frames plus one new tag:
//! `9 = Ack(version vector)`. The server sends one `Ack` right after `Welcome`
//! so the client learns what the server holds and can push what it lacks.

use std::sync::Arc;

use axum::extract::ws::{CloseFrame, Message, WebSocket};
use dashmap::DashMap;
use nisaba_sync::{
    Config, DenyAllSeedVerifier, DocId, DocRoom, Frame, PeerId, Role, SyncError, SystemClock,
    close_signal,
};
use tokio::sync::{Mutex, mpsc};

use crate::store::Store;

pub const PROTOCOL_VERSION: u8 = 3;
const ACK_TAG: u8 = 9;

/// Close codes. 4000-range codes are the old protocol's; 4400 and 4409 are new.
pub mod close {
    pub const PROTOCOL: u16 = 4000;
    pub const FORBIDDEN: u16 = 4003;
    pub const TOO_LARGE: u16 = 4130;
    pub const VERSION: u16 = 4400;
    pub const INTERNAL: u16 = 4500;
}

/// The open rooms, one per project, opened on first use.
#[derive(Clone)]
pub struct Rooms {
    store: Store,
    config: Arc<Config>,
    open: Arc<DashMap<String, Arc<DocRoom>>>,
    opening: Arc<Mutex<()>>,
}

impl Rooms {
    #[must_use]
    pub fn new(store: Store) -> Self {
        Self {
            store,
            config: Arc::new(Config::default()),
            open: Arc::new(DashMap::new()),
            opening: Arc::new(Mutex::new(())),
        }
    }

    /// The room for `project`, replaying snapshot and log if it is not open.
    pub async fn get(&self, project: &str) -> Result<Arc<DocRoom>, SyncError> {
        if let Some(room) = self.open.get(project) {
            return Ok(Arc::clone(&room));
        }
        let _guard = self.opening.lock().await;
        if let Some(room) = self.open.get(project) {
            return Ok(Arc::clone(&room));
        }
        let store = Arc::new(self.store.clone());
        let room = Arc::new(
            DocRoom::open(
                DocId::new(project)?,
                store.clone(),
                store,
                Arc::clone(&self.config),
                Arc::new(SystemClock),
                Arc::new(DenyAllSeedVerifier),
            )
            .await?,
        );
        self.open.insert(project.to_owned(), Arc::clone(&room));
        Ok(room)
    }

    /// Applies an update made by the server itself (seeding, asset upload,
    /// restore). It goes through the same path as a browser's update.
    pub async fn apply(&self, project: &str, update: &[u8]) -> Result<(), SyncError> {
        let room = self.get(project).await?;
        room.handle_update(server_peer(), Role::Author, update)
            .await
    }
}

fn server_peer() -> PeerId {
    PeerId::new(u64::MAX).expect("non-zero")
}

fn ack(vv: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(5 + vv.len());
    out.push(ACK_TAG);
    out.extend_from_slice(&u32::try_from(vv.len()).unwrap_or(u32::MAX).to_be_bytes());
    out.extend_from_slice(vv);
    out
}

async fn refuse(socket: &mut WebSocket, code: u16, msg: &str) {
    let _ = socket
        .send(Message::Binary(
            Frame::Error {
                code,
                msg: msg.to_owned(),
            }
            .encode()
            .into(),
        ))
        .await;
    let _ = socket
        .send(Message::Close(Some(CloseFrame {
            code,
            reason: msg.to_owned().into(),
        })))
        .await;
}

/// Runs one client connection for `project`. Access was checked before the
/// upgrade; `role` is the member's role in the project.
pub async fn run(mut socket: WebSocket, rooms: Rooms, project: String, role: Role) {
    let max = rooms.config.max_update_bytes;
    let Ok(Some(Ok(Message::Binary(hello)))) =
        tokio::time::timeout(std::time::Duration::from_secs(10), socket.recv()).await
    else {
        return refuse(&mut socket, close::PROTOCOL, "expected hello").await;
    };
    let Ok(Frame::Hello {
        proto,
        doc_id,
        peer,
        last_vv,
        ..
    }) = Frame::decode(&hello, max)
    else {
        return refuse(&mut socket, close::PROTOCOL, "expected hello").await;
    };
    if proto != PROTOCOL_VERSION {
        return refuse(
            &mut socket,
            close::VERSION,
            &format!("server speaks protocol {PROTOCOL_VERSION}; reload the page"),
        )
        .await;
    }
    if doc_id != project {
        return refuse(&mut socket, close::PROTOCOL, "hello names another project").await;
    }
    let Ok(peer) = PeerId::new(peer) else {
        return refuse(&mut socket, close::PROTOCOL, "peer id must be non-zero").await;
    };
    let room = match rooms.get(&project).await {
        Ok(room) => room,
        Err(e) => return refuse(&mut socket, close::INTERNAL, &e.to_string()).await,
    };
    let (tx, mut rx) = mpsc::channel::<Frame>(1024);
    let close_code = close_signal(1000);
    let joined = match room.join(peer, role, &last_vv, Vec::new(), tx, close_code.clone()) {
        Ok(joined) => joined,
        Err(e) => return refuse(&mut socket, close::PROTOCOL, &e.to_string()).await,
    };
    // The welcome was queued on `rx` by join; the first ack follows it.
    let mut acked_after_welcome = false;
    loop {
        tokio::select! {
            biased;
            frame = rx.recv() => {
                let Some(frame) = frame else { break };
                let is_welcome = matches!(frame, Frame::Welcome { .. });
                if socket.send(Message::Binary(frame.encode().into())).await.is_err() { break }
                if is_welcome && !acked_after_welcome {
                    acked_after_welcome = true;
                    let vv = room.authority().version_vector().encode();
                    if socket.send(Message::Binary(ack(&vv).into())).await.is_err() { break }
                }
            }
            msg = socket.recv() => {
                let Some(Ok(msg)) = msg else { break };
                let Message::Binary(bytes) = msg else { continue };
                match Frame::decode(&bytes, max) {
                    Ok(Frame::Update(update)) => {
                        match room.handle_update(peer, role, &update).await {
                            Ok(()) => {
                                let vv = room.authority().version_vector().encode();
                                if socket.send(Message::Binary(ack(&vv).into())).await.is_err() { break }
                            }
                            Err(e) => {
                                let code = match e {
                                    SyncError::Access(_) | SyncError::ReviewPolicy(_) => close::FORBIDDEN,
                                    SyncError::Limit(_) => close::TOO_LARGE,
                                    SyncError::Loro(_) => close::PROTOCOL,
                                    _ => close::INTERNAL,
                                };
                                refuse(&mut socket, code, &e.to_string()).await;
                                break;
                            }
                        }
                    }
                    Ok(Frame::Presence(state)) => { let _ = room.handle_presence(peer, state); }
                    Ok(Frame::Heartbeat) => { let _ = room.handle_heartbeat(peer); }
                    Ok(Frame::Bye) => break,
                    Ok(_) | Err(_) => {
                        refuse(&mut socket, close::PROTOCOL, "unexpected frame").await;
                        break;
                    }
                }
            }
        }
    }
    room.leave(peer, joined.generation);
    let code = close_code.load(std::sync::atomic::Ordering::Acquire);
    if code != 1000 {
        refuse(&mut socket, code, "resync required").await;
    }
}
