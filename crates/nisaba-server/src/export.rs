//! PDF export in a child process.
//!
//! A document is a program, and a program can ask for a terabyte of memory or
//! loop forever. The server therefore never compiles an export itself. It
//! starts its own binary again with the hidden subcommand `__export`, writes
//! the project to the child's standard input and reads the PDF from its
//! standard output.
//!
//! On Linux the child limits its address space (`RLIMIT_AS`) and its CPU time
//! (`RLIMIT_CPU`), reads every font into memory, and then gives up all file
//! system access and TCP with Landlock, best effort on the running kernel. On
//! every system the server kills the child at the deadline, runs at most
//! `parallel` children at once, and lets at most `queue` more requests wait.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use nisaba_compile_core::FileContents;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

use crate::config::ExportLimits;
use crate::web::Date;

/// The hidden subcommand that runs one export.
pub const SUBCOMMAND: &str = "__export";

/// Exit status of a child whose document has errors; they are on stderr.
const EXIT_COMPILE: i32 = 2;
/// Exit status of a child that could not set up its limits or read its input.
const EXIT_SETUP: i32 = 3;

const MAGIC: &[u8; 8] = b"nisaba1\n";
/// Error output kept from a child.
const MAX_STDERR: u64 = 64 * 1024;

/// Why an export produced no PDF.
#[derive(Debug, PartialEq, Eq)]
pub enum ExportError {
    /// Every slot and every queue place is taken.
    Busy,
    /// The document has errors.
    Compile(String),
    OutOfMemory {
        limit_mb: u64,
    },
    OutOfTime {
        seconds: u64,
    },
    /// Anything else, such as a child that could not start.
    Failed(String),
}

impl std::fmt::Display for ExportError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Busy => write!(
                f,
                "the server is busy with other exports; try again in a minute"
            ),
            Self::Compile(message) => write!(f, "{message}"),
            Self::OutOfMemory { limit_mb } => write!(
                f,
                "the export ran out of memory: the document needs more than {limit_mb} MiB"
            ),
            Self::OutOfTime { seconds } => write!(
                f,
                "the export ran out of time: the document took longer than {seconds} seconds"
            ),
            Self::Failed(message) => write!(f, "the export failed: {message}"),
        }
    }
}

/// Runs exports in child processes within the limits.
#[derive(Clone)]
pub struct Exporter {
    exe: Arc<PathBuf>,
    limits: ExportLimits,
    slots: Arc<tokio::sync::Semaphore>,
    waiting: Arc<AtomicUsize>,
}

impl Exporter {
    /// `exe` is the server binary, which the children run.
    #[must_use]
    pub fn new(exe: PathBuf, limits: ExportLimits) -> Self {
        Self {
            exe: Arc::new(exe),
            slots: Arc::new(tokio::sync::Semaphore::new(limits.parallel)),
            waiting: Arc::default(),
            limits,
        }
    }

    /// The binary of this process, even after it was replaced on disk.
    #[must_use]
    pub fn this_binary() -> PathBuf {
        if cfg!(target_os = "linux") {
            PathBuf::from("/proc/self/exe")
        } else {
            std::env::current_exe().unwrap_or_else(|_| PathBuf::from("nisaba"))
        }
    }

    /// Compiles `files` to a PDF dated `today` in a child process.
    pub async fn pdf(
        &self,
        files: Vec<(String, FileContents)>,
        today: Date,
    ) -> Result<Vec<u8>, ExportError> {
        let _slot = if let Ok(slot) = Arc::clone(&self.slots).try_acquire_owned() {
            slot
        } else {
            let place = self.waiting.fetch_add(1, Ordering::SeqCst);
            let waited = if place < self.limits.queue {
                Arc::clone(&self.slots).acquire_owned().await.ok()
            } else {
                None
            };
            self.waiting.fetch_sub(1, Ordering::SeqCst);
            waited.ok_or(ExportError::Busy)?
        };
        self.run_child(&encode(today, &files)).await
    }

    async fn run_child(&self, input: &[u8]) -> Result<Vec<u8>, ExportError> {
        let failed = |e: std::io::Error| ExportError::Failed(e.to_string());
        let mut child = tokio::process::Command::new(&*self.exe)
            .arg(SUBCOMMAND)
            .arg(self.limits.memory_mb.to_string())
            .arg(self.limits.seconds.to_string())
            .env_clear()
            // Fewer malloc arenas and threads keep the address space small.
            .env("MALLOC_ARENA_MAX", "2")
            .env("RAYON_NUM_THREADS", "2")
            .current_dir(std::env::temp_dir())
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(failed)?;
        let mut stdin = child.stdin.take().expect("piped stdin");
        let mut stdout = child.stdout.take().expect("piped stdout");
        let mut stderr = child.stderr.take().expect("piped stderr");
        let max_pdf = self.limits.memory_mb.saturating_mul(1 << 20);
        let work = async {
            let write = async {
                // A child that dies early closes the pipe; its exit says why.
                let _ = stdin.write_all(input).await;
                drop(stdin);
            };
            let mut pdf = Vec::new();
            let mut errors = Vec::new();
            let mut pdf_pipe = (&mut stdout).take(max_pdf);
            let mut error_pipe = (&mut stderr).take(MAX_STDERR);
            let read_pdf = pdf_pipe.read_to_end(&mut pdf);
            let read_errors = error_pipe.read_to_end(&mut errors);
            let ((), read_pdf, read_errors) = tokio::join!(write, read_pdf, read_errors);
            read_pdf?;
            read_errors?;
            let status = child.wait().await?;
            Ok::<_, std::io::Error>((status, pdf, errors))
        };
        let deadline = Duration::from_secs(self.limits.seconds);
        let Ok(finished) = tokio::time::timeout(deadline, work).await else {
            // Dropping the child kills it.
            return Err(ExportError::OutOfTime {
                seconds: self.limits.seconds,
            });
        };
        let (status, pdf, errors) = finished.map_err(failed)?;
        let errors = String::from_utf8_lossy(&errors).trim().to_owned();
        if status.success() {
            return Ok(pdf);
        }
        Err(self.explain(status, errors))
    }

    fn explain(&self, status: std::process::ExitStatus, errors: String) -> ExportError {
        let out_of_memory = ExportError::OutOfMemory {
            limit_mb: self.limits.memory_mb,
        };
        let out_of_time = ExportError::OutOfTime {
            seconds: self.limits.seconds,
        };
        if status.code() == Some(EXIT_COMPILE) {
            return ExportError::Compile(errors);
        }
        // Rust aborts when an allocation fails.
        if errors.contains("memory allocation of") || errors.contains("capacity overflow") {
            return out_of_memory;
        }
        #[cfg(unix)]
        {
            use std::os::unix::process::ExitStatusExt as _;
            match status.signal() {
                // SIGXCPU: the CPU-time limit.
                Some(24) => return out_of_time,
                // SIGABRT without a message, or SIGKILL from the kernel's
                // out-of-memory killer.
                Some(6 | 9) => return out_of_memory,
                _ => {}
            }
        }
        let last = errors.lines().last().unwrap_or("").to_owned();
        ExportError::Failed(format!("{status}: {last}"))
    }
}

/// The child: `nisaba __export MEMORY_MB SECONDS`. Reads one request from
/// stdin, writes the PDF to stdout and returns the exit status.
#[must_use]
pub fn child_main(args: &[String]) -> i32 {
    use std::io::{Read as _, Write as _};
    let (Some(memory_mb), Some(seconds)) = (
        args.first().and_then(|a| a.parse::<u64>().ok()),
        args.get(1).and_then(|a| a.parse::<u64>().ok()),
    ) else {
        eprintln!("usage: nisaba {SUBCOMMAND} MEMORY_MB SECONDS");
        return EXIT_SETUP;
    };
    if let Err(e) = limit(memory_mb, seconds) {
        eprintln!("cannot set the export limits: {e}");
        return EXIT_SETUP;
    }
    let mut input = Vec::new();
    if let Err(e) = std::io::stdin().read_to_end(&mut input) {
        eprintln!("cannot read the request: {e}");
        return EXIT_SETUP;
    }
    let fonts = match crate::web::fonts() {
        Ok(fonts) => fonts,
        Err(e) => {
            eprintln!("{e}");
            return EXIT_SETUP;
        }
    };
    if let Err(e) = sandbox() {
        eprintln!("cannot restrict the export: {e}");
        return EXIT_SETUP;
    }
    let Some((today, files)) = decode(&input) else {
        eprintln!("the request is malformed");
        return EXIT_SETUP;
    };
    drop(input);
    match crate::web::compile_pdf(fonts, files, today) {
        Ok(pdf) => {
            let mut out = std::io::stdout().lock();
            if out.write_all(&pdf).and_then(|()| out.flush()).is_err() {
                return EXIT_SETUP;
            }
            0
        }
        Err(message) => {
            eprintln!("{message}");
            EXIT_COMPILE
        }
    }
}

/// Caps the address space at `memory_mb` MiB and CPU time at `seconds` plus
/// one, so a child the parent fails to kill still stops.
#[cfg(target_os = "linux")]
fn limit(memory_mb: u64, seconds: u64) -> Result<(), String> {
    use rustix::process::{Resource, Rlimit, setrlimit};
    let set = |resource, value: u64| {
        setrlimit(
            resource,
            Rlimit {
                current: Some(value),
                maximum: Some(value),
            },
        )
        .map_err(|e| e.to_string())
    };
    set(Resource::As, memory_mb.saturating_mul(1 << 20))?;
    set(Resource::Cpu, seconds.saturating_add(1))?;
    // A crash writes no core file.
    set(Resource::Core, 0)
}

#[cfg(not(target_os = "linux"))]
fn limit(_memory_mb: u64, _seconds: u64) -> Result<(), String> {
    Ok(())
}

/// Takes away every file system right and TCP bind and connect from this
/// thread and every thread it starts later, and stops it from signalling
/// other processes or reaching abstract Unix sockets. Landlock applies what
/// the running kernel supports and leaves the rest. Returns whether the
/// kernel enforces any of it.
#[cfg(target_os = "linux")]
pub fn sandbox() -> Result<bool, String> {
    use landlock::{ABI, Access as _, AccessFs, AccessNet, Ruleset, RulesetAttr as _, Scope};
    let abi = ABI::V6;
    Ruleset::default()
        .handle_access(AccessFs::from_all(abi))
        .and_then(|r| r.handle_access(AccessNet::from_all(abi)))
        .and_then(|r| r.scope(Scope::from_all(abi)))
        .and_then(Ruleset::create)
        .and_then(landlock::RulesetCreated::restrict_self)
        .map(|status| status.ruleset != landlock::RulesetStatus::NotEnforced)
        .map_err(|e| e.to_string())
}

#[cfg(not(target_os = "linux"))]
pub fn sandbox() -> Result<bool, String> {
    Ok(false)
}

/// The request: magic, the date, then each file as a kind byte (0 text,
/// 1 bytes), its path and its contents, each length a little-endian `u64`.
fn encode(today: Date, files: &[(String, FileContents)]) -> Vec<u8> {
    let mut out = MAGIC.to_vec();
    out.extend_from_slice(&today.0.to_le_bytes());
    out.extend_from_slice(&[today.1, today.2]);
    for (path, contents) in files {
        let (kind, data) = match contents {
            FileContents::Text(text) => (0, text.as_bytes()),
            FileContents::Bytes(bytes) => (1, bytes.as_slice()),
        };
        out.push(kind);
        for part in [path.as_bytes(), data] {
            out.extend_from_slice(&(part.len() as u64).to_le_bytes());
            out.extend_from_slice(part);
        }
    }
    out
}

fn decode(mut input: &[u8]) -> Option<(Date, Vec<(String, FileContents)>)> {
    fn take<'a>(input: &mut &'a [u8], n: usize) -> Option<&'a [u8]> {
        let (head, tail) = input.split_at_checked(n)?;
        *input = tail;
        Some(head)
    }
    fn part<'a>(input: &mut &'a [u8]) -> Option<&'a [u8]> {
        let len = u64::from_le_bytes(take(input, 8)?.try_into().ok()?);
        take(input, usize::try_from(len).ok()?)
    }
    if take(&mut input, MAGIC.len())? != MAGIC {
        return None;
    }
    let year = i32::from_le_bytes(take(&mut input, 4)?.try_into().ok()?);
    let md = take(&mut input, 2)?;
    let mut files = Vec::new();
    while !input.is_empty() {
        let kind = take(&mut input, 1)?[0];
        let path = String::from_utf8(part(&mut input)?.to_vec()).ok()?;
        let data = part(&mut input)?.to_vec();
        let contents = match kind {
            0 => FileContents::Text(String::from_utf8(data).ok()?),
            1 => FileContents::Bytes(data),
            _ => return None,
        };
        files.push((path, contents));
    }
    Some(((year, md[0], md[1]), files))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_round_trip() {
        let files = vec![
            ("main.typ".to_owned(), FileContents::Text("= Hi".into())),
            ("a.png".to_owned(), FileContents::Bytes(vec![0, 1, 2])),
        ];
        let bytes = encode((2026, 11, 18), &files);
        let (date, back) = decode(&bytes).unwrap();
        assert_eq!(date, (2026, 11, 18));
        assert_eq!(back.len(), 2);
        assert!(matches!(&back[0].1, FileContents::Text(t) if t == "= Hi"));
        assert!(matches!(&back[1].1, FileContents::Bytes(b) if b == &[0, 1, 2]));
        assert!(decode(&bytes[..bytes.len() - 1]).is_none());
        assert!(decode(b"other").is_none());
    }

    /// The restriction applies to the thread that asks for it, so a thread
    /// of the test process can try it without affecting the other tests.
    #[cfg(target_os = "linux")]
    #[test]
    fn the_sandbox_takes_away_files_and_network() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let manifest = concat!(env!("CARGO_MANIFEST_DIR"), "/Cargo.toml");
        assert!(std::fs::File::open(manifest).is_ok());
        std::thread::spawn(move || {
            if !sandbox().unwrap() {
                eprintln!("this kernel has no Landlock; nothing to test");
                return;
            }
            assert!(std::fs::File::open(manifest).is_err(), "a file opened");
            assert!(std::fs::read_dir("/").is_err(), "a directory was listed");
            assert!(
                std::net::TcpStream::connect(addr).is_err(),
                "a TCP connection opened"
            );
        })
        .join()
        .unwrap();
    }
}
