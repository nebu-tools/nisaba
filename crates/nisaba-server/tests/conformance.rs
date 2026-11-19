//! Every route of `api/openapi.json` against the real binary.
//!
//! For each operation the test makes a fresh server with a project, a
//! member, an asset and a package, then calls the operation with valid
//! input and checks the status and the body against the description, and
//! calls it with each kind of invalid input the description implies: no
//! session, another origin, a body of the wrong shape, not JSON or too
//! large, a project or account that does not exist, and a caller whose role
//! is too low. Every error answer must be an `ApiError` whose code the
//! operation lists, with the status the description gives that code.

mod common;

use std::collections::HashMap;
use std::io::Write as _;

use common::*;
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

const DESCRIPTION: &str = include_str!("../../../api/openapi.json");

/// Checks `value` against `schema`, for the subset of JSON Schema the
/// description uses.
fn validate(schema: &Value, value: &Value, doc: &Value, at: &str) -> Result<(), String> {
    if schema == &Value::Bool(true) {
        return Ok(());
    }
    if let Some(r) = schema["$ref"].as_str() {
        let name = r.rsplit('/').next().unwrap();
        return validate(
            &doc["components"]["schemas"][name],
            value,
            doc,
            &format!("{at}<{name}>"),
        );
    }
    for key in ["oneOf", "anyOf"] {
        if let Some(options) = schema[key].as_array() {
            return if options.iter().any(|s| validate(s, value, doc, at).is_ok()) {
                Ok(())
            } else {
                Err(format!("{at}: {value} matches none of {key}"))
            };
        }
    }
    if let Some(c) = schema.get("const")
        && c != value
    {
        return Err(format!("{at}: {value} is not {c}"));
    }
    if let Some(values) = schema["enum"].as_array()
        && !values.contains(value)
    {
        return Err(format!("{at}: {value} is not one of {values:?}"));
    }
    let types: Vec<&str> = match &schema["type"] {
        Value::String(t) => vec![t.as_str()],
        Value::Array(t) => t.iter().filter_map(Value::as_str).collect(),
        _ => return Ok(()),
    };
    let fits = |t: &str| match t {
        "string" => value.is_string(),
        "integer" => value.is_i64() || value.is_u64(),
        "number" => value.is_number(),
        "boolean" => value.is_boolean(),
        "null" => value.is_null(),
        "array" => value.is_array(),
        "object" => value.is_object(),
        _ => false,
    };
    if !types.iter().any(|t| fits(t)) {
        return Err(format!("{at}: {value} is not {types:?}"));
    }
    if let Some(min) = schema["minimum"].as_f64()
        && value.as_f64().is_some_and(|v| v < min)
    {
        return Err(format!("{at}: {value} is below {min}"));
    }
    if let Some(items) = value.as_array() {
        for (i, item) in items.iter().enumerate() {
            validate(&schema["items"], item, doc, &format!("{at}[{i}]"))?;
        }
    }
    if let Some(object) = value.as_object() {
        let props = schema["properties"].as_object();
        for name in schema["required"].as_array().into_iter().flatten() {
            let name = name.as_str().unwrap();
            if !object.contains_key(name) {
                return Err(format!("{at}: {name} is missing"));
            }
        }
        for (key, v) in object {
            match props.and_then(|p| p.get(key)) {
                Some(prop) => validate(prop, v, doc, &format!("{at}.{key}"))?,
                None if schema["additionalProperties"] == Value::Bool(false) => {
                    return Err(format!("{at}: {key} is not described"));
                }
                None => {}
            }
        }
    }
    Ok(())
}

/// A package registry that serves one package.
async fn registry() -> (String, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let mut tar = tar::Builder::new(Vec::new());
    for (path, data) in [
        (
            "typst.toml",
            "[package]\nname = \"shells\"\nversion = \"0.1.0\"\nentrypoint = \"lib.typ\"\n",
        ),
        ("lib.typ", "#let whelk = [Whelk]"),
    ] {
        let mut header = tar::Header::new_gnu();
        header.set_size(data.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        tar.append_data(&mut header, path, data.as_bytes()).unwrap();
    }
    let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    gz.write_all(&tar.into_inner().unwrap()).unwrap();
    let body = gz.finish().unwrap();
    let task = tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            let body = body.clone();
            tokio::spawn(async move {
                let mut buf = [0u8; 2048];
                let n = socket.read(&mut buf).await.unwrap_or(0);
                let found =
                    String::from_utf8_lossy(&buf[..n]).contains("/preview/shells-0.1.0.tar.gz");
                let head = if found {
                    format!(
                        "HTTP/1.1 200 OK\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                        body.len()
                    )
                } else {
                    "HTTP/1.1 404 Not Found\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"
                        .into()
                };
                let _ = socket.write_all(head.as_bytes()).await;
                if found {
                    let _ = socket.write_all(&body).await;
                }
            });
        }
    });
    (url, task)
}

/// A server with the administrator of the route examples, a project they
/// own with an asset, a member, and a second account that is no member.
struct Fixture {
    server: Server,
    _dir: tempfile::TempDir,
    _registry: tokio::task::JoinHandle<()>,
    admin: reqwest::Client,
    admin_cookie: String,
    /// The member: a viewer and no administrator.
    keeper: reqwest::Client,
    keeper_id: String,
    project: String,
    hash: String,
    font: String,
}

const ADMIN: &str = "curator@example.org";
const ADMIN_PASSWORD: &str = "barnacle tide";

async fn fixture(op: &str) -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let (url, registry) = registry().await;
    let server = Server::start_with(dir.path(), &["--package-registry", &url]);
    let base = server.base.clone();
    let admin = browser(&base);
    if op == "setup" {
        return Fixture {
            server,
            _dir: dir,
            _registry: registry,
            admin_cookie: String::new(),
            keeper: browser(&base),
            keeper_id: String::new(),
            admin,
            project: String::new(),
            hash: String::new(),
            font: String::new(),
        };
    }
    let res = admin
        .post(format!("{base}/api/setup"))
        .json(&json!({"email": ADMIN, "password": ADMIN_PASSWORD}))
        .send()
        .await
        .unwrap();
    let admin_cookie = res.headers()["set-cookie"]
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_owned();
    let mut emails = vec!["keeper@example.org"];
    if op != "user_create" {
        emails.push("warden@example.org");
    }
    let mut ids = HashMap::new();
    for email in emails {
        let res = admin
            .post(format!("{base}/api/admin/users"))
            .json(&json!({"email": email, "password": PASSWORD}))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 201);
        let user: Value = res.json().await.unwrap();
        ids.insert(email, user["id"].as_str().unwrap().to_owned());
    }
    let project = project_of(&admin, &base, "= Rock pools").await;
    let hash = upload_dot(&admin, &base, &project).await;
    let res = admin
        .post(format!("{base}/api/projects/{project}/members"))
        .json(&json!({"email": "keeper@example.org", "role": "viewer"}))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 201);
    let (keeper, _) = log_in_as(&base, "keeper@example.org").await;
    let fonts: Value = admin
        .get(format!("{base}/fonts/fonts.json"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let font = fonts[0]["file"].as_str().unwrap().to_owned();
    Fixture {
        server,
        _dir: dir,
        _registry: registry,
        admin,
        admin_cookie,
        keeper,
        keeper_id: ids["keeper@example.org"].clone(),
        project,
        hash,
        font,
    }
}

/// The path of an operation with its parameters filled from the fixture,
/// or with `missing` for the project and the account.
fn fill(path: &str, f: &Fixture, missing: bool) -> String {
    let nobody = "0000000000000000";
    let no_hash = "0".repeat(64);
    path.replace(
        "{project}",
        if missing {
            "00000000000000000000000000000000"
        } else {
            &f.project
        },
    )
    .replace("{user}", if missing { nobody } else { &f.keeper_id })
    .replace("{hash}", if missing { &no_hash } else { &f.hash })
    .replace("{namespace}", "preview")
    .replace("{name}", if missing { "nothing" } else { "shells" })
    .replace("{archive}", "0.1.0.tar.gz")
    .replace("{file}", if missing { "nothing.ttf" } else { &f.font })
}

struct Op {
    name: String,
    method: String,
    path: String,
    spec: Value,
}

impl Op {
    fn changes_state(&self) -> bool {
        !matches!(self.method.as_str(), "GET" | "HEAD")
    }

    fn auth(&self) -> &str {
        self.spec["x-nisaba-auth"].as_str().unwrap()
    }

    fn json_body(&self) -> Option<(Value, usize)> {
        let body = &self.spec["requestBody"];
        let example = body["content"]["application/json"]["example"].clone();
        let max = usize::try_from(body["x-nisaba-max-bytes"].as_u64().unwrap_or(0)).unwrap();
        (!example.is_null()).then_some((example, max))
    }

    fn bytes_body(&self) -> bool {
        self.spec["requestBody"]["content"]["application/octet-stream"].is_object()
    }

    fn query(&self) -> &'static str {
        if self.name == "asset_upload" {
            "?path=figures/limpet.png"
        } else {
            ""
        }
    }

    /// Every error code this operation may send.
    fn codes(&self) -> Vec<String> {
        self.spec["responses"]
            .as_object()
            .unwrap()
            .values()
            .filter_map(|r| r["x-nisaba-error-codes"].as_array())
            .flatten()
            .map(|c| c.as_str().unwrap().to_owned())
            .collect()
    }

    fn success(&self) -> Vec<u16> {
        self.spec["responses"]
            .as_object()
            .unwrap()
            .iter()
            .filter(|(_, r)| r["x-nisaba-error-codes"].is_null())
            .map(|(s, _)| s.parse().unwrap())
            .collect()
    }
}

fn operations(doc: &Value) -> Vec<Op> {
    let mut out = Vec::new();
    for (path, methods) in doc["paths"].as_object().unwrap() {
        for (method, op) in methods.as_object().unwrap() {
            out.push(Op {
                name: op["operationId"].as_str().unwrap().to_owned(),
                method: method.to_ascii_uppercase(),
                path: path.clone(),
                spec: op.clone(),
            });
        }
    }
    out
}

/// Sends one request: `client` with its cookies, `origin` as the Origin
/// header, and `body`.
async fn call(
    f: &Fixture,
    op: &Op,
    client: &reqwest::Client,
    origin: Option<&str>,
    path: &str,
    body: Option<Vec<u8>>,
) -> reqwest::Response {
    let method = reqwest::Method::from_bytes(op.method.as_bytes()).unwrap();
    let url = format!("{}{path}{}", f.server.base, op.query());
    let mut req = client
        .request(method, url)
        .header("origin", origin.unwrap_or("https://elsewhere.example"));
    if let Some(body) = body {
        req = req.header("content-type", "application/json").body(body);
    }
    req.send().await.unwrap()
}

/// Checks an error answer against the description.
async fn error_is_described(doc: &Value, op: &Op, res: reqwest::Response, want: &str, what: &str) {
    let status = res.status().as_u16();
    let body: Value = res.json().await.unwrap_or(Value::Null);
    let schema = json!({"$ref": "#/components/schemas/ApiError"});
    validate(&schema, &body, doc, "error").unwrap_or_else(|e| panic!("{} {what}: {e}", op.name));
    let code = body["code"].as_str().unwrap();
    assert_eq!(code, want, "{} {what}: {body}", op.name);
    assert!(
        op.codes().contains(&code.to_owned()),
        "{} {what}: {code} is not described",
        op.name
    );
    let described = doc["x-nisaba-errors"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["code"] == code)
        .unwrap()["status"]
        .as_u64()
        .unwrap();
    assert_eq!(u64::from(status), described, "{} {what}", op.name);
}

async fn check(doc: &Value, op: &Op) {
    let f = fixture(&op.name).await;
    let base = f.server.base.clone();
    let own = Some(base.as_str());
    let path = fill(&op.path, &f, false);
    let valid_body = || {
        if op.bytes_body() {
            Some(DOT.to_vec())
        } else {
            op.json_body()
                .map(|(example, _)| serde_json::to_vec(&example).unwrap())
        }
    };

    if op.name == "sync" {
        let mut ws = socket(&base, &f.admin_cookie, &f.project)
            .await
            .expect("an upgrade");
        drop(ws.close(None).await);
        let res = call(&f, op, &f.admin, own, &path, None).await;
        error_is_described(doc, op, res, "invalid_request", "without an upgrade").await;
        let res = call(&f, op, &f.admin, None, &path, None).await;
        error_is_described(doc, op, res, "cross_origin", "from another origin").await;
        return;
    }

    // Invalid input first, so the valid call may change the fixture.
    if op.auth() != "anyone" {
        let res = call(&f, op, &browser(&base), own, &path, valid_body()).await;
        error_is_described(doc, op, res, "not_signed_in", "without a session").await;
    }
    if op.changes_state() {
        let res = call(&f, op, &f.admin, None, &path, valid_body()).await;
        error_is_described(doc, op, res, "cross_origin", "from another origin").await;
    }
    if let Some((_, max)) = op.json_body() {
        for (what, body) in [
            ("an unknown field", br#"{"tide": "low"}"#.to_vec()),
            ("not JSON", b"low tide".to_vec()),
        ] {
            let res = call(&f, op, &f.admin, own, &path, Some(body)).await;
            error_is_described(doc, op, res, "invalid_request", what).await;
        }
        let res = call(&f, op, &f.admin, own, &path, Some(vec![b' '; max + 1])).await;
        error_is_described(doc, op, res, "payload_too_large", "too large").await;
    }
    if op.path.contains("{project}") || op.path.contains("{user}") || op.path.contains("{hash}") {
        let missing = fill(&op.path, &f, true);
        let res = call(&f, op, &f.admin, own, &missing, valid_body()).await;
        error_is_described(
            doc,
            op,
            res,
            "not_found",
            "for something that does not exist",
        )
        .await;
    }
    if matches!(op.auth(), "editor" | "owner" | "admin") {
        let res = call(&f, op, &f.keeper, own, &path, valid_body()).await;
        error_is_described(doc, op, res, "forbidden", "for a viewer").await;
    }

    let res = call(&f, op, &f.admin, own, &path, valid_body()).await;
    let status = res.status().as_u16();
    let content = res
        .headers()
        .get("content-type")
        .map(|v| v.to_str().unwrap().to_owned());
    let bytes = res.bytes().await.unwrap();
    assert!(
        op.success().contains(&status),
        "{} answered {status}, not one of {:?}: {}",
        op.name,
        op.success(),
        String::from_utf8_lossy(&bytes)
    );
    let response = &op.spec["responses"][status.to_string()];
    if let Some(schema) = response["content"]["application/json"]["schema"].as_object() {
        let body: Value = serde_json::from_slice(&bytes).unwrap();
        validate(&Value::Object(schema.clone()), &body, doc, &op.name)
            .unwrap_or_else(|e| panic!("{}: {e}", op.name));
    } else if let Some(media) = response["content"]
        .as_object()
        .and_then(|c| c.keys().next())
    {
        let content = content.unwrap_or_default();
        let base_media = media.split(';').next().unwrap();
        assert!(
            content.starts_with(base_media) || op.name == "font_file",
            "{}: {content} is not {media}",
            op.name
        );
    } else {
        assert!(
            bytes.is_empty(),
            "{}: a body where none is described",
            op.name
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn every_route_answers_as_described() {
    let doc: Value = serde_json::from_str(DESCRIPTION).unwrap();
    let ops = operations(&doc);
    assert!(ops.len() >= 29, "{} operations", ops.len());
    let doc = std::sync::Arc::new(doc);
    let ops = std::sync::Arc::new(ops);
    // Four at a time, each with its own server.
    let next = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let workers: Vec<_> = (0..4)
        .map(|_| {
            let (doc, ops, next) = (doc.clone(), ops.clone(), next.clone());
            tokio::spawn(async move {
                loop {
                    let i = next.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    let Some(op) = ops.get(i) else { return };
                    check(&doc, op).await;
                }
            })
        })
        .collect();
    for w in workers {
        w.await.unwrap();
    }
}

/// The validator refuses what it should, so a pass above means something.
#[test]
fn the_validator_refuses_a_wrong_shape() {
    let doc: Value = serde_json::from_str(DESCRIPTION).unwrap();
    let user = json!({"$ref": "#/components/schemas/User"});
    assert!(
        validate(
            &user,
            &json!({"id": "1", "email": "e", "name": "n", "is_admin": false}),
            &doc,
            "u"
        )
        .is_ok()
    );
    for bad in [
        json!({"id": "1", "email": "e", "name": "n"}),
        json!({"id": 1, "email": "e", "name": "n", "is_admin": false}),
        json!([]),
    ] {
        assert!(validate(&user, &bad, &doc, "u").is_err(), "{bad}");
    }
    let code = json!({"$ref": "#/components/schemas/ErrorCode"});
    assert!(validate(&code, &json!("not_found"), &doc, "c").is_ok());
    assert!(validate(&code, &json!("lost_at_sea"), &doc, "c").is_err());
}
