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
//! /`openat` calls populate an fd -> path map (re-derived from
//! `/proc/<pid>/fd` right after the matching *exit*-stop, once the fd
//! actually exists -- refreshing at entry raced real interleaving
//! from other threads); `write` calls with a known fd get their
//! buffer hex-dumped by reading the tracee's own memory via
//! `/proc/<pid>/mem`.
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

use std::collections::{HashMap, HashSet};
use std::ffi::CString;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::time::{Duration, Instant};

use nix::libc::{self, c_char, c_int, c_long, c_void, pid_t};

const PTRACE_TRACEME: c_long = 0;
const PTRACE_SETOPTIONS: c_long = 0x4200;
const PTRACE_GETEVENTMSG: c_long = 0x4201;
const PTRACE_GETREGS: c_long = 12;
const PTRACE_SYSCALL: c_long = 24;
const PTRACE_O_TRACESYSGOOD: c_long = 0x0000_0001;
const PTRACE_O_TRACEFORK: c_long = 0x0000_0002;
const PTRACE_O_TRACEVFORK: c_long = 0x0000_0004;
const PTRACE_O_TRACECLONE: c_long = 0x0000_0008;
const TRACE_OPTIONS: c_long =
    PTRACE_O_TRACESYSGOOD | PTRACE_O_TRACEFORK | PTRACE_O_TRACEVFORK | PTRACE_O_TRACECLONE;

// `bosminer` is an async Rust runtime and almost certainly spawns
// worker threads for blocking hardware I/O (the same pattern our own
// `linux_hw` code uses via `spawn_blocking`) -- confirmed the hard
// way: an earlier single-threaded trace attempt sat idle for 100+
// seconds while `bosminer`'s own log showed it had already
// discovered all 77 chips per chain, in real time, on some other
// thread this tracer never saw. `PTRACE_O_TRACECLONE` (plus FORK/
// VFORK for safety) makes new threads/children automatically
// tracees too; each is a distinct waitable pid multiplexed via
// `waitpid(-1, ...)`, with its own independent enter/exit toggle
// state (see `parent_trace`'s `entering` map).

// Candidate ARM EABI syscall numbers, used only to decide which
// already-logged calls to try to decode further -- see module docs.
const SYS_OPEN: u32 = 5;
const SYS_WRITE: u32 = 4;
const SYS_OPENAT: u32 = 322;
const SYS_IOCTL: u32 = 54;

// TCSETS2 = _IOW('T', 0x2B, struct termios2): dir=WRITE=1, type='T'
// =0x54, nr=0x2B, size=44 in the generic Linux ioctl encoding. On
// x86, this is the *only* way to set a non-standard baud rate like
// 3,125,000, since plain `struct termios` has no room for it there.
// But on ARM (and most non-x86 archs, which use the "generic"
// asm-generic/termbits.h), plain `struct termios` *already* has the
// same c_ispeed/c_ospeed fields and 44-byte layout as termios2 --
// TCGETS2/SETS2 exist mainly for x86 compat, so glibc/musl's
// tcsetattr() on this target most likely goes through plain TCSETS
// (0x5402) instead. Decode both the same way; whichever one actually
// appears settles which this platform uses.
const TCSETS2: u32 = 0x402c_542b;
const TCSETS: u32 = 0x5402;
// termios/termios2 layout (asm-generic/termbits.h, what ARM Linux
// uses for both): c_iflag/c_oflag/c_cflag/c_lflag (u32 x4), c_line
// (u8), c_cc[19] (u8 x19), c_ispeed, c_ospeed (u32 x2) -- 44 bytes
// total, no padding (offset 36 for c_ispeed is already 4-aligned).
const TERMIOS2_LEN: usize = 44;
const CBAUD_MASK: u32 = 0o010017;
const BOTHER: u32 = 0o010000;

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

fn set_trace_options(pid: pid_t) {
    unsafe {
        raw_ptrace(
            PTRACE_SETOPTIONS,
            pid,
            std::ptr::null_mut(),
            TRACE_OPTIONS as *mut c_void,
        );
    }
}

fn parent_trace(main_pid: pid_t) {
    // Wait for the initial SIGTRAP from execve, then enable
    // TRACESYSGOOD (unambiguous syscall-stops) and TRACECLONE/FORK/
    // VFORK (follow new threads -- see the const doc above).
    let mut status: c_int = 0;
    unsafe { libc::waitpid(main_pid, &mut status, 0) };
    set_trace_options(main_pid);

    // All threads of a process share one address space, so a single
    // /proc/<any-tid>/mem handle against the main pid works for
    // reading any traced thread's memory.
    let mut mem =
        File::open(format!("/proc/{main_pid}/mem")).expect("opening tracee /proc/pid/mem");
    let mut fd_paths: HashMap<u32, String> = HashMap::new();
    let mut other_syscalls: HashMap<u32, OtherSyscallStats> = HashMap::new();
    // Per-thread enter/exit toggle -- each traced tid alternates
    // independently, interleaved with every other tid's stops.
    let mut entering: HashMap<pid_t, bool> = HashMap::from([(main_pid, true)]);
    // Tids whose most recent entry-stop was open/openat, awaiting
    // the matching exit-stop to refresh fd_paths (see
    // log_syscall_entry's docs for why not at entry).
    let mut pending_open: HashSet<pid_t> = HashSet::new();
    let start = Instant::now();
    let mut last_summary = start;

    unsafe {
        raw_ptrace(
            PTRACE_SYSCALL,
            main_pid,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        );
    }

    loop {
        let stopped_pid = unsafe { libc::waitpid(-1, &mut status, 0) };
        if stopped_pid < 0 {
            break; // no more tracees left at all
        }

        if libc::WIFEXITED(status) || libc::WIFSIGNALED(status) {
            entering.remove(&stopped_pid);
            if stopped_pid == main_pid {
                println!(
                    "[{:>8.3}] main thread exited",
                    start.elapsed().as_secs_f64()
                );
                break;
            }
            continue;
        }

        if !libc::WIFSTOPPED(status) {
            continue;
        }

        // A ptrace event stop (new thread/fork/vfork) encodes as
        // status>>8 == (SIGTRAP | (event<<8)), per ptrace(2). Handled
        // before the syscall-stop check below since it's a distinct
        // stop reason, not a syscall boundary for `stopped_pid` itself.
        let raw_event = (status >> 8) & 0xff;
        if libc::WSTOPSIG(status) == libc::SIGTRAP && raw_event != 0 {
            let mut new_tid: libc::c_ulong = 0;
            unsafe {
                raw_ptrace(
                    PTRACE_GETEVENTMSG,
                    stopped_pid,
                    std::ptr::null_mut(),
                    &mut new_tid as *mut libc::c_ulong as *mut c_void,
                );
            }
            let new_tid = new_tid as pid_t;
            if new_tid > 0 {
                set_trace_options(new_tid);
                entering.insert(new_tid, true);
                println!(
                    "[{:>8.3}] new thread/process {new_tid}",
                    start.elapsed().as_secs_f64()
                );
            }
            unsafe {
                raw_ptrace(
                    PTRACE_SYSCALL,
                    stopped_pid,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                );
            }
            continue;
        }

        let stopped_by_syscall = libc::WSTOPSIG(status) == (libc::SIGTRAP | 0x80);
        if !stopped_by_syscall {
            let sig = libc::WSTOPSIG(status);
            // A bare SIGTRAP here (as opposed to SIGTRAP|0x80) is
            // ptrace machinery, not a real signal -- e.g. a newly
            // cloned thread's very first stop. Swallow it (data=0)
            // rather than re-injecting a signal the tracee never
            // actually sent itself; forward anything else untouched.
            let inject = if sig == libc::SIGTRAP { 0 } else { sig };
            unsafe {
                raw_ptrace(
                    PTRACE_SYSCALL,
                    stopped_pid,
                    std::ptr::null_mut(),
                    inject as *mut c_void,
                );
            }
            continue;
        }

        let was_entering = *entering.entry(stopped_pid).or_insert(true);
        if was_entering {
            if let Some(regs) = get_regs(stopped_pid) {
                let is_open = log_syscall_entry(
                    &mut mem,
                    &regs,
                    &fd_paths,
                    &mut other_syscalls,
                    start.elapsed(),
                );
                if is_open {
                    pending_open.insert(stopped_pid);
                }
            }
        } else if pending_open.remove(&stopped_pid) {
            // The open/openat this tid entered has now completed;
            // its fd (if any) exists and is safe to resolve.
            refresh_fd_paths(main_pid, &mut fd_paths);
        }
        entering.insert(stopped_pid, !was_entering);

        unsafe {
            raw_ptrace(
                PTRACE_SYSCALL,
                stopped_pid,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            );
        }

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

/// Logs one syscall entry; returns `true` for `open`/`openat` so the
/// caller can refresh the fd -> path map on the *matching exit*, not
/// here. The fd isn't allocated until the syscall actually completes
/// -- refreshing at entry raced real open/close/write interleaving
/// from other threads, sometimes attributing a write to a stale,
/// reused fd number from an entirely different, already-closed file.
fn log_syscall_entry(
    mem: &mut File,
    regs: &ArmRegs,
    fd_paths: &HashMap<u32, String>,
    other_syscalls: &mut HashMap<u32, OtherSyscallStats>,
    elapsed: Duration,
) -> bool {
    let nr = regs.syscall_nr();
    let (a0, a1, a2) = (regs.arg(0), regs.arg(1), regs.arg(2));

    match nr {
        SYS_OPEN => {
            let path = read_cstr(mem, a0);
            println!("[{:>8.3}] open({path:?})", elapsed.as_secs_f64());
            return true;
        }
        SYS_OPENAT => {
            let path = read_cstr(mem, a1);
            println!("[{:>8.3}] openat(.., {path:?})", elapsed.as_secs_f64());
            return true;
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
        SYS_IOCTL if a1 == TCSETS2 || a1 == TCSETS => {
            // Both are "set" ioctls: the caller fully populates the
            // termios struct *before* the kernel touches it, so it's
            // safe to read at entry (unlike a GET ioctl, where the
            // kernel fills the struct in as a side effect -- would
            // need an exit-time read this tracer doesn't implement).
            let path = fd_paths
                .get(&a0)
                .cloned()
                .unwrap_or_else(|| format!("fd{a0}"));
            let which = if a1 == TCSETS2 { "TCSETS2" } else { "TCSETS" };
            let raw = read_bytes(mem, a2, TERMIOS2_LEN);
            if let Some(baud) = decode_termios2_baud(&raw) {
                println!(
                    "[{:>8.3}] ioctl({path}, {which}) -> baud {baud}",
                    elapsed.as_secs_f64()
                );
            }
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
    false
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

/// Decode the requested baud rate out of a raw `struct termios2`
/// (see `TERMIOS2_LEN`'s doc comment for the field layout). Returns
/// `None` if `raw` is short (a failed/partial memory read).
fn decode_termios2_baud(raw: &[u8]) -> Option<u32> {
    if raw.len() < TERMIOS2_LEN {
        return None;
    }
    let cflag = u32::from_le_bytes(raw[8..12].try_into().unwrap());
    let ispeed = u32::from_le_bytes(raw[36..40].try_into().unwrap());
    let cbaud = cflag & CBAUD_MASK;
    if cbaud == BOTHER {
        // Custom rate (the only way to express something like
        // 3,125,000, which has no legacy Bxxx constant) -- c_ispeed
        // carries the actual value directly, not an index.
        return Some(ispeed);
    }
    // Standard Bxxx encoding (asm-generic/termbits.h); only the rates
    // plausibly relevant to a BM13xx power-on/ramp sequence.
    Some(match cbaud {
        0o000000 => 0,
        0o000015 => 9_600,
        0o000016 => 19_200,
        0o000017 => 38_400,
        0o010001 => 57_600,
        0o010002 => 115_200,
        0o010003 => 230_400,
        0o010012 => 1_500_000,
        0o010013 => 2_000_000,
        0o010015 => 3_000_000,
        _ => return Some(u32::MAX - cbaud), // unrecognized -- tag distinctly rather than silently mislabel
    })
}
