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
//!
//! The store checks each session's [`Admit`] in the same step as the append,
//! so an update from a session whose access ended is neither stored nor sent
//! on.
//!
//! The quota counts what is stored and what unfinished multi-part updates
//! have reserved ([`Room::reserve`]). An update fits only in what neither
//! holds.

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

/// Whether a session may still write. The store asks at the moment of the
/// append.
pub type Admit = Arc<dyn Fn() -> bool + Send + Sync>;

/// A session in a room. Its numbers are never reused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Peer(u64);

#[derive(Debug, Clone)]
pub struct Limits {
    /// The largest Presence frame.
    pub max_presence: usize,
    pub max_catch_up: usize,
    pub max_peers: usize,
    /// Frames queued for one client before the room drops it as too slow.
    pub queue: usize,
    /// Compact after this many log entries...
    pub compact_entries: usize,
    /// ...or this many log bytes, whichever comes first.
    pub compact_bytes: usize,
    /// The stored size of a project, its snapshot plus its log, past which
    /// the room refuses updates. An update may be as large as what is left.
    /// Kept below `max_catch_up` so a full project still fits in one
    /// catch-up. Assets are stored apart and do not count.
    pub max_document: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_presence: 64 << 10,
            max_catch_up: 64 << 20,
            max_peers: 64,
            queue: 1024,
            compact_entries: 500,
            compact_bytes: 8 << 20,
            max_document: 32 << 20,
        }
    }
}

struct Session {
    role: Role,
    tx: mpsc::Sender<Arc<Frame>>,
    admit: Admit,
}

struct State {
    doc: LoroDoc,
    sessions: HashMap<Peer, Session>,
}

/// The log size since the last snapshot, and the snapshot's size.
struct Log {
    entries: usize,
    bytes: usize,
    snapshot: usize,
}

/// What counts against `max_document`.
#[derive(Default)]
struct Quota {
    /// `snapshot + bytes` of the log, readable without waiting for `write`.
    stored: usize,
    /// Bytes held for updates that have not arrived or been written yet.
    reserved: usize,
}

/// Space held in a room's quota for one update. Dropping it gives the space
/// back; writing the update turns it into stored bytes.
pub struct Reservation {
    quota: Arc<Mutex<Quota>>,
    len: usize,
}

impl Drop for Reservation {
    fn drop(&mut self) {
        lock(&self.quota).reserved -= self.len;
    }
}

pub struct Room<S> {
    project: String,
    store: Arc<S>,
    limits: Limits,
    write: tokio::sync::Mutex<Log>,
    quota: Arc<Mutex<Quota>>,
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
        let log = Log {
            entries: stored.log.len(),
            bytes: stored.log.iter().map(Vec::len).sum(),
            snapshot: stored.snapshot.as_ref().map_or(0, Vec::len),
        };
        Ok(Self {
            project: project.to_owned(),
            store,
            limits,
            quota: Arc::new(Mutex::new(Quota {
                stored: log.snapshot + log.bytes,
                reserved: 0,
            })),
            write: tokio::sync::Mutex::new(log),
            state: Mutex::new(State {
                doc,
                sessions: HashMap::new(),
            }),
            next_peer: AtomicU64::new(1),
        })
    }

    /// Starts a session from the client's Hello. The returned queue starts
    /// with Welcome and then carries every frame for this client. When the
    /// room drops a client as too slow, the queue ends. The session's
    /// updates are written only while `admit` allows it.
    pub fn join(
        &self,
        role: Role,
        hello: Frame,
        admit: Admit,
    ) -> SyncResult<(Peer, mpsc::Receiver<Arc<Frame>>)> {
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
        tx.try_send(Arc::new(Frame::Welcome {
            catch_up,
            version: server.encode(),
        }))
        .expect("a new queue has room");
        let peer = Peer(self.next_peer.fetch_add(1, Ordering::Relaxed));
        state.sessions.insert(peer, Session { role, tx, admit });
        Ok((peer, rx))
    }

    pub fn leave(&self, peer: Peer) {
        self.lock().sessions.remove(&peer);
    }

    /// Handles one frame from a joined client. Replies go to its queue.
    /// `held` is the space reserved for it when it came in parts.
    pub async fn receive(
        &self,
        peer: Peer,
        frame: Frame,
        held: Option<Reservation>,
    ) -> SyncResult<()> {
        let (role, admit) = self
            .lock()
            .sessions
            .get(&peer)
            .map(|s| (s.role, Arc::clone(&s.admit)))
            .ok_or(SyncError::TooSlow)?;
        match frame {
            Frame::Update(update) => {
                if role != Role::Editor {
                    return Err(SyncError::Forbidden);
                }
                let version = self.write_update(&update, Some(peer), held, &admit).await?;
                self.send(peer, Frame::Ack(version));
                Ok(())
            }
            Frame::Presence(state) => {
                if state.len() > self.limits.max_presence {
                    return Err(SyncError::TooLarge(state.len()));
                }
                let frame = Arc::new(Frame::Presence(state));
                let mut st = self.lock();
                fan_out(&mut st, &frame, Some(peer));
                Ok(())
            }
            Frame::Heartbeat => {
                self.send(peer, Frame::Heartbeat);
                Ok(())
            }
            Frame::Hello { .. }
            | Frame::Welcome { .. }
            | Frame::Ack(_)
            | Frame::Begin { .. }
            | Frame::Part { .. } => Err(SyncError::Unexpected),
        }
    }

    /// Applies an update the server made itself, such as an upload or a
    /// restore. It takes the same path as a client's update.
    pub async fn apply(&self, update: &[u8]) -> SyncResult<()> {
        let always: Admit = Arc::new(|| true);
        self.write_update(update, None, None, &always)
            .await
            .map(drop)
    }

    /// Reads the document.
    pub fn read<T>(&self, f: impl FnOnce(&LoroDoc) -> T) -> T {
        f(&self.lock().doc)
    }

    pub fn snapshot(&self) -> SyncResult<Vec<u8>> {
        Ok(self.lock().doc.export(ExportMode::Snapshot)?)
    }

    /// How many bytes the project may still grow by: neither stored nor
    /// reserved.
    #[must_use]
    pub fn space(&self) -> usize {
        let quota = lock(&self.quota);
        self.left(&quota, 0)
    }

    /// The bytes stored for the project, snapshot and log.
    #[must_use]
    pub fn stored(&self) -> usize {
        lock(&self.quota).stored
    }

    /// Holds `len` bytes of the quota for an update that is on its way, or
    /// refuses it when they are not there.
    pub fn reserve(&self, len: usize) -> SyncResult<Reservation> {
        let mut quota = lock(&self.quota);
        let left = self.left(&quota, 0);
        if len > left {
            return Err(SyncError::ProjectFull { need: len, left });
        }
        quota.reserved += len;
        Ok(Reservation {
            quota: Arc::clone(&self.quota),
            len,
        })
    }

    /// What is left when `mine` of the reserved bytes are the caller's own.
    fn left(&self, quota: &Quota, mine: usize) -> usize {
        self.limits
            .max_document
            .saturating_sub(quota.stored + quota.reserved - mine)
    }

    #[must_use]
    pub fn peers(&self) -> usize {
        self.lock().sessions.len()
    }

    /// Checks, appends, imports and fans out one update, then compacts if the
    /// log is long. Returns the encoded version vector after the import.
    async fn write_update(
        &self,
        update: &[u8],
        from: Option<Peer>,
        held: Option<Reservation>,
        admit: &Admit,
    ) -> SyncResult<Vec<u8>> {
        let meta = LoroDoc::decode_import_blob_meta(update, true)?;
        // A throwaway import proves the bytes decode before they reach the log.
        LoroDoc::new().import(update)?;

        let mut log = self.write.lock().await;
        if self.lock().doc.oplog_vv().includes_vv(&meta.partial_end_vv) {
            return Ok(self.lock().doc.oplog_vv().encode());
        }
        // The update holds exactly its own length while it is written, so
        // no Begin can take that space in the meantime.
        let mut held = held.unwrap_or(Reservation {
            quota: Arc::clone(&self.quota),
            len: 0,
        });
        {
            let mut quota = lock(&self.quota);
            let left = self.left(&quota, held.len);
            if update.len() > left {
                return Err(SyncError::ProjectFull {
                    need: update.len(),
                    left,
                });
            }
            quota.reserved = quota.reserved - held.len + update.len();
            held.len = update.len();
        }
        let bytes = update.to_vec();
        let admit = Arc::clone(admit);
        // The store asks `admit` inside its own lock, in the step that
        // appends, so access cannot end between the check and the write.
        if !blocking(&self.store, &self.project, move |s, p| {
            s.append(p, &bytes, &*admit)
        })
        .await?
        {
            return Err(SyncError::Ended);
        }
        log.entries += 1;
        log.bytes += update.len();
        {
            let mut quota = lock(&self.quota);
            quota.stored = log.snapshot + log.bytes;
            quota.reserved -= held.len;
            held.len = 0;
        }

        let version = {
            let mut st = self.lock();
            st.doc.import(update)?;
            fan_out(&mut st, &Arc::new(Frame::Update(update.to_vec())), from);
            st.doc.oplog_vv().encode()
        };

        if log.entries >= self.limits.compact_entries || log.bytes >= self.limits.compact_bytes {
            let (covers, snapshot) = {
                let st = self.lock();
                (st.doc.oplog_vv(), st.doc.export(ExportMode::Snapshot)?)
            };
            let snapshot_len = snapshot.len();
            match blocking(&self.store, &self.project, move |s, p| {
                s.compact(p, &covers, &snapshot)
            })
            .await
            {
                Ok(()) => {
                    *log = Log {
                        entries: 0,
                        bytes: 0,
                        snapshot: snapshot_len,
                    };
                    lock(&self.quota).stored = snapshot_len;
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
            && s.tx.try_send(Arc::new(frame)).is_err()
        {
            st.sessions.remove(&peer);
        }
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        lock(&self.state)
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Queues `frame` for every session but `except`. A session whose queue is
/// full is dropped; its client sees the queue end and reconnects.
fn fan_out(state: &mut State, frame: &Arc<Frame>, except: Option<Peer>) {
    state
        .sessions
        .retain(|peer, s| Some(*peer) == except || s.tx.try_send(Arc::clone(frame)).is_ok());
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

    fn open() -> Admit {
        Arc::new(|| true)
    }

    async fn room(store: &Arc<MemoryStore>, limits: Limits) -> Room<MemoryStore> {
        Room::open("p", Arc::clone(store), limits).await.unwrap()
    }

    fn welcome(rx: &mut mpsc::Receiver<Arc<Frame>>) -> (Vec<u8>, Vec<u8>) {
        match Arc::unwrap_or_clone(rx.try_recv().unwrap()) {
            Frame::Welcome { catch_up, version } => (catch_up, version),
            other => panic!("expected Welcome, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn an_update_is_stored_before_its_ack() {
        let store = Arc::new(MemoryStore::default());
        let room = room(&store, Limits::default()).await;
        let (a, mut rx_a) = room.join(Role::Editor, hello(&[]), open()).unwrap();
        let (_b, mut rx_b) = room.join(Role::Editor, hello(&[]), open()).unwrap();
        welcome(&mut rx_a);
        welcome(&mut rx_b);

        let doc = LoroDoc::new();
        let update = edit(&doc, "hello");
        room.receive(a, Frame::Update(update.clone()), None)
            .await
            .unwrap();

        assert_eq!(store.load("p").unwrap().log, vec![update.clone()]);
        let Frame::Ack(vv) = Arc::unwrap_or_clone(rx_a.try_recv().unwrap()) else {
            panic!("expected Ack")
        };
        assert!(
            VersionVector::decode(&vv)
                .unwrap()
                .includes_vv(&doc.oplog_vv())
        );
        assert_eq!(*rx_b.try_recv().unwrap(), Frame::Update(update));
    }

    #[tokio::test]
    async fn a_duplicate_is_acked_and_not_stored_again() {
        let store = Arc::new(MemoryStore::default());
        let room = room(&store, Limits::default()).await;
        let (a, mut rx) = room.join(Role::Editor, hello(&[]), open()).unwrap();
        welcome(&mut rx);
        let update = edit(&LoroDoc::new(), "x");
        room.receive(a, Frame::Update(update.clone()), None)
            .await
            .unwrap();
        room.receive(a, Frame::Update(update), None).await.unwrap();
        assert_eq!(store.load("p").unwrap().log.len(), 1);
        assert!(matches!(*rx.try_recv().unwrap(), Frame::Ack(_)));
        assert!(matches!(*rx.try_recv().unwrap(), Frame::Ack(_)));
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
        let (_p, mut rx) = room.join(Role::Viewer, hello(&[]), open()).unwrap();
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
            .join(Role::Editor, hello(&doc.oplog_vv().encode()), open())
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
    async fn a_project_stops_growing_at_its_limit() {
        let store = Arc::new(MemoryStore::default());
        let limits = Limits {
            max_document: 4096,
            compact_entries: 2,
            ..Limits::default()
        };
        let doc = LoroDoc::new();
        // Text that does not compress, so snapshots stay as large as the edits.
        let mut seed = 1u32;
        let mut page = || -> String {
            (0..1000)
                .map(|_| {
                    seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12345);
                    char::from(b'a' + u8::try_from((seed >> 16) % 26).unwrap())
                })
                .collect()
        };
        let refused = {
            let room = room(&store, limits.clone()).await;
            let mut refused = None;
            for _ in 0..10 {
                if let Err(e) = room.apply(&edit(&doc, &page())).await {
                    refused = Some(e);
                    break;
                }
            }
            refused
        };
        assert!(
            matches!(refused, Some(SyncError::ProjectFull { .. })),
            "{refused:?}"
        );
        // The limit counts what is stored, so it holds after a reopen too.
        let room = room(&store, limits).await;
        assert!(room.apply(&edit(&doc, &page())).await.is_err());
    }

    #[tokio::test]
    async fn viewers_and_commenters_may_not_write() {
        let store = Arc::new(MemoryStore::default());
        let room = room(&store, Limits::default()).await;
        for role in [Role::Viewer, Role::Commenter] {
            let (p, _rx) = room.join(role, hello(&[]), open()).unwrap();
            let err = room
                .receive(p, Frame::Update(edit(&LoroDoc::new(), "x")), None)
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
        let (p, _rx) = room.join(Role::Editor, hello(&[]), open()).unwrap();
        assert!(
            room.receive(p, Frame::Update(vec![1, 2, 3]), None)
                .await
                .is_err()
        );
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
                open(),
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
        let (_slow, mut rx) = room.join(Role::Editor, hello(&[]), open()).unwrap();
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
        let (a, mut rx_a) = room.join(Role::Viewer, hello(&[]), open()).unwrap();
        let (_b, mut rx_b) = room.join(Role::Viewer, hello(&[]), open()).unwrap();
        welcome(&mut rx_a);
        welcome(&mut rx_b);
        room.receive(a, Frame::Presence(vec![7]), None)
            .await
            .unwrap();
        assert_eq!(*rx_b.try_recv().unwrap(), Frame::Presence(vec![7]));
        assert!(rx_a.try_recv().is_err());
    }

    /// Text that does not compress, so its update is about as long as it.
    fn noise(len: usize) -> String {
        let mut seed = 7u32;
        (0..len)
            .map(|_| {
                seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12345);
                char::from(b'a' + u8::try_from((seed >> 16) % 26).unwrap())
            })
            .collect()
    }

    #[tokio::test]
    async fn an_update_may_use_all_the_space_left() {
        let store = Arc::new(MemoryStore::default());
        let room = room(&store, Limits::default()).await;
        let (a, mut rx) = room.join(Role::Editor, hello(&[]), open()).unwrap();
        welcome(&mut rx);
        let update = edit(&LoroDoc::new(), &noise(6 << 20));
        assert!(update.len() > 6 << 20);
        let held = room.reserve(update.len()).unwrap();
        room.receive(a, Frame::Update(update.clone()), Some(held))
            .await
            .unwrap();
        assert!(matches!(*rx.try_recv().unwrap(), Frame::Ack(_)));
        assert_eq!(room.space(), (32 << 20) - update.len());
        assert_eq!(room.stored(), update.len());
    }

    #[tokio::test]
    async fn an_update_larger_than_the_space_left_is_refused_before_it_arrives() {
        let store = Arc::new(MemoryStore::default());
        let limits = Limits {
            max_document: 1 << 20,
            ..Limits::default()
        };
        let room = room(&store, limits).await;
        room.apply(&edit(&LoroDoc::new(), &noise(600 << 10)))
            .await
            .unwrap();
        let left = room.space();
        assert!(left < 500 << 10, "{left}");
        let err = room.reserve(left + 1).err().unwrap();
        assert!(matches!(err, SyncError::ProjectFull { .. }), "{err:?}");
        assert_eq!(err.close_code(), crate::close::TOO_LARGE);
        drop(room.reserve(left).unwrap());
        // The room checks again when the update is whole.
        let err = room
            .apply(&edit(&LoroDoc::new(), &noise(600 << 10)))
            .await
            .unwrap_err();
        assert!(matches!(err, SyncError::ProjectFull { .. }), "{err:?}");
        assert_eq!(store.load("p").unwrap().log.len(), 1);
    }

    #[tokio::test]
    async fn unfinished_updates_hold_their_space_until_written_or_dropped() {
        let store = Arc::new(MemoryStore::default());
        let limits = Limits {
            max_document: 1 << 20,
            ..Limits::default()
        };
        let room = room(&store, limits).await;
        let (a, mut rx) = room.join(Role::Editor, hello(&[]), open()).unwrap();
        welcome(&mut rx);
        let first = room.reserve(600 << 10).unwrap();
        // Two Begins that each fit alone do not both fit.
        let err = room.reserve(600 << 10).err().unwrap();
        assert!(matches!(err, SyncError::ProjectFull { .. }), "{err:?}");
        // Nor does a single-frame update that would take the held space.
        let big = edit(&LoroDoc::new(), &noise(500 << 10));
        let err = room
            .receive(a, Frame::Update(big.clone()), None)
            .await
            .unwrap_err();
        assert!(matches!(err, SyncError::ProjectFull { .. }), "{err:?}");
        assert!(store.load("p").unwrap().log.is_empty());
        // A dropped transfer gives its space back.
        drop(first);
        assert_eq!(room.space(), 1 << 20);
        room.receive(a, Frame::Update(big.clone()), None)
            .await
            .unwrap();
        assert_eq!(room.space(), (1 << 20) - big.len());
        // A reservation larger than its update gives the rest back.
        let held = room.reserve(room.space()).unwrap();
        assert_eq!(room.space(), 0);
        let small = edit(&LoroDoc::new(), "small");
        room.receive(a, Frame::Update(small.clone()), Some(held))
            .await
            .unwrap();
        assert_eq!(room.space(), (1 << 20) - big.len() - small.len());
    }

    /// A store whose append waits at a gate the test opens.
    struct Gated {
        inner: MemoryStore,
        reached: std::sync::Barrier,
        go: std::sync::Barrier,
    }

    impl RoomStore for Gated {
        fn load(&self, project: &str) -> Result<Stored, String> {
            self.inner.load(project)
        }
        fn append(
            &self,
            project: &str,
            update: &[u8],
            admit: &dyn Fn() -> bool,
        ) -> Result<bool, String> {
            self.reached.wait();
            self.go.wait();
            self.inner.append(project, update, admit)
        }
        fn compact(
            &self,
            project: &str,
            covers: &VersionVector,
            snapshot: &[u8],
        ) -> Result<(), String> {
            self.inner.compact(project, covers, snapshot)
        }
    }

    #[tokio::test]
    async fn access_that_ends_while_an_update_waits_to_be_written_stops_it() {
        let store = Arc::new(Gated {
            inner: MemoryStore::default(),
            reached: std::sync::Barrier::new(2),
            go: std::sync::Barrier::new(2),
        });
        let room = Arc::new(
            Room::open("p", Arc::clone(&store), Limits::default())
                .await
                .unwrap(),
        );
        let allowed = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let admit: Admit = {
            let allowed = Arc::clone(&allowed);
            Arc::new(move || allowed.load(Ordering::SeqCst))
        };
        let (a, mut rx_a) = room.join(Role::Editor, hello(&[]), admit).unwrap();
        let (_b, mut rx_b) = room.join(Role::Editor, hello(&[]), open()).unwrap();
        welcome(&mut rx_a);
        welcome(&mut rx_b);
        let write = tokio::spawn({
            let room = Arc::clone(&room);
            async move {
                room.receive(a, Frame::Update(edit(&LoroDoc::new(), "late")), None)
                    .await
            }
        });
        // The update has passed every check before the write and waits there.
        let gate = Arc::clone(&store);
        tokio::task::spawn_blocking(move || gate.reached.wait())
            .await
            .unwrap();
        allowed.store(false, Ordering::SeqCst);
        let gate = Arc::clone(&store);
        tokio::task::spawn_blocking(move || gate.go.wait())
            .await
            .unwrap();
        let err = write.await.unwrap().unwrap_err();
        assert!(matches!(err, SyncError::Ended), "{err:?}");
        assert!(store.inner.load("p").unwrap().log.is_empty());
        assert!(
            rx_b.try_recv().is_err(),
            "the update reached another client"
        );
        assert!(rx_a.try_recv().is_err(), "the update was acked");
        assert_eq!(room.space(), Limits::default().max_document);
    }

    /// The quota is checked to the byte, for an update in one frame and for
    /// one that came in parts with its space reserved.
    #[tokio::test]
    async fn the_quota_holds_to_the_byte() {
        let update = edit(&LoroDoc::new(), &noise(64 << 10));
        for (max, fits) in [(update.len(), true), (update.len() - 1, false)] {
            for parts in [false, true] {
                let store = Arc::new(MemoryStore::default());
                let limits = Limits {
                    max_document: max,
                    ..Limits::default()
                };
                let room = room(&store, limits).await;
                let (a, mut rx) = room.join(Role::Editor, hello(&[]), open()).unwrap();
                welcome(&mut rx);
                let held = if parts {
                    match room.reserve(update.len()) {
                        Ok(held) => Some(held),
                        Err(e) => {
                            assert!(!fits, "{max} {parts}: {e}");
                            continue;
                        }
                    }
                } else {
                    None
                };
                let got = room.receive(a, Frame::Update(update.clone()), held).await;
                assert_eq!(got.is_ok(), fits, "{max} {parts}: {got:?}");
                assert_eq!(room.space(), if fits { 0 } else { max });
            }
        }
    }
}
