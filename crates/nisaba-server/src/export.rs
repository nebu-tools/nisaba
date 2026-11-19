//! PDF export in a child process.
//!
//! A document is a program, and a program can ask for a terabyte of memory or
//! loop forever. The server therefore never compiles an export itself. It
//! starts its own binary again with the hidden subcommand `__export`, writes
//! the project to the child's standard input and reads the PDF from its
//! standard output.
//!
//! On Linux the child limits its address space (`RLIMIT_AS`) and its CPU time
//! (`RLIMIT_CPU`), asks to be killed when the server dies, reads its input
//! and every font into memory, and then confines itself
//! ([`crate::sandbox`]). On every system the server kills the child at the
//! deadline, runs at most `parallel` children at once, and lets at most
//! `queue` more requests wait.
//!
//! The sandbox fails closed. The server decides at start whether this kernel
//! keeps every promise of the sandbox ([`sandbox::probe`]). If it does not,
//! every export is refused with [`ExportError::Unsandboxed`] unless the
//! server was started with `--allow-unsandboxed-export`. The child confines
//! itself and stops when any part of the sandbox it was told to require
//! cannot be enforced.
//!
//! A child never outlives its request. It is killed and reaped when the
//! deadline passes, when the request is dropped because the client went
//! away, and when the server stops ([`Exporter::stop`]). If the server itself
//! is killed, the kernel kills the child (`PR_SET_PDEATHSIG`).

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use nisaba_compile_core::{FileContents, FontEntry, LiveCompiler, PdfKind};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

use crate::api::{ExportMode, Problem};
use crate::config::ExportLimits;
use crate::dates::Date;
use crate::sandbox::{self, Sandbox};

/// Everything one export compiles: the project's files, the packages they
/// import, the fonts the administrator added, the date and the PDF kind.
/// Font files among the project's files are added as fonts too.
pub struct ExportRequest {
    pub today: Date,
    pub files: Vec<(String, FileContents)>,
    /// Each package as `@namespace/name:version` and its files.
    pub packages: Vec<crate::packages::Unpacked>,
    pub fonts: Vec<Arc<Vec<u8>>>,
    /// PDF/A-2b rather than a plain PDF.
    pub archival: bool,
}

/// The hidden subcommand that runs one export.
pub const SUBCOMMAND: &str = "__export";

/// Exit status of a child whose document has errors; they are on stderr.
const EXIT_COMPILE: i32 = 2;
/// Exit status of a child that could not set up its limits or read its input.
const EXIT_SETUP: i32 = 3;
/// Exit status of a child that was told to require the sandbox and could not
/// enforce it.
const EXIT_UNSANDBOXED: i32 = 4;

const MAGIC: &[u8; 8] = b"nisaba2\n";
/// Error output kept from a child.
const MAX_STDERR: u64 = 64 * 1024;

/// Why an export produced no PDF.
#[derive(Debug, PartialEq, Eq)]
pub enum ExportError {
    /// Every slot and every queue place is taken.
    Busy,
    /// The document has errors.
    Compile(Vec<Problem>),
    OutOfMemory {
        limit_mb: u64,
    },
    OutOfTime {
        seconds: u64,
    },
    /// This system cannot sandbox an export, and the server was not started
    /// with `--allow-unsandboxed-export`.
    Unsandboxed(String),
    /// The server is stopping.
    Stopping,
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
            Self::Compile(problems) => {
                let errors: Vec<_> = problems.iter().filter(|p| p.severity == "error").collect();
                match errors.as_slice() {
                    [] => write!(f, "the document has errors"),
                    [one] => write!(f, "the document has an error: {}", one.message),
                    [first, rest @ ..] => write!(
                        f,
                        "the document has {} errors; the first: {}",
                        rest.len() + 1,
                        first.message
                    ),
                }
            }
            Self::OutOfMemory { limit_mb } => write!(
                f,
                "the export ran out of memory: the document needs more than {limit_mb} MiB"
            ),
            Self::OutOfTime { seconds } => write!(
                f,
                "the export ran out of time: the document took longer than {seconds} seconds"
            ),
            Self::Unsandboxed(why) => write!(
                f,
                "exports are turned off on this server: it cannot sandbox them ({why}). \
                 The server admin can start it with --allow-unsandboxed-export to allow them"
            ),
            Self::Stopping => write!(f, "the server is stopping; try again in a minute"),
            Self::Failed(message) => write!(f, "the export failed: {message}"),
        }
    }
}

/// Runs exports in child processes within the limits.
#[derive(Clone)]
pub struct Exporter {
    exe: Arc<PathBuf>,
    limits: ExportLimits,
    sandbox: Sandbox,
    /// A permit for each slot and each queue place. A request holds one
    /// from the moment it is admitted until it ends, however it ends.
    admitted: Arc<tokio::sync::Semaphore>,
    slots: Arc<tokio::sync::Semaphore>,
    stopping: Arc<tokio::sync::watch::Sender<bool>>,
}

impl Exporter {
    /// `exe` is the server binary, which the children run. Decides on the
    /// sandbox with [`sandbox::probe`].
    #[must_use]
    pub fn new(exe: PathBuf, limits: ExportLimits) -> Self {
        Self::with_sandbox(exe, limits, sandbox::probe())
    }

    /// As [`Exporter::new`], with the sandbox as given. For tests of a
    /// system without one.
    #[must_use]
    pub fn with_sandbox(exe: PathBuf, limits: ExportLimits, sandbox: Sandbox) -> Self {
        Self {
            exe: Arc::new(exe),
            admitted: Arc::new(tokio::sync::Semaphore::new(
                limits.parallel.saturating_add(limits.queue),
            )),
            slots: Arc::new(tokio::sync::Semaphore::new(limits.parallel)),
            stopping: Arc::new(tokio::sync::watch::Sender::new(false)),
            sandbox,
            limits,
        }
    }

    /// Whether exports run, and how.
    #[must_use]
    pub fn mode(&self) -> ExportMode {
        match (&self.sandbox, self.limits.allow_unsandboxed) {
            (Sandbox::On, _) => ExportMode::Sandboxed,
            (Sandbox::Off(_), true) => ExportMode::Unsandboxed,
            (Sandbox::Off(_), false) => ExportMode::Off,
        }
    }

    /// The line the server logs at start.
    #[must_use]
    pub fn describe(&self) -> String {
        match (&self.sandbox, self.limits.allow_unsandboxed) {
            (Sandbox::On, _) => "exports are sandboxed: Landlock and seccomp hold every \
                 promise on this kernel"
                .into(),
            (Sandbox::Off(why), false) => format!(
                "exports are turned off: this system cannot sandbox them ({why}); \
                 start with --allow-unsandboxed-export to allow them"
            ),
            (Sandbox::Off(why), true) => format!(
                "exports are NOT sandboxed ({why}); allowed by --allow-unsandboxed-export; \
                 the memory, CPU and time limits still hold"
            ),
        }
    }

    /// Kills every running child and refuses new exports. For shutdown.
    pub fn stop(&self) {
        self.stopping.send_replace(true);
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

    /// Compiles a request to a PDF in a child process.
    pub async fn pdf(&self, request: ExportRequest) -> Result<Vec<u8>, ExportError> {
        if let Sandbox::Off(why) = &self.sandbox
            && !self.limits.allow_unsandboxed
        {
            return Err(ExportError::Unsandboxed(why.clone()));
        }
        let mut stopping = self.stopping.subscribe();
        if *stopping.borrow_and_update() {
            return Err(ExportError::Stopping);
        }
        tokio::select! {
            result = self.queue_and_run(&request) => result,
            _ = stopping.changed() => Err(ExportError::Stopping),
        }
    }

    async fn queue_and_run(&self, request: &ExportRequest) -> Result<Vec<u8>, ExportError> {
        // Both permits go back when this future ends or is dropped.
        let _admitted = Arc::clone(&self.admitted)
            .try_acquire_owned()
            .map_err(|_| ExportError::Busy)?;
        let _slot = Arc::clone(&self.slots)
            .acquire_owned()
            .await
            .map_err(|_| ExportError::Stopping)?;
        self.run_child(&encode(request)).await
    }

    async fn run_child(&self, input: &[u8]) -> Result<Vec<u8>, ExportError> {
        let failed = |e: std::io::Error| ExportError::Failed(e.to_string());
        let sandbox = if self.sandbox == Sandbox::On {
            "required"
        } else {
            "optional"
        };
        // The child refuses to run with a descriptor it does not need.
        crate::sandbox::keep_descriptors();
        let child = tokio::process::Command::new(&*self.exe)
            .arg(SUBCOMMAND)
            .arg(self.limits.memory_mb.to_string())
            .arg(self.limits.seconds.to_string())
            .arg(sandbox)
            .arg(std::process::id().to_string())
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
        let mut child = Reaped(Some(child));
        let child = child.0.as_mut().expect("just spawned");
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
            // Kills and reaps it; so does dropping it if this future is
            // dropped first.
            let _ = child.kill().await;
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
            let problems = serde_json::from_str(&errors).unwrap_or_else(|_| {
                vec![Problem {
                    severity: "error".into(),
                    message: errors,
                    path: None,
                    start: None,
                    end: None,
                }]
            });
            return ExportError::Compile(problems);
        }
        if status.code() == Some(EXIT_UNSANDBOXED) {
            return ExportError::Unsandboxed(errors);
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

/// Kills and reaps the child when dropped before it was waited for.
struct Reaped(Option<tokio::process::Child>);

impl Drop for Reaped {
    fn drop(&mut self) {
        let Some(mut child) = self.0.take() else {
            return;
        };
        if matches!(child.try_wait(), Ok(Some(_))) {
            return;
        }
        let _ = child.start_kill();
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                let _ = child.wait().await;
            });
        }
    }
}

/// The child: `nisaba __export MEMORY_MB SECONDS required|optional PARENT`.
/// Reads one request from stdin, writes the PDF to stdout and returns the
/// exit status. With `required` it confines itself first and stops when any
/// part of the sandbox cannot be enforced. With `optional` it does not
/// confine itself.
#[must_use]
pub fn child_main(args: &[String]) -> i32 {
    use std::io::{Read as _, Write as _};
    let (Some(memory_mb), Some(seconds), Some(required), Some(parent)) = (
        args.first().and_then(|a| a.parse::<u64>().ok()),
        args.get(1).and_then(|a| a.parse::<u64>().ok()),
        args.get(2).and_then(|a| match a.as_str() {
            "required" => Some(true),
            "optional" => Some(false),
            _ => None,
        }),
        args.get(3).and_then(|a| a.parse::<u32>().ok()),
    ) else {
        eprintln!("usage: nisaba {SUBCOMMAND} MEMORY_MB SECONDS required|optional PARENT_PID");
        return EXIT_SETUP;
    };
    if let Err(e) = die_with(parent) {
        eprintln!("cannot tie the export to the server: {e}");
        return EXIT_SETUP;
    }
    if let Err(e) = limit(memory_mb, seconds) {
        eprintln!("cannot set the export limits: {e}");
        return EXIT_SETUP;
    }
    let mut input = Vec::new();
    if let Err(e) = std::io::stdin().read_to_end(&mut input) {
        eprintln!("cannot read the request: {e}");
        return EXIT_SETUP;
    }
    let fonts = match embedded_fonts() {
        Ok(fonts) => fonts,
        Err(e) => {
            eprintln!("{e}");
            return EXIT_SETUP;
        }
    };
    if required && let Err(why) = sandbox::alone().and_then(|()| sandbox::confine()) {
        eprintln!("{why}");
        return EXIT_UNSANDBOXED;
    }
    let Some(request) = decode(&input) else {
        eprintln!("the request is malformed");
        return EXIT_SETUP;
    };
    drop(input);
    match compile(fonts, request) {
        Ok(pdf) => {
            let mut out = std::io::stdout().lock();
            if out.write_all(&pdf).and_then(|()| out.flush()).is_err() {
                return EXIT_SETUP;
            }
            0
        }
        Err(problems) => {
            eprintln!(
                "{}",
                serde_json::to_string(&problems).unwrap_or_else(|_| "[]".into())
            );
            EXIT_COMPILE
        }
    }
}

/// The fonts embedded with the web client: the catalogue and each face's
/// bytes.
pub fn embedded_fonts() -> Result<(Vec<FontEntry>, Vec<Vec<u8>>), String> {
    let catalogue = crate::web::embedded_fonts();
    let data = catalogue
        .iter()
        .map(|entry| {
            crate::web::embedded_font(&entry.file)
                .map(std::borrow::Cow::into_owned)
                .ok_or_else(|| format!("font {} missing from the build", entry.file))
        })
        .collect::<Result<_, _>>()?;
    Ok((catalogue, data))
}

/// Whether a project file is a font Typst may read.
fn is_font(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    [".ttf", ".otf", ".ttc", ".otc"]
        .iter()
        .any(|e| lower.ends_with(e))
}

/// Compiles a request with `fonts`, the embedded catalogue and its bytes.
/// The export child runs this; it reads nothing from disk.
pub fn compile(
    fonts: (Vec<FontEntry>, Vec<Vec<u8>>),
    request: ExportRequest,
) -> Result<Vec<u8>, Vec<Problem>> {
    let failed = |message: String| {
        vec![Problem {
            severity: "error".into(),
            message,
            path: None,
            start: None,
            end: None,
        }]
    };
    let (catalogue, data) = fonts;
    let mut compiler = LiveCompiler::new("main.typ", &catalogue).map_err(failed)?;
    for (index, bytes) in data.into_iter().enumerate() {
        compiler.add_font(index, bytes).map_err(failed)?;
    }
    for font in request.fonts {
        compiler.add_font_file(Arc::unwrap_or_clone(font));
    }
    let (year, month, day) = request.today;
    compiler.set_today(year, month, day);
    for (path, contents) in request.files {
        if is_font(&path)
            && let FileContents::Bytes(bytes) = &contents
        {
            compiler.add_font_file(bytes.clone());
        }
        compiler.write(&path, contents);
    }
    for (spec, files) in request.packages {
        compiler.add_package(&spec, files).map_err(failed)?;
    }
    let result = compiler.compile();
    if !result.ok {
        return Err(result
            .problems
            .into_iter()
            .map(|p| Problem {
                severity: p.severity.into(),
                message: p.message,
                path: p.path,
                start: p.start.and_then(|n| u32::try_from(n).ok()),
                end: p.end.and_then(|n| u32::try_from(n).ok()),
            })
            .collect());
    }
    let kind = if request.archival {
        PdfKind::A2b
    } else {
        PdfKind::Plain
    };
    compiler.pdf(kind).map_err(failed)
}

/// [`compile`] in this process, with every embedded font. For tests and
/// tools; the server never compiles in its own process.
pub fn compile_here(request: ExportRequest) -> Result<Vec<u8>, Vec<Problem>> {
    compile(
        embedded_fonts().map_err(|e| {
            vec![Problem {
                severity: "error".into(),
                message: e,
                path: None,
                start: None,
                end: None,
            }]
        })?,
        request,
    )
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

/// Asks the kernel to kill this process when the server ends, then checks
/// that the server had not already ended.
#[cfg(target_os = "linux")]
fn die_with(parent: u32) -> Result<(), String> {
    use rustix::process::{Signal, getppid, set_parent_process_death_signal};
    set_parent_process_death_signal(Some(Signal::KILL)).map_err(|e| e.to_string())?;
    match getppid() {
        Some(p) if p.as_raw_nonzero().get().cast_unsigned() == parent => Ok(()),
        _ => Err("the server already ended".into()),
    }
}

#[cfg(not(target_os = "linux"))]
fn die_with(_parent: u32) -> Result<(), String> {
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn limit(_memory_mb: u64, _seconds: u64) -> Result<(), String> {
    Ok(())
}

/// The request: magic, the date, a flag byte (1 for PDF/A), then entries.
/// Each entry is a kind byte and its parts, each part a little-endian `u64`
/// length and its bytes: 0 text (path, text), 1 bytes (path, bytes), 2 font
/// (bytes), 3 package file (spec, path, bytes).
fn encode(request: &ExportRequest) -> Vec<u8> {
    fn entry(out: &mut Vec<u8>, kind: u8, parts: &[&[u8]]) {
        out.push(kind);
        for part in parts {
            out.extend_from_slice(&(part.len() as u64).to_le_bytes());
            out.extend_from_slice(part);
        }
    }
    let mut out = MAGIC.to_vec();
    let (year, month, day) = request.today;
    out.extend_from_slice(&year.to_le_bytes());
    out.extend_from_slice(&[month, day, u8::from(request.archival)]);
    for (path, contents) in &request.files {
        match contents {
            FileContents::Text(text) => entry(&mut out, 0, &[path.as_bytes(), text.as_bytes()]),
            FileContents::Bytes(bytes) => entry(&mut out, 1, &[path.as_bytes(), bytes]),
        }
    }
    for font in &request.fonts {
        entry(&mut out, 2, &[font]);
    }
    for (spec, files) in &request.packages {
        for (path, bytes) in files {
            entry(&mut out, 3, &[spec.as_bytes(), path.as_bytes(), bytes]);
        }
    }
    out
}

fn decode(mut input: &[u8]) -> Option<ExportRequest> {
    fn take<'a>(input: &mut &'a [u8], n: usize) -> Option<&'a [u8]> {
        let (head, tail) = input.split_at_checked(n)?;
        *input = tail;
        Some(head)
    }
    fn part<'a>(input: &mut &'a [u8]) -> Option<&'a [u8]> {
        let len = u64::from_le_bytes(take(input, 8)?.try_into().ok()?);
        take(input, usize::try_from(len).ok()?)
    }
    fn text(input: &mut &[u8]) -> Option<String> {
        String::from_utf8(part(input)?.to_vec()).ok()
    }
    if take(&mut input, MAGIC.len())? != MAGIC {
        return None;
    }
    let year = i32::from_le_bytes(take(&mut input, 4)?.try_into().ok()?);
    let rest = take(&mut input, 3)?;
    let mut request = ExportRequest {
        today: (year, rest[0], rest[1]),
        files: Vec::new(),
        packages: Vec::new(),
        fonts: Vec::new(),
        archival: rest[2] == 1,
    };
    let mut packages: Vec<crate::packages::Unpacked> = Vec::new();
    while !input.is_empty() {
        match take(&mut input, 1)?[0] {
            0 => {
                let path = text(&mut input)?;
                request
                    .files
                    .push((path, FileContents::Text(text(&mut input)?)));
            }
            1 => {
                let path = text(&mut input)?;
                request
                    .files
                    .push((path, FileContents::Bytes(part(&mut input)?.to_vec())));
            }
            2 => request.fonts.push(Arc::new(part(&mut input)?.to_vec())),
            3 => {
                let spec = text(&mut input)?;
                let path = text(&mut input)?;
                let bytes = part(&mut input)?.to_vec();
                match packages.last_mut() {
                    Some((last, files)) if *last == spec => files.push((path, bytes)),
                    _ => packages.push((spec, vec![(path, bytes)])),
                }
            }
            _ => return None,
        }
    }
    request.packages = packages;
    Some(request)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_round_trip() {
        let request = ExportRequest {
            today: (2026, 11, 18),
            files: vec![
                ("main.typ".to_owned(), FileContents::Text("= Hi".into())),
                ("a.png".to_owned(), FileContents::Bytes(vec![0, 1, 2])),
            ],
            packages: vec![(
                "@preview/shells:0.1.0".into(),
                vec![
                    ("typst.toml".into(), b"x".to_vec()),
                    ("lib.typ".into(), b"y".to_vec()),
                ],
            )],
            fonts: vec![Arc::new(vec![9, 9])],
            archival: true,
        };
        let bytes = encode(&request);
        let back = decode(&bytes).unwrap();
        assert_eq!(back.today, (2026, 11, 18));
        assert!(back.archival);
        assert_eq!(back.files.len(), 2);
        assert!(matches!(&back.files[0].1, FileContents::Text(t) if t == "= Hi"));
        assert!(matches!(&back.files[1].1, FileContents::Bytes(b) if b == &[0, 1, 2]));
        assert_eq!(back.packages, request.packages);
        assert_eq!(back.fonts, request.fonts);
        assert!(decode(&bytes[..bytes.len() - 1]).is_none());
        assert!(decode(b"other").is_none());
    }

    /// One slot and one queue place. A waiter that is dropped while it
    /// waits gives its place back, however often that happens.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_cancelled_waiter_gives_its_place_back() {
        use futures_util::FutureExt as _;
        let limits = ExportLimits {
            parallel: 1,
            queue: 1,
            ..ExportLimits::default()
        };
        // A child that never answers holds the slot.
        let exporter = Exporter::with_sandbox("/bin/sleep".into(), limits, Sandbox::On);
        let request = || {
            exporter.pdf(ExportRequest {
                today: (2026, 11, 18),
                files: Vec::new(),
                packages: Vec::new(),
                fonts: Vec::new(),
                archival: false,
            })
        };
        let mut running = Box::pin(request());
        assert!((&mut running).now_or_never().is_none(), "holds the slot");
        for _ in 0..100 {
            let mut waiter = Box::pin(request());
            assert!((&mut waiter).now_or_never().is_none(), "queued");
        }
        let mut next = Box::pin(request());
        assert_eq!((&mut next).now_or_never(), None, "queued, not refused");
        let mut over = Box::pin(request());
        assert_eq!((&mut over).now_or_never(), Some(Err(ExportError::Busy)));
    }
}
