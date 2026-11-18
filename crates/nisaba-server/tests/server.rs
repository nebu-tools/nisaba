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
    /// The first-run setup code, when the server printed one.
    setup_code: Option<String>,
}

impl Server {
    fn start(data: &std::path::Path) -> Self {
        Self::start_with(data, &[])
    }

    fn start_with(data: &std::path::Path, args: &[&str]) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_nisaba"))
            .args(["--data", data.to_str().unwrap(), "--listen", "127.0.0.1:0"])
            .args(args)
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("start nisaba");
        let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
        let mut base = None;
        let mut setup_code = None;
        for line in lines.by_ref() {
            let line = line.unwrap();
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
        Self {
            child,
            base: base.expect("listening line"),
            setup_code,
        }
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

/// A browser on a page of the server at `origin`: it sends that `Origin`
/// and keeps cookies.
fn browser(origin: &str) -> reqwest::Client {
    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert("origin", origin.parse().unwrap());
    reqwest::Client::builder()
        .cookie_store(true)
        .default_headers(headers)
        .build()
        .unwrap()
}

fn cookie_of(res: &reqwest::Response) -> String {
    res.headers()["set-cookie"]
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_owned()
}

const PASSWORD: &str = "correct horse";

async fn sign_up(base: &str) -> (reqwest::Client, String) {
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

async fn log_in_as(base: &str, email: &str) -> (reqwest::Client, String) {
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

async fn log_in(base: &str) -> (reqwest::Client, String) {
    log_in_as(base, "a@example.org").await
}

/// Adds accounts to a data directory before the server starts. The server
/// has no invitations yet, so this is the only way to get a second one.
fn accounts(dir: &std::path::Path, emails: &[&str]) {
    let store = nisaba_server::store::Store::open(dir).unwrap();
    let hash = nisaba_server::http::hash_password(PASSWORD).unwrap();
    for (i, email) in emails.iter().enumerate() {
        store
            .create_user(&format!("user{i}"), email, email, &hash)
            .unwrap();
    }
}

async fn project_with(client: &reqwest::Client, base: &str, files: serde_json::Value) -> String {
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

async fn new_project(client: &reqwest::Client, base: &str) -> String {
    project_with(
        client,
        base,
        serde_json::json!([{"path": "main.typ", "text": "= Tide pools\n\nBarnacles close at low tide.\n"}]),
    )
    .await
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

/// Opens a sync socket from a page of the server.
async fn socket(
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
async fn close_code(ws: &mut Ws) -> u16 {
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

    /// The entry of the file at `path`, with its id.
    fn entry(&self, path: &str) -> (String, loro::LoroMap) {
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
    async fn type_text(&mut self, s: &str) {
        let text = self.main_text();
        text.insert(text.len_unicode(), s).unwrap();
        self.push().await;
    }

    /// Commits and sends everything the server has not acknowledged, then
    /// waits for the acknowledgement.
    async fn push_saved(&mut self) {
        self.push().await;
        self.saved().await;
    }

    async fn push(&mut self) {
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
    let today = nisaba_server::web::export_date(None, nisaba_server::web::utc_days());
    let date = format!("{:04}-{:02}-{:02}", today.0, today.1, today.2);
    let pdf = client
        .get(format!(
            "{}/api/projects/{project}/export.pdf?today={date}",
            server.base
        ))
        .send()
        .await
        .unwrap();
    assert!(pdf.status().is_success(), "{}", pdf.text().await.unwrap());
    let bytes = pdf.bytes().await.unwrap();
    assert!(bytes.starts_with(b"%PDF"));
    // Typst's PDF output is deterministic, so compiling the text the client
    // holds must give byte-for-byte the PDF the server exported.
    let local = nisaba_server::web::pdf(
        vec![(
            "main.typ".into(),
            nisaba_compile_core::FileContents::Text(a.main_text().to_string()),
        )],
        today,
    )
    .unwrap();
    assert_eq!(local, bytes.to_vec());
}

/// The export sees what the preview sees: a renamed file is gone under its
/// old path, and a deleted file is gone. web/src/preview/agreement.test.ts
/// checks the preview side with the same document.
#[tokio::test]
async fn export_follows_a_rename_and_a_delete() {
    let dir = tempfile::tempdir().unwrap();
    let server = Server::start(dir.path());
    let base = &server.base;
    let (client, cookie) = sign_up(base).await;
    let project = project_with(
        &client,
        base,
        serde_json::json!([
            {"path": "main.typ", "text": "#include \"part.typ\"\n= Title"},
            {"path": "part.typ", "text": "Part one."},
        ]),
    )
    .await;
    assert_eq!(export(&client, base, &project).await.status(), 200);
    let mut peer = Peer::connect(base, &cookie, &project, 91).await;

    peer.entry("part.typ")
        .1
        .insert("path", "renamed.typ")
        .unwrap();
    peer.push_saved().await;
    let res = export(&client, base, &project).await;
    assert_eq!(res.status(), 422);
    assert!(res.text().await.unwrap().contains("part.typ"));

    let main = peer.main_text();
    main.delete(0, main.len_unicode()).unwrap();
    main.insert(0, "#include \"renamed.typ\"\n= Title").unwrap();
    peer.push_saved().await;
    assert_eq!(export(&client, base, &project).await.status(), 200);

    let (id, _) = peer.entry("renamed.typ");
    peer.doc.get_map("files").delete(&id).unwrap();
    peer.push_saved().await;
    let res = export(&client, base, &project).await;
    assert_eq!(res.status(), 422);
    assert!(res.text().await.unwrap().contains("renamed.typ"));
}

#[tokio::test]
async fn a_cross_site_socket_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let server = Server::start(dir.path());
    let (client, cookie) = sign_up(&server.base).await;
    let project = new_project(&client, &server.base).await;
    let url = format!("{}/sync/{project}", server.base.replace("http://", "ws://"));
    for origin in [Some("https://elsewhere.example"), None] {
        let mut req = url.clone().into_client_request().unwrap();
        req.headers_mut().insert("cookie", cookie.parse().unwrap());
        if let Some(origin) = origin {
            req.headers_mut().insert("origin", origin.parse().unwrap());
        }
        assert!(
            tokio_tungstenite::connect_async(req).await.is_err(),
            "{origin:?}"
        );
    }
}

/// Many setups at once with different emails: exactly one owner.
#[tokio::test]
async fn setup_creates_exactly_one_owner() {
    let dir = tempfile::tempdir().unwrap();
    let server = Server::start(dir.path());
    let tries = (0..8).map(|i| {
        let base = server.base.clone();
        tokio::spawn(async move {
            browser(&base)
                .post(format!("{base}/api/setup"))
                .json(&serde_json::json!({"email": format!("o{i}@example.org"), "password": PASSWORD}))
                .send()
                .await
                .unwrap()
                .status()
        })
    });
    let mut created = 0;
    for t in tries {
        let status = t.await.unwrap();
        if status.is_success() {
            created += 1;
        } else {
            assert_eq!(status, 409);
        }
    }
    assert_eq!(created, 1);
    let store = nisaba_server::store::Store::open(dir.path()).unwrap();
    assert_eq!(store.user_count().unwrap(), 1);
}

/// Behind a public https address, setup needs the code from the log, and
/// the cookie is `Secure` with the `__Host-` prefix.
#[tokio::test]
async fn setup_off_loopback_needs_the_code() {
    let dir = tempfile::tempdir().unwrap();
    let origin = "https://nisaba.example.org";
    let server = Server::start_with(dir.path(), &["--public-url", origin]);
    let code = server.setup_code.clone().expect("a setup code in the log");
    let base = &server.base;
    let client = browser(origin);
    let session: serde_json::Value = client
        .get(format!("{base}/api/session"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(session["setup_code_needed"], true);
    let setup = |code: Option<String>| {
        let client = client.clone();
        async move {
            let mut body = serde_json::json!({"email": "a@example.org", "password": PASSWORD});
            if let Some(code) = code {
                body["setup_code"] = code.into();
            }
            client
                .post(format!("{base}/api/setup"))
                .json(&body)
                .send()
                .await
                .unwrap()
        }
    };
    assert_eq!(setup(None).await.status(), 403);
    assert_eq!(setup(Some("0000-1111".into())).await.status(), 403);
    let res = setup(Some(code.to_uppercase().replace('-', ""))).await;
    assert_eq!(res.status(), 200);
    let cookie = res.headers()["set-cookie"].to_str().unwrap().to_owned();
    assert!(cookie.starts_with("__Host-nisaba_session="), "{cookie}");
    for part in ["; Secure", "; HttpOnly", "; SameSite=Lax", "; Path=/"] {
        assert!(cookie.contains(part), "{cookie} lacks {part}");
    }
    assert_eq!(setup(Some(code)).await.status(), 409);
}

/// A loopback server with a loopback address needs no code.
#[tokio::test]
async fn setup_on_loopback_needs_no_code() {
    let dir = tempfile::tempdir().unwrap();
    let server = Server::start(dir.path());
    assert!(server.setup_code.is_none());
    sign_up(&server.base).await;
}

#[test]
fn plain_http_off_loopback_needs_the_flag() {
    let dir = tempfile::tempdir().unwrap();
    let run = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_nisaba"))
            .args([
                "--data",
                dir.path().to_str().unwrap(),
                "--listen",
                "127.0.0.1:0",
            ])
            .args(args)
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap()
    };
    let refused = run(&["--public-url", "http://192.0.2.7:8080"])
        .wait_with_output()
        .unwrap();
    assert!(!refused.status.success());
    assert!(String::from_utf8_lossy(&refused.stderr).contains("--allow-insecure-http"));
    let server = Server::start_with(
        dir.path(),
        &[
            "--public-url",
            "http://192.0.2.7:8080",
            "--allow-insecure-http",
        ],
    );
    assert!(server.setup_code.is_some());
}

#[tokio::test]
async fn cross_site_writes_are_refused() {
    let dir = tempfile::tempdir().unwrap();
    let server = Server::start(dir.path());
    let base = &server.base;
    let body = serde_json::json!({"email": "a@example.org", "password": PASSWORD});
    for origin in [Some("https://elsewhere.example"), Some("null"), None] {
        let mut req = reqwest::Client::new()
            .post(format!("{base}/api/setup"))
            .json(&body);
        if let Some(origin) = origin {
            req = req.header("origin", origin);
        }
        assert_eq!(req.send().await.unwrap().status(), 403, "{origin:?}");
    }
    sign_up(base).await;
}

/// A 1×1 PNG.
const DOT: &[u8] = &[
    0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44, 0x52,
    0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1f, 0x15, 0xc4,
    0x89, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9c, 0x63, 0xf8, 0xcf, 0xc0, 0xf0,
    0x1f, 0x00, 0x05, 0x00, 0x01, 0xff, 0x89, 0x99, 0x3d, 0x1d, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45,
    0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
];

const WITH_IMAGE: &str = "= Tide pools\n#image(\"dot.png\", width: 1cm)\n";

/// Uploads `DOT` as `dot.png` into `project` and returns its hash.
async fn upload_dot(client: &reqwest::Client, base: &str, project: &str) -> String {
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

/// Bob tries every project route against Alice's project, and tries to
/// reach Alice's asset through his own project.
#[tokio::test]
async fn no_route_reaches_another_accounts_project() {
    let dir = tempfile::tempdir().unwrap();
    accounts(dir.path(), &["alice@example.org", "bob@example.org"]);
    let server = Server::start(dir.path());
    let base = &server.base;
    let (alice, _) = log_in_as(base, "alice@example.org").await;
    let (bob, bob_cookie) = log_in_as(base, "bob@example.org").await;
    let theirs = project_with(
        &alice,
        base,
        serde_json::json!([{"path": "main.typ", "text": WITH_IMAGE}]),
    )
    .await;
    let hash = upload_dot(&alice, base, &theirs).await;
    // Alice herself gets everything.
    let own = alice
        .get(format!("{base}/api/projects/{theirs}/assets/{hash}"))
        .send()
        .await
        .unwrap();
    assert_eq!(own.status(), 200);
    assert_eq!(own.headers()["x-content-type-options"], "nosniff");
    assert_eq!(own.bytes().await.unwrap().as_ref(), DOT);
    let pdf = alice
        .get(format!("{base}/api/projects/{theirs}/export.pdf"))
        .send()
        .await
        .unwrap();
    assert_eq!(pdf.status(), 200, "{}", pdf.text().await.unwrap());

    // Bob, against Alice's project.
    let projects: serde_json::Value = bob
        .get(format!("{base}/api/projects"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(projects, serde_json::json!([]));
    for path in [
        format!("/api/projects/{theirs}/files"),
        format!("/api/projects/{theirs}/assets/{hash}"),
        format!("/api/projects/{theirs}/export.pdf"),
    ] {
        let res = bob.get(format!("{base}{path}")).send().await.unwrap();
        assert_eq!(res.status(), 404, "GET {path}");
    }
    let res = bob
        .post(format!("{base}/api/projects/{theirs}/assets?path=x.png"))
        .header("content-type", "application/octet-stream")
        .body(DOT)
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 404, "upload");
    let mut ws = socket(base, &bob_cookie, &theirs).await.unwrap();
    assert_eq!(close_code(&mut ws).await, 4004);

    // Bob, through his own project, with Alice's hash.
    let mine = project_with(
        &bob,
        base,
        serde_json::json!([{"path": "main.typ", "text": WITH_IMAGE}]),
    )
    .await;
    let res = bob
        .get(format!("{base}/api/projects/{mine}/assets/{hash}"))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 404, "asset through his own project");
    let mut peer = Peer::connect(base, &bob_cookie, &mine, 77).await;
    let update = nisaba_server::project::add_files(
        Some(&peer.doc.export(ExportMode::Snapshot).unwrap()),
        vec![nisaba_server::project::NewFile::Asset {
            path: "dot.png".into(),
            hash: hash.clone(),
        }],
    )
    .unwrap();
    peer.doc.import(&update).unwrap();
    peer.ws
        .send(Message::Binary(Frame::Update(update).encode().into()))
        .await
        .unwrap();
    peer.saved().await;
    let files = bob
        .get(format!("{base}/api/projects/{mine}/files"))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(files.contains(&hash), "the reference is in Bob's document");
    let res = bob
        .get(format!("{base}/api/projects/{mine}/export.pdf"))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 422, "export must not read Alice's bytes");
    assert!(res.text().await.unwrap().contains("dot.png"));
}

#[tokio::test]
async fn a_socket_without_a_session_is_told_to_sign_in() {
    let dir = tempfile::tempdir().unwrap();
    let server = Server::start(dir.path());
    let (client, _) = sign_up(&server.base).await;
    let project = new_project(&client, &server.base).await;
    let mut ws = socket(&server.base, "nisaba_session=wrong", &project)
        .await
        .unwrap();
    assert_eq!(close_code(&mut ws).await, 4401);
}

#[tokio::test]
async fn an_account_has_at_most_32_sockets() {
    let dir = tempfile::tempdir().unwrap();
    let server = Server::start(dir.path());
    let (client, cookie) = sign_up(&server.base).await;
    let project = new_project(&client, &server.base).await;
    let mut open = Vec::new();
    for i in 0..32 {
        open.push(Peer::connect(&server.base, &cookie, &project, 100 + i).await);
    }
    let mut ws = socket(&server.base, &cookie, &project).await.unwrap();
    assert_eq!(close_code(&mut ws).await, 4029);
    drop(open.pop());
    // The slot comes back once the server sees the socket end.
    for _ in 0..50 {
        let mut ws = socket(&server.base, &cookie, &project).await.unwrap();
        let hello = Frame::Hello {
            protocol: PROTOCOL_VERSION,
            version: Vec::new(),
        };
        ws.send(Message::Binary(hello.encode().into()))
            .await
            .unwrap();
        match ws.next().await {
            Some(Ok(Message::Binary(_))) => return,
            _ => tokio::time::sleep(Duration::from_millis(100)).await,
        }
    }
    panic!("the freed slot never came back");
}

#[tokio::test]
async fn a_large_upload_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let server = Server::start(dir.path());
    let (client, _) = sign_up(&server.base).await;
    let project = new_project(&client, &server.base).await;
    let res = client
        .post(format!(
            "{}/api/projects/{project}/assets?path=big.bin",
            server.base
        ))
        .header("content-type", "application/octet-stream")
        .body(vec![0u8; (32 << 20) + 1])
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 413);
}

async fn export(client: &reqwest::Client, base: &str, project: &str) -> reqwest::Response {
    client
        .get(format!("{base}/api/projects/{project}/export.pdf"))
        .send()
        .await
        .unwrap()
}

async fn project_of(client: &reqwest::Client, base: &str, text: &str) -> String {
    project_with(
        client,
        base,
        serde_json::json!([{"path": "main.typ", "text": text}]),
    )
    .await
}

/// A document that asks for a terabyte gets an answer, and the server goes
/// on exporting.
#[tokio::test]
async fn an_export_that_runs_out_of_memory_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let server = Server::start_with(dir.path(), &["--export-memory-mb", "512"]);
    let base = &server.base;
    let (client, _) = sign_up(base).await;
    let huge = project_of(&client, base, "#(\"x\" * 1000000000000)").await;
    let res = export(&client, base, &huge).await;
    assert_eq!(res.status(), 422);
    let message = res.text().await.unwrap();
    assert!(message.contains("out of memory"), "{message}");
    let fine = new_project(&client, base).await;
    let res = export(&client, base, &fine).await;
    assert_eq!(res.status(), 200, "{}", res.text().await.unwrap());
}

/// A document that never ends is killed at the deadline, and while it runs
/// with one slot and no queue, a second export is told to try again.
#[tokio::test]
async fn an_export_that_runs_out_of_time_is_killed() {
    let dir = tempfile::tempdir().unwrap();
    let server = Server::start_with(
        dir.path(),
        &[
            "--export-seconds",
            "2",
            "--export-parallel",
            "1",
            "--export-queue",
            "0",
        ],
    );
    let base = &server.base;
    let (client, _) = sign_up(base).await;
    let endless = project_of(
        &client,
        base,
        "#for a in range(100000) { for b in range(100000) { } }",
    )
    .await;
    let started = std::time::Instant::now();
    let slow = tokio::spawn({
        let (client, base, endless) = (client.clone(), base.clone(), endless.clone());
        async move { export(&client, &base, &endless).await }
    });
    tokio::time::sleep(Duration::from_millis(500)).await;
    let busy = export(&client, base, &endless).await;
    assert_eq!(busy.status(), 503);
    assert!(busy.headers().contains_key("retry-after"));
    assert!(busy.text().await.unwrap().contains("busy"));
    let res = slow.await.unwrap();
    assert_eq!(res.status(), 422);
    let message = res.text().await.unwrap();
    assert!(message.contains("out of time"), "{message}");
    assert!(started.elapsed() < Duration::from_secs(10));
    let fine = new_project(&client, base).await;
    assert_eq!(export(&client, base, &fine).await.status(), 200);
}
