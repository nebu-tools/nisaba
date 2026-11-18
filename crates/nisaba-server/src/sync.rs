//! The sync endpoint and the open project rooms.
//!
//! The room in `nisaba-sync` does the work. This module opens one room per
//! project on first use and moves frames between a WebSocket and its room.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::extract::ws::{CloseFrame, Message, WebSocket};
use nisaba_sync::{Frame, Limits, Role, Room, SyncError, close};

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

/// Runs one client connection. Access was checked before the upgrade.
pub async fn run(mut socket: WebSocket, rooms: Rooms, project: String, role: Role) {
    let max = Limits::default().max_update;
    let hello = match tokio::time::timeout(HELLO_WAIT, socket.recv()).await {
        Ok(Some(Ok(Message::Binary(bytes)))) => Frame::decode(&bytes, max),
        _ => return close_with(&mut socket, close::MALFORMED, "expected Hello").await,
    };
    let hello = match hello {
        Ok(frame) => frame,
        Err(e) => return refuse(&mut socket, &e.into()).await,
    };
    let room = match rooms.get(&project).await {
        Ok(room) => room,
        Err(e) => return refuse(&mut socket, &e).await,
    };
    let (peer, mut rx) = match room.join(role, hello) {
        Ok(joined) => joined,
        Err(e) => return refuse(&mut socket, &e).await,
    };
    let outcome: Result<(), SyncError> = loop {
        tokio::select! {
            biased;
            frame = rx.recv() => {
                let Some(frame) = frame else { break Err(SyncError::TooSlow) };
                if socket.send(Message::Binary(frame.encode().into())).await.is_err() {
                    break Ok(());
                }
            }
            msg = tokio::time::timeout(IDLE, socket.recv()) => {
                let Ok(Some(Ok(msg))) = msg else { break Ok(()) };
                let bytes = match msg {
                    Message::Binary(bytes) => bytes,
                    Message::Close(_) => break Ok(()),
                    _ => continue,
                };
                let frame = match Frame::decode(&bytes, max) {
                    Ok(frame) => frame,
                    Err(e) => break Err(e.into()),
                };
                if let Err(e) = room.receive(peer, frame).await {
                    break Err(e);
                }
            }
        }
    };
    room.leave(peer);
    if let Err(e) = outcome {
        tracing::info!(project, error = %e, "closing sync session");
        refuse(&mut socket, &e).await;
    }
}
