//! Server settings: one table of every setting, read from a file, the
//! environment and the command line, in that order, each overriding the one
//! before; the public address; and the export limits.
//!
//! Each setting has a key such as `export.memory_mb`. In a TOML file it is
//! `memory_mb` under `[export]`; in the environment it is
//! `NISABA_EXPORT_MEMORY_MB`; on the command line `--export-memory-mb`.
//! `--config FILE` or `NISABA_CONFIG` names the file. An unknown key in the
//! file or an unknown flag is an error, so a typo does not go unnoticed.
//!
//! The public address is the one setting from which the server knows its own
//! name. The session cookie, the origin check on every state-changing request
//! and on the sync socket, and the first-run setup code all follow from it.

use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;

use crate::passwords::Cost;

/// Everything the settings set.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Settings {
    pub data: PathBuf,
    pub listen: String,
    /// The address people type into the browser, such as
    /// `https://nisaba.example.org`. Defaults to `http://` and the listen
    /// address when that is a loopback address.
    pub public_url: Option<String>,
    /// Accept a plain-http public address that is not loopback. Session
    /// cookies then cross the network unencrypted.
    pub allow_insecure_http: bool,
    pub log_format: LogFormat,
    pub export: ExportLimits,
    /// Memory for unfinished multi-part updates across all sync sockets, in
    /// MiB. A Begin beyond it is told to try again later.
    pub sync_transfer_mb: u64,
    pub packages: Packages,
    pub passwords: Cost,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LogFormat {
    /// One line per event, for people.
    Text,
    /// One JSON object per line, for log collectors.
    Json,
}

/// Where packages come from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Packages {
    /// Download a package the cache lacks.
    pub download: bool,
    /// The registry's address; `{namespace}/{name}-{version}.tar.gz` follows.
    pub registry: String,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            data: PathBuf::from("nisaba-data"),
            listen: "127.0.0.1:8080".into(),
            public_url: None,
            allow_insecure_http: false,
            log_format: LogFormat::Text,
            export: ExportLimits::default(),
            sync_transfer_mb: 256,
            packages: Packages {
                download: true,
                registry: "https://packages.typst.org".into(),
            },
            passwords: Cost::default(),
        }
    }
}

/// Limits on one PDF export, which runs in a child process.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExportLimits {
    /// Address space of the child, in MiB.
    pub memory_mb: u64,
    /// Wall-clock deadline, after which the child is killed.
    pub seconds: u64,
    /// Exports that run at the same time.
    pub parallel: usize,
    /// Exports that may wait for a free slot. Beyond this the server answers
    /// "busy, try again".
    pub queue: usize,
    /// Run exports even when this system cannot sandbox them.
    pub allow_unsandboxed: bool,
}

impl Default for ExportLimits {
    fn default() -> Self {
        Self {
            memory_mb: 1024,
            seconds: 60,
            parallel: 2,
            queue: 8,
            allow_unsandboxed: false,
        }
    }
}

/// How a setting is written on the command line.
#[derive(Clone, Copy)]
enum Form {
    /// `--flag VALUE`.
    Value,
    /// `--flag` alone sets the setting to this.
    Switch(bool),
}

/// One setting.
struct Spec {
    key: &'static str,
    flag: &'static str,
    form: Form,
    help: &'static str,
    set: fn(&mut Settings, &str) -> Result<(), String>,
}

fn whole(value: &str, min: u64) -> Result<u64, String> {
    match value.trim().parse::<u64>() {
        Ok(n) if n >= min => Ok(n),
        _ => Err(format!("needs a whole number of at least {min}")),
    }
}

fn boolean(value: &str) -> Result<bool, String> {
    match value.trim() {
        "true" | "1" | "yes" => Ok(true),
        "false" | "0" | "no" => Ok(false),
        _ => Err("needs true or false".into()),
    }
}

fn count(value: &str, min: u64) -> Result<usize, String> {
    usize::try_from(whole(value, min)?).map_err(|e| e.to_string())
}

fn kib(value: &str) -> Result<u32, String> {
    u32::try_from(whole(value, 1)?).map_err(|e| e.to_string())
}

const SPECS: &[Spec] = &[
    Spec {
        key: "data",
        flag: "--data",
        form: Form::Value,
        help: "the data directory (default nisaba-data)",
        set: |s, v| {
            s.data = PathBuf::from(v);
            Ok(())
        },
    },
    Spec {
        key: "listen",
        flag: "--listen",
        form: Form::Value,
        help: "the address to listen on (default 127.0.0.1:8080)",
        set: |s, v| {
            v.parse::<SocketAddr>()
                .map_err(|_| "needs an address and a port, such as 127.0.0.1:8080".to_owned())?;
            v.clone_into(&mut s.listen);
            Ok(())
        },
    },
    Spec {
        key: "public_url",
        flag: "--public-url",
        form: Form::Value,
        help: "the https address people use, such as https://nisaba.example.org",
        set: |s, v| {
            PublicAddress::parse(v)?;
            s.public_url = Some(v.to_owned());
            Ok(())
        },
    },
    Spec {
        key: "allow_insecure_http",
        flag: "--allow-insecure-http",
        form: Form::Switch(true),
        help: "accept a plain-http public address on the network",
        set: |s, v| {
            s.allow_insecure_http = boolean(v)?;
            Ok(())
        },
    },
    Spec {
        key: "log_format",
        flag: "--log-format",
        form: Form::Value,
        help: "text or json (default text)",
        set: |s, v| {
            s.log_format = match v.trim() {
                "text" => LogFormat::Text,
                "json" => LogFormat::Json,
                _ => return Err("needs text or json".into()),
            };
            Ok(())
        },
    },
    Spec {
        key: "export.memory_mb",
        flag: "--export-memory-mb",
        form: Form::Value,
        help: "address space of one export, at least 64 (default 1024)",
        set: |s, v| {
            s.export.memory_mb = whole(v, 64)?;
            Ok(())
        },
    },
    Spec {
        key: "export.seconds",
        flag: "--export-seconds",
        form: Form::Value,
        help: "deadline of one export, at least 1 (default 60)",
        set: |s, v| {
            s.export.seconds = whole(v, 1)?;
            Ok(())
        },
    },
    Spec {
        key: "export.parallel",
        flag: "--export-parallel",
        form: Form::Value,
        help: "exports that run at once, at least 1 (default 2)",
        set: |s, v| {
            s.export.parallel = count(v, 1)?;
            Ok(())
        },
    },
    Spec {
        key: "export.queue",
        flag: "--export-queue",
        form: Form::Value,
        help: "exports that wait for a slot (default 8)",
        set: |s, v| {
            s.export.queue = count(v, 0)?;
            Ok(())
        },
    },
    Spec {
        key: "export.allow_unsandboxed",
        flag: "--allow-unsandboxed-export",
        form: Form::Switch(true),
        help: "run exports even where this system cannot sandbox them",
        set: |s, v| {
            s.export.allow_unsandboxed = boolean(v)?;
            Ok(())
        },
    },
    Spec {
        key: "sync.transfer_mb",
        flag: "--sync-transfer-mb",
        form: Form::Value,
        help: "memory of all updates in parts at once, at least 32 (default 256)",
        set: |s, v| {
            // One update may be 32 MiB, so less could never take one.
            s.sync_transfer_mb = whole(v, 32)?;
            Ok(())
        },
    },
    Spec {
        key: "packages.download",
        flag: "--no-package-downloads",
        form: Form::Switch(false),
        help: "never download a package the cache lacks",
        set: |s, v| {
            s.packages.download = boolean(v)?;
            Ok(())
        },
    },
    Spec {
        key: "packages.registry",
        flag: "--package-registry",
        form: Form::Value,
        help: "where packages are downloaded from (default https://packages.typst.org)",
        set: |s, v| {
            let v = v.trim().trim_end_matches('/');
            if !(v.starts_with("https://") || v.starts_with("http://")) {
                return Err("needs an http or https address".into());
            }
            v.clone_into(&mut s.packages.registry);
            Ok(())
        },
    },
    Spec {
        key: "passwords.memory_kib",
        flag: "--password-memory-kib",
        form: Form::Value,
        help: "Argon2id memory per hash, at least 19456 (default 19456)",
        set: |s, v| {
            s.passwords.memory_kib = kib(v)?;
            Ok(())
        },
    },
    Spec {
        key: "passwords.iterations",
        flag: "--password-iterations",
        form: Form::Value,
        help: "Argon2id passes per hash, at least 2 (default 2)",
        set: |s, v| {
            s.passwords.iterations = kib(v)?;
            Ok(())
        },
    },
];

fn env_name(key: &str) -> String {
    format!("NISABA_{}", key.replace('.', "_").to_ascii_uppercase())
}

/// The settings part of the usage text.
#[must_use]
pub fn settings_help() -> String {
    use std::fmt::Write as _;
    let mut out = String::new();
    for spec in SPECS {
        let flag = match spec.form {
            Form::Value => format!("{} VALUE", spec.flag),
            Form::Switch(_) => spec.flag.to_owned(),
        };
        let _ = writeln!(out, "  {flag:<30} {}", spec.help);
        let _ = writeln!(
            out,
            "  {:<30} file: {}, environment: {}",
            "",
            spec.key,
            env_name(spec.key)
        );
    }
    out
}

impl Settings {
    /// Reads the settings from the file `--config` or `NISABA_CONFIG`
    /// names, then the environment `env` gives, then `args`. Returns the
    /// arguments that are not settings, in order.
    pub fn load(
        args: &[String],
        env: impl Fn(&str) -> Option<String>,
    ) -> Result<(Self, Vec<String>), String> {
        let mut s = Self::default();
        let mut rest = Vec::new();
        let mut flags = Vec::new();
        let mut config = env("NISABA_CONFIG").map(PathBuf::from);
        let mut args = args.iter();
        while let Some(arg) = args.next() {
            if arg == "--config" {
                config = Some(PathBuf::from(args.next().ok_or("--config needs a file")?));
                continue;
            }
            match SPECS.iter().find(|spec| spec.flag == arg) {
                Some(spec) => {
                    let value = match spec.form {
                        Form::Value => args
                            .next()
                            .ok_or_else(|| format!("{arg} needs a value"))?
                            .clone(),
                        Form::Switch(to) => to.to_string(),
                    };
                    flags.push((spec, value));
                }
                None if arg.starts_with("--") => return Err(format!("unknown argument {arg}")),
                None => rest.push(arg.clone()),
            }
        }
        if let Some(path) = config {
            let text = std::fs::read_to_string(&path)
                .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
            for (key, value) in flatten(&text).map_err(|e| format!("{}: {e}", path.display()))? {
                let spec = SPECS
                    .iter()
                    .find(|spec| spec.key == key)
                    .ok_or_else(|| format!("{}: unknown setting {key}", path.display()))?;
                (spec.set)(&mut s, &value).map_err(|e| format!("{}: {key} {e}", path.display()))?;
            }
        }
        for spec in SPECS {
            let name = env_name(spec.key);
            if let Some(value) = env(&name) {
                (spec.set)(&mut s, &value).map_err(|e| format!("{name} {e}"))?;
            }
        }
        for (spec, value) in flags {
            (spec.set)(&mut s, &value).map_err(|e| format!("{} {e}", spec.flag))?;
        }
        s.passwords
            .params()
            .map_err(|e| format!("passwords: {e}"))?;
        Ok((s, rest))
    }

    /// Every setting as `key = value`, for `nisaba check`.
    #[must_use]
    pub fn describe(&self) -> Vec<(String, String)> {
        vec![
            ("data".into(), self.data.display().to_string()),
            ("listen".into(), self.listen.clone()),
            (
                "public_url".into(),
                self.public_url
                    .clone()
                    .unwrap_or_else(|| "(the listen address)".into()),
            ),
            (
                "allow_insecure_http".into(),
                self.allow_insecure_http.to_string(),
            ),
            (
                "log_format".into(),
                format!("{:?}", self.log_format).to_lowercase(),
            ),
            ("export.memory_mb".into(), self.export.memory_mb.to_string()),
            ("export.seconds".into(), self.export.seconds.to_string()),
            ("export.parallel".into(), self.export.parallel.to_string()),
            ("export.queue".into(), self.export.queue.to_string()),
            (
                "export.allow_unsandboxed".into(),
                self.export.allow_unsandboxed.to_string(),
            ),
            ("sync.transfer_mb".into(), self.sync_transfer_mb.to_string()),
            (
                "packages.download".into(),
                self.packages.download.to_string(),
            ),
            ("packages.registry".into(), self.packages.registry.clone()),
            (
                "passwords.memory_kib".into(),
                self.passwords.memory_kib.to_string(),
            ),
            (
                "passwords.iterations".into(),
                self.passwords.iterations.to_string(),
            ),
        ]
    }
}

/// The settings of a TOML file as `(key, value)`: top-level values under
/// their name, values in a table under `table.name`.
fn flatten(text: &str) -> Result<Vec<(String, String)>, String> {
    let table: toml::Table = text
        .parse()
        .map_err(|e: toml::de::Error| e.message().to_owned())?;
    let mut out = Vec::new();
    let scalar = |key: &str, v: &toml::Value| match v {
        toml::Value::String(s) => Ok(s.clone()),
        toml::Value::Integer(n) => Ok(n.to_string()),
        toml::Value::Boolean(b) => Ok(b.to_string()),
        _ => Err(format!("{key} must be a string, a number or true or false")),
    };
    for (key, value) in &table {
        if let toml::Value::Table(inner) = value {
            for (name, v) in inner {
                let full = format!("{key}.{name}");
                out.push((full.clone(), scalar(&full, v)?));
            }
        } else {
            out.push((key.clone(), scalar(key, value)?));
        }
    }
    Ok(out)
}

/// The address people use to reach the server.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PublicAddress {
    /// `scheme://host[:port]` as a browser writes it in an `Origin` header:
    /// lower case, without a default port.
    origin: String,
    https: bool,
    loopback: bool,
}

impl PublicAddress {
    /// Parses `http://` or `https://` followed by a host and an optional
    /// port. A path, query, fragment or user name is refused.
    pub fn parse(url: &str) -> Result<Self, String> {
        let bad = |why: &str| format!("public URL {url}: {why}");
        let lower = url.trim().to_ascii_lowercase();
        let (https, rest) = if let Some(rest) = lower.strip_prefix("https://") {
            (true, rest)
        } else if let Some(rest) = lower.strip_prefix("http://") {
            (false, rest)
        } else {
            return Err(bad("must start with http:// or https://"));
        };
        let authority = rest.strip_suffix('/').unwrap_or(rest);
        if authority.is_empty() || authority.contains(['/', '?', '#', '@']) {
            return Err(bad(
                "must be a scheme, a host and an optional port, nothing else",
            ));
        }
        let (host, port) = if let Some(v6) = authority.strip_prefix('[') {
            let (inside, after) = v6.split_once(']').ok_or_else(|| bad("unclosed ["))?;
            (format!("[{inside}]"), after.strip_prefix(':'))
        } else {
            match authority.split_once(':') {
                Some((h, p)) => (h.to_owned(), Some(p)),
                None => (authority.to_owned(), None),
            }
        };
        let port = match port {
            None => None,
            Some(p) => Some(
                p.parse::<u16>()
                    .map_err(|_| bad("the port is not a number"))?,
            ),
        };
        let default = if https { 443 } else { 80 };
        let origin = match port {
            Some(p) if p != default => {
                format!("{}://{host}:{p}", if https { "https" } else { "http" })
            }
            _ => format!("{}://{host}", if https { "https" } else { "http" }),
        };
        Ok(Self {
            origin,
            https,
            loopback: is_loopback_host(&host),
        })
    }

    /// Decides the public address from the listen address and the setting.
    ///
    /// Without `--public-url`, a loopback listener is its own public address
    /// over plain http, and any other listener is refused, because the server
    /// cannot guess the name people will use. A plain-http public address
    /// that is not loopback is refused unless `allow_insecure_http` is set.
    pub fn resolve(
        listen: SocketAddr,
        public_url: Option<&str>,
        allow_insecure_http: bool,
    ) -> Result<Self, String> {
        let public = match public_url {
            Some(url) => Self::parse(url)?,
            None if listen.ip().is_loopback() => Self::parse(&format!("http://{listen}"))?,
            None => {
                return Err(format!(
                    "the server listens on {listen}, which other machines can reach; \
                     set --public-url to the https address people use"
                ));
            }
        };
        if !public.https && !public.loopback && !allow_insecure_http {
            return Err(format!(
                "the public URL {} is plain http on a network address, so passwords and \
                 session cookies would cross the network unencrypted; use an https URL, or \
                 pass --allow-insecure-http to accept that",
                public.origin
            ));
        }
        Ok(public)
    }

    #[must_use]
    pub fn origin(&self) -> &str {
        &self.origin
    }

    #[must_use]
    pub fn is_https(&self) -> bool {
        self.https
    }

    #[must_use]
    pub fn is_loopback(&self) -> bool {
        self.loopback
    }

    /// Whether a request comes from a page of this server: its `Origin`
    /// header names exactly the public address. A request without one is
    /// refused too; browsers send it on every request that is not a plain
    /// GET and on every WebSocket handshake.
    #[must_use]
    pub fn same_origin(&self, headers: &axum::http::HeaderMap) -> bool {
        headers
            .get(axum::http::header::ORIGIN)
            .and_then(|o| o.to_str().ok())
            .is_some_and(|o| o.eq_ignore_ascii_case(&self.origin))
    }

    /// `__Host-nisaba_session` behind https, `nisaba_session` otherwise. The
    /// `__Host-` prefix makes the browser refuse the cookie unless it is
    /// `Secure`, has `Path=/` and names no domain.
    #[must_use]
    pub fn cookie_name(&self) -> &'static str {
        if self.https {
            "__Host-nisaba_session"
        } else {
            "nisaba_session"
        }
    }

    /// A `Set-Cookie` value for the session cookie.
    #[must_use]
    pub fn session_cookie(&self, value: &str, max_age: i64) -> String {
        let secure = if self.https { "; Secure" } else { "" };
        format!(
            "{}={value}; HttpOnly; SameSite=Lax; Path=/; Max-Age={max_age}{secure}",
            self.cookie_name()
        )
    }
}

/// `localhost`, a name under `.localhost`, `127.0.0.0/8` or `::1`.
fn is_loopback_host(host: &str) -> bool {
    let bare = host.trim_start_matches('[').trim_end_matches(']');
    host == "localhost"
        || host.ends_with(".localhost")
        || bare.parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn addr(s: &str) -> SocketAddr {
        s.parse().unwrap()
    }

    #[test]
    fn origins_are_written_as_browsers_write_them() {
        let p = PublicAddress::parse("HTTPS://Nisaba.Example.org:443/").unwrap();
        assert_eq!(p.origin(), "https://nisaba.example.org");
        assert!(p.is_https() && !p.is_loopback());
        let p = PublicAddress::parse("http://[::1]:8080").unwrap();
        assert_eq!(p.origin(), "http://[::1]:8080");
        assert!(p.is_loopback());
        for bad in [
            "ftp://x",
            "https://x/path",
            "https://u@x",
            "https://x:port",
            "https://",
        ] {
            assert!(PublicAddress::parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn plain_http_is_for_loopback_only() {
        let lan = addr("192.168.1.5:8080");
        let local = addr("127.0.0.1:8080");
        assert_eq!(
            PublicAddress::resolve(local, None, false).unwrap().origin(),
            "http://127.0.0.1:8080"
        );
        assert!(PublicAddress::resolve(lan, None, false).is_err());
        assert!(PublicAddress::resolve(lan, Some("http://192.168.1.5:8080"), false).is_err());
        assert!(PublicAddress::resolve(lan, Some("http://192.168.1.5:8080"), true).is_ok());
        assert!(PublicAddress::resolve(lan, Some("https://nisaba.example.org"), false).is_ok());
        assert!(PublicAddress::resolve(local, Some("http://localhost:8080"), false).is_ok());
    }

    #[test]
    fn the_cookie_is_secure_behind_https() {
        let https = PublicAddress::parse("https://nisaba.example.org").unwrap();
        let c = https.session_cookie("t", 60);
        assert!(c.starts_with("__Host-nisaba_session=t;"));
        assert!(c.contains("; Secure") && c.contains("HttpOnly") && c.contains("SameSite=Lax"));
        let http = PublicAddress::parse("http://127.0.0.1:1").unwrap();
        let c = http.session_cookie("t", 60);
        assert!(c.starts_with("nisaba_session=t;") && !c.contains("Secure"));
    }

    fn load(args: &[&str]) -> Result<Settings, String> {
        let args: Vec<String> = args.iter().map(|s| (*s).to_owned()).collect();
        Settings::load(&args, |_| None).map(|(s, _)| s)
    }

    #[test]
    fn arguments_parse() {
        let s = load(&[
            "--export-seconds",
            "5",
            "--export-queue",
            "0",
            "--allow-insecure-http",
            "--allow-unsandboxed-export",
            "--sync-transfer-mb",
            "64",
            "--no-package-downloads",
        ])
        .unwrap();
        assert_eq!(s.sync_transfer_mb, 64);
        assert_eq!(Settings::default().sync_transfer_mb, 256);
        assert!(load(&["--sync-transfer-mb", "31"]).is_err());
        assert_eq!(s.export.seconds, 5);
        assert_eq!(s.export.queue, 0);
        assert!(s.allow_insecure_http);
        assert!(s.export.allow_unsandboxed);
        assert!(!s.packages.download);
        assert!(!Settings::default().export.allow_unsandboxed);
        assert!(load(&["--export-parallel", "0"]).is_err());
        assert!(load(&["--export-memory-mb", "63"]).is_err());
        assert!(load(&["--password-memory-kib", "1024"]).is_err());
        assert!(load(&["--listen", "nowhere"]).is_err());
        assert!(load(&["--nope"]).is_err());
        assert!(load(&["--data"]).is_err());
    }

    /// A file, then the environment, then the command line: each later one
    /// wins, and a typo in the file is an error.
    #[test]
    fn file_environment_and_flags_stack() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("nisaba.toml");
        std::fs::write(
            &file,
            "data = \"/srv/tide\"\nlog_format = \"json\"\n[export]\nseconds = 9\nparallel = 3\n\
             [packages]\ndownload = false\n",
        )
        .unwrap();
        let path = file.to_str().unwrap().to_owned();
        let env = |name: &str| match name {
            "NISABA_CONFIG" => Some(path.clone()),
            "NISABA_EXPORT_SECONDS" => Some("12".into()),
            "NISABA_LISTEN" => Some("127.0.0.1:9000".into()),
            _ => None,
        };
        let args = [
            "--export-parallel".to_owned(),
            "4".to_owned(),
            "extra".to_owned(),
        ];
        let (s, rest) = Settings::load(&args, env).unwrap();
        assert_eq!(rest, ["extra"]);
        assert_eq!(s.data, PathBuf::from("/srv/tide"));
        assert_eq!(s.log_format, LogFormat::Json);
        assert_eq!(s.export.seconds, 12, "the environment beats the file");
        assert_eq!(s.export.parallel, 4, "a flag beats the file");
        assert_eq!(s.listen, "127.0.0.1:9000");
        assert!(!s.packages.download);
        std::fs::write(&file, "[export]\nsecond = 9\n").unwrap();
        let err = Settings::load(&[], env).unwrap_err();
        assert!(err.contains("unknown setting export.second"), "{err}");
        let env = |name: &str| (name == "NISABA_EXPORT_SECONDS").then(|| "0".to_owned());
        assert!(Settings::load(&[], env).is_err());
    }
}
