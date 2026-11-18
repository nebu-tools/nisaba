//! One open project.
//!
//! Two locks keep the room consistent:
//!
//! * `write` (async) lets one update at a time go through check, durable
//!   append and import, and lets a compaction swap the log for a snapshot.
//! * `state` (sync, never held across an await) guards the document and the
//!   sessions. Import and fan-out happen under it in one step, and so does a
//!   join, so a joining client gets each update once: in its catch-up or as a
//!   fanned-out frame, never both and never neither.
//!
//! The update is appended before it is imported, so the document never holds
//! an operation that is not durable. Every version vector the room sends is
//! therefore safe to count as saved.
//!
//! The store asks each session's [`Access`] in the same step as the append,
//! so an update from a session whose access ended, or whose role stopped
//! allowing writes, is neither stored nor sent on.
//!
//! Compaction runs after the update that made it due has been acknowledged.
//! The snapshot is made without the write lock; only the store's swap of
//! the covered log rows for the snapshot takes it. An update that arrives
//! while the snapshot is made stays in the log.
//!
//! The quota counts what is stored and what unfinished multi-part updates
//! have reserved ([`Room::reserve`]). An update fits only in what neither
//! holds. When it would fit but for a reservation, the refusal is "try again
//! later" and not "the project is full", because the reservation ends.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use loro::{ExportMode, LoroDoc, VersionVector};
use tokio::sync::mpsc;

use crate::error::SyncError;
use crate::protocol::{Frame, PROTOCOL_VERSION};
use crate::store::{LogSize, RoomStore};

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

/// A session's current role, or `None` once its access ended. The room asks
/// it for every update, and the store asks it again at the moment of the
/// append, so a role change or a sign-out takes effect at once.
pub type Access = Arc<dyn Fn() -> Option<Role> + Send + Sync>;

/// An [`Access`] that never ends, for updates the server makes itself.
#[must_use]
pub fn always(role: Role) -> Access {
    Arc::new(move || Some(role))
}

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
    tx: mpsc::Sender<Arc<Frame>>,
    access: Access,
}

struct State {
    doc: LoroDoc,
    sessions: HashMap<Peer, Session>,
}

/// The log size since the last snapshot, and the snapshot's size. Changed
/// only under the `write` lock.
struct Log {
    size: LogSize,
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
    /// Set by [`Room::close`]: the room takes no update and no session.
    closed: AtomicBool,
    /// The log is long enough to compact. Changed under the write lock.
    due: AtomicBool,
    /// A compaction task runs.
    compacting: AtomicBool,
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
            // Every row was checked before it was written, so this means the
            // database was changed by hand or damaged.
            if let Err(e) = doc.import(update) {
                tracing::error!(project, error = %e, "a stored update does not import");
                return Err(SyncError::Storage(format!(
                    "a stored update of this project does not import: {e}"
                )));
            }
        }
        let log = Log {
            size: LogSize {
                entries: stored.log.len(),
                bytes: stored.log.iter().map(Vec::len).sum(),
            },
            snapshot: stored.snapshot.as_ref().map_or(0, Vec::len),
        };
        Ok(Self {
            project: project.to_owned(),
            store,
            limits,
            quota: Arc::new(Mutex::new(Quota {
                stored: log.snapshot + log.size.bytes,
                reserved: 0,
            })),
            write: tokio::sync::Mutex::new(log),
            state: Mutex::new(State {
                doc,
                sessions: HashMap::new(),
            }),
            next_peer: AtomicU64::new(1),
            closed: AtomicBool::new(false),
            due: AtomicBool::new(false),
            compacting: AtomicBool::new(false),
        })
    }

    /// Starts a session from the client's Hello. The returned queue starts
    /// with Welcome and then carries every frame for this client. When the
    /// room drops a client as too slow, the queue ends. The session's
    /// updates are written only while `access` allows it.
    pub fn join(
        &self,
        hello: Frame,
        access: Access,
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
        if self.closed.load(Ordering::Acquire) {
            return Err(SyncError::Closed);
        }
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
        state.sessions.insert(peer, Session { tx, access });
        Ok((peer, rx))
    }

    pub fn leave(&self, peer: Peer) {
        self.lock().sessions.remove(&peer);
    }

    /// Handles one frame from a joined client. Replies go to its queue.
    /// `held` is the space reserved for it when it came in parts.
    pub async fn receive(
        self: &Arc<Self>,
        peer: Peer,
        frame: Frame,
        held: Option<Reservation>,
    ) -> SyncResult<()> {
        let access = self
            .lock()
            .sessions
            .get(&peer)
            .map(|s| Arc::clone(&s.access))
            .ok_or(SyncError::TooSlow)?;
        match frame {
            Frame::Update(update) => {
                let version = self
                    .write_update(&update, Some(peer), held, &access)
                    .await?;
                self.send(peer, Frame::Ack(version));
                self.compact_if_due();
                Ok(())
            }
            Frame::Presence(state) => {
                if state.len() > self.limits.max_presence {
                    return Err(SyncError::PresenceTooLarge(state.len()));
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
    /// restore. It takes the same path as a client's update. `access` is the
    /// person on whose behalf the server makes it.
    pub async fn apply(self: &Arc<Self>, update: &[u8], access: &Access) -> SyncResult<()> {
        self.write_update(update, None, None, access).await?;
        self.compact_if_due();
        Ok(())
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
        self.fits(&quota, len, 0)?;
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

    /// Whether `need` bytes fit when `mine` of the reserved bytes are the
    /// caller's own. The project is full when they do not fit beside what
    /// is stored. When only other unfinished updates are in the way, the
    /// space may come back, so the answer is to try again.
    fn fits(&self, quota: &Quota, need: usize, mine: usize) -> SyncResult<()> {
        let left = self.left(quota, mine);
        if need <= left {
            Ok(())
        } else if need <= self.limits.max_document.saturating_sub(quota.stored) {
            Err(SyncError::Busy)
        } else {
            Err(SyncError::ProjectFull { need, left })
        }
    }

    #[must_use]
    pub fn peers(&self) -> usize {
        self.lock().sessions.len()
    }

    /// Stops the room: it takes no update and no session from now on. Waits
    /// for the update or compaction that is being written, so that when this
    /// returns, nothing of this room is on its way to the store.
    pub async fn close(&self) {
        self.closed.store(true, Ordering::Release);
        drop(self.write.lock().await);
    }

    /// Whether [`Room::close`] was called.
    #[must_use]
    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }

    /// Checks, appends, imports and fans out one update. Returns the encoded
    /// version vector after the import.
    async fn write_update(
        &self,
        update: &[u8],
        from: Option<Peer>,
        held: Option<Reservation>,
        access: &Access,
    ) -> SyncResult<Vec<u8>> {
        // When the store refuses an append that the role still allows, the
        // project itself is gone.
        let refusal = |access: &Access| match access() {
            None => SyncError::Ended,
            Some(Role::Editor) => SyncError::Closed,
            Some(_) => SyncError::Forbidden,
        };
        if access() != Some(Role::Editor) {
            return Err(refusal(access));
        }
        let meta = LoroDoc::decode_import_blob_meta(update, true)?;
        // A throwaway import proves the bytes decode before they reach the log.
        LoroDoc::new().import(update)?;

        let mut log = self.write.lock().await;
        if self.closed.load(Ordering::Acquire) {
            return Err(SyncError::Closed);
        }
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
            self.fits(&quota, update.len(), held.len)?;
            quota.reserved = quota.reserved - held.len + update.len();
            held.len = update.len();
        }
        let bytes = update.to_vec();
        let admit = Arc::clone(access);
        // The store asks `admit` inside its own lock, in the step that
        // appends, so access cannot end between the check and the write.
        if !blocking(&self.store, &self.project, move |s, p| {
            s.append(p, &bytes, &|| admit() == Some(Role::Editor))
        })
        .await?
        {
            return Err(refusal(access));
        }
        log.size.entries += 1;
        log.size.bytes += update.len();
        {
            let mut quota = lock(&self.quota);
            quota.stored = log.snapshot + log.size.bytes;
            quota.reserved -= held.len;
            held.len = 0;
        }

        let version = {
            let mut st = self.lock();
            st.doc.import(update)?;
            fan_out(&mut st, &Arc::new(Frame::Update(update.to_vec())), from);
            st.doc.oplog_vv().encode()
        };
        self.due.store(self.is_due(&log.size), Ordering::Release);
        Ok(version)
    }

    fn is_due(&self, size: &LogSize) -> bool {
        size.entries >= self.limits.compact_entries || size.bytes >= self.limits.compact_bytes
    }

    /// Starts a compaction when one is due and none runs. The caller has
    /// already queued its Ack, so no client waits for the compaction.
    fn compact_if_due(self: &Arc<Self>) {
        if !self.due.load(Ordering::Acquire)
            || self
                .compacting
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .is_err()
        {
            return;
        }
        let room = Arc::clone(self);
        tokio::spawn(async move {
            if let Err(e) = room.compact().await {
                tracing::warn!(project = %room.project, error = %e, "compaction failed");
            }
            room.compacting.store(false, Ordering::Release);
        });
    }

    /// Writes a snapshot of the document and drops the log rows it covers.
    /// The snapshot is made without the write lock, so updates go on while
    /// it is made; rows they add are not covered and stay.
    pub async fn compact(self: &Arc<Self>) -> SyncResult<()> {
        let room = Arc::clone(self);
        let (covers, snapshot) = tokio::task::spawn_blocking(move || {
            let st = room.lock();
            Ok::<_, SyncError>((st.doc.oplog_vv(), st.doc.export(ExportMode::Snapshot)?))
        })
        .await
        .map_err(|e| SyncError::Storage(e.to_string()))??;
        let mut log = self.write.lock().await;
        if self.closed.load(Ordering::Acquire) {
            return Ok(());
        }
        let snapshot_len = snapshot.len();
        // The log still holds everything until the store's swap commits, so
        // a failed compaction loses nothing. The next update tries again.
        let left = blocking(&self.store, &self.project, move |s, p| {
            s.compact(p, &covers, &snapshot)
        })
        .await?;
        *log = Log {
            size: left,
            snapshot: snapshot_len,
        };
        self.due.store(self.is_due(&left), Ordering::Release);
        lock(&self.quota).stored = snapshot_len + left.bytes;
        Ok(())
    }

    /// Whether a compaction task runs now.
    #[must_use]
    pub fn is_compacting(&self) -> bool {
        self.compacting.load(Ordering::Acquire)
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

    impl<S: RoomStore> Room<S> {
        async fn apply_edit(self: &Arc<Self>, update: &[u8]) -> SyncResult<()> {
            self.apply(update, &always(Role::Editor)).await
        }

        /// Waits until no compaction runs.
        async fn settled(&self) {
            while self.is_compacting() {
                tokio::task::yield_now().await;
            }
        }
    }

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

    fn editor() -> Access {
        always(Role::Editor)
    }

    async fn room(store: &Arc<MemoryStore>, limits: Limits) -> Arc<Room<MemoryStore>> {
        Arc::new(Room::open("p", Arc::clone(store), limits).await.unwrap())
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
        let (a, mut rx_a) = room.join(hello(&[]), editor()).unwrap();
        let (_b, mut rx_b) = room.join(hello(&[]), editor()).unwrap();
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
        let (a, mut rx) = room.join(hello(&[]), editor()).unwrap();
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
            room.apply_edit(&edit(&doc, "one ")).await.unwrap();
            room.apply_edit(&edit(&doc, "two")).await.unwrap();
        }
        let room = room(&store, Limits::default()).await;
        let (_p, mut rx) = room.join(hello(&[]), always(Role::Viewer)).unwrap();
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
        room.apply_edit(&edit(&doc, "x")).await.unwrap();
        let (_p, mut rx) = room
            .join(hello(&doc.oplog_vv().encode()), editor())
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
                room.apply_edit(&edit(&doc, word)).await.unwrap();
                room.settled().await;
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
                if let Err(e) = room.apply_edit(&edit(&doc, &page())).await {
                    refused = Some(e);
                    break;
                }
                room.settled().await;
            }
            refused
        };
        assert!(
            matches!(refused, Some(SyncError::ProjectFull { .. })),
            "{refused:?}"
        );
        // The limit counts what is stored, so it holds after a reopen too.
        let room = room(&store, limits).await;
        assert!(room.apply_edit(&edit(&doc, &page())).await.is_err());
    }

    #[tokio::test]
    async fn viewers_and_commenters_may_not_write() {
        let store = Arc::new(MemoryStore::default());
        let room = room(&store, Limits::default()).await;
        for role in [Role::Viewer, Role::Commenter] {
            let (p, _rx) = room.join(hello(&[]), always(role)).unwrap();
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
        let (p, _rx) = room.join(hello(&[]), editor()).unwrap();
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
                Frame::Hello {
                    protocol: PROTOCOL_VERSION + 1,
                    version: Vec::new(),
                },
                editor(),
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
        let (_slow, mut rx) = room.join(hello(&[]), editor()).unwrap();
        let doc = LoroDoc::new();
        for word in ["a", "b", "c"] {
            room.apply_edit(&edit(&doc, word)).await.unwrap();
        }
        assert_eq!(room.peers(), 0);
        while rx.try_recv().is_ok() {}
        assert!(rx.recv().await.is_none());
    }

    #[tokio::test]
    async fn presence_goes_to_the_others_only() {
        let room = room(&Arc::new(MemoryStore::default()), Limits::default()).await;
        let (a, mut rx_a) = room.join(hello(&[]), always(Role::Viewer)).unwrap();
        let (_b, mut rx_b) = room.join(hello(&[]), always(Role::Viewer)).unwrap();
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
        let (a, mut rx) = room.join(hello(&[]), editor()).unwrap();
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
        room.apply_edit(&edit(&LoroDoc::new(), &noise(600 << 10)))
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
            .apply_edit(&edit(&LoroDoc::new(), &noise(600 << 10)))
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
        let (a, mut rx) = room.join(hello(&[]), editor()).unwrap();
        welcome(&mut rx);
        let first = room.reserve(600 << 10).unwrap();
        // Two Begins that each fit alone do not both fit. The project is not
        // full, so the second is told to try again.
        let err = room.reserve(600 << 10).err().unwrap();
        assert!(matches!(err, SyncError::Busy), "{err:?}");
        assert_eq!(err.close_code(), crate::close::TRY_AGAIN);
        // Nor does a single-frame update that would take the held space.
        let big = edit(&LoroDoc::new(), &noise(500 << 10));
        let err = room
            .receive(a, Frame::Update(big.clone()), None)
            .await
            .unwrap_err();
        assert!(matches!(err, SyncError::Busy), "{err:?}");
        // One that would not fit in the empty project either is refused for good.
        let err = room.reserve((1 << 20) + 1).err().unwrap();
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
        ) -> Result<LogSize, String> {
            self.inner.compact(project, covers, snapshot)
        }
    }

    /// Access that ends, or a role that stops allowing writes, while an
    /// update waits to be written stops it: nothing is stored, sent on or
    /// acknowledged.
    #[tokio::test]
    async fn access_that_ends_while_an_update_waits_to_be_written_stops_it() {
        for (after, expected) in [(None, "Ended"), (Some(Role::Viewer), "Forbidden")] {
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
            let role = Arc::new(Mutex::new(Some(Role::Editor)));
            let access: Access = {
                let role = Arc::clone(&role);
                Arc::new(move || *role.lock().unwrap())
            };
            let (a, mut rx_a) = room.join(hello(&[]), access).unwrap();
            let (_b, mut rx_b) = room.join(hello(&[]), editor()).unwrap();
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
            *role.lock().unwrap() = after;
            let gate = Arc::clone(&store);
            tokio::task::spawn_blocking(move || gate.go.wait())
                .await
                .unwrap();
            let err = write.await.unwrap().unwrap_err();
            assert!(format!("{err:?}").starts_with(expected), "{err:?}");
            assert!(store.inner.load("p").unwrap().log.is_empty());
            assert!(
                rx_b.try_recv().is_err(),
                "the update reached another client"
            );
            assert!(rx_a.try_recv().is_err(), "the update was acked");
            assert_eq!(room.space(), Limits::default().max_document);
        }
    }

    /// A role is read for each update, so a viewer who becomes an editor
    /// may write at once, and an editor who becomes a viewer may not.
    #[tokio::test]
    async fn a_role_change_takes_effect_for_the_next_update() {
        let store = Arc::new(MemoryStore::default());
        let room = room(&store, Limits::default()).await;
        let role = Arc::new(Mutex::new(Some(Role::Viewer)));
        let access: Access = {
            let role = Arc::clone(&role);
            Arc::new(move || *role.lock().unwrap())
        };
        let (a, mut rx) = room.join(hello(&[]), access).unwrap();
        welcome(&mut rx);
        let doc = LoroDoc::new();
        let first = edit(&doc, "one");
        let err = room
            .receive(a, Frame::Update(first.clone()), None)
            .await
            .unwrap_err();
        assert!(matches!(err, SyncError::Forbidden), "{err:?}");
        *role.lock().unwrap() = Some(Role::Editor);
        room.receive(a, Frame::Update(first), None).await.unwrap();
        assert!(matches!(*rx.try_recv().unwrap(), Frame::Ack(_)));
        *role.lock().unwrap() = Some(Role::Commenter);
        let err = room
            .receive(a, Frame::Update(edit(&doc, "two")), None)
            .await
            .unwrap_err();
        assert!(matches!(err, SyncError::Forbidden), "{err:?}");
        assert_eq!(store.load("p").unwrap().log.len(), 1);
    }

    /// A closed room takes no update and no session, and closing waits for
    /// the write that is under way.
    #[tokio::test]
    async fn a_closed_room_takes_nothing() {
        let store = Arc::new(MemoryStore::default());
        let room = room(&store, Limits::default()).await;
        let (a, mut rx) = room.join(hello(&[]), editor()).unwrap();
        welcome(&mut rx);
        room.close().await;
        let err = room
            .receive(a, Frame::Update(edit(&LoroDoc::new(), "x")), None)
            .await
            .unwrap_err();
        assert!(matches!(err, SyncError::Closed), "{err:?}");
        assert!(matches!(
            room.join(hello(&[]), editor()),
            Err(SyncError::Closed)
        ));
        assert!(store.load("p").unwrap().log.is_empty());
    }

    /// The Ack of the update that makes a compaction due goes out before
    /// the compaction runs, and updates go on while it runs.
    #[tokio::test]
    async fn compaction_runs_after_the_ack() {
        struct Slow(Gated);
        impl RoomStore for Slow {
            fn load(&self, p: &str) -> Result<Stored, String> {
                self.0.inner.load(p)
            }
            fn append(&self, p: &str, u: &[u8], a: &dyn Fn() -> bool) -> Result<bool, String> {
                self.0.inner.append(p, u, a)
            }
            fn compact(
                &self,
                p: &str,
                covers: &VersionVector,
                snapshot: &[u8],
            ) -> Result<LogSize, String> {
                self.0.reached.wait();
                self.0.go.wait();
                self.0.inner.compact(p, covers, snapshot)
            }
        }
        let slow = Arc::new(Slow(Gated {
            inner: MemoryStore::default(),
            reached: std::sync::Barrier::new(2),
            go: std::sync::Barrier::new(2),
        }));
        let limits = Limits {
            compact_entries: 2,
            ..Limits::default()
        };
        let room = Arc::new(Room::open("p", Arc::clone(&slow), limits).await.unwrap());
        let (a, mut rx) = room.join(hello(&[]), editor()).unwrap();
        welcome(&mut rx);
        let doc = LoroDoc::new();
        for word in ["a", "b"] {
            room.receive(a, Frame::Update(edit(&doc, word)), None)
                .await
                .unwrap();
            assert!(matches!(*rx.try_recv().unwrap(), Frame::Ack(_)));
        }
        // The compaction waits inside the store; the room is not blocked.
        let gate = Arc::clone(&slow);
        let reached = tokio::task::spawn_blocking(move || gate.0.reached.wait());
        reached.await.unwrap();
        assert!(room.is_compacting());
        let late = edit(&doc, "c");
        let receive = tokio::spawn({
            let room = Arc::clone(&room);
            async move { room.receive(a, Frame::Update(late), None).await }
        });
        let gate = Arc::clone(&slow);
        tokio::task::spawn_blocking(move || gate.0.go.wait())
            .await
            .unwrap();
        receive.await.unwrap().unwrap();
        assert!(matches!(*rx.try_recv().unwrap(), Frame::Ack(_)));
        room.settled().await;
        // The update that came during the compaction is in the log, the
        // two before it are in the snapshot.
        let Stored { snapshot, log } = slow.0.inner.load("p").unwrap();
        assert!(snapshot.is_some());
        assert_eq!(log.len(), 1);
        let reopened = Room::open("p", Arc::clone(&slow), Limits::default())
            .await
            .unwrap();
        assert_eq!(reopened.read(|d| d.get_text("main").to_string()), "abc");
        assert_eq!(reopened.stored(), room.stored());
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
                let (a, mut rx) = room.join(hello(&[]), editor()).unwrap();
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
