//! The harness of the end-to-end tests against the real `nisaba` binary.
//!
//! Each test starts the binary on a free port with a fresh data directory,
//! talks to it over HTTP and WebSocket like a browser would, and kills it.

#![allow(dead_code, reason = "each test file uses a part of the harness")]

use std::io::{BufRead as _, BufReader, Read as _};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::{SinkExt as _, StreamExt as _};
use loro::{ExportMode, LoroDoc, LoroValue, ValueOrContainer, VersionVector};
use nisaba_sync::{Assembler, Frame, PART, PROTOCOL_VERSION, split};
use tokio_tungstenite::tungstenite::{Message, client::IntoClientRequest as _};

pub struct Server {
    pub child: Child,
    pub base: String,
    /// The first-run setup code, when the server printed one.
    pub setup_code: Option<String>,
    /// What the server printed at start.
    pub start: Vec<String>,
    /// What the server wrote to its standard error.
    pub logs: Arc<Mutex<String>>,
}

impl Server {
    pub fn start(data: &std::path::Path) -> Self {
        Self::start_with(data, &[])
    }

    pub fn start_with(data: &std::path::Path, args: &[&str]) -> Self {
        Self::launch(data, args, &[])
    }

    /// Starts `nisaba serve` on a free port with `args` and the environment
    /// `env`, and waits for the line that names the public address. Its
    /// standard error is kept in `logs`.
    pub fn launch(data: &std::path::Path, args: &[&str], env: &[(&str, &str)]) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_nisaba"))
            .args([
                "serve",
                "--data",
                data.to_str().unwrap(),
                "--listen",
                "127.0.0.1:0",
            ])
            .args(args)
            .envs(env.iter().copied())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("start nisaba");
        let logs = Arc::new(Mutex::new(String::new()));
        let mut stderr = child.stderr.take().unwrap();
        let keep = Arc::clone(&logs);
        std::thread::spawn(move || {
            let mut buf = [0u8; 4096];
            while let Ok(n) = stderr.read(&mut buf) {
                if n == 0 {
                    break;
                }
                keep.lock()
                    .unwrap()
                    .push_str(&String::from_utf8_lossy(&buf[..n]));
            }
        });
        let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
        let mut base = None;
        let mut setup_code = None;
        let mut start = Vec::new();
        for line in lines.by_ref() {
            let line = line.unwrap();
            start.push(line.clone());
            let mut words = line.split_whitespace();
            match (words.next(), words.next()) {
                (Some("listening"), _) => {
                    base = line
                        .split_whitespace()
                        .find(|w| w.starts_with("http://"))
                        .map(str::to_owned);
                }
                (Some("setup"), Some("code")) => setup_code = words.next().map(str::to_owned),
                // The last line the server prints at start.
                (Some("public"), Some("address")) => break,
                _ => {}
            }
        }
        // Keep reading, so log lines never fill the pipe and block the server.
        std::thread::spawn(move || for _ in lines {});
        let base = base.unwrap_or_else(|| {
            panic!(
                "no listening line; the server said {start:?} and logged {}",
                logs.lock().unwrap()
            )
        });
        Self {
            child,
            base,
            setup_code,
            start,
            logs,
        }
    }

    /// SIGKILL: no shutdown hooks, no flushing.
    pub fn kill9(mut self) {
        self.child.kill().unwrap();
        self.child.wait().unwrap();
    }

    /// SIGTERM, then waits up to `secs` for the process to end.
    pub fn terminate(mut self, secs: u64) -> std::process::ExitStatus {
        let status = Command::new("kill")
            .args(["-TERM", &self.child.id().to_string()])
            .status()
            .unwrap();
        assert!(status.success());
        let end = std::time::Instant::now() + Duration::from_secs(secs);
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                return status;
            }
            assert!(std::time::Instant::now() < end, "the server did not stop");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Waits for the process to end by itself, as after an injected crash.
    pub fn wait_for_exit(mut self, secs: u64) -> std::process::ExitStatus {
        let end = std::time::Instant::now() + Duration::from_secs(secs);
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                return status;
            }
            assert!(std::time::Instant::now() < end, "the server did not end");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    pub fn logs(&self) -> String {
        self.logs.lock().unwrap().clone()
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// A browser on a page of the server at `origin`: it sends that `Origin`
/// and keeps cookies.
pub fn browser(origin: &str) -> reqwest::Client {
    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert("origin", origin.parse().unwrap());
    reqwest::Client::builder()
        .cookie_store(true)
        .default_headers(headers)
        .build()
        .unwrap()
}

pub fn cookie_of(res: &reqwest::Response) -> String {
    res.headers()["set-cookie"]
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_owned()
}

pub const PASSWORD: &str = "correct horse";

/// Makes an account in a data directory, as `nisaba setup` or an
/// administrator would, and returns its id.
pub fn account(dir: &std::path::Path, email: &str, admin: bool) -> String {
    let store = nisaba_server::store::Store::open_shared(dir).unwrap();
    let params = nisaba_server::passwords::Cost::default().params().unwrap();
    let hash = nisaba_server::passwords::hash_with(&params, PASSWORD).unwrap();
    let user = nisaba_server::api::User {
        id: nisaba_server::store::new_id(),
        email: email.to_owned(),
        name: email.split('@').next().unwrap_or(email).to_owned(),
        is_admin: admin,
    };
    store.create_account(&user, &hash).unwrap().unwrap();
    user.id
}

pub async fn sign_up(base: &str) -> (reqwest::Client, String) {
    let client = browser(base);
    let res = client
        .post(format!("{base}/api/setup"))
        .json(&serde_json::json!({"email": "a@example.org", "password": PASSWORD}))
        .send()
        .await
        .unwrap();
    assert!(res.status().is_success(), "{}", res.status());
    let cookie = cookie_of(&res);
    (client, cookie)
}

pub async fn log_in_as(base: &str, email: &str) -> (reqwest::Client, String) {
    let client = browser(base);
    let res = client
        .post(format!("{base}/api/login"))
        .json(&serde_json::json!({"email": email, "password": PASSWORD}))
        .send()
        .await
        .unwrap();
    assert!(res.status().is_success(), "{}", res.status());
    let cookie = cookie_of(&res);
    (client, cookie)
}

pub async fn log_in(base: &str) -> (reqwest::Client, String) {
    log_in_as(base, "a@example.org").await
}

/// Adds accounts to a data directory before the server starts. The first
/// is an administrator.
pub fn accounts(dir: &std::path::Path, emails: &[&str]) {
    for (i, email) in emails.iter().enumerate() {
        account(dir, email, i == 0);
    }
}

pub async fn project_with(
    client: &reqwest::Client,
    base: &str,
    files: serde_json::Value,
) -> String {
    let res = client
        .post(format!("{base}/api/projects"))
        .json(&serde_json::json!({"name": "Tide pools", "files": files}))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 201, "{}", res.text().await.unwrap());
    let res: serde_json::Value = res.json().await.unwrap();
    res["id"].as_str().unwrap().to_owned()
}

pub async fn new_project(client: &reqwest::Client, base: &str) -> String {
    project_with(
        client,
        base,
        serde_json::json!([{"path": "main.typ", "text": "= Tide pools\n\nBarnacles close at low tide.\n"}]),
    )
    .await
}

pub async fn main_text(client: &reqwest::Client, base: &str, project: &str) -> String {
    let files: serde_json::Value = client
        .get(format!("{base}/api/projects/{project}/files"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    files
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["path"] == "main.typ")
        .unwrap()["text"]
        .as_str()
        .unwrap()
        .to_owned()
}

pub type Ws =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// Opens a sync socket from a page of the server.
pub async fn socket(
    base: &str,
    cookie: &str,
    project: &str,
) -> Result<Ws, tokio_tungstenite::tungstenite::Error> {
    let url = format!("{}/sync/{project}", base.replace("http://", "ws://"));
    let mut req = url.into_client_request().unwrap();
    req.headers_mut().insert("cookie", cookie.parse().unwrap());
    req.headers_mut().insert("origin", base.parse().unwrap());
    tokio_tungstenite::connect_async(req)
        .await
        .map(|(ws, _)| ws)
}

/// The close code the server ends a socket with, after a Hello.
pub async fn close_code(ws: &mut Ws) -> u16 {
    let hello = Frame::Hello {
        protocol: PROTOCOL_VERSION,
        version: Vec::new(),
    };
    let _ = ws.send(Message::Binary(hello.encode().into())).await;
    loop {
        let msg = tokio::time::timeout(Duration::from_secs(10), ws.next())
            .await
            .expect("an answer within 10 s");
        match msg {
            Some(Ok(Message::Close(Some(frame)))) => return frame.code.into(),
            Some(Ok(_)) => {}
            other => panic!("socket ended without a close code: {other:?}"),
        }
    }
}

/// A minimal sync client: a Loro document plus the last acknowledged version.
pub struct Peer {
    pub ws: Ws,
    pub doc: LoroDoc,
    pub acked: VersionVector,
    pub parts: Assembler,
}

/// Sends `frame` as the protocol says: whole, or in parts when it is longer
/// than [`PART`].
pub async fn send(ws: &mut Ws, frame: &Frame) {
    for message in split(frame, PART) {
        ws.send(Message::Binary(message.into())).await.unwrap();
    }
}

/// The next message, which must be a close; returns its code.
pub async fn closed_with(ws: &mut Ws) -> u16 {
    loop {
        let msg = tokio::time::timeout(Duration::from_secs(10), ws.next())
            .await
            .expect("a close within 10 s");
        match msg {
            Some(Ok(Message::Close(Some(frame)))) => return frame.code.into(),
            Some(Ok(Message::Binary(bytes))) => {
                let frame = Frame::decode(&bytes, PART).unwrap();
                assert!(
                    !matches!(frame, Frame::Ack(_)),
                    "an Ack came after access ended"
                );
            }
            Some(Ok(_)) => {}
            other => panic!("socket ended without a close code: {other:?}"),
        }
    }
}

pub enum In {
    Frame(Frame),
    Ack(VersionVector),
}

impl Peer {
    pub async fn connect(base: &str, cookie: &str, project: &str, peer: u64) -> Self {
        let mut ws = socket(base, cookie, project).await.unwrap();
        let hello = Frame::Hello {
            protocol: PROTOCOL_VERSION,
            version: Vec::new(),
        };
        ws.send(Message::Binary(hello.encode().into()))
            .await
            .unwrap();
        let doc = LoroDoc::new();
        doc.set_peer_id(peer).unwrap();
        let mut me = Self {
            ws,
            doc,
            acked: VersionVector::default(),
            parts: Assembler::new(PART),
        };
        let In::Frame(Frame::Welcome { catch_up, version }) = me.recv().await else {
            panic!("expected Welcome first");
        };
        if !catch_up.is_empty() {
            me.doc.import(&catch_up).unwrap();
        }
        me.acked = VersionVector::decode(&version).unwrap();
        me
    }

    pub async fn recv(&mut self) -> In {
        loop {
            let msg = tokio::time::timeout(Duration::from_secs(10), self.ws.next())
                .await
                .expect("frame within 10 s")
                .expect("socket open")
                .unwrap();
            let Message::Binary(bytes) = msg else {
                continue;
            };
            let Some(frame) = self.parts.push(&bytes, |_| Ok(())).unwrap() else {
                continue;
            };
            match &frame {
                Frame::Ack(vv) => return In::Ack(VersionVector::decode(vv).unwrap()),
                Frame::Update(u) => {
                    self.doc.import(u).unwrap();
                }
                _ => {}
            }
            return In::Frame(frame);
        }
    }

    pub fn main_text(&self) -> loro::LoroText {
        let mut found = None;
        self.doc.get_map("files").for_each(|_, v| {
            if let ValueOrContainer::Container(loro::Container::Map(m)) = v
                && matches!(m.get("path"), Some(ValueOrContainer::Value(LoroValue::String(p))) if &*p == "main.typ")
                && let Some(ValueOrContainer::Container(loro::Container::Text(t))) = m.get("text")
            {
                found = Some(t);
            }
        });
        found.expect("main.typ")
    }

    /// The entry of the file at `path`, with its id.
    pub fn entry(&self, path: &str) -> (String, loro::LoroMap) {
        let mut found = None;
        self.doc.get_map("files").for_each(|id, v| {
            if let ValueOrContainer::Container(loro::Container::Map(m)) = v
                && matches!(m.get("path"), Some(ValueOrContainer::Value(LoroValue::String(p))) if *p == *path)
            {
                found = Some((id.to_string(), m));
            }
        });
        found.expect(path)
    }

    /// Types `s` at the end of main.typ and sends everything the server has
    /// not acknowledged.
    pub async fn type_text(&mut self, s: &str) {
        let text = self.main_text();
        text.insert(text.len_unicode(), s).unwrap();
        self.push().await;
    }

    /// Commits and sends everything the server has not acknowledged, then
    /// waits for the acknowledgement.
    pub async fn push_saved(&mut self) {
        self.push().await;
        self.saved().await;
    }

    pub async fn push(&mut self) {
        self.doc.commit();
        let update = self.doc.export(ExportMode::updates(&self.acked)).unwrap();
        send(&mut self.ws, &Frame::Update(update)).await;
    }

    /// Waits until the server has acknowledged every local change.
    pub async fn saved(&mut self) {
        let local = self.doc.oplog_vv();
        while !self.acked.includes_vv(&local) {
            if let In::Ack(vv) = self.recv().await {
                self.acked = vv;
            }
        }
    }

    /// Waits until main.typ contains `needle` from a remote update.
    pub async fn sees(&mut self, needle: &str) {
        while !self.main_text().to_string().contains(needle) {
            self.recv().await;
        }
    }
}

/// A 1×1 PNG.
pub const DOT: &[u8] = &[
    0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44, 0x52,
    0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1f, 0x15, 0xc4,
    0x89, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9c, 0x63, 0xf8, 0xcf, 0xc0, 0xf0,
    0x1f, 0x00, 0x05, 0x00, 0x01, 0xff, 0x89, 0x99, 0x3d, 0x1d, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45,
    0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
];

pub const WITH_IMAGE: &str = "= Tide pools\n#image(\"dot.png\", width: 1cm)\n";

/// Uploads `DOT` as `dot.png` into `project` and returns its hash.
pub async fn upload_dot(client: &reqwest::Client, base: &str, project: &str) -> String {
    let res = client
        .post(format!("{base}/api/projects/{project}/assets?path=dot.png"))
        .header("content-type", "application/octet-stream")
        .body(DOT)
        .send()
        .await
        .unwrap();
    assert!(res.status().is_success());
    res.json::<serde_json::Value>().await.unwrap()["hash"]
        .as_str()
        .unwrap()
        .to_owned()
}

pub async fn export(client: &reqwest::Client, base: &str, project: &str) -> reqwest::Response {
    client
        .get(format!("{base}/api/projects/{project}/export.pdf"))
        .send()
        .await
        .unwrap()
}

pub async fn project_of(client: &reqwest::Client, base: &str, text: &str) -> String {
    project_with(
        client,
        base,
        serde_json::json!([{"path": "main.typ", "text": text}]),
    )
    .await
}

/// The server in this process, so a test can take access away through the
/// store the way an admin tool will.
pub struct InProcess {
    pub base: String,
    pub store: nisaba_server::store::Store,
    pub app: nisaba_server::http::App,
    pub task: tokio::task::JoinHandle<()>,
}

impl InProcess {
    pub async fn start(dir: &std::path::Path) -> Self {
        use nisaba_server::config::ExportLimits;
        use nisaba_server::export::Exporter;
        Self::start_with(
            dir,
            Exporter::new(env!("CARGO_BIN_EXE_nisaba").into(), ExportLimits::default()),
        )
        .await
    }

    pub async fn start_with(
        dir: &std::path::Path,
        exporter: nisaba_server::export::Exporter,
    ) -> Self {
        use nisaba_server::config::PublicAddress;
        let store = nisaba_server::store::Store::open(dir).unwrap();
        store.prepare_setup(false).unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let public = PublicAddress::resolve(addr, None, false).unwrap();
        let app = nisaba_server::http::App::new(
            store.clone(),
            nisaba_server::http::Options {
                public,
                exporter,
                transfer_bytes: 256 << 20,
                passwords: nisaba_server::passwords::Cost::default(),
                packages: nisaba_server::config::Settings::default().packages,
            },
        )
        .unwrap();
        let router = nisaba_server::http::router(app.clone());
        let task = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        Self {
            base: format!("http://{addr}"),
            store,
            app,
            task,
        }
    }

    pub fn user_id(&self, email: &str) -> String {
        self.store.credentials(email).unwrap().unwrap().0.id
    }
}

impl Drop for InProcess {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// `len` letters that do not compress.
pub fn noise(len: usize, seed: u64) -> String {
    let mut x = seed | 1;
    (0..len)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            char::from(b'a' + u8::try_from(x % 26).unwrap())
        })
        .collect()
}

/// The children of `pid` and their states, zombies included.
#[cfg(target_os = "linux")]
pub fn children(pid: u32) -> Vec<(u32, char)> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir("/proc").unwrap().flatten() {
        let Ok(child) = entry.file_name().to_string_lossy().parse::<u32>() else {
            continue;
        };
        let Ok(stat) = std::fs::read_to_string(format!("/proc/{child}/stat")) else {
            continue;
        };
        // pid (comm) state ppid ...
        let Some((_, rest)) = stat.rsplit_once(')') else {
            continue;
        };
        let mut fields = rest.split_whitespace();
        let state = fields.next().and_then(|s| s.chars().next()).unwrap_or('?');
        if fields.next().and_then(|p| p.parse::<u32>().ok()) == Some(pid) {
            out.push((child, state));
        }
    }
    out
}

/// Whether `pid` exists at all, as a zombie too.
#[cfg(target_os = "linux")]
pub fn exists(pid: u32) -> bool {
    std::path::Path::new(&format!("/proc/{pid}")).exists()
}

/// Waits up to `secs` for `check`.
pub async fn within(secs: u64, mut check: impl FnMut() -> bool) -> bool {
    let end = std::time::Instant::now() + Duration::from_secs(secs);
    while std::time::Instant::now() < end {
        if check() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    check()
}

pub async fn try_log_in(base: &str, email: &str, password: &str) -> reqwest::Response {
    try_log_in_with(&browser(base), base, email, password).await
}

pub async fn try_log_in_with(
    client: &reqwest::Client,
    base: &str,
    email: &str,
    password: &str,
) -> reqwest::Response {
    client
        .post(format!("{base}/api/login"))
        .json(&serde_json::json!({"email": email, "password": password}))
        .send()
        .await
        .unwrap()
}
