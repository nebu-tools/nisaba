//! The sync endpoint and the open project rooms.
//!
//! The room in `nisaba-sync` does the work. This module opens one room per
//! project on first use and moves frames between a WebSocket and its room.
//!
//! A multi-part update holds memory and quota until its last part arrives.
//! Three limits keep that bounded: its declared length is reserved against
//! the project's quota at the Begin (4130 when the project is too full for it,
//! 1013 when only another unfinished transfer is in the way), all
//! unfinished updates together may hold at most the server's transfer budget
//! (1013, try again later), and each must arrive by a deadline counted from
//! its Begin (4008, send it again). Sends have a deadline too, so a client
//! that stops reading is closed (4002) and holds nothing for long.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::extract::ws::{CloseFrame, Message};
use futures_util::{Sink, SinkExt as _, Stream, StreamExt as _};
use nisaba_sync::{
    Assembler, Frame, PART, Reservation, Room, SyncError, UPDATE_HEADER, close, split,
};
use tokio::sync::{OnceCell, OwnedSemaphorePermit, Semaphore, watch};
use tokio::time::Instant;

use crate::access::{End as Ended, Grant};
use crate::store::Store;

/// A client that sends nothing for this long is gone. Clients send a
/// Heartbeat every 20 seconds.
const IDLE: Duration = Duration::from_secs(nisaba_sync::IDLE_SECONDS);
const HELLO_WAIT: Duration = Duration::from_secs(nisaba_sync::HELLO_SECONDS);
/// How long one message, at most a part of 1 MiB, may take to reach a
/// client that is reading.
const SEND_WAIT: Duration = Duration::from_secs(30);
/// How long the close frame may take before the socket is dropped anyway.
const CLOSE_WAIT: Duration = Duration::from_secs(2);
/// A multi-part update must arrive within this much time from its Begin...
const TRANSFER_FLOOR: Duration = Duration::from_secs(30);
/// ...plus this much for each MiB it declares, counting a started MiB whole.
/// 32 MiB then has 4 minutes 46 seconds, about 115 KiB a second.
const TRANSFER_PER_MIB: Duration = Duration::from_secs(8);

/// The deadline of a multi-part frame of `total` bytes, from its Begin.
#[must_use]
pub fn transfer_time(total: usize) -> Duration {
    let mib = u32::try_from(total.div_ceil(1 << 20)).unwrap_or(u32::MAX);
    TRANSFER_FLOOR.saturating_add(TRANSFER_PER_MIB.saturating_mul(mib))
}

pub type ProjectRoom = Room<Store>;

type Slot = Arc<OnceCell<Arc<ProjectRoom>>>;

/// The open rooms, one per project.
#[derive(Clone)]
pub struct Rooms {
    store: Arc<Store>,
    open: Arc<Mutex<HashMap<String, Slot>>>,
    /// Bytes that unfinished multi-part frames may hold across all sockets.
    transfers: Arc<Semaphore>,
    /// Set when the server stops; sockets close with 1012.
    stopping: Arc<watch::Sender<bool>>,
}

impl Rooms {
    #[must_use]
    pub fn new(store: Store, transfer_bytes: usize) -> Self {
        Self {
            store: Arc::new(store),
            open: Arc::default(),
            transfers: Arc::new(Semaphore::new(transfer_bytes.min(Semaphore::MAX_PERMITS))),
            stopping: Arc::new(watch::channel(false).0),
        }
    }

    /// The room for `project`, loaded from the store if it is not open.
    /// Rooms of different projects open at the same time; two requests for
    /// one project share one load.
    pub async fn get(&self, project: &str) -> Result<Arc<ProjectRoom>, SyncError> {
        let stopping = *self.stopping.borrow();
        if stopping {
            return Err(SyncError::Closed);
        }
        let slot = Arc::clone(self.lock().entry(project.to_owned()).or_default());
        let room = slot
            .get_or_try_init(|| async {
                Room::open(project, Arc::clone(&self.store), crate::limits::room())
                    .await
                    .map(Arc::new)
            })
            .await;
        match room {
            Ok(room) => Ok(Arc::clone(room)),
            Err(e) => {
                // A load that failed leaves no slot behind, so the next
                // request tries again.
                let mut open = self.lock();
                if open
                    .get(project)
                    .is_some_and(|s| Arc::ptr_eq(s, &slot) && s.get().is_none())
                {
                    open.remove(project);
                }
                Err(e)
            }
        }
    }

    /// Closes and forgets the room of a deleted project. Waits for the
    /// update it is writing.
    pub async fn evict(&self, project: &str) {
        let slot = self.lock().remove(project);
        if let Some(room) = slot.as_ref().and_then(|s| s.get()) {
            room.close().await;
        }
    }

    /// Closes every room that no socket and no request holds, so memory
    /// follows the projects in use rather than every project ever opened.
    /// The room's state is already on disk.
    #[must_use]
    pub fn sweep(&self) -> usize {
        let mut open = self.lock();
        let before = open.len();
        open.retain(|_, slot| match slot.get() {
            Some(room) => Arc::strong_count(room) > 1 || room.is_compacting(),
            None => Arc::strong_count(slot) > 1,
        });
        before - open.len()
    }

    /// How many rooms are open.
    #[must_use]
    pub fn len(&self) -> usize {
        self.lock().len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
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

    /// Tells every socket to close with 1012 after the frame it is on, and
    /// opens no room from now on.
    pub fn stop(&self) {
        self.stopping.send_replace(true);
    }

    /// Closes every room, waiting for the updates and compactions they are
    /// writing. After this, nothing of any room is on its way to the store.
    pub async fn quiesce(&self) {
        let rooms: Vec<_> = self
            .lock()
            .values()
            .filter_map(|s| s.get().cloned())
            .collect();
        for room in &rooms {
            room.close().await;
        }
        // A compaction that made its snapshot before the close does not
        // write it; wait until each has seen the close.
        for room in rooms {
            while room.is_compacting() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Slot>> {
        self.open
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// The socket of one sync connection: axum's WebSocket, or a test's.
pub trait Socket:
    Stream<Item = Result<Message, axum::Error>> + Sink<Message, Error = axum::Error> + Unpin + Send
{
}

impl<S> Socket for S where
    S: Stream<Item = Result<Message, axum::Error>>
        + Sink<Message, Error = axum::Error>
        + Unpin
        + Send
{
}

/// Sends the close frame if the client takes it within [`CLOSE_WAIT`]. The
/// caller drops the socket afterwards either way.
async fn close_with(socket: &mut impl Socket, code: u16, reason: &str) {
    let frame = Message::Close(Some(CloseFrame {
        code,
        reason: reason.to_owned().into(),
    }));
    let _ = tokio::time::timeout(CLOSE_WAIT, socket.send(frame)).await;
}

async fn refuse(socket: &mut impl Socket, e: &SyncError) {
    close_with(socket, e.close_code(), &e.to_string()).await;
}

/// Sends `frame`, in parts when it is long. Each message must go out within
/// [`SEND_WAIT`].
async fn send(socket: &mut impl Socket, frame: &Frame) -> Result<(), End> {
    for message in split(frame, PART) {
        match tokio::time::timeout(SEND_WAIT, socket.send(Message::Binary(message.into()))).await {
            Ok(Ok(())) => {}
            Ok(Err(_)) => return Err(End::Gone),
            Err(_) => return Err(End::Refused(SyncError::TooSlow)),
        }
    }
    Ok(())
}

enum End {
    /// The client closed or went quiet.
    Gone,
    Refused(SyncError),
    /// The grant ended.
    Ended(Ended),
    /// The server stops.
    Stopping,
}

const STOPPING: &str = "the server is restarting";

/// Resolves when the server stops. Cancel-safe.
async fn stopped(stopping: &mut watch::Receiver<bool>) {
    let _ = stopping.wait_for(|s| *s).await.map(drop);
}

/// A multi-part frame on its way in.
struct Transfer {
    deadline: Instant,
    /// The quota its declared length holds, handed to the room with it.
    held: Reservation,
    /// Its share of the server's transfer budget.
    _budget: OwnedSemaphorePermit,
}

/// Runs one client connection. Access was checked before the upgrade, and
/// `grant` keeps the socket open only while that access lasts: a sign-out,
/// an expired session, a deleted account, a removed membership or a deleted
/// project closes it at once, even while a send waits on the client, and the
/// room writes no update after that. A role change that takes away writing
/// closes it with 4003; one that gives writing takes effect at once.
///
/// When the server stops, the socket closes with 1012 between frames, so an
/// update that has arrived is written and acknowledged first.
#[expect(clippy::too_many_lines, reason = "one loop over the socket's events")]
pub async fn run(mut socket: impl Socket, rooms: Rooms, project: String, grant: Grant) {
    let mut ending = grant.ending();
    let mut stopping = rooms.stopping.subscribe();
    let mut parts = Assembler::new(PART);
    let hello = match tokio::time::timeout(HELLO_WAIT, socket.next()).await {
        Ok(Some(Ok(Message::Binary(bytes)))) => parts.push(&bytes, |_| Err(SyncError::Unexpected)),
        _ => return close_with(&mut socket, close::MALFORMED, "expected Hello").await,
    };
    let hello = match hello {
        Ok(Some(frame)) => frame,
        Ok(None) => return refuse(&mut socket, &SyncError::Unexpected).await,
        Err(e) => return refuse(&mut socket, &e).await,
    };
    if let Some(end) = grant.ended() {
        return close_with(&mut socket, end.code, end.reason).await;
    }
    let room = match rooms.get(&project).await {
        Ok(room) => room,
        Err(SyncError::Closed) => {
            return close_with(&mut socket, close::RESTARTING, STOPPING).await;
        }
        Err(e) => return refuse(&mut socket, &e).await,
    };
    let (peer, mut rx) = match room.join(hello, grant.access()) {
        Ok(joined) => joined,
        Err(SyncError::Closed) => {
            let end = grant.ended();
            let (code, reason) = end.map_or((close::RESTARTING, STOPPING), |e| (e.code, e.reason));
            return close_with(&mut socket, code, reason).await;
        }
        Err(e) => return refuse(&mut socket, &e).await,
    };
    let mut transfer: Option<Transfer> = None;
    let end = loop {
        let deadline = transfer.as_ref().map(|t| t.deadline);
        let expired = tokio::time::sleep_until(deadline.unwrap_or_else(Instant::now));
        tokio::select! {
            biased;
            end = ending.wait() => break End::Ended(end),
            () = stopped(&mut stopping) => break End::Stopping,
            () = expired, if deadline.is_some() => break End::Refused(SyncError::TimedOut),
            frame = rx.recv() => {
                let Some(frame) = frame else { break End::Refused(SyncError::TooSlow) };
                let expired = tokio::time::sleep_until(deadline.unwrap_or_else(Instant::now));
                tokio::select! {
                    biased;
                    end = ending.wait() => break End::Ended(end),
                    () = expired, if deadline.is_some() => break End::Refused(SyncError::TimedOut),
                    sent = send(&mut socket, &frame) => if let Err(end) = sent { break end },
                }
            }
            msg = tokio::time::timeout(IDLE, socket.next()) => {
                let Ok(Some(Ok(msg))) = msg else { break End::Gone };
                let bytes = match msg {
                    Message::Binary(bytes) => bytes,
                    Message::Close(_) => break End::Gone,
                    _ => continue,
                };
                if let Some(end) = grant.ended() {
                    break End::Ended(end);
                }
                let frame = match parts.push(&bytes, |total| {
                    let held = room.reserve(total.saturating_sub(UPDATE_HEADER))?;
                    let budget = u32::try_from(total)
                        .ok()
                        .and_then(|n| Arc::clone(&rooms.transfers).try_acquire_many_owned(n).ok())
                        .ok_or(SyncError::Busy)?;
                    transfer = Some(Transfer {
                        deadline: Instant::now() + transfer_time(total),
                        held,
                        _budget: budget,
                    });
                    Ok(())
                }) {
                    Ok(Some(frame)) => frame,
                    Ok(None) => continue,
                    Err(e) => break End::Refused(e),
                };
                let held = transfer.take().map(|t| t.held);
                match room.receive(peer, frame, held).await {
                    Ok(()) => {}
                    Err(SyncError::Ended | SyncError::Forbidden) if grant.ended().is_some() => {
                        break End::Ended(grant.ended().unwrap_or(crate::access::End {
                            code: close::SIGNED_OUT,
                            reason: "signed out; sign in again",
                        }));
                    }
                    Err(SyncError::Closed) => {
                        break match grant.ended() {
                            Some(end) => End::Ended(end),
                            None => End::Stopping,
                        };
                    }
                    Err(e) => break End::Refused(e),
                }
            }
        }
    };
    room.leave(peer);
    // Whatever was on its way in is given back before the close, which may
    // wait on the client.
    drop((transfer, parts));
    match end {
        End::Gone => {}
        End::Refused(e) => {
            tracing::info!(project, code = e.close_code(), "closing a sync socket");
            refuse(&mut socket, &e).await;
        }
        End::Ended(end) => close_with(&mut socket, end.code, end.reason).await,
        End::Stopping => close_with(&mut socket, close::RESTARTING, STOPPING).await,
    }
}

#[cfg(test)]
mod tests {
    use std::pin::Pin;
    use std::task::{Context, Poll};

    use futures_channel::mpsc;
    use loro::{ExportMode, LoroDoc};
    use nisaba_sync::PROTOCOL_VERSION;

    use super::*;

    /// The server's end of a socket. `to_client` holds as many messages as
    /// the test asks, so a client that does not read stops the server's
    /// sends the way a full TCP window does.
    struct Fake {
        from_client: mpsc::UnboundedReceiver<Message>,
        to_client: mpsc::Sender<Message>,
    }

    impl Stream for Fake {
        type Item = Result<Message, axum::Error>;
        fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
            self.from_client.poll_next_unpin(cx).map(|m| m.map(Ok))
        }
    }

    impl Sink<Message> for Fake {
        type Error = axum::Error;
        fn poll_ready(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
        ) -> Poll<Result<(), Self::Error>> {
            self.to_client.poll_ready(cx).map_err(axum::Error::new)
        }
        fn start_send(mut self: Pin<&mut Self>, item: Message) -> Result<(), Self::Error> {
            self.to_client.start_send(item).map_err(axum::Error::new)
        }
        fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }
        fn poll_close(
            mut self: Pin<&mut Self>,
            _: &mut Context<'_>,
        ) -> Poll<Result<(), Self::Error>> {
            self.to_client.close_channel();
            Poll::Ready(Ok(()))
        }
    }

    struct Client {
        tx: mpsc::UnboundedSender<Message>,
        rx: mpsc::Receiver<Message>,
        parts: Assembler,
        session: tokio::task::JoinHandle<()>,
    }

    impl Client {
        fn send(&self, frame: &Frame) {
            for m in split(frame, PART) {
                self.tx.unbounded_send(Message::Binary(m.into())).unwrap();
            }
        }

        /// The next whole frame; panics on a close.
        async fn frame(&mut self) -> Frame {
            loop {
                match self.rx.next().await {
                    Some(Message::Binary(bytes)) => {
                        if let Some(frame) = self.parts.push(&bytes, |_| Ok(())).unwrap() {
                            return frame;
                        }
                    }
                    other => panic!("expected a frame, got {other:?}"),
                }
            }
        }

        /// Reads until the close frame and returns its code, or `None` when
        /// the socket ended without one.
        async fn closed(&mut self) -> Option<u16> {
            while let Some(m) = self.rx.next().await {
                if let Message::Close(frame) = m {
                    return frame.map(|f| f.code);
                }
            }
            None
        }
    }

    struct Setup {
        _dir: tempfile::TempDir,
        store: Store,
        rooms: Rooms,
    }

    const BUDGET: usize = 256 << 20;

    fn setup(budget: usize) -> Setup {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        store
            .create_account(
                &crate::api::User {
                    id: "u".into(),
                    email: "u@example.org".into(),
                    name: "U".into(),
                    is_admin: false,
                },
                "x",
            )
            .unwrap()
            .unwrap();
        store.create_session("t", "u").unwrap();
        store.create_project("p", "P", "u").unwrap();
        store.create_project("q", "Q", "u").unwrap();
        let rooms = Rooms::new(store.clone(), budget);
        Setup {
            _dir: dir,
            store,
            rooms,
        }
    }

    impl Setup {
        /// Opens a session to `project` whose client buffers `window`
        /// messages beyond the one every sender may always put in.
        async fn connect(&self, project: &str, window: usize) -> Client {
            let grant = self.store.grant("t", project).unwrap().unwrap();
            let (tx, from_client) = mpsc::unbounded();
            let (to_client, rx) = mpsc::channel(window);
            let fake = Fake {
                from_client,
                to_client,
            };
            let session = tokio::spawn(run(fake, self.rooms.clone(), project.to_owned(), grant));
            let mut client = Client {
                tx,
                rx,
                parts: Assembler::new(PART),
                session,
            };
            client.send(&Frame::Hello {
                protocol: PROTOCOL_VERSION,
                version: Vec::new(),
            });
            assert!(matches!(client.frame().await, Frame::Welcome { .. }));
            client
        }

        fn budget_left(&self) -> usize {
            self.rooms.transfers.available_permits()
        }
    }

    /// Lets every task run until all of them wait, without moving the clock
    /// past any deadline that matters here.
    async fn settle() {
        tokio::time::sleep(Duration::from_millis(1)).await;
    }

    fn noise(len: usize) -> String {
        let mut x = 0x9e37_79b9_7f4a_7c15_u64;
        (0..len)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                char::from(b'a' + u8::try_from(x % 26).unwrap())
            })
            .collect()
    }

    fn edit(text: &str) -> Vec<u8> {
        let doc = LoroDoc::new();
        doc.get_text("main").insert(0, text).unwrap();
        doc.commit();
        doc.export(ExportMode::all_updates()).unwrap()
    }

    #[test]
    fn the_transfer_deadline_grows_with_the_size_from_a_floor() {
        assert_eq!(transfer_time(1), Duration::from_secs(38));
        assert_eq!(transfer_time(1 << 20), Duration::from_secs(38));
        assert_eq!(transfer_time((1 << 20) + 1), Duration::from_secs(46));
        assert_eq!(transfer_time(32 << 20), Duration::from_secs(30 + 8 * 32));
    }

    #[tokio::test(start_paused = true)]
    async fn a_transfer_past_its_deadline_is_closed_and_gives_back_what_it_held() {
        let s = setup(BUDGET);
        let room = s.rooms.get("p").await.unwrap();
        let full = room.space();
        let mut a = s.connect("p", 8).await;
        let total = (1 << 20) + UPDATE_HEADER;
        a.send(&Frame::Begin {
            total: u32::try_from(total).unwrap(),
        });
        a.send(&Frame::Part {
            index: 0,
            bytes: vec![0; 1000],
        });
        let begun = Instant::now();
        settle().await;
        assert_eq!(room.space(), full - (1 << 20), "the Begin holds its quota");
        assert_eq!(s.budget_left(), BUDGET - total, "and its memory");
        assert_eq!(a.closed().await, Some(close::TIMED_OUT));
        let took = begun.elapsed();
        assert!(
            took >= transfer_time(total) && took < transfer_time(total) + Duration::from_secs(1),
            "{took:?}"
        );
        a.session.await.unwrap();
        assert_eq!(room.space(), full);
        assert_eq!(s.budget_left(), BUDGET);
    }

    #[tokio::test(start_paused = true)]
    async fn a_begin_beyond_the_transfer_budget_is_told_to_try_again() {
        let s = setup(40 << 20);
        let other = s.rooms.get("q").await.unwrap();
        let other_space = other.space();
        let first = s.connect("p", 8).await;
        let mut second = s.connect("q", 8).await;
        let begin = Frame::Begin { total: 30 << 20 };
        first.send(&begin);
        settle().await;
        second.send(&begin);
        assert_eq!(second.closed().await, Some(close::TRY_AGAIN));
        // What the refused Begin reserved in its own project is back.
        second.session.await.unwrap();
        assert_eq!(other.space(), other_space);
        // Once the first transfer is gone, the same Begin is taken.
        drop(first.tx);
        first.session.await.unwrap();
        assert_eq!(s.budget_left(), 40 << 20);
        let third = s.connect("q", 8).await;
        third.send(&begin);
        settle().await;
        assert_eq!(s.budget_left(), 10 << 20);
        assert!(!third.session.is_finished());
    }

    #[tokio::test(start_paused = true)]
    async fn a_begin_that_does_not_fit_beside_another_is_told_to_try_again() {
        let s = setup(BUDGET);
        let room = s.rooms.get("p").await.unwrap();
        let a = s.connect("p", 8).await;
        let mut b = s.connect("p", 8).await;
        // Each fits alone; together they hold one byte more than is left.
        let half = Frame::Begin {
            total: u32::try_from(room.space() / 2 + 1 + UPDATE_HEADER).unwrap(),
        };
        a.send(&half);
        settle().await;
        b.send(&half);
        assert_eq!(b.closed().await, Some(close::TRY_AGAIN));
        assert!(!a.session.is_finished());
    }

    #[tokio::test(start_paused = true)]
    async fn a_client_that_stops_reading_is_closed() {
        let s = setup(BUDGET);
        let room = s.rooms.get("p").await.unwrap();
        let a = s.connect("p", 0).await;
        for word in ["one", "two", "three"] {
            room.apply(&edit(word), &nisaba_sync::always(nisaba_sync::Role::Editor))
                .await
                .unwrap();
        }
        let start = Instant::now();
        tokio::time::timeout(Duration::from_secs(600), a.session)
            .await
            .expect("the session ends")
            .unwrap();
        let took = start.elapsed();
        assert!(
            took >= SEND_WAIT && took <= SEND_WAIT + CLOSE_WAIT + Duration::from_secs(1),
            "{took:?}"
        );
        assert_eq!(room.peers(), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn an_ended_grant_closes_without_waiting_for_a_send() {
        let s = setup(BUDGET);
        let room = s.rooms.get("p").await.unwrap();
        let a = s.connect("p", 0).await;
        for word in ["one", "two", "three"] {
            room.apply(&edit(word), &nisaba_sync::always(nisaba_sync::Role::Editor))
                .await
                .unwrap();
        }
        // The session is now stuck in a send the client never takes.
        tokio::time::sleep(Duration::from_secs(10)).await;
        assert!(!a.session.is_finished());
        s.store.delete_session("t").unwrap();
        let start = Instant::now();
        tokio::time::timeout(Duration::from_secs(600), a.session)
            .await
            .expect("the session ends")
            .unwrap();
        assert!(start.elapsed() <= CLOSE_WAIT, "{:?}", start.elapsed());
        assert_eq!(room.peers(), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn a_slow_reader_gets_a_large_snapshot_in_parts() {
        let s = setup(BUDGET);
        let room = s.rooms.get("p").await.unwrap();
        let big = noise(6 << 20);
        room.apply(&edit(&big), &nisaba_sync::always(nisaba_sync::Role::Editor))
            .await
            .unwrap();
        let grant = s.store.grant("t", "p").unwrap().unwrap();
        let (tx, from_client) = mpsc::unbounded();
        let (to_client, mut rx) = mpsc::channel(0);
        let session = tokio::spawn(run(
            Fake {
                from_client,
                to_client,
            },
            s.rooms.clone(),
            "p".into(),
            grant,
        ));
        tx.unbounded_send(Message::Binary(
            Frame::Hello {
                protocol: PROTOCOL_VERSION,
                version: Vec::new(),
            }
            .encode()
            .into(),
        ))
        .unwrap();
        let mut parts = Assembler::new(PART);
        let mut messages = 0;
        let welcome = loop {
            // Each message waits most of the send deadline before it is read.
            tokio::time::sleep(SEND_WAIT.saturating_sub(Duration::from_secs(5))).await;
            let Some(Message::Binary(bytes)) = rx.next().await else {
                panic!("the session closed after {messages} messages");
            };
            messages += 1;
            if let Some(frame) = parts.push(&bytes, |_| Ok(())).unwrap() {
                break frame;
            }
        };
        assert!(messages > 6, "{messages}");
        let Frame::Welcome { catch_up, .. } = welcome else {
            panic!("expected Welcome")
        };
        let doc = LoroDoc::new();
        doc.import(&catch_up).unwrap();
        assert_eq!(doc.get_text("main").to_string(), big);
        assert!(!session.is_finished());
    }
}
