//! The export sandbox.
//!
//! An export child is sandboxed when four promises hold:
//!
//! - Files: it reads only the inputs the server writes to its standard input
//!   and writes only its result to its standard output. It can open, create,
//!   truncate, rename or delete no file.
//! - Sockets: it can create no socket of any family, and it holds no file
//!   descriptor but its standard input, output and error.
//! - Processes: it cannot signal, trace or read the memory of another
//!   process, or change another process's limits or scheduling.
//! - Privileges: it runs with `no_new_privs`.
//!
//! Landlock (ABI 3, Linux 6.2) keeps the file promise. A seccomp filter
//! keeps the socket and process promises. Before the server starts a child
//! it marks every descriptor it holds close-on-exec ([`keep_descriptors`]),
//! also those it was started with. The child checks its descriptors and
//! threads itself before it confines itself.
//!
//! [`probe`] decides at start whether the running kernel keeps every
//! promise. There is no middle state: an export is sandboxed or it is not.

/// Whether exports are sandboxed on this system.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Sandbox {
    /// Every promise holds.
    On,
    /// At least one promise cannot be kept, and why.
    Off(String),
}

/// What the running kernel offers the sandbox.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Kernel {
    /// The Landlock ABI version, or 0 when there is no Landlock.
    pub landlock_abi: i32,
    /// Whether this process can install the seccomp filter, and why not.
    pub seccomp: Result<(), String>,
}

/// The lowest Landlock ABI that keeps the file promise: ABI 3 added the
/// right to truncate.
pub const LANDLOCK_ABI: i32 = 3;

/// Whether `kernel` keeps every promise.
#[must_use]
pub fn decide(kernel: &Kernel) -> Sandbox {
    if kernel.landlock_abi < 1 {
        return Sandbox::Off("this kernel has no Landlock, or it is turned off".into());
    }
    if kernel.landlock_abi < LANDLOCK_ABI {
        return Sandbox::Off(format!(
            "this kernel has Landlock ABI {}; the sandbox needs ABI {LANDLOCK_ABI} \
             (Linux 6.2) to stop truncation",
            kernel.landlock_abi
        ));
    }
    if let Err(why) = &kernel.seccomp {
        return Sandbox::Off(format!(
            "this kernel cannot install the seccomp filter: {why}"
        ));
    }
    Sandbox::On
}

/// Detects what the running kernel offers, on a thread that ends at once,
/// and decides. Both Landlock and seccomp restrict only the thread that asks.
#[must_use]
pub fn probe() -> Sandbox {
    std::thread::spawn(|| decide(&detect()))
        .join()
        .unwrap_or_else(|_| Sandbox::Off("the check panicked".into()))
}

#[cfg(target_os = "linux")]
fn detect() -> Kernel {
    use landlock::{AccessFs, LandlockStatus, Ruleset, RulesetAttr as _};
    // A best-effort ruleset reports the kernel's ABI in its status.
    let landlock_abi = Ruleset::default()
        .handle_access(AccessFs::Execute)
        .and_then(Ruleset::create)
        .and_then(landlock::RulesetCreated::restrict_self)
        .map_or(0, |status| match status.landlock {
            LandlockStatus::Available {
                effective_abi,
                kernel_abi,
            } => kernel_abi.unwrap_or(effective_abi as i32),
            LandlockStatus::NotEnabled | LandlockStatus::NotImplemented => 0,
        });
    let seccomp =
        filter().and_then(|program| seccompiler::apply_filter(&program).map_err(|e| e.to_string()));
    Kernel {
        landlock_abi,
        seccomp,
    }
}

#[cfg(not(target_os = "linux"))]
fn detect() -> Kernel {
    Kernel {
        landlock_abi: 0,
        seccomp: Err("seccomp exists only on Linux".into()),
    }
}

/// Marks every descriptor of this process beyond 0, 1 and 2 close-on-exec,
/// so a child started after it inherits none of them. Rust opens its own
/// files this way. This covers the rest: the descriptors the server was
/// started with, which a supervisor or a test runner may leave open, and any
/// that a C library opened.
#[cfg(target_os = "linux")]
pub fn keep_descriptors() {
    close_fds::set_fds_cloexec_threadsafe(3, &[]);
}

/// Nothing to do where there is no sandbox to keep the promise for.
#[cfg(not(target_os = "linux"))]
pub fn keep_descriptors() {}

/// Fails unless this process has no file descriptor but 0, 1 and 2 and no
/// thread but this one. A thread started before [`confine`] would escape it,
/// and a descriptor would let the child use what it points to.
#[cfg(target_os = "linux")]
pub fn alone() -> Result<(), String> {
    use rustix::fd::AsRawFd as _;
    use rustix::fs::{Dir, Mode, OFlags, open};
    let fds = open(
        "/proc/self/fd",
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(|e| format!("cannot list the open files: {e}"))?;
    let listing = fds.as_raw_fd();
    let mut dir = Dir::new(fds).map_err(|e| format!("cannot list the open files: {e}"))?;
    let mut extra = Vec::new();
    while let Some(entry) = dir.read() {
        let entry = entry.map_err(|e| format!("cannot list the open files: {e}"))?;
        let Ok(fd) = entry.file_name().to_string_lossy().parse::<i32>() else {
            continue;
        };
        if fd > 2 && fd != listing {
            extra.push(fd);
        }
    }
    if !extra.is_empty() {
        return Err(format!(
            "the export inherited file descriptors it does not need: {extra:?}"
        ));
    }
    let threads = std::fs::read_dir("/proc/self/task")
        .map_err(|e| format!("cannot list the threads: {e}"))?
        .count();
    if threads != 1 {
        return Err(format!(
            "the export has {threads} threads before the sandbox"
        ));
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
pub fn alone() -> Result<(), String> {
    Err("the sandbox exists only on Linux".into())
}

/// Confines this thread and every thread and process it starts later:
/// `no_new_privs`, then Landlock with every file right of ABI 3 handled and
/// none granted, then the seccomp filter. Fails, and leaves the thread
/// partly confined, when any of them cannot be enforced in full.
#[cfg(target_os = "linux")]
pub fn confine() -> Result<(), String> {
    use landlock::{
        ABI, Access as _, AccessFs, CompatLevel, Compatible as _, Ruleset, RulesetAttr as _,
        RulesetStatus,
    };
    rustix::thread::set_no_new_privs(true).map_err(|e| format!("no_new_privs: {e}"))?;
    let status = Ruleset::default()
        .set_compatibility(CompatLevel::HardRequirement)
        .handle_access(AccessFs::from_all(ABI::V3))
        .and_then(Ruleset::create)
        .and_then(landlock::RulesetCreated::restrict_self)
        .map_err(|e| format!("Landlock: {e}"))?;
    if status.ruleset != RulesetStatus::FullyEnforced {
        return Err(format!("Landlock: {:?}", status.ruleset));
    }
    let program = filter()?;
    seccompiler::apply_filter(&program).map_err(|e| format!("seccomp: {e}"))?;
    match rustix::thread::no_new_privs() {
        Ok(true) => Ok(()),
        _ => Err("no_new_privs is not set".into()),
    }
}

#[cfg(not(target_os = "linux"))]
pub fn confine() -> Result<(), String> {
    Err("the sandbox exists only on Linux".into())
}

/// System calls the filter refuses whatever their arguments.
#[cfg(target_os = "linux")]
const ALWAYS: &[i64] = &[
    libc::SYS_socket,
    libc::SYS_socketpair,
    libc::SYS_io_uring_setup,
    libc::SYS_io_uring_enter,
    libc::SYS_io_uring_register,
    libc::SYS_ptrace,
    libc::SYS_process_vm_readv,
    libc::SYS_process_vm_writev,
    libc::SYS_process_madvise,
    libc::SYS_pidfd_open,
    libc::SYS_pidfd_getfd,
    libc::SYS_pidfd_send_signal,
    libc::SYS_tkill,
    libc::SYS_kcmp,
    libc::SYS_perf_event_open,
    libc::SYS_bpf,
    libc::SYS_setpriority,
    libc::SYS_ioprio_set,
    libc::SYS_migrate_pages,
    libc::SYS_move_pages,
    libc::SYS_fchmodat,
    FCHMODAT2,
    libc::SYS_fchownat,
    libc::SYS_utimensat,
    libc::SYS_setxattr,
    libc::SYS_lsetxattr,
    libc::SYS_removexattr,
    libc::SYS_lremovexattr,
    SETXATTRAT,
    REMOVEXATTRAT,
    #[cfg(target_arch = "x86_64")]
    libc::SYS_chmod,
    #[cfg(target_arch = "x86_64")]
    libc::SYS_chown,
    #[cfg(target_arch = "x86_64")]
    libc::SYS_lchown,
    #[cfg(target_arch = "x86_64")]
    libc::SYS_utime,
    #[cfg(target_arch = "x86_64")]
    libc::SYS_utimes,
    #[cfg(target_arch = "x86_64")]
    libc::SYS_futimesat,
];

/// Numbers shared by every architecture since Linux 5.1 that `libc` does
/// not name on every architecture.
#[cfg(target_os = "linux")]
const FCHMODAT2: i64 = 452;
#[cfg(target_os = "linux")]
const SETXATTRAT: i64 = 463;
#[cfg(target_os = "linux")]
const REMOVEXATTRAT: i64 = 466;

/// The seccomp filter. It allows every system call but these, which fail
/// with `EPERM`:
///
/// - every socket, and `io_uring`, whose operations seccomp cannot see;
/// - tracing another process or reading its memory or descriptors;
/// - signals to any process but this one, including `SIGIO` through
///   `F_SETOWN`;
/// - changing another process's limits, scheduling or memory placement;
/// - changing the mode, owner, times or extended attributes of a file by
///   path, which Landlock does not cover.
///
/// On `x86_64` every x32 system call fails too, so the x32 numbers cannot go
/// around the filter. A call for another architecture kills the process.
#[cfg(target_os = "linux")]
fn filter() -> Result<seccompiler::BpfProgram, String> {
    use seccompiler::{
        SeccompAction,
        SeccompCmpArgLen::Dword,
        SeccompCmpOp::{Eq, Ne},
        SeccompCondition, SeccompFilter, SeccompRule, TargetArch,
    };

    const F_SETOWN: u64 = 8;
    const F_SETOWN_EX: u64 = 15;

    let arch = if cfg!(target_arch = "x86_64") {
        TargetArch::x86_64
    } else if cfg!(target_arch = "aarch64") {
        TargetArch::aarch64
    } else if cfg!(target_arch = "riscv64") {
        TargetArch::riscv64
    } else {
        return Err("there is no seccomp filter for this architecture".into());
    };
    let fail = |e: seccompiler::BackendError| e.to_string();
    // A rule that matches when every (argument, comparison, value) holds.
    let rule = |conditions: &[(u8, seccompiler::SeccompCmpOp, u64)]| {
        conditions
            .iter()
            .map(|(arg, op, value)| SeccompCondition::new(*arg, Dword, op.clone(), *value))
            .collect::<Result<Vec<_>, _>>()
            .and_then(SeccompRule::new)
            .map_err(fail)
    };
    let me = u64::from(
        rustix::process::getpid()
            .as_raw_nonzero()
            .get()
            .cast_unsigned(),
    );

    let mut rules: std::collections::BTreeMap<_, _> =
        ALWAYS.iter().map(|&nr| (nr, Vec::new())).collect();
    // A signal to any pid but this process's own; 0 and -1 mean many.
    for nr in [
        libc::SYS_kill,
        libc::SYS_tgkill,
        libc::SYS_rt_sigqueueinfo,
        libc::SYS_rt_tgsigqueueinfo,
    ] {
        rules.insert(nr, vec![rule(&[(0, Ne, me)])?]);
    }
    // A change to another process; 0 means this one.
    for nr in [
        libc::SYS_prlimit64,
        libc::SYS_sched_setaffinity,
        libc::SYS_sched_setparam,
        libc::SYS_sched_setscheduler,
        libc::SYS_sched_setattr,
    ] {
        rules.insert(nr, vec![rule(&[(0, Ne, me), (0, Ne, 0)])?]);
    }
    rules.insert(
        libc::SYS_fcntl,
        vec![rule(&[(1, Eq, F_SETOWN)])?, rule(&[(1, Eq, F_SETOWN_EX)])?],
    );
    let eperm = rustix::io::Errno::PERM.raw_os_error().cast_unsigned();
    let filter = SeccompFilter::new(
        rules,
        SeccompAction::Allow,
        SeccompAction::Errno(eperm),
        arch,
    )
    .map_err(fail)?;
    let mut program: seccompiler::BpfProgram = filter.try_into().map_err(fail)?;
    if arch == TargetArch::x86_64 {
        // After the architecture check, which is the first three
        // instructions: a number with the x32 bit fails.
        const LD_W_ABS: u16 = 0x20;
        const JGE_K: u16 = 0x35;
        const RET_K: u16 = 0x06;
        const X32_SYSCALL_BIT: u32 = 0x4000_0000;
        const RET_ERRNO: u32 = 0x0005_0000;
        let op = |code, jt, jf, k| seccompiler::sock_filter { code, jt, jf, k };
        program.splice(
            3..3,
            [
                op(LD_W_ABS, 0, 0, 0),
                op(JGE_K, 0, 1, X32_SYSCALL_BIT),
                op(RET_K, 0, 0, RET_ERRNO | eperm),
            ],
        );
    }
    Ok(program)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kernel(landlock_abi: i32, seccomp: Result<(), String>) -> Kernel {
        Kernel {
            landlock_abi,
            seccomp,
        }
    }

    #[test]
    fn the_sandbox_is_on_only_when_every_promise_holds() {
        let off = |k: &Kernel| match decide(k) {
            Sandbox::Off(why) => why,
            Sandbox::On => panic!("{k:?} is not enough"),
        };
        assert!(off(&kernel(0, Ok(()))).contains("no Landlock"));
        for abi in [1, 2] {
            let why = off(&kernel(abi, Ok(())));
            assert!(why.contains(&format!("ABI {abi}")), "{why}");
            assert!(why.contains("Linux 6.2"), "{why}");
        }
        let why = off(&kernel(6, Err("EINVAL".into())));
        assert!(why.contains("seccomp") && why.contains("EINVAL"), "{why}");
        for abi in [3, 4, 6, 9, 12] {
            assert_eq!(decide(&kernel(abi, Ok(()))), Sandbox::On, "ABI {abi}");
        }
    }
}
