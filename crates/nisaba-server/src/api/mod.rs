//! The HTTP API and the sync protocol, described once.
//!
//! [`types`] holds the JSON bodies and [`routes`] every route. From them,
//! [`description`] makes one `OpenAPI` 3.1 document, `api/openapi.json`, which
//! also describes the sync socket's frames and close codes and the server's
//! limits under `x-nisaba-*` keys. [`typescript`] makes the web client's
//! types, `web/src/app/api.gen.ts`, from that document alone.
//!
//! The test `the_description_and_the_typescript_are_current` fails when
//! either file differs from what the code makes; run it with
//! `NISABA_WRITE_API=1` to write both.

pub mod routes;
pub mod types;

use std::collections::BTreeMap;
use std::fmt::Write as _;

use schemars::generate::{Contract, SchemaSettings};
use serde_json::{Map, Value, json};

pub use types::*;

use crate::limits;
use routes::{Auth, Body, ROUTES, Reply, Route};

fn generator(contract: Contract) -> schemars::SchemaGenerator {
    let mut settings = SchemaSettings::draft2020_12();
    settings.definitions_path = "/components/schemas".into();
    settings.contract = contract;
    settings.into_generator()
}

/// The `OpenAPI` 3.1 document of the whole API.
#[must_use]
#[expect(clippy::too_many_lines, reason = "one document, built in order")]
pub fn description() -> Value {
    // Requests are described as the server reads them, replies as it writes
    // them: a field with a default is optional in a request, and a reply
    // field is optional only when the server may leave it out.
    let mut requests = generator(Contract::Deserialize);
    let mut replies = generator(Contract::Serialize);
    let mut paths: BTreeMap<&str, Map<String, Value>> = BTreeMap::new();
    let error_ref = json!({"$ref": "#/components/schemas/ApiError"});
    for route in ROUTES {
        let mut op = Map::new();
        op.insert("operationId".into(), route.id.into());
        op.insert("summary".into(), route.summary.into());
        op.insert("x-nisaba-auth".into(), route.auth.name().into());
        if route.auth != Auth::Anyone {
            op.insert("security".into(), json!([{"session": []}]));
        }
        let mut parameters = Vec::new();
        for segment in route.path.split('/') {
            if let Some(name) = segment.strip_prefix('{').and_then(|s| s.strip_suffix('}')) {
                parameters.push(json!({
                    "name": name, "in": "path", "required": true,
                    "schema": {"type": "string"},
                }));
            }
        }
        if let Some(query) = route.query {
            let reference = (query.schema)(&mut requests);
            op.insert("x-nisaba-query".into(), reference.to_value());
            let defs = requests.definitions().clone();
            let schema = defs.get(query.name).cloned().unwrap_or(Value::Null);
            let required: Vec<&str> = schema["required"]
                .as_array()
                .map(|r| r.iter().filter_map(Value::as_str).collect())
                .unwrap_or_default();
            if let Some(props) = schema["properties"].as_object() {
                for (name, prop) in props {
                    let mut prop = prop.clone();
                    let description = prop
                        .as_object_mut()
                        .and_then(|p| p.remove("description"))
                        .unwrap_or(Value::Null);
                    parameters.push(json!({
                        "name": name, "in": "query",
                        "required": required.contains(&name.as_str()),
                        "description": description,
                        "schema": prop,
                    }));
                }
            }
        }
        if !parameters.is_empty() {
            op.insert("parameters".into(), parameters.into());
        }
        match route.body {
            Body::None => {}
            Body::Json { ty, max } => {
                let schema = (ty.schema)(&mut requests).to_value();
                let mut content = json!({"schema": schema});
                if let Some(example) = route.example {
                    content["example"] =
                        serde_json::from_str(example).expect("route examples are JSON");
                }
                op.insert(
                    "requestBody".into(),
                    json!({
                        "required": true,
                        "content": {"application/json": content},
                        "x-nisaba-max-bytes": max,
                    }),
                );
            }
            Body::Bytes { max } => {
                op.insert(
                    "requestBody".into(),
                    json!({
                        "required": true,
                        "content": {"application/octet-stream": {}},
                        "x-nisaba-max-bytes": max,
                    }),
                );
            }
        }
        let mut responses = Map::new();
        for (status, reply) in std::iter::once(&route.ok).chain(route.also) {
            let content = match reply {
                Reply::Empty | Reply::Upgrade => None,
                Reply::Json(ty) => Some(json!({"application/json": {
                    "schema": (ty.schema)(&mut replies).to_value()}})),
                Reply::List(ty) => Some(json!({"application/json": {
                    "schema": {"type": "array", "items": (ty.schema)(&mut replies).to_value()}}})),
                Reply::Bytes(media) => Some(json!({ *media: {} })),
            };
            let mut response = json!({"description": describe_status(*status)});
            if let Some(content) = content {
                response["content"] = content;
            }
            responses.insert(status.to_string(), response);
        }
        let mut by_status: BTreeMap<u16, Vec<&str>> = BTreeMap::new();
        for code in route.error_codes() {
            by_status
                .entry(code.status())
                .or_default()
                .push(code_name(code));
        }
        for (status, codes) in by_status {
            responses.insert(
                status.to_string(),
                json!({
                    "description": describe_status(status),
                    "content": {"application/json": {"schema": error_ref}},
                    "x-nisaba-error-codes": codes,
                }),
            );
        }
        op.insert("responses".into(), responses.into());
        paths
            .entry(route.path)
            .or_default()
            .insert(route.method.to_ascii_lowercase(), op.into());
    }
    for extra in routes::extra_types() {
        let _ = (extra.schema)(&mut replies);
    }
    let mut schemas = requests.take_definitions(true);
    for (name, schema) in replies.take_definitions(true) {
        if let Some(existing) = schemas.get(&name) {
            assert_eq!(
                existing, &schema,
                "{name} is read and written differently; give the two shapes two types"
            );
        }
        schemas.insert(name, schema);
    }
    let schemas: BTreeMap<String, Value> = schemas.into_iter().collect();
    let errors: Vec<Value> = ErrorCode::ALL
        .iter()
        .map(|&code| {
            json!({"code": code_name(code), "status": code.status(), "meaning": code.meaning()})
        })
        .collect();
    let frames: Vec<Value> = nisaba_sync::FRAMES
        .iter()
        .map(|f| {
            json!({
                "tag": f.tag, "name": f.name, "from": f.from, "meaning": f.meaning,
                "fields": f.fields.iter().map(|(name, kind)| json!({
                    "name": name,
                    "kind": match kind {
                        nisaba_sync::FieldKind::U8 => "u8",
                        nisaba_sync::FieldKind::U32 => "u32",
                        nisaba_sync::FieldKind::Blob => "blob",
                    },
                })).collect::<Vec<_>>(),
            })
        })
        .collect();
    let close_codes: Vec<Value> = nisaba_sync::close::ALL
        .iter()
        .map(|c| {
            json!({"code": c.code, "name": c.name, "meaning": c.meaning,
                   "action": c.action, "sent": c.sent})
        })
        .collect();
    let room = limits::room();
    let secs = |d: std::time::Duration| d.as_secs();
    json!({
        "openapi": "3.1.0",
        "info": {
            "title": "Nisaba",
            "version": env!("CARGO_PKG_VERSION"),
            "description": "The HTTP API and the sync protocol of the Nisaba server. Every \
                            error answer is an ApiError. Every request other than GET and HEAD \
                            must carry an Origin header equal to the server's public address. \
                            Generated from crates/nisaba-server/src/api; do not edit.",
        },
        "paths": paths,
        "components": {
            "schemas": schemas,
            "securitySchemes": {
                "session": {
                    "type": "apiKey", "in": "cookie", "name": "nisaba_session",
                    "description": "The session cookie: `__Host-nisaba_session` behind an \
                                    https public address, `nisaba_session` behind plain http.",
                },
            },
        },
        "x-nisaba-errors": errors,
        "x-nisaba-sync": {
            "path": "/sync/{project}",
            "protocol": nisaba_sync::PROTOCOL_VERSION,
            "partBytes": nisaba_sync::PART,
            "heartbeatSeconds": nisaba_sync::HEARTBEAT_SECONDS,
            "idleSeconds": nisaba_sync::IDLE_SECONDS,
            "helloSeconds": nisaba_sync::HELLO_SECONDS,
            "frames": frames,
            "closeCodes": close_codes,
        },
        "x-nisaba-limits": {
            "requestBodyBytes": limits::MAX_BODY,
            "assetBytes": limits::MAX_ASSET,
            "importBytes": limits::MAX_IMPORT,
            "documentBytes": room.max_document,
            "presenceBytes": room.max_presence,
            "socketsPerAccount": limits::SOCKETS_PER_ACCOUNT,
            "socketsPerProject": room.max_peers,
            "passwordMinChars": limits::PASSWORD_MIN,
            "passwordMaxBytes": limits::PASSWORD_MAX,
            "nameMaxChars": limits::NAME_MAX,
            "projectNameMaxChars": limits::PROJECT_NAME_MAX,
            "emailMaxBytes": limits::EMAIL_MAX,
            "pathMaxBytes": limits::PATH_MAX,
            "sessionSeconds": secs(limits::SESSION_TTL),
            "sessionMaxSeconds": secs(limits::SESSION_MAX_AGE),
        },
    })
}

fn code_name(code: ErrorCode) -> &'static str {
    code.name()
}

fn describe_status(status: u16) -> &'static str {
    match status {
        101 => "Switching Protocols",
        200 => "OK",
        201 => "Created",
        204 => "No Content",
        206 => "Partial Content",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        409 => "Conflict",
        413 => "Content Too Large",
        416 => "Range Not Satisfiable",
        422 => "Unprocessable Content",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        507 => "Insufficient Storage",
        _ => "Answer",
    }
}

/// The description as the committed file holds it.
#[must_use]
pub fn description_text() -> String {
    let mut text = serde_json::to_string_pretty(&description()).expect("JSON");
    text.push('\n');
    text
}

/// The web client's TypeScript, made from the description alone.
#[must_use]
#[expect(clippy::too_many_lines, reason = "one file, written in order")]
pub fn typescript(description: &Value) -> String {
    let mut out = String::from(
        "// Generated from api/openapi.json, which is generated from\n\
         // crates/nisaba-server/src/api. Do not edit.\n\
         // Regenerate: NISABA_WRITE_API=1 cargo test -p nisaba-server --lib api\n",
    );
    let schemas = description["components"]["schemas"]
        .as_object()
        .cloned()
        .unwrap_or_default();
    for (name, schema) in &schemas {
        out.push('\n');
        doc(&mut out, schema["description"].as_str(), "");
        let _ = writeln!(out, "export type {name} = {}", ts(schema, ""));
    }

    out.push_str("\n/** The HTTP status of each error code. */\n");
    out.push_str("export const ERROR_STATUS: Record<ErrorCode, number> = {\n");
    for e in description["x-nisaba-errors"]
        .as_array()
        .into_iter()
        .flatten()
    {
        doc(&mut out, e["meaning"].as_str(), "  ");
        let _ = writeln!(
            out,
            "  {}: {},",
            e["code"].as_str().unwrap_or(""),
            e["status"]
        );
    }
    out.push_str("}\n");

    let sync = &description["x-nisaba-sync"];
    let _ = writeln!(
        out,
        "\n/** The sync protocol version. */\nexport const PROTOCOL = {}\n\
         /** The longest WebSocket message's payload; longer frames go in parts. */\n\
         export const PART = {}\n\
         /** The client sends a Heartbeat this often. */\n\
         export const HEARTBEAT_SECONDS = {}\n\
         /** The server closes a socket that sends nothing for this long. */\n\
         export const IDLE_SECONDS = {}",
        sync["protocol"], sync["partBytes"], sync["heartbeatSeconds"], sync["idleSeconds"]
    );
    out.push_str("\n/** The type byte of each sync frame. */\nexport const FrameType = {\n");
    for f in sync["frames"].as_array().into_iter().flatten() {
        let fields: Vec<String> = f["fields"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|x| {
                format!(
                    "{} ({})",
                    x["name"].as_str().unwrap_or(""),
                    x["kind"].as_str().unwrap_or("")
                )
            })
            .collect();
        let text = format!(
            "{} From {}. Fields: {}.",
            f["meaning"].as_str().unwrap_or(""),
            f["from"].as_str().unwrap_or(""),
            if fields.is_empty() {
                "none".to_owned()
            } else {
                fields.join(", ")
            }
        );
        doc(&mut out, Some(&text), "  ");
        let _ = writeln!(out, "  {}: {},", f["name"].as_str().unwrap_or(""), f["tag"]);
    }
    out.push_str("} as const\n");
    out.push_str("\n/** WebSocket close codes, by name. */\nexport const Close = {\n");
    for c in sync["closeCodes"].as_array().into_iter().flatten() {
        let mut text = format!(
            "{} Client: {}",
            c["meaning"].as_str().unwrap_or(""),
            c["action"].as_str().unwrap_or("")
        );
        if c["sent"] == false {
            text.push_str(" The server does not send this code.");
        }
        doc(&mut out, Some(&text), "  ");
        let _ = writeln!(
            out,
            "  {}: {},",
            c["name"].as_str().unwrap_or(""),
            c["code"]
        );
    }
    out.push_str("} as const\n\nexport type CloseCode = (typeof Close)[keyof typeof Close]\n");

    out.push_str("\n/** Fixed limits of the server. */\nexport const LIMITS = {\n");
    for (k, v) in description["x-nisaba-limits"]
        .as_object()
        .into_iter()
        .flatten()
    {
        let _ = writeln!(out, "  {k}: {v},");
    }
    out.push_str("} as const\n");

    out.push_str(
        "\n/** Every route: its method, path, who may call it, what it takes and answers, and \
         the error codes it may send. */\nexport interface Routes {\n",
    );
    let mut consts =
        String::from("\n/** Method and path of every route. */\nexport const ROUTES = {\n");
    for (path, methods) in description["paths"].as_object().into_iter().flatten() {
        for (method, op) in methods.as_object().into_iter().flatten() {
            let id = op["operationId"].as_str().unwrap_or("");
            let method = method.to_ascii_uppercase();
            doc(&mut out, op["summary"].as_str(), "  ");
            let query = op["x-nisaba-query"]
                .as_object()
                .map_or("never".to_owned(), |q| ts(&Value::Object(q.clone()), ""));
            let body = match op["requestBody"]["content"].as_object() {
                Some(c) if c.contains_key("application/json") => {
                    ts(&c["application/json"]["schema"], "    ")
                }
                Some(_) => "Blob | ArrayBuffer | Uint8Array".to_owned(),
                None => "never".to_owned(),
            };
            let mut reply = Vec::new();
            let mut codes = Vec::new();
            for (status, r) in op["responses"].as_object().into_iter().flatten() {
                if let Some(c) = r["x-nisaba-error-codes"].as_array() {
                    codes.extend(
                        c.iter()
                            .filter_map(Value::as_str)
                            .map(|c| format!("\"{c}\"")),
                    );
                    continue;
                }
                let t = match r["content"].as_object() {
                    Some(c) if c.contains_key("application/json") => {
                        ts(&c["application/json"]["schema"], "    ")
                    }
                    Some(_) => "Blob".to_owned(),
                    None => "void".to_owned(),
                };
                reply.push(format!("{status}: {t}"));
            }
            let _ = writeln!(
                out,
                "  {id}: {{\n    method: \"{method}\"\n    path: \"{path}\"\n    \
                 auth: \"{}\"\n    query: {query}\n    body: {body}\n    \
                 reply: {{ {} }}\n    errors: {}\n  }}",
                op["x-nisaba-auth"].as_str().unwrap_or(""),
                reply.join("; "),
                codes.join(" | ")
            );
            let _ = writeln!(
                consts,
                "  {id}: {{ method: \"{method}\", path: \"{path}\" }},"
            );
        }
    }
    out.push_str("}\n");
    consts.push_str("} as const satisfies { [K in keyof Routes]: { method: Routes[K][\"method\"]; path: Routes[K][\"path\"] } }\n");
    out.push_str(&consts);
    out
}

/// Writes `text` as a `JSDoc` comment.
fn doc(out: &mut String, text: Option<&str>, indent: &str) {
    let Some(text) = text.filter(|t| !t.is_empty()) else {
        return;
    };
    let _ = writeln!(out, "{indent}/**");
    for line in text.lines() {
        let line = line.replace("*/", "* /");
        if line.is_empty() {
            let _ = writeln!(out, "{indent} *");
        } else {
            let _ = writeln!(out, "{indent} * {line}");
        }
    }
    let _ = writeln!(out, "{indent} */");
}

/// The TypeScript of one JSON Schema, for the subset schemars writes.
fn ts(schema: &Value, indent: &str) -> String {
    match schema {
        Value::Bool(true) => return "unknown".into(),
        Value::Bool(false) => return "never".into(),
        _ => {}
    }
    if let Some(r) = schema["$ref"].as_str() {
        return r.rsplit('/').next().unwrap_or("unknown").to_owned();
    }
    if let Some(c) = schema.get("const") {
        return c.to_string();
    }
    if let Some(values) = schema["enum"].as_array() {
        return values
            .iter()
            .map(Value::to_string)
            .collect::<Vec<_>>()
            .join(" | ");
    }
    for key in ["oneOf", "anyOf"] {
        if let Some(variants) = schema[key].as_array() {
            return variants
                .iter()
                .map(|v| ts(v, indent))
                .collect::<Vec<_>>()
                .join(" | ");
        }
    }
    let types: Vec<&str> = match &schema["type"] {
        Value::String(t) => vec![t.as_str()],
        Value::Array(ts) => ts.iter().filter_map(Value::as_str).collect(),
        _ => return "unknown".into(),
    };
    types
        .iter()
        .map(|t| match *t {
            "string" => "string".to_owned(),
            "integer" | "number" => "number".to_owned(),
            "boolean" => "boolean".to_owned(),
            "null" => "null".to_owned(),
            "array" => format!("Array<{}>", ts(&schema["items"], indent)),
            "object" => object(schema, indent),
            _ => "unknown".to_owned(),
        })
        .collect::<Vec<_>>()
        .join(" | ")
}

fn object(schema: &Value, indent: &str) -> String {
    let Some(props) = schema["properties"].as_object() else {
        return match &schema["additionalProperties"] {
            Value::Object(_) => {
                format!(
                    "Record<string, {}>",
                    ts(&schema["additionalProperties"], indent)
                )
            }
            _ => "Record<string, unknown>".to_owned(),
        };
    };
    let required: Vec<&str> = schema["required"]
        .as_array()
        .map(|r| r.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    let inner = format!("{indent}  ");
    let mut out = String::from("{\n");
    for (name, prop) in props {
        doc(&mut out, prop["description"].as_str(), &inner);
        let optional = if required.contains(&name.as_str()) {
            ""
        } else {
            "?"
        };
        let _ = writeln!(out, "{inner}{name}{optional}: {}", ts(prop, &inner));
    }
    out.push_str(indent);
    out.push('}');
    out
}

/// The route of `id`, for code that needs its path or limits.
#[must_use]
pub fn route(id: &str) -> &'static Route {
    routes::route(id)
}

#[cfg(test)]
mod tests {
    use super::*;

    const DESCRIPTION: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../api/openapi.json");
    const TYPESCRIPT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../web/src/app/api.gen.ts");

    #[test]
    fn the_description_and_the_typescript_are_current() {
        let description = description_text();
        let parsed: Value = serde_json::from_str(&description).unwrap();
        let typescript = typescript(&parsed);
        if std::env::var_os("NISABA_WRITE_API").is_some() {
            std::fs::create_dir_all(std::path::Path::new(DESCRIPTION).parent().unwrap()).unwrap();
            std::fs::write(DESCRIPTION, &description).unwrap();
            std::fs::write(TYPESCRIPT, &typescript).unwrap();
            return;
        }
        let stale = "is stale; run NISABA_WRITE_API=1 cargo test -p nisaba-server --lib api";
        let have = std::fs::read_to_string(DESCRIPTION).unwrap_or_default();
        assert!(have == description, "api/openapi.json {stale}");
        let have = std::fs::read_to_string(TYPESCRIPT).unwrap_or_default();
        assert!(have == typescript, "web/src/app/api.gen.ts {stale}");
    }

    #[test]
    fn every_error_code_has_its_name() {
        for code in ErrorCode::ALL {
            assert_eq!(serde_json::to_value(code).unwrap(), code.name());
        }
    }

    #[test]
    fn every_route_is_unique_and_its_types_are_described() {
        let d = description();
        let mut seen = std::collections::HashSet::new();
        for r in ROUTES {
            assert!(seen.insert((r.method, r.path)), "{} {}", r.method, r.path);
            assert!(
                d["paths"][r.path][r.method.to_ascii_lowercase()].is_object(),
                "{}",
                r.id
            );
        }
        let schemas = d["components"]["schemas"].as_object().unwrap();
        for name in ["ApiError", "ErrorCode", "Role", "User", "Project", "Member"] {
            assert!(schemas.contains_key(name), "{name}");
        }
        let text = d.to_string();
        for r in text.match_indices("#/components/schemas/") {
            let rest = &text[r.0 + r.1.len()..];
            let name: String = rest.chars().take_while(|c| *c != '"').collect();
            assert!(schemas.contains_key(&name), "dangling reference to {name}");
        }
    }
}
