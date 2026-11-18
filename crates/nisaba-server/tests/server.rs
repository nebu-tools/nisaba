//! End-to-end tests against the real `nisaba` binary.
//!
//! Each test starts the binary on a free port with a fresh data directory,
//! talks to it over HTTP and WebSocket like a browser would, and kills it.

use std::io::{BufRead as _, BufReader};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use futures_util::{SinkExt as _, StreamExt as _};
use loro::{ExportMode, LoroDoc, LoroValue, ValueOrContainer, VersionVector};
use nisaba_sync::{Frame, PROTOCOL_VERSION};
use tokio_tungstenite::tungstenite::{Message, client::IntoClientRequest as _};

struct Server {
    child: Child,
    base: String,
}

impl Server {
    fn start(data: &std::path::Path) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_nisaba"))
            .args(["--data", data.to_str().unwrap(), "--listen", "127.0.0.1:0"])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("start nisaba");
        let mut line = String::new();
        BufReader::new(child.stdout.take().unwrap())
            .read_line(&mut line)
            .unwrap();
        let base = line
            .split_whitespace()
            .find(|w| w.starts_with("http://"))
            .expect("listening line")
            .to_owned();
        Self { child, base }
    }

    /// SIGKILL: no shutdown hooks, no flushing.
    fn kill9(mut self) {
        self.child.kill().unwrap();
        self.child.wait().unwrap();
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn http() -> reqwest::Client {
    reqwest::Client::builder()
        .cookie_store(true)
        .build()
        .unwrap()
}

async fn sign_up(base: &str) -> (reqwest::Client, String) {
    let client = http();
    let res = client
        .post(format!("{base}/api/setup"))
        .json(&serde_json::json!({"email": "a@example.org", "password": "correct horse"}))
        .send()
        .await
        .unwrap();
    assert!(res.status().is_success(), "{}", res.status());
    let cookie = res
        .headers()
        .get("set-cookie")
        .unwrap()
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_owned();
    (client, cookie)
}

async fn log_in(base: &str) -> (reqwest::Client, String) {
    let client = http();
    let res = client
        .post(format!("{base}/api/login"))
        .json(&serde_json::json!({"email": "a@example.org", "password": "correct horse"}))
        .send()
        .await
        .unwrap();
    assert!(res.status().is_success());
    let cookie = res.headers()["set-cookie"]
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_owned();
    (client, cookie)
}

async fn new_project(client: &reqwest::Client, base: &str) -> String {
    let res: serde_json::Value = client
        .post(format!("{base}/api/projects"))
        .json(&serde_json::json!({"name": "Tide pools", "files": [
            {"path": "main.typ", "text": "= Tide pools\n\nBarnacles close at low tide.\n"}
        ]}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    res["id"].as_str().unwrap().to_owned()
}

async fn main_text(client: &reqwest::Client, base: &str, project: &str) -> String {
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

type Ws =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// A minimal sync client: a Loro document plus the last acknowledged version.
struct Peer {
    ws: Ws,
    doc: LoroDoc,
    acked: VersionVector,
}

enum In {
    Frame(Frame),
    Ack(VersionVector),
}

impl Peer {
    async fn connect(base: &str, cookie: &str, project: &str, peer: u64) -> Self {
        let url = format!("{}/sync/{project}", base.replace("http://", "ws://"));
        let mut req = url.into_client_request().unwrap();
        req.headers_mut().insert("cookie", cookie.parse().unwrap());
        let (mut ws, _) = tokio_tungstenite::connect_async(req).await.unwrap();
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

    async fn recv(&mut self) -> In {
        loop {
            let msg = tokio::time::timeout(Duration::from_secs(10), self.ws.next())
                .await
                .expect("frame within 10 s")
                .expect("socket open")
                .unwrap();
            let Message::Binary(bytes) = msg else {
                continue;
            };
            let frame = Frame::decode(&bytes, 1 << 26).unwrap();
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

    fn main_text(&self) -> loro::LoroText {
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

    /// Types `s` at the end of main.typ and sends everything the server has
    /// not acknowledged.
    async fn type_text(&mut self, s: &str) {
        let text = self.main_text();
        text.insert(text.len_unicode(), s).unwrap();
        self.doc.commit();
        let update = self.doc.export(ExportMode::updates(&self.acked)).unwrap();
        self.ws
            .send(Message::Binary(Frame::Update(update).encode().into()))
            .await
            .unwrap();
    }

    /// Waits until the server has acknowledged every local change.
    async fn saved(&mut self) {
        let local = self.doc.oplog_vv();
        while !self.acked.includes_vv(&local) {
            if let In::Ack(vv) = self.recv().await {
                self.acked = vv;
            }
        }
    }

    /// Waits until main.typ contains `needle` from a remote update.
    async fn sees(&mut self, needle: &str) {
        while !self.main_text().to_string().contains(needle) {
            self.recv().await;
        }
    }
}

#[tokio::test]
async fn two_clients_converge() {
    let dir = tempfile::tempdir().unwrap();
    let server = Server::start(dir.path());
    let (client, cookie) = sign_up(&server.base).await;
    let project = new_project(&client, &server.base).await;
    let mut a = Peer::connect(&server.base, &cookie, &project, 11).await;
    let mut b = Peer::connect(&server.base, &cookie, &project, 22).await;
    a.type_text(" Anemones").await;
    b.type_text(" Limpets").await;
    a.saved().await;
    b.saved().await;
    a.sees("Limpets").await;
    b.sees("Anemones").await;
    assert_eq!(a.main_text().to_string(), b.main_text().to_string());
    assert_eq!(
        main_text(&client, &server.base, &project).await,
        a.main_text().to_string()
    );
}

#[tokio::test]
async fn an_acknowledged_edit_survives_kill_9() {
    let dir = tempfile::tempdir().unwrap();
    let server = Server::start(dir.path());
    let (client, cookie) = sign_up(&server.base).await;
    let project = new_project(&client, &server.base).await;
    let mut a = Peer::connect(&server.base, &cookie, &project, 33).await;
    for i in 0..50 {
        a.type_text(&format!(" edit{i}")).await;
    }
    a.saved().await;
    let expected = a.main_text().to_string();
    // Kill the moment the last ack arrives: nothing after it may matter.
    server.kill9();

    let server = Server::start(dir.path());
    let (client, cookie) = log_in(&server.base).await;
    assert_eq!(main_text(&client, &server.base, &project).await, expected);
    let b = Peer::connect(&server.base, &cookie, &project, 44).await;
    assert_eq!(b.main_text().to_string(), expected);
}

#[tokio::test]
async fn export_reads_the_same_state_as_sync() {
    let dir = tempfile::tempdir().unwrap();
    let server = Server::start(dir.path());
    let (client, cookie) = sign_up(&server.base).await;
    let project = new_project(&client, &server.base).await;
    let mut a = Peer::connect(&server.base, &cookie, &project, 55).await;
    a.type_text("\n\nWrittenOnlyThroughSync").await;
    a.saved().await;
    let pdf = client
        .get(format!("{}/api/projects/{project}/export.pdf", server.base))
        .send()
        .await
        .unwrap();
    assert!(pdf.status().is_success(), "{}", pdf.text().await.unwrap());
    let bytes = pdf.bytes().await.unwrap();
    assert!(bytes.starts_with(b"%PDF"));
    // Typst's PDF output is deterministic, so compiling the text the client
    // holds must give byte-for-byte the PDF the server exported.
    let local = nisaba_server::web::pdf(vec![(
        "main.typ".into(),
        nisaba_compile_core::FileContents::Text(a.main_text().to_string()),
    )])
    .unwrap();
    assert_eq!(local, bytes.to_vec());
}

#[tokio::test]
async fn a_cross_site_socket_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let server = Server::start(dir.path());
    let (client, cookie) = sign_up(&server.base).await;
    let project = new_project(&client, &server.base).await;
    let url = format!("{}/sync/{project}", server.base.replace("http://", "ws://"));
    let mut req = url.into_client_request().unwrap();
    req.headers_mut().insert("cookie", cookie.parse().unwrap());
    req.headers_mut()
        .insert("origin", "https://elsewhere.example".parse().unwrap());
    assert!(tokio_tungstenite::connect_async(req).await.is_err());
}
