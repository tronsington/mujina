//! Low-overhead variant of `s19k-trace`, purpose-built because the
//! original's PTRACE_SYSCALL step-through-every-syscall approach adds
//! enough latency (Round 4's HANDOFF.md write-up estimated ~5.6x) that
//! `bosminer`'s own internal hashchain-init timeout fires before it
//! ever reaches real operation -- confirmed directly in Round 14.5:
//! traced under the original tool, `bosminer` discovered all 231
//! chips and set baud successfully, then hit `{ERR:I8} hashchain
//! initialization timeout elapsed` on all three chains and gave up,
//! never reaching the frequency ramp we needed to observe.
//!
//! The fix: install a seccomp-bpf filter (`PTRACE_O_TRACESECCOMP`)
//! *in the tracee*, before it execs, that requests a ptrace stop only
//! for the `write` syscall and lets every other syscall (the
//! thousands/sec an async runtime makes -- epoll, timers, futexes)
//! run completely untouched. Only register writes -- a handful per
//! second at most -- ever pay the ptrace stop/continue round trip.
//! Filters are inherited across fork/clone/execve, so this covers
//! every worker thread `bosminer` spawns without per-thread setup.
//!
//! Unlike the original tool, this doesn't track fd -> path via
//! open/openat interception (that needs a second, exit-side stop per
//! open call, which would mean a mixed-mode filter). Instead, each
//! write-stop resolves `/proc/<tid>/fd/<fd>` directly via readlink --
//! the fd is already open by the time write() is called, so this is
//! simpler and doesn't need any entry/exit bookkeeping at all.
//!
//! Usage: s19k-trace-fast <program> [args...]

use std::collections::HashMap;
use std::ffi::CString;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::time::Instant;

use nix::libc::{self, c_char, c_int, c_long, c_void, pid_t};

const PTRACE_TRACEME: c_long = 0;
const PTRACE_CONT: c_long = 7;
const PTRACE_SETOPTIONS: c_long = 0x4200;
const PTRACE_GETEVENTMSG: c_long = 0x4201;
const PTRACE_GETREGS: c_long = 12;
const PTRACE_O_TRACESYSGOOD: c_long = 0x0000_0001;
const PTRACE_O_TRACEFORK: c_long = 0x0000_0002;
const PTRACE_O_TRACEVFORK: c_long = 0x0000_0004;
const PTRACE_O_TRACECLONE: c_long = 0x0000_0008;
const PTRACE_O_TRACESECCOMP: c_long = 0x0000_0080;
const TRACE_OPTIONS: c_long = PTRACE_O_TRACESYSGOOD
    | PTRACE_O_TRACEFORK
    | PTRACE_O_TRACEVFORK
    | PTRACE_O_TRACECLONE
    | PTRACE_O_TRACESECCOMP;
const PTRACE_EVENT_SECCOMP: i32 = 7;

const PR_SET_NO_NEW_PRIVS: c_int = 38;
const PR_SET_SECCOMP: c_int = 22;
const SECCOMP_MODE_FILTER: libc::c_ulong = 2;

const SYS_WRITE: u32 = 4; // ARM EABI, same guess the original tool uses

// Classic BPF opcodes (linux/filter.h), just the handful needed here.
const BPF_LD: u16 = 0x00;
const BPF_W: u16 = 0x00;
const BPF_ABS: u16 = 0x20;
const BPF_JMP: u16 = 0x05;
const BPF_JEQ: u16 = 0x10;
const BPF_K: u16 = 0x00;
const BPF_RET: u16 = 0x06;

const SECCOMP_RET_TRACE: u32 = 0x7ff0_0000;
const SECCOMP_RET_ALLOW: u32 = 0x7fff_0000;

#[repr(C)]
struct SockFilter {
    code: u16,
    jt: u8,
    jf: u8,
    k: u32,
}

#[repr(C)]
struct SockFprog {
    len: u16,
    filter: *const SockFilter,
}

/// `struct seccomp_data { int nr; __u32 arch; __u64 ip; __u64
/// args[6]; }` -- `nr` (the syscall number) is the first 4 bytes, so
/// offset 0 needs no arch-specific adjustment.
const SECCOMP_DATA_NR_OFFSET: u32 = 0;

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
        eprintln!("usage: s19k-trace-fast <program> [args...]");
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

        // Self-stop *before* installing the filter, so the parent
        // gets a chance to set PTRACE_O_TRACESECCOMP first. Installing
        // the filter before the tracer has that option armed means
        // any syscall the filter traps in that window (execve, or the
        // handful of early libc/runtime syscalls right after) gets
        // ENOSYS'd instead of a clean ptrace stop, since a
        // SECCOMP_RET_TRACE action with no armed tracer just fails
        // the syscall outright -- confirmed the hard way (Round
        // 14.5b): installing the filter here first made bosminer exit
        // instantly, before a single write() -- or even execve --
        // completed.
        libc::raise(libc::SIGSTOP);

        // Required by the kernel before an unprivileged (from
        // seccomp's point of view) filter install -- harmless for
        // root, and some kernels enforce it regardless of privilege.
        libc::prctl(PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0);

        // TEMP diagnostic (Round 14.5c): unconditional TRACE again,
        // now WITH the SIGSTOP ordering fix, to isolate whether any
        // seccomp stop happens at all vs. specifically the nr==
        // SYS_WRITE comparison being wrong.
        let program: [SockFilter; 1] = [SockFilter {
            code: BPF_RET | BPF_K,
            jt: 0,
            jf: 0,
            k: SECCOMP_RET_TRACE,
        }];
        let prog = SockFprog {
            len: program.len() as u16,
            filter: program.as_ptr(),
        };
        let rc = libc::prctl(
            PR_SET_SECCOMP,
            SECCOMP_MODE_FILTER,
            &prog as *const SockFprog as libc::c_ulong,
            0,
            0,
        );
        if rc != 0 {
            eprintln!("prctl(PR_SET_SECCOMP) failed: {rc}");
            std::process::exit(1);
        }
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

fn read_regs(pid: pid_t) -> ArmRegs {
    let mut regs = ArmRegs::default();
    unsafe {
        raw_ptrace(
            PTRACE_GETREGS,
            pid,
            std::ptr::null_mut(),
            &mut regs as *mut ArmRegs as *mut c_void,
        );
    }
    regs
}

fn read_tracee_bytes(mem: &mut File, addr: u32, len: usize) -> Option<Vec<u8>> {
    let mut buf = vec![0u8; len];
    mem.seek(SeekFrom::Start(addr as u64)).ok()?;
    mem.read_exact(&mut buf).ok()?;
    Some(buf)
}

fn parent_trace(main_pid: pid_t) {
    let mut status: c_int = 0;
    unsafe { libc::waitpid(main_pid, &mut status, 0) };
    set_trace_options(main_pid);

    let mut mem =
        File::open(format!("/proc/{main_pid}/mem")).expect("opening tracee /proc/pid/mem");
    let mut fd_path_cache: HashMap<(pid_t, u32), String> = HashMap::new();
    let mut seccomp_stop_count: u64 = 0;
    let start = Instant::now();

    unsafe {
        raw_ptrace(
            PTRACE_CONT,
            main_pid,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        );
    }

    loop {
        let stopped_pid = unsafe { libc::waitpid(-1, &mut status, 0) };
        if stopped_pid < 0 {
            break;
        }

        if libc::WIFEXITED(status) || libc::WIFSIGNALED(status) {
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

        let raw_event = (status >> 8) & 0xff;
        let is_seccomp_stop =
            libc::WSTOPSIG(status) == libc::SIGTRAP && raw_event == PTRACE_EVENT_SECCOMP;

        // New thread/fork/vfork event (not a seccomp stop) -- same
        // handling as the original tool: start tracing it too.
        if libc::WSTOPSIG(status) == libc::SIGTRAP && raw_event != 0 && !is_seccomp_stop {
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
                println!(
                    "[{:>8.3}] new thread/process {new_tid}",
                    start.elapsed().as_secs_f64()
                );
            }
            unsafe {
                raw_ptrace(
                    PTRACE_CONT,
                    stopped_pid,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                );
            }
            continue;
        }

        if is_seccomp_stop {
            let regs = read_regs(stopped_pid);
            seccomp_stop_count += 1;
            if seccomp_stop_count <= 20 {
                println!(
                    "[{:>8.3}] DEBUG seccomp stop #{seccomp_stop_count}: pid={stopped_pid} nr={}",
                    start.elapsed().as_secs_f64(),
                    regs.syscall_nr()
                );
            }
            if regs.syscall_nr() == SYS_WRITE {
                let fd = regs.arg(0);
                let buf_addr = regs.arg(1);
                let count = regs.arg(2) as usize;

                let path = fd_path_cache
                    .entry((stopped_pid, fd))
                    .or_insert_with(|| {
                        std::fs::read_link(format!("/proc/{stopped_pid}/fd/{fd}"))
                            .map(|p| p.to_string_lossy().into_owned())
                            .unwrap_or_else(|_| format!("<fd {fd}>"))
                    })
                    .clone();

                if path.contains("ttyS") {
                    let len = count.min(256);
                    if let Some(bytes) = read_tracee_bytes(&mut mem, buf_addr, len) {
                        println!(
                            "[{:>8.3}] write({path}, len={count}) {:02x?}",
                            start.elapsed().as_secs_f64(),
                            bytes
                        );
                    }
                }
            }
            unsafe {
                raw_ptrace(
                    PTRACE_CONT,
                    stopped_pid,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                );
            }
            continue;
        }

        // Any other stop (signal delivery, etc.) -- forward the
        // signal if it looks like a real one, swallow bare SIGTRAP.
        let sig = libc::WSTOPSIG(status);
        let inject = if sig == libc::SIGTRAP { 0 } else { sig };
        unsafe {
            raw_ptrace(
                PTRACE_CONT,
                stopped_pid,
                std::ptr::null_mut(),
                inject as *mut c_void,
            );
        }
    }
}
