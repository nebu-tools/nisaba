//! Server settings: the command line, the public address and the export
//! limits.
//!
//! The public address is the one setting from which the server knows its own
//! name. The session cookie, the origin check on every state-changing request
//! and on the sync socket, and the first-run setup code all follow from it.

use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;

pub const USAGE: &str =
    "usage: nisaba [--data DIR] [--listen ADDR] [--public-url URL] [--allow-insecure-http]
              [--export-memory-mb N] [--export-seconds N] [--export-parallel N] [--export-queue N]
              [--allow-unsandboxed-export] [--sync-transfer-mb N]";

/// Everything the command line sets.
#[derive(Clone, Debug)]
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
    pub export: ExportLimits,
    /// Memory for unfinished multi-part updates across all sync sockets, in
    /// MiB. A Begin beyond it is told to try again later.
    pub sync_transfer_mb: u64,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            data: PathBuf::from("nisaba-data"),
            listen: "127.0.0.1:8080".into(),
            public_url: None,
            allow_insecure_http: false,
            export: ExportLimits::default(),
            sync_transfer_mb: 256,
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

impl Settings {
    /// Parses the arguments after the program name.
    pub fn parse(args: impl IntoIterator<Item = String>) -> Result<Self, String> {
        let mut s = Self::default();
        let mut args = args.into_iter();
        while let Some(arg) = args.next() {
            let mut value = || args.next().ok_or_else(|| format!("{arg} needs a value"));
            match arg.as_str() {
                "--data" => s.data = PathBuf::from(value()?),
                "--listen" => s.listen = value()?,
                "--public-url" => s.public_url = Some(value()?),
                "--allow-insecure-http" => s.allow_insecure_http = true,
                "--allow-unsandboxed-export" => s.export.allow_unsandboxed = true,
                "--export-memory-mb" => s.export.memory_mb = number(&arg, &value()?, 64)?,
                "--export-seconds" => s.export.seconds = number(&arg, &value()?, 1)?,
                // One update may be 32 MiB, so less could never take one.
                "--sync-transfer-mb" => s.sync_transfer_mb = number(&arg, &value()?, 32)?,
                "--export-parallel" => {
                    s.export.parallel =
                        usize::try_from(number(&arg, &value()?, 1)?).map_err(|e| e.to_string())?;
                }
                "--export-queue" => {
                    s.export.queue =
                        usize::try_from(number(&arg, &value()?, 0)?).map_err(|e| e.to_string())?;
                }
                other => return Err(format!("unknown argument {other}")),
            }
        }
        Ok(s)
    }
}

fn number(name: &str, value: &str, min: u64) -> Result<u64, String> {
    match value.parse::<u64>() {
        Ok(n) if n >= min => Ok(n),
        _ => Err(format!("{name} needs a whole number of at least {min}")),
    }
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

    #[test]
    fn arguments_parse() {
        let s = Settings::parse(
            [
                "--export-seconds",
                "5",
                "--export-queue",
                "0",
                "--allow-insecure-http",
                "--allow-unsandboxed-export",
                "--sync-transfer-mb",
                "64",
            ]
            .map(String::from),
        )
        .unwrap();
        assert_eq!(s.sync_transfer_mb, 64);
        assert_eq!(Settings::default().sync_transfer_mb, 256);
        assert!(Settings::parse(["--sync-transfer-mb", "31"].map(String::from)).is_err());
        assert_eq!(s.export.seconds, 5);
        assert_eq!(s.export.queue, 0);
        assert!(s.allow_insecure_http);
        assert!(s.export.allow_unsandboxed);
        assert!(!Settings::default().export.allow_unsandboxed);
        assert!(Settings::parse(["--export-parallel", "0"].map(String::from)).is_err());
        assert!(Settings::parse(["--nope"].map(String::from)).is_err());
    }
}
