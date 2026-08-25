//! Minimal ptrace-based syscall tracer, purpose-built to observe
//! bosminer's real UART/GPIO/PSU I/O during a live bring-up without
//! touching its file descriptors ourselves.
//!
//! Three rounds of software-only hypothesis testing on chip discovery
//! (see HANDOFF.md) hit a wall with no way forward short of capturing
//! bosminer's actual byte-level traffic. No hardware logic analyzer
//! is available, and this kernel wasn't built with kprobe/uprobe
//! support, so this is the software equivalent: launch bosminer
//! directly under ptrace (like `strace <command>`), and at every
//! syscall log its number and first three register arguments. `open`
//! /`openat` calls populate an fd -> path map (by reading the path
//! string from the tracee's own memory via `/proc/<pid>/mem`);
//! `write` calls with a known fd get their buffer hex-dumped the same
//! way.
//!
//! `libc::ptrace` is C-variadic, and Rust FFI requires exact argument
//! types at variadic call sites (no automatic promotion the way a C
//! caller would get) -- getting that subtly wrong would silently
//! corrupt reads rather than error cleanly. Every ptrace call here
//! instead goes through the raw `syscall(2)` wrapper
//! (`libc::syscall(libc::SYS_ptrace, ...)`), which has no variadic
//! ambiguity: the kernel ptrace syscall ABI is a fixed 4 register-
//! sized arguments on every architecture.
//!
//! Also deliberately does NOT hardcode ARM's syscall numbers for
//! write/open beyond a best-effort guess (write=4, open=5, openat=322
//! on ARM EABI -- the low ones match the historical i386 table ARM's
//! was derived from). Getting one wrong would silently miss exactly
//! the calls we care about, so every *other* syscall is still
//! tallied (count + last args, printed periodically) rather than
//! ignored -- an unexpectedly frequent number with small counts
//! typical of these frame sizes would be the tell that a guess above
//! is wrong. Not printed per-call: a busy async runtime makes
//! thousands of epoll/timer/etc. syscalls a second, and printing all
//! of them would flood the (tmpfs, space-limited) output.
//!
//! Usage: s19k-trace <program> [args...]
//! Ctrl-C (or a timeout wrapper) to stop; the child dies with the
//! tracer since there's no detach path implemented (not needed here
//! -- always followed by a clean `/etc/init.d/S99bosminer restart`).

use std::collections::HashMap;
use std::ffi::CString;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::time::{Duration, Instant};

use nix::libc::{self, c_char, c_int, c_long, c_void, pid_t};

const PTRACE_TRACEME: c_long = 0;
const PTRACE_CONT: c_long = 7;
const PTRACE_SETOPTIONS: c_long = 0x4200;
const PTRACE_GETREGS: c_long = 12;
const PTRACE_SYSCALL: c_long = 24;
const PTRACE_O_TRACESYSGOOD: c_long = 0x0000_0001;

// Candidate ARM EABI syscall numbers, used only to decide which
// already-logged calls to try to decode further -- see module docs.
const SYS_OPEN: u32 = 5;
const SYS_WRITE: u32 = 4;
const SYS_OPENAT: u32 = 322;

/// Issue a raw ptrace(2) request via the syscall(2) wrapper, avoiding
/// `libc::ptrace`'s C-variadic call-site ambiguity (see module docs).
unsafe fn raw_ptrace(request: c_long, pid: pid_t, addr: *mut c_void, data: *mut c_void) -> c_long {
    unsafe {
        libc::syscall(
            libc::SYS_ptrace,
            request,
            pid as c_long,
            addr as c_long,
            data as c_long,
        )
    }
}

/// Raw ARM `pt_regs` layout, per the kernel ptrace ABI: r0-r15, cpsr,
/// then the original r0 (r0 is overwritten with the return value by
/// syscall exit, so orig_r0 is the only reliable "first arg" at every
/// stop). Not exposed with named fields by the `libc` crate for this
/// target, so defined directly here.
#[repr(C)]
#[derive(Debug, Default, Clone, Copy)]
struct ArmRegs {
    uregs: [u32; 18],
}

impl ArmRegs {
    fn syscall_nr(&self) -> u32 {
        self.uregs[7]
    }
    fn arg(&self, i: usize) -> u32 {
        // r0..r5 map to uregs[0..6]; arg(0) is orig_r0 (see above).
        if i == 0 {
            self.uregs[17]
        } else {
            self.uregs[i]
        }
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() {
        eprintln!("usage: s19k-trace <program> [args...]");
        std::process::exit(1);
    }

    let pid = unsafe { libc::fork() };
    if pid < 0 {
        eprintln!("fork failed");
        std::process::exit(1);
    }
    if pid == 0 {
        child_exec(&args);
    }
    parent_trace(pid);
}

fn child_exec(args: &[String]) -> ! {
    unsafe {
        raw_ptrace(
            PTRACE_TRACEME,
            0,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        );
    }
    let program = CString::new(args[0].as_bytes()).unwrap();
    let cargs: Vec<CString> = args
        .iter()
        .map(|a| CString::new(a.as_bytes()).unwrap())
        .collect();
    let mut argv: Vec<*const c_char> = cargs.iter().map(|c| c.as_ptr()).collect();
    argv.push(std::ptr::null());
    unsafe {
        libc::execv(program.as_ptr(), argv.as_ptr());
    }
    // execv only returns on failure.
    eprintln!("execv failed for {}", args[0]);
    std::process::exit(1);
}

fn parent_trace(pid: pid_t) {
    // Wait for the initial SIGTRAP from execve, then enable
    // TRACESYSGOOD so syscall-stops are unambiguous.
    let mut status: c_int = 0;
    unsafe { libc::waitpid(pid, &mut status, 0) };
    unsafe {
        raw_ptrace(
            PTRACE_SETOPTIONS,
            pid,
            std::ptr::null_mut(),
            PTRACE_O_TRACESYSGOOD as *mut c_void,
        );
    }

    let mut mem = File::open(format!("/proc/{pid}/mem")).expect("opening tracee /proc/pid/mem");
    let mut fd_paths: HashMap<u32, String> = HashMap::new();
    let mut other_syscalls: HashMap<u32, OtherSyscallStats> = HashMap::new();
    let start = Instant::now();
    let mut last_summary = start;
    let mut entering = true; // alternates enter/exit on each syscall-stop

    loop {
        unsafe {
            raw_ptrace(
                PTRACE_SYSCALL,
                pid,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            );
        }
        let waited = unsafe { libc::waitpid(pid, &mut status, 0) };
        if waited < 0 {
            break;
        }
        if libc::WIFEXITED(status) || libc::WIFSIGNALED(status) {
            println!("[{:>8.3}] child exited", start.elapsed().as_secs_f64());
            break;
        }
        let stopped_by_syscall =
            libc::WIFSTOPPED(status) && libc::WSTOPSIG(status) == (libc::SIGTRAP | 0x80);
        if !stopped_by_syscall {
            // A real signal, not a syscall-stop -- pass it through.
            let sig = libc::WSTOPSIG(status);
            unsafe {
                raw_ptrace(PTRACE_CONT, pid, std::ptr::null_mut(), sig as *mut c_void);
            }
            continue;
        }

        if entering && let Some(regs) = get_regs(pid) {
            log_syscall_entry(
                &mut mem,
                pid,
                &regs,
                &mut fd_paths,
                &mut other_syscalls,
                start.elapsed(),
            );
        }
        entering = !entering;

        if last_summary.elapsed() > Duration::from_secs(10) {
            print_other_syscall_summary(&other_syscalls);
            last_summary = Instant::now();
        }
    }
    print_other_syscall_summary(&other_syscalls);
}

fn get_regs(pid: pid_t) -> Option<ArmRegs> {
    let mut regs = ArmRegs::default();
    let rc = unsafe {
        raw_ptrace(
            PTRACE_GETREGS,
            pid,
            std::ptr::null_mut(),
            &mut regs as *mut ArmRegs as *mut c_void,
        )
    };
    if rc < 0 { None } else { Some(regs) }
}

fn log_syscall_entry(
    mem: &mut File,
    pid: pid_t,
    regs: &ArmRegs,
    fd_paths: &mut HashMap<u32, String>,
    other_syscalls: &mut HashMap<u32, OtherSyscallStats>,
    elapsed: Duration,
) {
    let nr = regs.syscall_nr();
    let (a0, a1, a2) = (regs.arg(0), regs.arg(1), regs.arg(2));

    match nr {
        SYS_OPEN => {
            let path = read_cstr(mem, a0);
            println!("[{:>8.3}] open({path:?})", elapsed.as_secs_f64());
            // The real fd isn't known until the matching exit-stop;
            // re-deriving the whole map from /proc/<pid>/fd right
            // after is simpler and correct regardless of timing.
            refresh_fd_paths(pid, fd_paths);
        }
        SYS_OPENAT => {
            let path = read_cstr(mem, a1);
            println!("[{:>8.3}] openat(.., {path:?})", elapsed.as_secs_f64());
            refresh_fd_paths(pid, fd_paths);
        }
        SYS_WRITE => {
            let path = fd_paths
                .get(&a0)
                .cloned()
                .unwrap_or_else(|| format!("fd{a0}"));
            let len = a2.min(256) as usize;
            let data = read_bytes(mem, a1, len);
            println!(
                "[{:>8.3}] write({path}, len={a2}) {data:02x?}",
                elapsed.as_secs_f64()
            );
        }
        _ => {
            // Everything else: tallied, not printed per-call -- a
            // busy async runtime makes thousands of epoll/timer/etc.
            // syscalls a second, and printing all of them would both
            // flood the (tmpfs, space-limited) output and add real
            // per-print overhead on top of ptrace's own per-syscall
            // cost. The tally (see print_other_syscall_summary) is
            // still a safety net if SYS_WRITE's guessed number above
            // is wrong: an unexpectedly high count for some other
            // number, with small a2 values typical of these frame
            // sizes, would be the tell.
            let stats = other_syscalls.entry(nr).or_default();
            stats.count += 1;
            stats.last_a0 = a0;
            stats.last_a2 = a2;
        }
    }
}

/// Tally for a syscall number not specifically decoded above.
#[derive(Debug, Default, Clone, Copy)]
struct OtherSyscallStats {
    count: u64,
    last_a0: u32,
    last_a2: u32,
}

/// Print a summary of untallied syscalls, most frequent first, so a
/// wrong SYS_WRITE/SYS_OPEN(AT) guess above still leaves a trail to
/// follow (see `log_syscall_entry`'s docs).
fn print_other_syscall_summary(other_syscalls: &HashMap<u32, OtherSyscallStats>) {
    let mut entries: Vec<_> = other_syscalls.iter().collect();
    entries.sort_by_key(|(_, stats)| std::cmp::Reverse(stats.count));
    println!("--- other syscalls (nr: count, last a0/a2) ---");
    for (nr, stats) in entries {
        println!(
            "  nr={nr}: count={} last_a0=0x{:x} last_a2=0x{:x}",
            stats.count, stats.last_a0, stats.last_a2
        );
    }
}

/// Re-derive the fd -> path map from `/proc/<pid>/fd` (symlinks show
/// the real target path). Simpler and more correct than trying to
/// track open()'s return value across the enter/exit boundary.
fn refresh_fd_paths(pid: pid_t, fd_paths: &mut HashMap<u32, String>) {
    let Ok(entries) = std::fs::read_dir(format!("/proc/{pid}/fd")) else {
        return;
    };
    for entry in entries.flatten() {
        let Ok(fd) = entry.file_name().to_string_lossy().parse::<u32>() else {
            continue;
        };
        if let Ok(target) = std::fs::read_link(entry.path()) {
            fd_paths.insert(fd, target.to_string_lossy().into_owned());
        }
    }
}

fn read_bytes(mem: &mut File, addr: u32, len: usize) -> Vec<u8> {
    let mut buf = vec![0u8; len];
    if mem.seek(SeekFrom::Start(u64::from(addr))).is_err() {
        return Vec::new();
    }
    match mem.read(&mut buf) {
        Ok(n) => {
            buf.truncate(n);
            buf
        }
        Err(_) => Vec::new(),
    }
}

fn read_cstr(mem: &mut File, addr: u32) -> String {
    let raw = read_bytes(mem, addr, 256);
    let end = raw.iter().position(|&b| b == 0).unwrap_or(raw.len());
    String::from_utf8_lossy(&raw[..end]).into_owned()
}
