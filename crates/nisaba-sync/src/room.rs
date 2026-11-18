//! One open project.
//!
//! Two locks keep the room consistent:
//!
//! * `write` (async) lets one update at a time go through check, durable
//!   append, import and compaction.
//! * `state` (sync, never held across an await) guards the document and the
//!   sessions. Import and fan-out happen under it in one step, and so does a
//!   join, so a joining client gets each update once: in its catch-up or as a
//!   fanned-out frame, never both and never neither.
//!
//! The update is appended before it is imported, so the document never holds
//! an operation that is not durable. Every version vector the room sends is
//! therefore safe to count as saved.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use loro::{ExportMode, LoroDoc, VersionVector};
use tokio::sync::mpsc;

use crate::error::SyncError;
use crate::protocol::{Frame, PROTOCOL_VERSION};
use crate::store::RoomStore;

type SyncResult<T> = Result<T, SyncError>;

/// A member's role, as far as the room cares.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Editor,
    /// May only add comments and suggestions. Until the server checks that an
    /// update holds nothing else, the room refuses a commenter's updates.
    Commenter,
    Viewer,
}

/// A session in a room. Its numbers are never reused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Peer(u64);

#[derive(Debug, Clone)]
pub struct Limits {
    pub max_update: usize,
    pub max_catch_up: usize,
    pub max_peers: usize,
    /// Frames queued for one client before the room drops it as too slow.
    pub queue: usize,
    /// Compact after this many log entries...
    pub compact_entries: usize,
    /// ...or this many log bytes, whichever comes first.
    pub compact_bytes: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_update: 4 << 20,
            max_catch_up: 64 << 20,
            max_peers: 64,
            queue: 1024,
            compact_entries: 500,
            compact_bytes: 8 << 20,
        }
    }
}

struct Session {
    role: Role,
    tx: mpsc::Sender<Frame>,
}

struct State {
    doc: LoroDoc,
    sessions: HashMap<Peer, Session>,
}

/// The log size since the last snapshot.
struct Log {
    entries: usize,
    bytes: usize,
}

pub struct Room<S> {
    project: String,
    store: Arc<S>,
    limits: Limits,
    write: tokio::sync::Mutex<Log>,
    state: Mutex<State>,
    next_peer: AtomicU64,
}

impl<S: RoomStore> Room<S> {
    /// Loads the project's snapshot and log from the store.
    pub async fn open(project: &str, store: Arc<S>, limits: Limits) -> SyncResult<Self> {
        let stored = blocking(&store, project, S::load).await?;
        let doc = LoroDoc::new();
        if let Some(snapshot) = &stored.snapshot {
            doc.import(snapshot)?;
        }
        for update in &stored.log {
            if let Err(e) = doc.import(update) {
                tracing::warn!(project, error = %e, "skipping a log entry that does not import");
            }
        }
        Ok(Self {
            project: project.to_owned(),
            store,
            limits,
            write: tokio::sync::Mutex::new(Log {
                entries: stored.log.len(),
                bytes: stored.log.iter().map(Vec::len).sum(),
            }),
            state: Mutex::new(State {
                doc,
                sessions: HashMap::new(),
            }),
            next_peer: AtomicU64::new(1),
        })
    }

    /// Starts a session from the client's Hello. The returned queue starts
    /// with Welcome and then carries every frame for this client. When the
    /// room drops a client as too slow, the queue ends.
    pub fn join(&self, role: Role, hello: Frame) -> SyncResult<(Peer, mpsc::Receiver<Frame>)> {
        let Frame::Hello { protocol, version } = hello else {
            return Err(SyncError::Unexpected);
        };
        if protocol != PROTOCOL_VERSION {
            return Err(SyncError::Version {
                expected: PROTOCOL_VERSION,
            });
        }
        let client = if version.is_empty() {
            None
        } else {
            Some(VersionVector::decode(&version)?)
        };
        let mut state = self.lock();
        if state.sessions.len() >= self.limits.max_peers {
            return Err(SyncError::Full);
        }
        let server = state.doc.oplog_vv();
        let catch_up = match client {
            Some(vv) if vv.includes_vv(&server) => Vec::new(),
            Some(vv) => state.doc.export(ExportMode::updates(&vv))?,
            None if server.is_empty() => Vec::new(),
            None => state.doc.export(ExportMode::Snapshot)?,
        };
        if catch_up.len() > self.limits.max_catch_up {
            return Err(SyncError::TooLarge(catch_up.len()));
        }
        let (tx, rx) = mpsc::channel(self.limits.queue);
        tx.try_send(Frame::Welcome {
            catch_up,
            version: server.encode(),
        })
        .expect("a new queue has room");
        let peer = Peer(self.next_peer.fetch_add(1, Ordering::Relaxed));
        state.sessions.insert(peer, Session { role, tx });
        Ok((peer, rx))
    }

    pub fn leave(&self, peer: Peer) {
        self.lock().sessions.remove(&peer);
    }

    /// Handles one frame from a joined client. Replies go to its queue.
    pub async fn receive(&self, peer: Peer, frame: Frame) -> SyncResult<()> {
        let role = self
            .lock()
            .sessions
            .get(&peer)
            .map(|s| s.role)
            .ok_or(SyncError::TooSlow)?;
        match frame {
            Frame::Update(update) => {
                if role != Role::Editor {
                    return Err(SyncError::Forbidden);
                }
                let version = self.write_update(&update, Some(peer)).await?;
                self.send(peer, Frame::Ack(version));
                Ok(())
            }
            Frame::Presence(state) => {
                if state.len() > self.limits.max_update {
                    return Err(SyncError::TooLarge(state.len()));
                }
                let frame = Frame::Presence(state);
                let mut st = self.lock();
                fan_out(&mut st, &frame, Some(peer));
                Ok(())
            }
            Frame::Heartbeat => {
                self.send(peer, Frame::Heartbeat);
                Ok(())
            }
            Frame::Hello { .. } | Frame::Welcome { .. } | Frame::Ack(_) => {
                Err(SyncError::Unexpected)
            }
        }
    }

    /// Applies an update the server made itself, such as an upload or a
    /// restore. It takes the same path as a client's update.
    pub async fn apply(&self, update: &[u8]) -> SyncResult<()> {
        self.write_update(update, None).await.map(drop)
    }

    /// Reads the document.
    pub fn read<T>(&self, f: impl FnOnce(&LoroDoc) -> T) -> T {
        f(&self.lock().doc)
    }

    pub fn snapshot(&self) -> SyncResult<Vec<u8>> {
        Ok(self.lock().doc.export(ExportMode::Snapshot)?)
    }

    #[must_use]
    pub fn peers(&self) -> usize {
        self.lock().sessions.len()
    }

    /// Checks, appends, imports and fans out one update, then compacts if the
    /// log is long. Returns the encoded version vector after the import.
    async fn write_update(&self, update: &[u8], from: Option<Peer>) -> SyncResult<Vec<u8>> {
        if update.len() > self.limits.max_update {
            return Err(SyncError::TooLarge(update.len()));
        }
        let meta = LoroDoc::decode_import_blob_meta(update, true)?;
        // A throwaway import proves the bytes decode before they reach the log.
        LoroDoc::new().import(update)?;

        let mut log = self.write.lock().await;
        if self.lock().doc.oplog_vv().includes_vv(&meta.partial_end_vv) {
            return Ok(self.lock().doc.oplog_vv().encode());
        }
        let bytes = update.to_vec();
        blocking(&self.store, &self.project, move |s, p| s.append(p, &bytes)).await?;
        log.entries += 1;
        log.bytes += update.len();

        let version = {
            let mut st = self.lock();
            st.doc.import(update)?;
            fan_out(&mut st, &Frame::Update(update.to_vec()), from);
            st.doc.oplog_vv().encode()
        };

        if log.entries >= self.limits.compact_entries || log.bytes >= self.limits.compact_bytes {
            let (covers, snapshot) = {
                let st = self.lock();
                (st.doc.oplog_vv(), st.doc.export(ExportMode::Snapshot)?)
            };
            match blocking(&self.store, &self.project, move |s, p| {
                s.compact(p, &covers, &snapshot)
            })
            .await
            {
                Ok(()) => {
                    *log = Log {
                        entries: 0,
                        bytes: 0,
                    }
                }
                // The log still holds everything, so a failed compaction loses
                // nothing. The next update tries again.
                Err(e) => tracing::warn!(project = %self.project, error = %e, "compaction failed"),
            }
        }
        Ok(version)
    }

    fn send(&self, peer: Peer, frame: Frame) {
        let mut st = self.lock();
        if let Some(s) = st.sessions.get(&peer)
            && s.tx.try_send(frame).is_err()
        {
            st.sessions.remove(&peer);
        }
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// Queues `frame` for every session but `except`. A session whose queue is
/// full is dropped; its client sees the queue end and reconnects.
fn fan_out(state: &mut State, frame: &Frame, except: Option<Peer>) {
    state
        .sessions
        .retain(|peer, s| Some(*peer) == except || s.tx.try_send(frame.clone()).is_ok());
}

async fn blocking<S: RoomStore, T: Send + 'static>(
    store: &Arc<S>,
    project: &str,
    f: impl FnOnce(&S, &str) -> Result<T, String> + Send + 'static,
) -> SyncResult<T> {
    let store = Arc::clone(store);
    let project = project.to_owned();
    tokio::task::spawn_blocking(move || f(&store, &project))
        .await
        .map_err(|e| SyncError::Storage(e.to_string()))?
        .map_err(SyncError::Storage)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{MemoryStore, Stored};
    use loro::LoroDoc;

    fn edit(doc: &LoroDoc, text: &str) -> Vec<u8> {
        let before = doc.oplog_vv();
        let t = doc.get_text("main");
        t.insert(t.len_unicode(), text).unwrap();
        doc.commit();
        doc.export(ExportMode::updates(&before)).unwrap()
    }

    fn hello(vv: &[u8]) -> Frame {
        Frame::Hello {
            protocol: PROTOCOL_VERSION,
            version: vv.to_vec(),
        }
    }

    async fn room(store: &Arc<MemoryStore>, limits: Limits) -> Room<MemoryStore> {
        Room::open("p", Arc::clone(store), limits).await.unwrap()
    }

    fn welcome(rx: &mut mpsc::Receiver<Frame>) -> (Vec<u8>, Vec<u8>) {
        match rx.try_recv().unwrap() {
            Frame::Welcome { catch_up, version } => (catch_up, version),
            other => panic!("expected Welcome, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn an_update_is_stored_before_its_ack() {
        let store = Arc::new(MemoryStore::default());
        let room = room(&store, Limits::default()).await;
        let (a, mut rx_a) = room.join(Role::Editor, hello(&[])).unwrap();
        let (_b, mut rx_b) = room.join(Role::Editor, hello(&[])).unwrap();
        welcome(&mut rx_a);
        welcome(&mut rx_b);

        let doc = LoroDoc::new();
        let update = edit(&doc, "hello");
        room.receive(a, Frame::Update(update.clone()))
            .await
            .unwrap();

        assert_eq!(store.load("p").unwrap().log, vec![update.clone()]);
        let Frame::Ack(vv) = rx_a.try_recv().unwrap() else {
            panic!("expected Ack")
        };
        assert!(
            VersionVector::decode(&vv)
                .unwrap()
                .includes_vv(&doc.oplog_vv())
        );
        assert_eq!(rx_b.try_recv().unwrap(), Frame::Update(update));
    }

    #[tokio::test]
    async fn a_duplicate_is_acked_and_not_stored_again() {
        let store = Arc::new(MemoryStore::default());
        let room = room(&store, Limits::default()).await;
        let (a, mut rx) = room.join(Role::Editor, hello(&[])).unwrap();
        welcome(&mut rx);
        let update = edit(&LoroDoc::new(), "x");
        room.receive(a, Frame::Update(update.clone()))
            .await
            .unwrap();
        room.receive(a, Frame::Update(update)).await.unwrap();
        assert_eq!(store.load("p").unwrap().log.len(), 1);
        assert!(matches!(rx.try_recv().unwrap(), Frame::Ack(_)));
        assert!(matches!(rx.try_recv().unwrap(), Frame::Ack(_)));
    }

    #[tokio::test]
    async fn a_new_client_catches_up_and_reopening_restores_the_text() {
        let store = Arc::new(MemoryStore::default());
        let doc = LoroDoc::new();
        {
            let room = room(&store, Limits::default()).await;
            room.apply(&edit(&doc, "one ")).await.unwrap();
            room.apply(&edit(&doc, "two")).await.unwrap();
        }
        let room = room(&store, Limits::default()).await;
        let (_p, mut rx) = room.join(Role::Viewer, hello(&[])).unwrap();
        let (catch_up, _) = welcome(&mut rx);
        let client = LoroDoc::new();
        client.import(&catch_up).unwrap();
        assert_eq!(client.get_text("main").to_string(), "one two");
    }

    #[tokio::test]
    async fn a_client_that_holds_everything_gets_an_empty_catch_up() {
        let store = Arc::new(MemoryStore::default());
        let room = room(&store, Limits::default()).await;
        let doc = LoroDoc::new();
        room.apply(&edit(&doc, "x")).await.unwrap();
        let (_p, mut rx) = room
            .join(Role::Editor, hello(&doc.oplog_vv().encode()))
            .unwrap();
        assert!(welcome(&mut rx).0.is_empty());
    }

    #[tokio::test]
    async fn compaction_keeps_the_text_and_empties_the_log() {
        let store = Arc::new(MemoryStore::default());
        let limits = Limits {
            compact_entries: 3,
            ..Limits::default()
        };
        let doc = LoroDoc::new();
        {
            let room = room(&store, limits.clone()).await;
            for word in ["a", "b", "c", "d"] {
                room.apply(&edit(&doc, word)).await.unwrap();
            }
        }
        let Stored { snapshot, log } = store.load("p").unwrap();
        assert!(snapshot.is_some());
        assert_eq!(log.len(), 1);
        let room = room(&store, limits).await;
        assert_eq!(room.read(|d| d.get_text("main").to_string()), "abcd");
    }

    #[tokio::test]
    async fn viewers_and_commenters_may_not_write() {
        let store = Arc::new(MemoryStore::default());
        let room = room(&store, Limits::default()).await;
        for role in [Role::Viewer, Role::Commenter] {
            let (p, _rx) = room.join(role, hello(&[])).unwrap();
            let err = room
                .receive(p, Frame::Update(edit(&LoroDoc::new(), "x")))
                .await
                .unwrap_err();
            assert!(matches!(err, SyncError::Forbidden));
        }
        assert!(store.load("p").unwrap().log.is_empty());
    }

    #[tokio::test]
    async fn garbage_never_reaches_the_log() {
        let store = Arc::new(MemoryStore::default());
        let room = room(&store, Limits::default()).await;
        let (p, _rx) = room.join(Role::Editor, hello(&[])).unwrap();
        assert!(room.receive(p, Frame::Update(vec![1, 2, 3])).await.is_err());
        assert!(store.load("p").unwrap().log.is_empty());
    }

    #[tokio::test]
    async fn a_wrong_protocol_version_is_refused() {
        let room = room(&Arc::new(MemoryStore::default()), Limits::default()).await;
        let err = room
            .join(
                Role::Editor,
                Frame::Hello {
                    protocol: PROTOCOL_VERSION + 1,
                    version: Vec::new(),
                },
            )
            .unwrap_err();
        assert!(matches!(err, SyncError::Version { .. }));
    }

    #[tokio::test]
    async fn a_slow_client_is_dropped() {
        let store = Arc::new(MemoryStore::default());
        let limits = Limits {
            queue: 2,
            ..Limits::default()
        };
        let room = room(&store, limits).await;
        let (_slow, mut rx) = room.join(Role::Editor, hello(&[])).unwrap();
        let doc = LoroDoc::new();
        for word in ["a", "b", "c"] {
            room.apply(&edit(&doc, word)).await.unwrap();
        }
        assert_eq!(room.peers(), 0);
        while rx.try_recv().is_ok() {}
        assert!(rx.recv().await.is_none());
    }

    #[tokio::test]
    async fn presence_goes_to_the_others_only() {
        let room = room(&Arc::new(MemoryStore::default()), Limits::default()).await;
        let (a, mut rx_a) = room.join(Role::Viewer, hello(&[])).unwrap();
        let (_b, mut rx_b) = room.join(Role::Viewer, hello(&[])).unwrap();
        welcome(&mut rx_a);
        welcome(&mut rx_b);
        room.receive(a, Frame::Presence(vec![7])).await.unwrap();
        assert_eq!(rx_b.try_recv().unwrap(), Frame::Presence(vec![7]));
        assert!(rx_a.try_recv().is_err());
    }
}
