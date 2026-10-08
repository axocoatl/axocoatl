//! The seccomp filter `--serve --harden` installs in each launched command
//! (Linux only), after its Landlock restriction and before `execve`, with
//! `PR_SET_NO_NEW_PRIVS`. Every descendant inherits it.
//!
//! It is a denylist on top of the container's own seccomp policy. These calls
//! fail with `EPERM`: `ptrace`, `process_vm_readv`, `process_vm_writev`,
//! `userfaultfd`, `perf_event_open`, `bpf`, `pidfd_getfd`, `kexec_load`,
//! `kexec_file_load`, `init_module`, `finit_module`, `delete_module`,
//! `add_key`, `request_key`, `keyctl`, `mount`, `umount2`, `pivot_root`,
//! `fsopen`, `fsconfig`, `fsmount`, `fspick`, `move_mount`, `open_tree`,
//! `setns`, `open_by_handle_at`; `unshare` and `clone` with `CLONE_NEWUSER`;
//! `socket` for `AF_PACKET`, `AF_VSOCK`, `AF_BLUETOOTH` and `AF_KEY`.
//! `clone3` and the `io_uring_*` calls fail with `ENOSYS`, so the C library
//! and libuv fall back to `clone` and ordinary system calls (the flags of
//! `clone3` sit in memory a filter cannot read). `memfd_create` stays
//! allowed.
//!
//! A read-only helper's filter ([`Filter::helper`]) also refuses, with
//! `EPERM`, `socket` for `AF_UNIX`, `socketpair` of any type but
//! `SOCK_STREAM` and `SOCK_SEQPACKET`, `inotify_add_watch`, `fanotify_init`
//! and `fanotify_mark`, setting or removing extended attributes (`setxattr`,
//! `lsetxattr`, `fsetxattr`, `setxattrat`, `removexattr`, `lremovexattr`,
//! `fremovexattr`, `removexattrat`), and every System V IPC and POSIX
//! message queue call (`shmget`, `shmat`, `shmdt`, `shmctl`, `msgget`,
//! `msgsnd`, `msgrcv`, `msgctl`, `semget`, `semop`, `semtimedop`, `semctl`,
//! `mq_open`, `mq_unlink`, `mq_timedsend`, `mq_timedreceive`, `mq_notify`,
//! `mq_getsetattr`). Those objects live outside any file system Landlock
//! covers, are shared by every user of the container and outlast the
//! command, so a helper could otherwise leave state behind there, or read
//! and change a writer's object whose mode lets any user. The helper holds
//! `CAP_DAC_READ_SEARCH`,
//! which lets it pass directories it could not enter before; Landlock keeps
//! it from opening, listing or executing anything there, but covers neither
//! connecting to (or sending to) a Unix socket by path nor watching a path,
//! nor extended attributes. Without these calls it can do none of them,
//! anywhere. Stream and sequenced-packet socket pairs, which are connected,
//! have no address and cannot send to one, stay allowed (Rust's standard
//! library, and so Cargo, uses a sequenced-packet pair to start processes);
//! a datagram pair could send to any Unix socket's path.
//!
//! A helper's file tools (`read_file`, `grep`, ...), which run without a
//! write restriction, get [`Filter::helper_file_tools`]: the helper's, which
//! also refuses changing a file's mode, owner or times (`chmod`, `fchmod`,
//! `fchmodat`, `fchmodat2`, `chown`, `fchown`, `lchown`, `fchownat`, `utime`,
//! `utimes`, `futimesat`, `utimensat`, where the architecture has them),
//! and opening any socket (`socket`, whatever its family; connected pairs
//! stay allowed). Landlock covers none of these; with them refused, and
//! Landlock refusing every write right, a file tool changes nothing, and
//! it reaches no TCP or UDP address, the egress proxy's loopback relay
//! included (the file tools run without the shell's TCP restriction, which
//! needs Landlock ABI 4).
//!
//! A call made under another architecture's numbering (32-bit compatibility
//! calls) kills the process, and on x86_64 every x32 call fails with `EPERM`,
//! so neither can step around the list.
//!
//! The filter cannot refuse reading another process's memory or environment
//! through `/proc/<pid>/mem` and `environ`: those are file reads. The kernel
//! guards them with a ptrace access check, which Landlock refuses for any
//! process outside the caller's domain. So `--harden` always launches the
//! command in a Landlock domain (its write restriction's, or one that only
//! refuses creating block devices) and refuses to launch it without Landlock.
//! Processes the command starts share its domain and stay reachable to it.

use std::io;

/// One classic BPF instruction (`struct sock_filter`).
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Instruction {
    pub code: u16,
    pub jt: u8,
    pub jf: u8,
    pub k: u32,
}

/// `struct sock_fprog`.
#[repr(C)]
struct Program {
    len: libc::c_ushort,
    filter: *const Instruction,
}

const LOAD_WORD: u16 = 0x20; // BPF_LD | BPF_W | BPF_ABS
const JUMP_EQUAL: u16 = 0x15; // BPF_JMP | BPF_JEQ | BPF_K
const JUMP_AT_LEAST: u16 = 0x35; // BPF_JMP | BPF_JGE | BPF_K
const JUMP_ANY_BIT: u16 = 0x45; // BPF_JMP | BPF_JSET | BPF_K
const AND: u16 = 0x54; // BPF_ALU | BPF_AND | BPF_K
const RETURN: u16 = 0x06; // BPF_RET | BPF_K

pub const RET_KILL_PROCESS: u32 = 0x8000_0000;
pub const RET_ERRNO: u32 = 0x0005_0000;
pub const RET_ALLOW: u32 = 0x7fff_0000;

/// `struct seccomp_data` offsets.
pub const NR_OFFSET: u32 = 0;
pub const ARCH_OFFSET: u32 = 4;
/// The low 32 bits of the first argument.
#[cfg(target_endian = "little")]
pub const ARG0_LOW_OFFSET: u32 = 16;
#[cfg(target_endian = "big")]
pub const ARG0_LOW_OFFSET: u32 = 20;
/// The low 32 bits of the second argument.
#[cfg(target_endian = "little")]
pub const ARG1_LOW_OFFSET: u32 = 24;
#[cfg(target_endian = "big")]
pub const ARG1_LOW_OFFSET: u32 = 28;

pub const X32_SYSCALL_BIT: u32 = 0x4000_0000;
const SECCOMP_MODE_FILTER: libc::c_ulong = 2;
const PR_SET_SECCOMP: libc::c_int = 22;

#[cfg(target_arch = "x86_64")]
pub const AUDIT_ARCH: u32 = 0xC000_003E;
#[cfg(target_arch = "aarch64")]
pub const AUDIT_ARCH: u32 = 0xC000_00B7;

// musl's aarch64 table in libc omits it; this is the generic number.
#[cfg(all(target_arch = "aarch64", target_env = "musl"))]
const SYS_KEXEC_FILE_LOAD: libc::c_long = 294;
#[cfg(not(all(target_arch = "aarch64", target_env = "musl")))]
const SYS_KEXEC_FILE_LOAD: libc::c_long = libc::SYS_kexec_file_load;

/// `fchmodat2` (Linux 6.6), `setxattrat` and `removexattrat` (Linux 6.13):
/// numbered alike on every architecture.
const SYS_FCHMODAT2: libc::c_long = 452;
const SYS_SETXATTRAT: libc::c_long = 463;
const SYS_REMOVEXATTRAT: libc::c_long = 466;

/// Calls refused with `EPERM` whatever their arguments.
pub fn denied_calls() -> Vec<u32> {
    [
        libc::SYS_ptrace,
        libc::SYS_process_vm_readv,
        libc::SYS_process_vm_writev,
        libc::SYS_userfaultfd,
        libc::SYS_perf_event_open,
        libc::SYS_bpf,
        libc::SYS_pidfd_getfd,
        libc::SYS_kexec_load,
        SYS_KEXEC_FILE_LOAD,
        libc::SYS_init_module,
        libc::SYS_finit_module,
        libc::SYS_delete_module,
        libc::SYS_add_key,
        libc::SYS_request_key,
        libc::SYS_keyctl,
        libc::SYS_mount,
        libc::SYS_umount2,
        libc::SYS_pivot_root,
        libc::SYS_fsopen,
        libc::SYS_fsconfig,
        libc::SYS_fsmount,
        libc::SYS_fspick,
        libc::SYS_move_mount,
        libc::SYS_open_tree,
        libc::SYS_setns,
        libc::SYS_open_by_handle_at,
    ]
    .iter()
    .map(|number| *number as u32)
    .collect()
}

/// What a read-only helper's commands are also refused: watching a path
/// (`inotify`, `fanotify`), setting or removing extended attributes, and
/// System V IPC and POSIX message queues ([`ipc_calls`]).
pub fn helper_denied_calls() -> Vec<u32> {
    [
        libc::SYS_inotify_add_watch,
        libc::SYS_fanotify_init,
        libc::SYS_fanotify_mark,
        libc::SYS_setxattr,
        libc::SYS_lsetxattr,
        libc::SYS_fsetxattr,
        SYS_SETXATTRAT,
        libc::SYS_removexattr,
        libc::SYS_lremovexattr,
        libc::SYS_fremovexattr,
        SYS_REMOVEXATTRAT,
    ]
    .iter()
    .map(|number| *number as u32)
    .chain(ipc_calls())
    .collect()
}

/// Every System V IPC (shared memory, message queues, semaphores) and POSIX
/// message queue call. Their objects are kept by the kernel outside any
/// path Landlock covers, shared by every user of the container's IPC
/// namespace and kept after the command ends.
pub fn ipc_calls() -> Vec<u32> {
    [
        libc::SYS_shmget,
        libc::SYS_shmat,
        libc::SYS_shmdt,
        libc::SYS_shmctl,
        libc::SYS_msgget,
        libc::SYS_msgsnd,
        libc::SYS_msgrcv,
        libc::SYS_msgctl,
        libc::SYS_semget,
        libc::SYS_semop,
        libc::SYS_semtimedop,
        libc::SYS_semctl,
        libc::SYS_mq_open,
        libc::SYS_mq_unlink,
        libc::SYS_mq_timedsend,
        libc::SYS_mq_timedreceive,
        libc::SYS_mq_notify,
        libc::SYS_mq_getsetattr,
    ]
    .iter()
    .map(|number| *number as u32)
    .collect()
}

/// What a read-only helper's file tools are also refused: changing a file's
/// mode, owner or times, and opening a socket of any family.
pub fn file_tool_denied_calls() -> Vec<u32> {
    [
        libc::SYS_fchmod,
        libc::SYS_fchmodat,
        SYS_FCHMODAT2,
        libc::SYS_fchown,
        libc::SYS_fchownat,
        libc::SYS_utimensat,
        libc::SYS_socket,
    ]
    .iter()
    .chain(LEGACY_FILE_CHANGES)
    .map(|number| *number as u32)
    .collect()
}

/// The older calls of [`file_tool_denied_calls`] that only some
/// architectures have.
#[cfg(target_arch = "x86_64")]
const LEGACY_FILE_CHANGES: &[libc::c_long] = &[
    libc::SYS_chmod,
    libc::SYS_chown,
    libc::SYS_lchown,
    libc::SYS_utime,
    libc::SYS_utimes,
    libc::SYS_futimesat,
];
#[cfg(not(target_arch = "x86_64"))]
const LEGACY_FILE_CHANGES: &[libc::c_long] = &[];

/// Calls answered `ENOSYS`, so callers fall back to older ones.
pub fn unimplemented_calls() -> Vec<u32> {
    [
        libc::SYS_clone3,
        libc::SYS_io_uring_setup,
        libc::SYS_io_uring_enter,
        libc::SYS_io_uring_register,
    ]
    .iter()
    .map(|number| *number as u32)
    .collect()
}

/// Socket families refused with `EPERM`.
pub const DENIED_FAMILIES: [u32; 4] = [
    libc::AF_PACKET as u32,
    libc::AF_VSOCK as u32,
    libc::AF_BLUETOOTH as u32,
    libc::AF_KEY as u32,
];

/// The bits of `socketpair`'s `type` that name the type (`SOCK_TYPE_MASK`);
/// the rest are flags (`SOCK_NONBLOCK`, `SOCK_CLOEXEC`).
pub const SOCKET_TYPE_MASK: u32 = 0xf;

/// The `socketpair` types a helper may make: connected pairs that cannot
/// address another socket.
pub const CONNECTED_PAIR_TYPES: [u32; 2] = [libc::SOCK_STREAM as u32, libc::SOCK_SEQPACKET as u32];

/// What the filter is built from; [`Filter::native`] uses this target's.
#[derive(Debug, Clone)]
pub struct Spec {
    pub arch: u32,
    /// Refuse x32 numbers (x86_64 only).
    pub x32: bool,
    pub denied: Vec<u32>,
    pub unimplemented: Vec<u32>,
    pub clone: u32,
    pub unshare: u32,
    pub socket: u32,
    /// Socket families `socket` refuses.
    pub denied_families: Vec<u32>,
    /// `socketpair`, refused for every type but [`CONNECTED_PAIR_TYPES`] (a
    /// helper's).
    pub connected_pairs_only: Option<u32>,
}

impl Spec {
    pub fn native() -> Self {
        Self {
            arch: AUDIT_ARCH,
            x32: cfg!(target_arch = "x86_64"),
            denied: denied_calls(),
            unimplemented: unimplemented_calls(),
            clone: libc::SYS_clone as u32,
            unshare: libc::SYS_unshare as u32,
            socket: libc::SYS_socket as u32,
            denied_families: DENIED_FAMILIES.to_vec(),
            connected_pairs_only: None,
        }
    }

    /// A read-only helper's: also no Unix sockets but connected pairs, no
    /// `inotify` or `fanotify` watches, no extended attribute changes and
    /// no System V IPC or POSIX message queues ([`helper_denied_calls`]).
    pub fn helper() -> Self {
        let mut spec = Self::native();
        spec.denied.extend(helper_denied_calls());
        spec.denied_families.push(libc::AF_UNIX as u32);
        spec.connected_pairs_only = Some(libc::SYS_socketpair as u32);
        spec
    }

    /// A read-only helper's file tools': the helper's, and no change of a
    /// file's mode, owner or times and no socket either
    /// ([`file_tool_denied_calls`]).
    pub fn helper_file_tools() -> Self {
        let mut spec = Self::helper();
        spec.denied.extend(file_tool_denied_calls());
        spec
    }
}

enum Target {
    Next,
    Eperm,
    Enosys,
    Kill,
    NewUser,
    Socket,
    SocketPair,
    Allow,
}

struct Builder {
    code: Vec<(u16, Target, Target, u32)>,
}

impl Builder {
    fn op(&mut self, code: u16, k: u32) {
        self.code.push((code, Target::Next, Target::Next, k));
    }

    fn jump(&mut self, code: u16, k: u32, jt: Target, jf: Target) {
        self.code.push((code, jt, jf, k));
    }
}

/// Build the program. Labels at the end are resolved to forward offsets.
pub fn build(spec: &Spec) -> Vec<Instruction> {
    let mut body = Builder { code: Vec::new() };
    body.op(LOAD_WORD, ARCH_OFFSET);
    body.jump(JUMP_EQUAL, spec.arch, Target::Next, Target::Kill);
    body.op(LOAD_WORD, NR_OFFSET);
    if spec.x32 {
        body.jump(JUMP_AT_LEAST, X32_SYSCALL_BIT, Target::Eperm, Target::Next);
    }
    for number in &spec.denied {
        body.jump(JUMP_EQUAL, *number, Target::Eperm, Target::Next);
    }
    for number in &spec.unimplemented {
        body.jump(JUMP_EQUAL, *number, Target::Enosys, Target::Next);
    }
    body.jump(JUMP_EQUAL, spec.clone, Target::NewUser, Target::Next);
    body.jump(JUMP_EQUAL, spec.unshare, Target::NewUser, Target::Next);
    body.jump(JUMP_EQUAL, spec.socket, Target::Socket, Target::Next);
    if let Some(socketpair) = spec.connected_pairs_only {
        body.jump(JUMP_EQUAL, socketpair, Target::SocketPair, Target::Next);
    }
    body.op(RETURN, RET_ALLOW);
    // NewUser block.
    let new_user = body.code.len();
    body.op(LOAD_WORD, ARG0_LOW_OFFSET);
    body.jump(
        JUMP_ANY_BIT,
        libc::CLONE_NEWUSER as u32,
        Target::Eperm,
        Target::Next,
    );
    body.op(RETURN, RET_ALLOW);
    // Socket block.
    let socket = body.code.len();
    body.op(LOAD_WORD, ARG0_LOW_OFFSET);
    for family in &spec.denied_families {
        body.jump(JUMP_EQUAL, *family, Target::Eperm, Target::Next);
    }
    body.op(RETURN, RET_ALLOW);
    // SocketPair block: only the connected types, whatever their flags.
    let socket_pair = body.code.len();
    body.op(LOAD_WORD, ARG1_LOW_OFFSET);
    body.op(AND, SOCKET_TYPE_MASK);
    for kind in CONNECTED_PAIR_TYPES {
        body.jump(JUMP_EQUAL, kind, Target::Allow, Target::Next);
    }
    body.op(RETURN, RET_ERRNO | libc::EPERM as u32);
    let eperm = body.code.len();
    body.op(RETURN, RET_ERRNO | libc::EPERM as u32);
    let enosys = body.code.len();
    body.op(RETURN, RET_ERRNO | libc::ENOSYS as u32);
    let kill = body.code.len();
    body.op(RETURN, RET_KILL_PROCESS);
    let allow = body.code.len();
    body.op(RETURN, RET_ALLOW);
    body.code
        .iter()
        .enumerate()
        .map(|(index, (code, jt, jf, k))| {
            let offset = |target: &Target| -> u8 {
                let to = match target {
                    Target::Next => return 0,
                    Target::Eperm => eperm,
                    Target::Enosys => enosys,
                    Target::Kill => kill,
                    Target::NewUser => new_user,
                    Target::Socket => socket,
                    Target::SocketPair => socket_pair,
                    Target::Allow => allow,
                };
                u8::try_from(to - index - 1).expect("seccomp jump within 255 instructions")
            };
            Instruction {
                code: *code,
                jt: offset(jt),
                jf: offset(jf),
                k: *k,
            }
        })
        .collect()
}

/// A built filter, prepared before `fork` so the child only makes system
/// calls.
pub struct Filter {
    instructions: Vec<Instruction>,
}

impl Filter {
    pub fn native() -> Self {
        Self {
            instructions: build(&Spec::native()),
        }
    }

    /// A read-only helper's filter ([`Spec::helper`]).
    pub fn helper() -> Self {
        Self {
            instructions: build(&Spec::helper()),
        }
    }

    /// A read-only helper's file tools' filter ([`Spec::helper_file_tools`]).
    pub fn helper_file_tools() -> Self {
        Self {
            instructions: build(&Spec::helper_file_tools()),
        }
    }

    /// Set `PR_SET_NO_NEW_PRIVS` and install the filter on the calling
    /// process. Async-signal-safe: no allocation, two system calls.
    pub fn install(&self) -> io::Result<()> {
        let program = Program {
            len: self.instructions.len() as libc::c_ushort,
            filter: self.instructions.as_ptr(),
        };
        // SAFETY: prctl with scalar arguments, then with a pointer to a
        // complete sock_fprog whose instructions outlive the call.
        unsafe {
            if libc::prctl(
                libc::PR_SET_NO_NEW_PRIVS,
                1 as libc::c_ulong,
                0 as libc::c_ulong,
                0 as libc::c_ulong,
                0 as libc::c_ulong,
            ) != 0
            {
                return Err(io::Error::last_os_error());
            }
            if libc::prctl(
                PR_SET_SECCOMP,
                SECCOMP_MODE_FILTER,
                &program as *const Program as libc::c_ulong,
                0 as libc::c_ulong,
                0 as libc::c_ulong,
            ) != 0
            {
                return Err(io::Error::last_os_error());
            }
        }
        Ok(())
    }
}

/// Run a filter over one call, as the kernel would: for tests.
pub fn evaluate(program: &[Instruction], arch: u32, nr: u32, arg0: u64) -> u32 {
    evaluate_with(program, arch, nr, [arg0, 0])
}

/// [`evaluate`] with the first two arguments.
pub fn evaluate_with(program: &[Instruction], arch: u32, nr: u32, args: [u64; 2]) -> u32 {
    let mut data = [0u8; 64];
    data[0..4].copy_from_slice(&nr.to_ne_bytes());
    data[4..8].copy_from_slice(&arch.to_ne_bytes());
    data[16..24].copy_from_slice(&args[0].to_ne_bytes());
    data[24..32].copy_from_slice(&args[1].to_ne_bytes());
    let mut accumulator = 0u32;
    let mut pc = 0usize;
    loop {
        let instruction = program[pc];
        pc += 1;
        match instruction.code {
            LOAD_WORD => {
                let at = instruction.k as usize;
                accumulator = u32::from_ne_bytes(data[at..at + 4].try_into().unwrap());
            }
            AND => accumulator &= instruction.k,
            JUMP_EQUAL | JUMP_AT_LEAST | JUMP_ANY_BIT => {
                let taken = match instruction.code {
                    JUMP_EQUAL => accumulator == instruction.k,
                    JUMP_AT_LEAST => accumulator >= instruction.k,
                    _ => accumulator & instruction.k != 0,
                };
                pc += usize::from(if taken {
                    instruction.jt
                } else {
                    instruction.jf
                });
            }
            RETURN => return instruction.k,
            other => panic!("unexpected instruction {other:#x}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn eperm() -> u32 {
        RET_ERRNO | libc::EPERM as u32
    }

    fn enosys() -> u32 {
        RET_ERRNO | libc::ENOSYS as u32
    }

    #[test]
    fn the_filter_refuses_its_list_and_allows_the_rest() {
        let spec = Spec::native();
        let program = build(&spec);
        assert!(program.len() < 100, "{}", program.len());
        let call = |nr: u32, arg0: u64| evaluate(&program, AUDIT_ARCH, nr, arg0);
        for nr in &spec.denied {
            assert_eq!(call(*nr, 0), eperm(), "{nr}");
        }
        for nr in &spec.unimplemented {
            assert_eq!(call(*nr, 0), enosys(), "{nr}");
        }
        let newuser = libc::CLONE_NEWUSER as u64;
        assert_eq!(call(spec.unshare, newuser), eperm());
        assert_eq!(
            call(spec.unshare, newuser | libc::CLONE_NEWNS as u64),
            eperm()
        );
        assert_eq!(call(spec.unshare, libc::CLONE_NEWNS as u64), RET_ALLOW);
        assert_eq!(call(spec.clone, newuser | libc::SIGCHLD as u64), eperm());
        assert_eq!(
            call(
                spec.clone,
                (libc::CLONE_VM | libc::CLONE_THREAD | libc::CLONE_SIGHAND) as u64
            ),
            RET_ALLOW
        );
        for family in DENIED_FAMILIES {
            assert_eq!(call(spec.socket, u64::from(family)), eperm());
        }
        for family in [
            libc::AF_UNIX,
            libc::AF_INET,
            libc::AF_INET6,
            libc::AF_NETLINK,
        ] {
            assert_eq!(call(spec.socket, family as u64), RET_ALLOW);
        }
        for allowed in [
            libc::SYS_read,
            libc::SYS_write,
            libc::SYS_execve,
            libc::SYS_memfd_create,
            libc::SYS_wait4,
            libc::SYS_connect,
        ] {
            assert_eq!(call(allowed as u32, 0), RET_ALLOW, "{allowed}");
        }
        // Another architecture's numbering kills the process.
        assert_eq!(
            evaluate(&program, 0x4000_0003, libc::SYS_read as u32, 0),
            RET_KILL_PROCESS
        );
    }

    /// A helper's filter refuses what the writer's does, and also Unix
    /// sockets (but connected pairs), `inotify` and `fanotify` watches,
    /// extended attribute changes, System V IPC and POSIX message queues;
    /// the writer's keeps them all.
    #[test]
    fn a_helpers_filter_also_refuses_unix_sockets_watches_and_attribute_changes() {
        let writer = build(&Spec::native());
        let helper = build(&Spec::helper());
        assert!(helper.len() < 150, "{}", helper.len());
        let call = |program: &[Instruction], nr: i64, args: [u64; 2]| {
            evaluate_with(program, AUDIT_ARCH, nr as u32, args)
        };
        for nr in Spec::native().denied {
            assert_eq!(call(&helper, nr.into(), [0, 0]), eperm(), "{nr}");
        }
        assert_eq!(call(&writer, libc::SYS_open_by_handle_at, [0, 0]), eperm());
        let unix = libc::AF_UNIX as u64;
        let stream = libc::SOCK_STREAM as u64;
        let flags = (libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK) as u64;
        let seqpacket = libc::SOCK_SEQPACKET as u64;
        for kind in [stream, stream | flags, seqpacket, seqpacket | flags] {
            assert_eq!(call(&helper, libc::SYS_socket, [unix, kind]), eperm());
            assert_eq!(call(&writer, libc::SYS_socket, [unix, kind]), RET_ALLOW);
            assert_eq!(call(&helper, libc::SYS_socketpair, [unix, kind]), RET_ALLOW);
        }
        // The type is the low four bits: a flag never turns a datagram pair
        // into an allowed one, nor high bits a refused type.
        assert_eq!(
            call(&helper, libc::SYS_socketpair, [unix, stream | 0x10]),
            RET_ALLOW
        );
        for kind in [
            libc::SOCK_DGRAM,
            libc::SOCK_RAW,
            libc::SOCK_RDM,
            libc::SOCK_DCCP,
            10, // SOCK_PACKET
            0,
            7,
            0xf,
        ] {
            let kind = kind as u64;
            assert_eq!(call(&helper, libc::SYS_socketpair, [unix, kind]), eperm());
            assert_eq!(
                call(&helper, libc::SYS_socketpair, [unix, kind | flags]),
                eperm()
            );
            assert_eq!(call(&writer, libc::SYS_socketpair, [unix, kind]), RET_ALLOW);
        }
        for family in [libc::AF_INET, libc::AF_INET6, libc::AF_NETLINK] {
            assert_eq!(
                call(&helper, libc::SYS_socket, [family as u64, stream]),
                RET_ALLOW
            );
        }
        for watch in [
            libc::SYS_inotify_add_watch,
            libc::SYS_fanotify_init,
            libc::SYS_fanotify_mark,
        ] {
            assert_eq!(call(&helper, watch, [0, 0]), eperm(), "{watch}");
            assert_eq!(call(&writer, watch, [0, 0]), RET_ALLOW, "{watch}");
        }
        for xattr in [
            libc::SYS_setxattr,
            libc::SYS_lsetxattr,
            libc::SYS_fsetxattr,
            SYS_SETXATTRAT,
            libc::SYS_removexattr,
            libc::SYS_lremovexattr,
            libc::SYS_fremovexattr,
            SYS_REMOVEXATTRAT,
        ] {
            assert_eq!(call(&helper, xattr, [0, 0]), eperm(), "{xattr}");
            assert_eq!(call(&writer, xattr, [0, 0]), RET_ALLOW, "{xattr}");
        }
        let ipc = ipc_calls();
        assert_eq!(ipc.len(), 18);
        for nr in [
            libc::SYS_shmget,
            libc::SYS_shmat,
            libc::SYS_shmctl,
            libc::SYS_msgget,
            libc::SYS_msgsnd,
            libc::SYS_msgrcv,
            libc::SYS_msgctl,
            libc::SYS_semget,
            libc::SYS_semop,
            libc::SYS_semtimedop,
            libc::SYS_semctl,
            libc::SYS_mq_open,
            libc::SYS_mq_unlink,
            libc::SYS_mq_timedsend,
            libc::SYS_mq_timedreceive,
        ] {
            assert!(ipc.contains(&(nr as u32)), "{nr}");
        }
        for nr in ipc {
            assert_eq!(call(&helper, nr.into(), [0, 0]), eperm(), "{nr}");
            assert_eq!(call(&writer, nr.into(), [0, 0]), RET_ALLOW, "{nr}");
        }
        // Reading extended attributes, and the shell's own changes of mode
        // and times in its scratch directory, stay allowed.
        for allowed in [
            libc::SYS_read,
            libc::SYS_openat,
            libc::SYS_execve,
            libc::SYS_getxattr,
            libc::SYS_fchmodat,
            libc::SYS_utimensat,
        ] {
            assert_eq!(call(&helper, allowed, [0, 0]), RET_ALLOW, "{allowed}");
        }
    }

    /// A helper's file tools' filter refuses what the helper's does, and
    /// every change of a file's mode, owner or times, which Landlock does
    /// not cover.
    #[test]
    fn a_helpers_file_tools_also_cannot_change_modes_owners_or_times() {
        let shell = build(&Spec::helper());
        let tools = build(&Spec::helper_file_tools());
        assert!(tools.len() < 160, "{}", tools.len());
        let call =
            |program: &[Instruction], nr: u32| evaluate_with(program, AUDIT_ARCH, nr, [0, 0]);
        for nr in Spec::helper().denied {
            assert_eq!(call(&tools, nr), eperm(), "{nr}");
        }
        let changes = file_tool_denied_calls();
        for nr in [
            libc::SYS_fchmod,
            libc::SYS_fchmodat,
            SYS_FCHMODAT2,
            libc::SYS_fchown,
            libc::SYS_fchownat,
            libc::SYS_utimensat,
        ] {
            assert!(changes.contains(&(nr as u32)), "{nr}");
        }
        #[cfg(target_arch = "x86_64")]
        for nr in [
            libc::SYS_chmod,
            libc::SYS_chown,
            libc::SYS_lchown,
            libc::SYS_utime,
            libc::SYS_utimes,
            libc::SYS_futimesat,
        ] {
            assert!(changes.contains(&(nr as u32)), "{nr}");
        }
        for nr in changes {
            assert_eq!(call(&tools, nr), eperm(), "{nr}");
            assert_eq!(call(&shell, nr), RET_ALLOW, "{nr}");
        }
        // No socket of any family, so no TCP or UDP address (the egress
        // proxy's loopback relay among them); the shell keeps TCP for
        // Landlock to refuse, and connected pairs stay allowed to both.
        let socket = libc::SYS_socket as u32;
        let stream = libc::SOCK_STREAM as u64;
        for family in [
            libc::AF_INET,
            libc::AF_INET6,
            libc::AF_UNIX,
            libc::AF_NETLINK,
            libc::AF_UNSPEC,
        ] {
            let args = [family as u64, stream];
            assert_eq!(evaluate_with(&tools, AUDIT_ARCH, socket, args), eperm());
        }
        for family in [libc::AF_INET, libc::AF_INET6] {
            let args = [family as u64, stream];
            assert_eq!(evaluate_with(&shell, AUDIT_ARCH, socket, args), RET_ALLOW);
        }
        let pair = [libc::AF_UNIX as u64, stream];
        let socketpair = libc::SYS_socketpair as u32;
        assert_eq!(
            evaluate_with(&tools, AUDIT_ARCH, socketpair, pair),
            RET_ALLOW
        );
        for allowed in [
            libc::SYS_read,
            libc::SYS_openat,
            libc::SYS_execve,
            libc::SYS_newfstatat,
            libc::SYS_getdents64,
            libc::SYS_wait4,
        ] {
            assert_eq!(call(&tools, allowed as u32), RET_ALLOW, "{allowed}");
        }
    }

    #[test]
    fn x32_numbers_are_refused_when_the_target_has_them() {
        let spec = Spec {
            x32: true,
            ..Spec::native()
        };
        let program = build(&spec);
        assert_eq!(
            evaluate(&program, AUDIT_ARCH, X32_SYSCALL_BIT | 101, 0),
            eperm()
        );
        assert_eq!(
            evaluate(&program, AUDIT_ARCH, libc::SYS_read as u32, 0),
            RET_ALLOW
        );
        assert_eq!(cfg!(target_arch = "x86_64"), Spec::native().x32);
    }
}
