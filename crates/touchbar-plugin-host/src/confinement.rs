use std::{
    ffi::CString,
    io, mem,
    os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};

const LANDLOCK_CREATE_RULESET_VERSION: u32 = 1;
const LANDLOCK_RULE_PATH_BENEATH: u32 = 1;

const FS_EXECUTE: u64 = 1 << 0;
const FS_WRITE_FILE: u64 = 1 << 1;
const FS_READ_FILE: u64 = 1 << 2;
const FS_READ_DIR: u64 = 1 << 3;
const FS_REMOVE_DIR: u64 = 1 << 4;
const FS_REMOVE_FILE: u64 = 1 << 5;
const FS_MAKE_CHAR: u64 = 1 << 6;
const FS_MAKE_DIR: u64 = 1 << 7;
const FS_MAKE_REG: u64 = 1 << 8;
const FS_MAKE_SOCK: u64 = 1 << 9;
const FS_MAKE_FIFO: u64 = 1 << 10;
const FS_MAKE_BLOCK: u64 = 1 << 11;
const FS_MAKE_SYM: u64 = 1 << 12;
const FS_REFER: u64 = 1 << 13;
const FS_TRUNCATE: u64 = 1 << 14;
const FS_IOCTL_DEV: u64 = 1 << 15;
const FS_RESOLVE_UNIX: u64 = 1 << 16;

const NET_BIND_TCP: u64 = 1 << 0;
const NET_CONNECT_TCP: u64 = 1 << 1;
const SCOPE_ABSTRACT_UNIX_SOCKET: u64 = 1 << 0;
const SCOPE_SIGNAL: u64 = 1 << 1;

const BPF_LD_W_ABS: u16 = 0x20;
const BPF_JMP_JEQ_K: u16 = 0x15;
const BPF_RET_K: u16 = 0x06;
const SECCOMP_RET_KILL_PROCESS: u32 = 0x8000_0000;
const SECCOMP_RET_ERRNO: u32 = 0x0005_0000;
const SECCOMP_RET_ALLOW: u32 = 0x7fff_0000;
const SECCOMP_MODE_FILTER: libc::c_ulong = 2;

#[cfg(target_arch = "aarch64")]
const AUDIT_ARCH_NATIVE: u32 = 0xc000_00b7;
#[cfg(target_arch = "x86_64")]
const AUDIT_ARCH_NATIVE: u32 = 0xc000_003e;

#[repr(C)]
struct RulesetAttr {
    handled_access_fs: u64,
    handled_access_net: u64,
    scoped: u64,
}

#[repr(C, packed)]
struct PathBeneathAttr {
    allowed_access: u64,
    parent_fd: i32,
}

/// Applies the mandatory defense-in-depth boundary for a supervised component
/// host. Call this after sealed package bytes are loaded and before Wasmtime
/// instantiates or invokes any guest code.
pub fn apply_component_confinement(live: bool) -> Result<()> {
    set_process_security_state()?;
    apply_landlock(live)?;
    apply_seccomp(live)?;
    Ok(())
}

fn set_process_security_state() -> Result<()> {
    // SAFETY: prctl takes integer values for these operations.
    if unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) } != 0 {
        return Err(io::Error::last_os_error()).context("set no_new_privs in component host");
    }
    // Prevent same-UID debuggers and core dumps from reading component-host
    // memory, which may contain broker-returned sensitive bytes.
    // SAFETY: prctl takes integer values for this operation.
    if unsafe { libc::prctl(libc::PR_SET_DUMPABLE, 0, 0, 0, 0) } != 0 {
        return Err(io::Error::last_os_error()).context("disable component-host dumpability");
    }
    let core_limit = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: core_limit points to a valid rlimit structure.
    if unsafe { libc::setrlimit(libc::RLIMIT_CORE, &core_limit) } != 0 {
        return Err(io::Error::last_os_error()).context("disable component-host core dumps");
    }
    let descriptor_limit = libc::rlimit {
        rlim_cur: 256,
        rlim_max: 256,
    };
    // SAFETY: descriptor_limit points to a valid rlimit structure. This host
    // owns only fixed broker/artifact descriptors, GPU/Wayland descriptors,
    // and transient runtime files; it never needs an unbounded descriptor set.
    if unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &descriptor_limit) } != 0 {
        return Err(io::Error::last_os_error()).context("bound component-host descriptors");
    }
    Ok(())
}

fn apply_landlock(live: bool) -> Result<()> {
    // SAFETY: a null attribute and VERSION flag query the supported ABI.
    let abi = unsafe {
        libc::syscall(
            libc::SYS_landlock_create_ruleset,
            std::ptr::null::<RulesetAttr>(),
            0,
            LANDLOCK_CREATE_RULESET_VERSION,
        )
    };
    if abi < 5 {
        if abi < 0 {
            return Err(io::Error::last_os_error())
                .context("Landlock is required for supervised components");
        }
        bail!("Landlock ABI 5 or newer is required; kernel provides ABI {abi}");
    }

    let mut handled_fs = FS_EXECUTE
        | FS_WRITE_FILE
        | FS_READ_FILE
        | FS_READ_DIR
        | FS_REMOVE_DIR
        | FS_REMOVE_FILE
        | FS_MAKE_CHAR
        | FS_MAKE_DIR
        | FS_MAKE_REG
        | FS_MAKE_SOCK
        | FS_MAKE_FIFO
        | FS_MAKE_BLOCK
        | FS_MAKE_SYM
        | FS_REFER
        | FS_TRUNCATE
        | FS_IOCTL_DEV;
    if abi >= 9 {
        handled_fs |= FS_RESOLVE_UNIX;
    }
    let attributes = RulesetAttr {
        handled_access_fs: handled_fs,
        handled_access_net: NET_BIND_TCP | NET_CONNECT_TCP,
        scoped: if abi >= 6 {
            SCOPE_ABSTRACT_UNIX_SOCKET | SCOPE_SIGNAL
        } else {
            0
        },
    };
    let attribute_size = if abi >= 6 {
        mem::size_of::<RulesetAttr>()
    } else {
        2 * mem::size_of::<u64>()
    };
    // SAFETY: attributes is initialized for attribute_size bytes.
    let ruleset = unsafe {
        libc::syscall(
            libc::SYS_landlock_create_ruleset,
            &attributes,
            attribute_size,
            0,
        )
    };
    if ruleset < 0 {
        return Err(io::Error::last_os_error()).context("create component Landlock ruleset");
    }
    // SAFETY: the syscall returned one uniquely owned descriptor.
    let ruleset = unsafe { OwnedFd::from_raw_fd(ruleset as RawFd) };

    let read_access = FS_READ_FILE | FS_READ_DIR;
    for path in [
        Path::new("/usr/lib"),
        Path::new("/usr/share/fonts"),
        Path::new("/usr/share/glvnd"),
        Path::new("/usr/share/drirc.d"),
        Path::new("/etc/fonts"),
        Path::new("/sys"),
    ] {
        add_existing_path_rule(ruleset.as_raw_fd(), path, read_access)?;
    }
    add_existing_path_rule(
        ruleset.as_raw_fd(),
        Path::new("/etc/ld.so.cache"),
        FS_READ_FILE,
    )?;
    add_existing_path_rule(
        ruleset.as_raw_fd(),
        Path::new("/dev/dri"),
        read_access | FS_WRITE_FILE | FS_IOCTL_DEV,
    )?;
    add_existing_path_rule(
        ruleset.as_raw_fd(),
        Path::new("/dev/udmabuf"),
        FS_READ_FILE | FS_WRITE_FILE | FS_IOCTL_DEV,
    )?;
    if live {
        if abi < 9 {
            bail!("live supervised components require Landlock ABI 9 for exact Unix sockets");
        }
        let socket = validated_wayland_socket()?;
        add_existing_path_rule(ruleset.as_raw_fd(), &socket, FS_RESOLVE_UNIX)
            .with_context(|| format!("allow compositor socket {}", socket.display()))?;
    }

    // SAFETY: ruleset is valid, no_new_privs is set, and flags zero is the
    // supported operation on ABI 5-9.
    if unsafe { libc::syscall(libc::SYS_landlock_restrict_self, ruleset.as_raw_fd(), 0) } != 0 {
        return Err(io::Error::last_os_error()).context("enter component Landlock domain");
    }
    Ok(())
}

fn validated_wayland_socket() -> Result<PathBuf> {
    let runtime = std::env::var_os("XDG_RUNTIME_DIR").context("XDG_RUNTIME_DIR is required")?;
    let runtime = PathBuf::from(runtime);
    if !runtime.is_absolute() {
        bail!("XDG_RUNTIME_DIR must be absolute");
    }
    let display = std::env::var_os("WAYLAND_DISPLAY").context("WAYLAND_DISPLAY is required")?;
    let display = Path::new(&display);
    if display.components().count() != 1 || display.as_os_str().is_empty() {
        bail!("WAYLAND_DISPLAY must be one relative path component");
    }
    let socket = runtime.join(display);
    let metadata = std::fs::symlink_metadata(&socket)
        .with_context(|| format!("inspect compositor socket {}", socket.display()))?;
    if !std::os::unix::fs::FileTypeExt::is_socket(&metadata.file_type()) {
        bail!("WAYLAND_DISPLAY does not identify a Unix socket");
    }
    Ok(socket)
}

fn add_existing_path_rule(ruleset: RawFd, path: &Path, allowed_access: u64) -> Result<()> {
    let path = CString::new(path.as_os_str().as_encoded_bytes())
        .context("Landlock path contains a nul byte")?;
    // SAFETY: path is nul-terminated; O_PATH does not access file content.
    let descriptor = unsafe { libc::open(path.as_ptr(), libc::O_PATH | libc::O_CLOEXEC) };
    if descriptor < 0 {
        let error = io::Error::last_os_error();
        if error.kind() == io::ErrorKind::NotFound {
            return Ok(());
        }
        return Err(error)
            .with_context(|| format!("open Landlock path {}", path.to_string_lossy()));
    }
    // SAFETY: open returned one uniquely owned descriptor.
    let descriptor = unsafe { OwnedFd::from_raw_fd(descriptor) };
    let attributes = PathBeneathAttr {
        allowed_access,
        parent_fd: descriptor.as_raw_fd(),
    };
    // SAFETY: both descriptors and the packed rule attributes are valid.
    if unsafe {
        libc::syscall(
            libc::SYS_landlock_add_rule,
            ruleset,
            LANDLOCK_RULE_PATH_BENEATH,
            &attributes,
            0,
        )
    } != 0
    {
        return Err(io::Error::last_os_error())
            .with_context(|| format!("add Landlock rule for {}", path.to_string_lossy()));
    }
    Ok(())
}

fn apply_seccomp(live: bool) -> Result<()> {
    let mut filter = vec![
        statement(BPF_LD_W_ABS, 4),
        jump(BPF_JMP_JEQ_K, AUDIT_ARCH_NATIVE, 1, 0),
        statement(BPF_RET_K, SECCOMP_RET_KILL_PROCESS),
        statement(BPF_LD_W_ABS, 0),
    ];
    add_sensitive_prctl_rule(&mut filter);

    let mut blocked = blocked_syscalls();
    if live {
        // Mesa is permitted to create threads, but never a child process.
        // Returning ENOSYS for clone3 makes libc use inspectable legacy clone.
        add_errno_rule(&mut filter, libc::SYS_clone3, libc::ENOSYS);
        add_thread_clone_only_rule(&mut filter);
    } else {
        // Headless hosts need no newly-created socket. Landlock ABI 5 cannot
        // scope pathname or abstract Unix connects, so allowing AF_UNIX here
        // would create an ambient desktop-service escape hatch.
        blocked.extend([
            libc::SYS_clone,
            libc::SYS_clone3,
            libc::SYS_socket,
            libc::SYS_socketpair,
        ]);
    }
    for syscall in blocked {
        filter.push(jump(BPF_JMP_JEQ_K, syscall as u32, 0, 1));
        filter.push(statement(BPF_RET_K, SECCOMP_RET_ERRNO | libc::EPERM as u32));
    }
    if live {
        // Live mode requires a new Wayland AF_UNIX connection. Landlock ABI 9
        // independently restricts it to the exact supervisor-selected socket.
        add_unix_socket_only_rule(&mut filter, libc::SYS_socket);
        add_unix_socket_only_rule(&mut filter, libc::SYS_socketpair);
    }
    filter.push(statement(BPF_RET_K, SECCOMP_RET_ALLOW));

    let program = libc::sock_fprog {
        len: u16::try_from(filter.len()).context("seccomp program is too large")?,
        filter: filter.as_mut_ptr(),
    };
    // SAFETY: program references a live, initialized BPF array for this call.
    if unsafe {
        libc::prctl(
            libc::PR_SET_SECCOMP,
            SECCOMP_MODE_FILTER,
            &program as *const libc::sock_fprog,
        )
    } != 0
    {
        return Err(io::Error::last_os_error()).context("install component seccomp filter");
    }
    Ok(())
}

fn blocked_syscalls() -> Vec<libc::c_long> {
    let syscalls = vec![
        libc::SYS_execve,
        libc::SYS_execveat,
        libc::SYS_kill,
        libc::SYS_tkill,
        libc::SYS_tgkill,
        libc::SYS_rt_sigqueueinfo,
        libc::SYS_rt_tgsigqueueinfo,
        libc::SYS_pidfd_open,
        libc::SYS_pidfd_getfd,
        libc::SYS_pidfd_send_signal,
        libc::SYS_ptrace,
        libc::SYS_process_vm_readv,
        libc::SYS_process_vm_writev,
        libc::SYS_process_madvise,
        libc::SYS_kcmp,
        libc::SYS_prlimit64,
        libc::SYS_setpriority,
        libc::SYS_sched_setaffinity,
        libc::SYS_sched_setscheduler,
        libc::SYS_sched_setparam,
        libc::SYS_ioprio_set,
        libc::SYS_mount,
        libc::SYS_umount2,
        libc::SYS_pivot_root,
        libc::SYS_chroot,
        libc::SYS_unshare,
        libc::SYS_setns,
        libc::SYS_bpf,
        libc::SYS_perf_event_open,
        libc::SYS_userfaultfd,
        libc::SYS_keyctl,
        libc::SYS_add_key,
        libc::SYS_request_key,
        libc::SYS_init_module,
        libc::SYS_finit_module,
        libc::SYS_delete_module,
        libc::SYS_kexec_load,
        libc::SYS_reboot,
        libc::SYS_swapon,
        libc::SYS_swapoff,
        libc::SYS_open_by_handle_at,
        libc::SYS_name_to_handle_at,
        libc::SYS_fanotify_init,
        libc::SYS_io_uring_setup,
        libc::SYS_io_uring_register,
        libc::SYS_io_uring_enter,
        libc::SYS_personality,
        libc::SYS_acct,
    ];
    #[cfg(target_arch = "x86_64")]
    let syscalls = {
        let mut syscalls = syscalls;
        syscalls.extend([libc::SYS_fork, libc::SYS_vfork]);
        syscalls
    };
    syscalls
}

fn add_errno_rule(filter: &mut Vec<libc::sock_filter>, syscall: libc::c_long, errno: i32) {
    filter.push(jump(BPF_JMP_JEQ_K, syscall as u32, 0, 1));
    filter.push(statement(BPF_RET_K, SECCOMP_RET_ERRNO | errno as u32));
}

fn add_thread_clone_only_rule(filter: &mut Vec<libc::sock_filter>) {
    const BPF_ALU_AND_K: u16 = 0x54;
    const SECCOMP_ARG_ZERO_LOW: u32 = 16;
    let required = (libc::CLONE_THREAD | libc::CLONE_SIGHAND | libc::CLONE_VM) as u32;
    filter.push(jump(BPF_JMP_JEQ_K, libc::SYS_clone as u32, 0, 4));
    filter.push(statement(BPF_LD_W_ABS, SECCOMP_ARG_ZERO_LOW));
    filter.push(statement(BPF_ALU_AND_K, required));
    filter.push(jump(BPF_JMP_JEQ_K, required, 1, 0));
    filter.push(statement(BPF_RET_K, SECCOMP_RET_ERRNO | libc::EPERM as u32));
}

fn add_sensitive_prctl_rule(filter: &mut Vec<libc::sock_filter>) {
    const SECCOMP_ARG_ZERO_LOW: u32 = 16;
    filter.push(jump(BPF_JMP_JEQ_K, libc::SYS_prctl as u32, 0, 5));
    filter.push(statement(BPF_LD_W_ABS, SECCOMP_ARG_ZERO_LOW));
    filter.push(jump(BPF_JMP_JEQ_K, libc::PR_SET_DUMPABLE as u32, 2, 0));
    filter.push(jump(BPF_JMP_JEQ_K, libc::PR_SET_PTRACER as u32, 1, 0));
    filter.push(jump(BPF_JMP_JEQ_K, libc::PR_SET_PDEATHSIG as u32, 0, 1));
    filter.push(statement(BPF_RET_K, SECCOMP_RET_ERRNO | libc::EPERM as u32));
}

fn add_unix_socket_only_rule(filter: &mut Vec<libc::sock_filter>, syscall: libc::c_long) {
    filter.push(jump(BPF_JMP_JEQ_K, syscall as u32, 0, 3));
    filter.push(statement(BPF_LD_W_ABS, 16));
    filter.push(jump(BPF_JMP_JEQ_K, libc::AF_UNIX as u32, 1, 0));
    filter.push(statement(
        BPF_RET_K,
        SECCOMP_RET_ERRNO | libc::EAFNOSUPPORT as u32,
    ));
}

const fn statement(code: u16, value: u32) -> libc::sock_filter {
    libc::sock_filter {
        code,
        jt: 0,
        jf: 0,
        k: value,
    }
}

const fn jump(code: u16, value: u32, jt: u8, jf: u8) -> libc::sock_filter {
    libc::sock_filter {
        code,
        jt,
        jf,
        k: value,
    }
}

#[cfg(test)]
mod tests {
    use std::{fs, process::Command};

    use tempfile::tempdir;

    use super::apply_component_confinement;

    #[test]
    fn supervised_boundary_denies_ambient_files_network_exec_and_kernel_authority() {
        const CHILD: &str = "TOUCHBAR_CONFINEMENT_TEST_CHILD";
        if std::env::var_os(CHILD).is_some() {
            child_probe();
            return;
        }
        let directory = tempdir().unwrap();
        let forbidden = directory.path().join("secret");
        fs::write(&forbidden, b"secret").unwrap();
        let output = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "confinement::tests::supervised_boundary_denies_ambient_files_network_exec_and_kernel_authority",
                "--nocapture",
            ])
            .env(CHILD, "1")
            .env("TOUCHBAR_FORBIDDEN_PATH", &forbidden)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "child failed:\nstdout={}\nstderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn child_probe() {
        let forbidden = std::env::var_os("TOUCHBAR_FORBIDDEN_PATH").unwrap();
        apply_component_confinement(false).unwrap();

        assert_eq!(
            fs::read(&forbidden).unwrap_err().kind(),
            std::io::ErrorKind::PermissionDenied
        );
        assert_eq!(
            fs::read("/etc/passwd").unwrap_err().kind(),
            std::io::ErrorKind::PermissionDenied
        );
        assert!(fs::read_dir("/usr/lib").is_ok());

        // Headless mode blocks all new sockets before address-family handling.
        // SAFETY: socket has no pointer arguments.
        let internet = unsafe { libc::socket(libc::AF_INET, libc::SOCK_STREAM, 0) };
        assert_eq!(internet, -1);
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::EPERM)
        );
        // Headless mode needs no new socket. In particular this closes the
        // Unix-domain gap on kernels that provide Landlock ABI 5-8.
        // SAFETY: socket has no pointer arguments.
        let unix = unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_STREAM, 0) };
        assert_eq!(unix, -1);
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::EPERM)
        );

        // The filter returns before dereferencing arguments for blocked calls.
        // SAFETY: seccomp rejects both calls with EPERM before implementation.
        assert_eq!(
            unsafe {
                libc::syscall(
                    libc::SYS_execve,
                    std::ptr::null::<libc::c_char>(),
                    std::ptr::null::<*const libc::c_char>(),
                    std::ptr::null::<*const libc::c_char>(),
                )
            },
            -1
        );
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::EPERM)
        );
        // SAFETY: seccomp rejects ptrace before interpreting remaining args.
        assert_eq!(unsafe { libc::syscall(libc::SYS_ptrace, 0, 0, 0, 0) }, -1);
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::EPERM)
        );
        // A compromise cannot create descendants, signal another same-user
        // process, or undo the nondumpable state.
        // SAFETY: fork takes no arguments and seccomp rejects its clone syscall.
        assert_eq!(unsafe { libc::fork() }, -1);
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::EPERM)
        );
        // SAFETY: signal zero is normally a read-only existence check; seccomp
        // rejects kill before it interprets either scalar argument.
        assert_eq!(unsafe { libc::kill(libc::getppid(), 0) }, -1);
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::EPERM)
        );
        // SAFETY: the filter rejects prctl before it can change dumpability.
        assert_eq!(unsafe { libc::prctl(libc::PR_SET_DUMPABLE, 1) }, -1);
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::EPERM)
        );
        // `prctl` is unavailable after the filter is installed, so the
        // successfully established nondumpable state cannot be reversed.
        // `getrlimit` is implemented with the now-blocked cross-process
        // `prlimit64` syscall on this libc, so exercise the descriptor ceiling
        // directly using an allowed read-only runtime file.
        let mut descriptors = Vec::new();
        let error = loop {
            match fs::File::open("/etc/ld.so.cache") {
                Ok(file) if descriptors.len() < 300 => descriptors.push(file),
                Ok(_) => panic!("descriptor ceiling was not enforced"),
                Err(error) => break error,
            }
        };
        assert_eq!(error.raw_os_error(), Some(libc::EMFILE));
        assert!(descriptors.len() < 256);
    }
}
