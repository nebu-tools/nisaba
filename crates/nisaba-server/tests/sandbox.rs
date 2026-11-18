//! The export sandbox on the running kernel.
//!
//! This test runs itself again as a child that confines itself the way an
//! export child does and then tries one thing it must not do. Each try also
//! runs unconfined, which must succeed, so a refusal means the sandbox
//! refused it. On Linux the test fails, and does not skip, when the kernel
//! cannot sandbox exports.

fn main() {
    #[cfg(target_os = "linux")]
    linux::main();
}

#[cfg(target_os = "linux")]
mod linux {
    use std::io::Read as _;
    use std::process::{Command, Stdio};

    use nisaba_server::sandbox::{self, Sandbox};

    pub fn main() {
        let args: Vec<String> = std::env::args().skip(1).collect();
        if args.first().map(String::as_str) == Some("probe") {
            std::process::exit(probe(&args[1..]));
        }
        if let Sandbox::Off(why) = sandbox::probe() {
            panic!("this kernel cannot sandbox exports: {why}");
        }
        let dir = tempfile::tempdir().unwrap();
        let secret = dir.path().join("secret");
        std::fs::write(&secret, "the server's data").unwrap();
        let udp = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        udp.set_nonblocking(true).unwrap();
        let tcp = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        tcp.set_nonblocking(true).unwrap();
        let unix_path = dir.path().join("socket");
        let unix = std::os::unix::net::UnixListener::bind(&unix_path).unwrap();
        let parent = std::process::id().to_string();
        let udp_addr = udp.local_addr().unwrap().to_string();
        let tcp_addr = tcp.local_addr().unwrap().to_string();
        let created = dir.path().join("created");
        let cases: [(&str, Vec<&str>); 8] = [
            ("udp", vec![&udp_addr]),
            ("tcp", vec![&tcp_addr]),
            ("unix", vec![unix_path.to_str().unwrap()]),
            ("socketpair", vec![]),
            ("read", vec![secret.to_str().unwrap()]),
            ("create", vec![created.to_str().unwrap()]),
            ("truncate", vec![secret.to_str().unwrap()]),
            ("signal", vec![&parent]),
        ];
        let mut failed = Vec::new();
        for (action, extra) in &cases {
            // Confined first, so nothing the free run did can hide a leak.
            let confined = run(true, action, extra);
            let leaked = match *action {
                "udp" => {
                    let mut buf = [0; 16];
                    std::thread::sleep(std::time::Duration::from_millis(100));
                    udp.recv(&mut buf).is_ok()
                }
                "tcp" => tcp.accept().is_ok(),
                "create" => created.exists(),
                "truncate" => std::fs::read_to_string(&secret).unwrap() != "the server's data",
                _ => false,
            };
            if confined.is_ok() || leaked {
                failed.push(format!("{action}: the sandbox allowed it ({confined:?})"));
            }
            let free = run(false, action, extra);
            if let Err(e) = free {
                failed.push(format!(
                    "{action}: fails even unconfined, so proves nothing: {e}"
                ));
            }
            match *action {
                "tcp" => drop(tcp.accept()),
                "create" => drop(std::fs::remove_file(&created)),
                "truncate" => std::fs::write(&secret, "the server's data").unwrap(),
                _ => {}
            }
            println!("{action}: refused under the sandbox, allowed without it");
        }
        drop(unix);
        failed.extend(inherited_descriptors());
        assert!(failed.is_empty(), "{failed:#?}");
        println!("the sandbox keeps every promise on this kernel");
    }

    /// Runs `action` in a child, confined or not; `Err` says how it failed.
    fn run(confined: bool, action: &str, extra: &[&str]) -> Result<(), String> {
        let out = Command::new(std::env::current_exe().unwrap())
            .arg("probe")
            .arg(if confined { "confined" } else { "free" })
            .arg(action)
            .args(extra)
            .stdin(Stdio::null())
            .output()
            .unwrap();
        let said = String::from_utf8_lossy(&out.stdout).trim().to_owned();
        match out.status.code() {
            Some(0) => Ok(()),
            Some(1) => Err(said),
            _ => panic!(
                "probe {action} crashed: {} {}",
                out.status,
                String::from_utf8_lossy(&out.stderr)
            ),
        }
    }

    /// The child: confines itself when asked, tries `action`, and exits 0
    /// when it worked and 1 when it was refused.
    fn probe(args: &[String]) -> i32 {
        let (mode, action) = (args[0].as_str(), args[1].as_str());
        let arg = || args[2].as_str();
        if mode == "confined"
            && let Err(e) = sandbox::confine()
        {
            eprintln!("cannot confine: {e}");
            return 2;
        }
        let tried: std::io::Result<()> = match action {
            "udp" => std::net::UdpSocket::bind("127.0.0.1:0")
                .and_then(|s| s.send_to(b"leak", arg()))
                .map(drop),
            "tcp" => std::net::TcpStream::connect(arg()).map(drop),
            "unix" => std::os::unix::net::UnixStream::connect(arg()).map(drop),
            "socketpair" => std::os::unix::net::UnixStream::pair().map(drop),
            "read" => std::fs::read(arg()).map(drop),
            "create" => std::fs::File::create_new(arg()).map(drop),
            "truncate" => std::fs::OpenOptions::new()
                .write(true)
                .truncate(true)
                .open(arg())
                .map(drop),
            "signal" => {
                let pid = rustix::process::Pid::from_raw(arg().parse().unwrap()).unwrap();
                // Signal 0 checks the right to signal and sends nothing.
                rustix::process::test_kill_process(pid).map_err(std::io::Error::from)
            }
            other => panic!("unknown probe {other}"),
        };
        match tried {
            Ok(()) => {
                println!("allowed");
                0
            }
            Err(e) => {
                println!("refused: {e}");
                1
            }
        }
    }

    /// An export child that inherits a descriptor it does not need stops
    /// before it confines itself, with the exit status for "unsandboxed".
    fn inherited_descriptors() -> Vec<String> {
        let dir = tempfile::tempdir().unwrap();
        let leaked = std::fs::File::create(dir.path().join("leaked")).unwrap();
        let export = |leak: bool| {
            // As the server does before it starts a child. The test runner
            // may have started this process with descriptors of its own.
            nisaba_server::sandbox::keep_descriptors();
            let flags = if leak {
                rustix::io::FdFlags::empty()
            } else {
                rustix::io::FdFlags::CLOEXEC
            };
            rustix::io::fcntl_setfd(&leaked, flags).unwrap();
            let mut child = Command::new(env!("CARGO_BIN_EXE_nisaba"))
                .args(["__export", "1024", "10", "required"])
                .arg(std::process::id().to_string())
                .stdin(Stdio::piped())
                .stdout(Stdio::null())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap();
            drop(child.stdin.take());
            let mut errors = String::new();
            child
                .stderr
                .take()
                .unwrap()
                .read_to_string(&mut errors)
                .unwrap();
            (child.wait().unwrap().code(), errors)
        };
        let mut failed = Vec::new();
        let (code, errors) = export(true);
        if code != Some(4) || !errors.contains("file descriptors") {
            failed.push(format!("a leaked descriptor: exit {code:?}, {errors}"));
        }
        // Without the leak it gets past the sandbox to the empty request.
        let (code, errors) = export(false);
        if code != Some(3) || !errors.contains("malformed") {
            failed.push(format!("no leaked descriptor: exit {code:?}, {errors}"));
        }
        failed
    }
}
