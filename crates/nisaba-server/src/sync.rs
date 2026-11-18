//! The sync endpoint and the open project rooms.
//!
//! The room in `nisaba-sync` does the work. This module opens one room per
//! project on first use and moves frames between a WebSocket and its room.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::extract::ws::{CloseFrame, Message, WebSocket};
use nisaba_sync::{Assembler, Frame, Limits, PART, Room, SyncError, UPDATE_HEADER, close, split};

use crate::access::Grant;
use crate::store::Store;

/// A client that sends nothing for this long is gone. Clients send a
/// Heartbeat every 20 seconds.
const IDLE: Duration = Duration::from_secs(60);
const HELLO_WAIT: Duration = Duration::from_secs(10);

pub type ProjectRoom = Room<Store>;

/// The open rooms, one per project.
#[derive(Clone)]
pub struct Rooms {
    store: Arc<Store>,
    open: Arc<Mutex<HashMap<String, Arc<ProjectRoom>>>>,
    opening: Arc<tokio::sync::Mutex<()>>,
}

impl Rooms {
    #[must_use]
    pub fn new(store: Store) -> Self {
        Self {
            store: Arc::new(store),
            open: Arc::default(),
            opening: Arc::default(),
        }
    }

    /// The room for `project`, loaded from the store if it is not open.
    pub async fn get(&self, project: &str) -> Result<Arc<ProjectRoom>, SyncError> {
        if let Some(room) = self.cached(project) {
            return Ok(room);
        }
        let _guard = self.opening.lock().await;
        if let Some(room) = self.cached(project) {
            return Ok(room);
        }
        let room = Arc::new(Room::open(project, Arc::clone(&self.store), Limits::default()).await?);
        self.open
            .lock()
            .expect("rooms lock")
            .insert(project.to_owned(), Arc::clone(&room));
        Ok(room)
    }

    /// Closes every room that no socket and no request holds, so memory
    /// follows the projects in use rather than every project ever opened.
    /// The room's state is already on disk.
    #[must_use]
    pub fn sweep(&self) -> usize {
        let mut open = self.open.lock().expect("rooms lock");
        let before = open.len();
        open.retain(|_, room| Arc::strong_count(room) > 1);
        before - open.len()
    }

    /// Sweeps every `every`.
    pub fn spawn_sweeper(&self, every: Duration) {
        let rooms = self.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(every);
            loop {
                tick.tick().await;
                let _closed = rooms.sweep();
            }
        });
    }

    fn cached(&self, project: &str) -> Option<Arc<ProjectRoom>> {
        self.open.lock().expect("rooms lock").get(project).cloned()
    }
}

async fn close_with(socket: &mut WebSocket, code: u16, reason: &str) {
    let _ = socket
        .send(Message::Close(Some(CloseFrame {
            code,
            reason: reason.to_owned().into(),
        })))
        .await;
}

async fn refuse(socket: &mut WebSocket, e: &SyncError) {
    close_with(socket, e.close_code(), &e.to_string()).await;
}

/// Why a grant ended, for the close frame.
#[must_use]
pub fn ended_reason(code: u16) -> &'static str {
    if code == close::NOT_FOUND {
        "you no longer have access to this project"
    } else {
        "signed out; sign in again"
    }
}

/// Sends `frame`, in parts when it is long.
async fn send(socket: &mut WebSocket, frame: &Frame) -> bool {
    for message in split(frame, PART) {
        if socket.send(Message::Binary(message.into())).await.is_err() {
            return false;
        }
    }
    true
}

enum End {
    /// The client closed or went quiet.
    Gone,
    Refused(SyncError),
    /// The grant ended with this close code.
    Ended(u16),
}

/// Runs one client connection. Access was checked before the upgrade, and
/// `grant` keeps the socket open only while that access lasts: a sign-out,
/// an expired session, a deleted account or a removed membership closes it
/// at once, and no frame received after that is applied.
pub async fn run(mut socket: WebSocket, rooms: Rooms, project: String, grant: Grant) {
    let mut ending = grant.ending();
    let mut parts = Assembler::new(PART);
    let hello = match tokio::time::timeout(HELLO_WAIT, socket.recv()).await {
        Ok(Some(Ok(Message::Binary(bytes)))) => parts.push(&bytes, |_| Err(SyncError::Unexpected)),
        _ => return close_with(&mut socket, close::MALFORMED, "expected Hello").await,
    };
    let hello = match hello {
        Ok(Some(frame)) => frame,
        Ok(None) => return refuse(&mut socket, &SyncError::Unexpected).await,
        Err(e) => return refuse(&mut socket, &e).await,
    };
    if let Some(code) = grant.ended() {
        return close_with(&mut socket, code, ended_reason(code)).await;
    }
    let room = match rooms.get(&project).await {
        Ok(room) => room,
        Err(e) => return refuse(&mut socket, &e).await,
    };
    let (peer, mut rx) = match room.join(grant.role, hello) {
        Ok(joined) => joined,
        Err(e) => return refuse(&mut socket, &e).await,
    };
    let end = loop {
        tokio::select! {
            biased;
            code = ending.wait() => break End::Ended(code),
            frame = rx.recv() => {
                let Some(frame) = frame else { break End::Refused(SyncError::TooSlow) };
                if !send(&mut socket, &frame).await {
                    break End::Gone;
                }
            }
            msg = tokio::time::timeout(IDLE, socket.recv()) => {
                let Ok(Some(Ok(msg))) = msg else { break End::Gone };
                let bytes = match msg {
                    Message::Binary(bytes) => bytes,
                    Message::Close(_) => break End::Gone,
                    _ => continue,
                };
                if let Some(code) = grant.ended() {
                    break End::Ended(code);
                }
                let frame = match parts.push(&bytes, |total| {
                    room.fits(total.saturating_sub(UPDATE_HEADER))
                }) {
                    Ok(Some(frame)) => frame,
                    Ok(None) => continue,
                    Err(e) => break End::Refused(e),
                };
                if let Err(e) = room.receive(peer, frame).await {
                    break End::Refused(e);
                }
            }
        }
    };
    room.leave(peer);
    match end {
        End::Gone => {}
        End::Refused(e) => {
            tracing::info!(project, error = %e, "closing sync session");
            refuse(&mut socket, &e).await;
        }
        End::Ended(code) => close_with(&mut socket, code, ended_reason(code)).await,
    }
}
