use std::{
    collections::BTreeMap,
    ffi::{CString, OsStr},
    fs,
    io::Read,
    os::{
        fd::{AsRawFd, FromRawFd, OwnedFd, RawFd},
        unix::{
            ffi::OsStrExt,
            fs::MetadataExt,
            process::{CommandExt, ExitStatusExt},
        },
    },
    path::{Component, Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

use touchbar_broker_schema::{
    CommandEvent, CommandOpened, CommandRunRequest, CommandValue, MAX_COMMAND_OUTPUT_CHUNK_BYTES,
    SchemaError,
};
use touchbar_policy::{
    CapabilityId, CapabilityScope, CommandArgument, CommandRule, CommandRunScope,
};
use touchbar_protocol::broker_ipc::BrokerErrorCode;
use url::Url;

use crate::{
    ActivationLedger, BackendRequest, OpenedResource, ResourceBackend, ResourceEventSink,
    ResourceHandle, ResourceLimits,
};

pub const COMMAND_RUN_OPERATION: &str = "run";

const COMMAND_RESERVED_BYTES: usize = 256 * 1024;
const RESOLVE_NO_XDEV: u64 = 0x01;
const RESOLVE_NO_MAGICLINKS: u64 = 0x02;
const RESOLVE_NO_SYMLINKS: u64 = 0x04;
const RESOLVE_BENEATH: u64 = 0x08;
const PROCESS_POLL_INTERVAL: Duration = Duration::from_millis(5);
const TERMINATION_GRACE: Duration = Duration::from_millis(100);
const MAXIMUM_COMMAND_ADDRESS_SPACE_BYTES: u64 = 1024 * 1024 * 1024;
const MAXIMUM_COMMAND_OPEN_FILES: u64 = 256;
const MAXIMUM_ADDITIONAL_USER_PROCESSES: u64 = 32;
const CGROUP2_SUPER_MAGIC: libc::c_long = 0x6367_7270;
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

static NEXT_CGROUP_ID: AtomicU64 = AtomicU64::new(1);

#[repr(C)]
struct OpenHow {
    flags: u64,
    mode: u64,
    resolve: u64,
}

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

pub struct CommandRunBackend {
    package_root: PathBuf,
    active: Arc<AtomicUsize>,
}

impl CommandRunBackend {
    pub fn new(package_root: PathBuf) -> Self {
        Self {
            package_root,
            active: Arc::new(AtomicUsize::new(0)),
        }
    }
}

struct CommandHandle {
    cancelled: Arc<AtomicBool>,
    containment: Arc<ProcessContainment>,
}

impl ResourceHandle for CommandHandle {
    fn close(&mut self) {
        self.cancelled.store(true, Ordering::Release);
        self.containment.kill_all();
    }
}

struct ActivePermit {
    active: Arc<AtomicUsize>,
}

impl Drop for ActivePermit {
    fn drop(&mut self) {
        self.active.fetch_sub(1, Ordering::AcqRel);
    }
}

struct PreparedCommand {
    rule: CommandRule,
    executable: OwnedFd,
    arguments: Vec<String>,
    approved_files: Vec<OwnedFd>,
    working_directory: Option<OwnedFd>,
}

struct CommandLandlock {
    ruleset: OwnedFd,
}

struct CommandSeccomp {
    filter: Vec<libc::sock_filter>,
}

impl CommandSeccomp {
    fn create() -> Self {
        let mut filter = vec![
            bpf_statement(BPF_LD_W_ABS, 4),
            bpf_jump(BPF_JMP_JEQ_K, AUDIT_ARCH_NATIVE, 1, 0),
            bpf_statement(BPF_RET_K, SECCOMP_RET_KILL_PROCESS),
            bpf_statement(BPF_LD_W_ABS, 0),
        ];
        for syscall in blocked_command_syscalls() {
            filter.push(bpf_jump(BPF_JMP_JEQ_K, syscall as u32, 0, 1));
            filter.push(bpf_statement(
                BPF_RET_K,
                SECCOMP_RET_ERRNO | libc::EPERM as u32,
            ));
        }
        filter.push(bpf_statement(BPF_RET_K, SECCOMP_RET_ALLOW));
        Self { filter }
    }
}

impl CommandLandlock {
    fn create() -> Result<Self, BrokerErrorCode> {
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
            return Err(BrokerErrorCode::Unsupported);
        }
        let mut handled = FS_EXECUTE
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
            handled |= FS_RESOLVE_UNIX;
        }
        let attributes = RulesetAttr {
            handled_access_fs: handled,
            handled_access_net: 0,
            scoped: 0,
        };
        let attribute_size = if abi >= 6 {
            std::mem::size_of::<RulesetAttr>()
        } else {
            2 * std::mem::size_of::<u64>()
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
            return Err(BrokerErrorCode::Unavailable);
        }
        // SAFETY: the syscall returned one uniquely owned descriptor.
        let ruleset = unsafe { OwnedFd::from_raw_fd(ruleset as RawFd) };

        // Landlock is an allowlist. Preserve ordinary host-command behavior by
        // allowing the existing filesystem tree except the cgroup v2 control
        // mount. Directory listing alone on the excluded ancestors permits
        // traversal without granting reads or writes to interface files.
        add_landlock_rule(ruleset.as_raw_fd(), Path::new("/"), FS_READ_DIR)?;
        add_landlock_children_except(ruleset.as_raw_fd(), Path::new("/"), &["sys"], handled)?;
        add_landlock_rule(ruleset.as_raw_fd(), Path::new("/sys"), FS_READ_DIR)?;
        add_landlock_children_except(ruleset.as_raw_fd(), Path::new("/sys"), &["fs"], handled)?;
        add_landlock_rule(ruleset.as_raw_fd(), Path::new("/sys/fs"), FS_READ_DIR)?;
        add_landlock_children_except(
            ruleset.as_raw_fd(),
            Path::new("/sys/fs"),
            &["cgroup"],
            handled,
        )?;
        Ok(Self { ruleset })
    }
}

struct CommandCgroup {
    path: PathBuf,
    processes: OwnedFd,
    kill: OwnedFd,
}

impl CommandCgroup {
    fn create(resource_id: u64) -> Result<Self, BrokerErrorCode> {
        let parent = current_cgroup_directory()?;
        let unique = NEXT_CGROUP_ID.fetch_add(1, Ordering::Relaxed);
        let path = parent.join(format!(
            "touchbar-command-{}-{resource_id}-{unique}",
            std::process::id()
        ));
        fs::create_dir(&path).map_err(|_| BrokerErrorCode::Unavailable)?;
        let result = (|| {
            write_cgroup_setting(&path, "cgroup.max.depth", b"0")?;
            write_cgroup_setting(&path, "cgroup.max.descendants", b"0")?;
            write_optional_cgroup_setting(&path, "pids.max", b"33")?;
            let memory_limit = MAXIMUM_COMMAND_ADDRESS_SPACE_BYTES.to_string();
            write_optional_cgroup_setting(&path, "memory.max", memory_limit.as_bytes())?;
            write_optional_cgroup_setting(&path, "memory.swap.max", b"0")?;
            write_optional_cgroup_setting(&path, "memory.oom.group", b"1")?;
            let processes = open_cgroup_file(&path, "cgroup.procs", libc::O_WRONLY)?;
            let kill = open_cgroup_file(&path, "cgroup.kill", libc::O_WRONLY)?;
            Ok(Self {
                path: path.clone(),
                processes,
                kill,
            })
        })();
        if result.is_err() {
            let _ = fs::remove_dir(&path);
        }
        result
    }

    fn kill_all(&self) {
        // cgroup.kill is recursive and remains valid if a command called
        // setsid/setpgid. EBUSY/ENOENT only mean teardown already won.
        write_descriptor(self.kill.as_raw_fd(), b"1");
    }

    fn wait_empty(&self) {
        let events = self.path.join("cgroup.events");
        let deadline = Instant::now() + TERMINATION_GRACE;
        while Instant::now() < deadline {
            if fs::read_to_string(&events)
                .ok()
                .is_some_and(|value| value.lines().any(|line| line == "populated 0"))
            {
                return;
            }
            thread::sleep(PROCESS_POLL_INTERVAL);
        }
    }
}

impl Drop for CommandCgroup {
    fn drop(&mut self) {
        self.kill_all();
        self.wait_empty();
        let _ = fs::remove_dir(&self.path);
    }
}

struct ProcessContainment {
    pidfd: OwnedFd,
    cgroup: CommandCgroup,
}

impl ProcessContainment {
    fn signal_leader(&self, signal: i32) {
        // SAFETY: pidfd is live; null siginfo with flags zero is the documented
        // pidfd_send_signal form and cannot target a reused numeric PID.
        unsafe {
            libc::syscall(
                libc::SYS_pidfd_send_signal,
                self.pidfd.as_raw_fd(),
                signal,
                std::ptr::null::<libc::siginfo_t>(),
                0_u32,
            );
        }
    }

    fn kill_all(&self) {
        self.signal_leader(libc::SIGKILL);
        self.cgroup.kill_all();
    }
}

impl ResourceBackend for CommandRunBackend {
    fn limits(&self, request: &BackendRequest) -> Option<ResourceLimits> {
        (request.capability == CapabilityId::CommandRunV1
            && request.operation == COMMAND_RUN_OPERATION
            && matches!(request.authorized_scope, CapabilityScope::CommandRun(_)))
        .then_some(ResourceLimits {
            reserved_buffered_bytes: COMMAND_RESERVED_BYTES,
            maximum_events_per_second: u16::MAX,
        })
    }

    fn authorize(
        &self,
        request: &BackendRequest,
        _activations: &mut ActivationLedger,
        _now_monotonic_micros: u64,
    ) -> Result<(), BrokerErrorCode> {
        decode_rule_and_values(request).map(|_| ())
    }

    fn open(
        &self,
        resource_id: u64,
        request: &BackendRequest,
        events: ResourceEventSink,
    ) -> Result<OpenedResource, BrokerErrorCode> {
        let scope = command_scope(request)?;
        let maximum_parallel = usize::from(scope.maximum_parallel_processes);
        let permit = reserve_process(Arc::clone(&self.active), maximum_parallel)?;
        let prepared = prepare_command(request, &self.package_root)?;
        let landlock = CommandLandlock::create()?;
        let seccomp = CommandSeccomp::create();
        let cancelled = Arc::new(AtomicBool::new(false));
        let cgroup = CommandCgroup::create(resource_id)?;
        let (child, process_group) = spawn_command(&prepared, &cgroup, &landlock, &seccomp)?;
        let pidfd = match open_pidfd(child.id()) {
            Ok(pidfd) => pidfd,
            Err(error) => {
                cgroup.kill_all();
                let mut child = child;
                let _ = child.wait();
                return Err(error);
            }
        };
        let containment = Arc::new(ProcessContainment { pidfd, cgroup });
        let response_payload = CommandOpened { resource_id }
            .encode()
            .map_err(schema_error)?;
        let worker_cancelled = Arc::clone(&cancelled);
        let worker_containment = Arc::clone(&containment);
        let child_slot = Arc::new(Mutex::new(Some(child)));
        let worker_child = Arc::clone(&child_slot);
        if thread::Builder::new()
            .name(format!("touchbar-command-{resource_id}"))
            .spawn(move || {
                let Some(mut child) = worker_child.lock().ok().and_then(|mut child| child.take())
                else {
                    worker_containment.kill_all();
                    events.finish(BrokerErrorCode::Internal);
                    return;
                };
                run_command(
                    &mut child,
                    process_group,
                    prepared.rule,
                    worker_cancelled,
                    worker_containment,
                    events,
                    permit,
                );
            })
            .is_err()
        {
            containment.kill_all();
            if let Some(mut child) = child_slot.lock().ok().and_then(|mut child| child.take()) {
                let _ = child.wait();
            }
            return Err(BrokerErrorCode::Unavailable);
        }
        Ok(OpenedResource {
            handle: Box::new(CommandHandle {
                cancelled,
                containment,
            }),
            response_payload,
        })
    }
}

fn reserve_process(
    active: Arc<AtomicUsize>,
    maximum: usize,
) -> Result<ActivePermit, BrokerErrorCode> {
    let result = active.fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
        (current < maximum).then_some(current + 1)
    });
    result
        .map(|_| ActivePermit { active })
        .map_err(|_| BrokerErrorCode::QuotaExceeded)
}

fn decode_rule_and_values(
    request: &BackendRequest,
) -> Result<(CommandRule, CommandRunRequest), BrokerErrorCode> {
    if request.capability != CapabilityId::CommandRunV1
        || request.operation != COMMAND_RUN_OPERATION
    {
        return Err(BrokerErrorCode::InvalidRequest);
    }
    let scope = command_scope(request)?;
    let submitted = CommandRunRequest::decode(&request.payload).map_err(schema_error)?;
    let mut matches = scope
        .commands
        .iter()
        .filter(|rule| rule.id == submitted.command_id);
    let rule = matches.next().ok_or(BrokerErrorCode::OutOfScope)?;
    if matches.next().is_some() {
        return Err(BrokerErrorCode::InvalidRequest);
    }
    validate_values(rule, &submitted.values)?;
    Ok((rule.clone(), submitted))
}

fn validate_values(rule: &CommandRule, values: &[CommandValue]) -> Result<(), BrokerErrorCode> {
    let mut values = values
        .iter()
        .map(|value| (command_value_name(value), value))
        .collect::<BTreeMap<_, _>>();
    for argument in &rule.arguments {
        let expected_name = match argument {
            CommandArgument::Literal { .. } => continue,
            CommandArgument::BoundedInteger { name, .. }
            | CommandArgument::FixedEnum { name, .. }
            | CommandArgument::BoundedText { name, .. }
            | CommandArgument::ApprovedFile { name, .. }
            | CommandArgument::Url { name, .. } => name,
        };
        let value = values
            .remove(expected_name.as_str())
            .ok_or(BrokerErrorCode::InvalidRequest)?;
        match (argument, value) {
            (
                CommandArgument::BoundedInteger {
                    minimum, maximum, ..
                },
                CommandValue::Integer { value, .. },
            ) => {
                if value < minimum || value > maximum {
                    return Err(BrokerErrorCode::OutOfScope);
                }
            }
            (CommandArgument::FixedEnum { values, .. }, CommandValue::FixedEnum { value, .. }) => {
                if !values.contains(value) {
                    return Err(BrokerErrorCode::OutOfScope);
                }
            }
            (
                CommandArgument::BoundedText { maximum_bytes, .. },
                CommandValue::Text { value, .. },
            ) => {
                if value.len() > *maximum_bytes as usize || value.contains('\0') {
                    return Err(BrokerErrorCode::OutOfScope);
                }
            }
            (CommandArgument::ApprovedFile { .. }, CommandValue::ApprovedFile { path, .. }) => {
                if !valid_relative_path(path) {
                    return Err(BrokerErrorCode::OutOfScope);
                }
            }
            (CommandArgument::Url { schemes, .. }, CommandValue::Url { value, .. }) => {
                let url = Url::parse(value).map_err(|_| BrokerErrorCode::InvalidRequest)?;
                if !schemes.contains(url.scheme()) {
                    return Err(BrokerErrorCode::OutOfScope);
                }
            }
            _ => return Err(BrokerErrorCode::InvalidRequest),
        }
    }
    if values.is_empty() {
        Ok(())
    } else {
        Err(BrokerErrorCode::InvalidRequest)
    }
}

fn prepare_command(
    request: &BackendRequest,
    package_root: &Path,
) -> Result<PreparedCommand, BrokerErrorCode> {
    let (rule, submitted) = decode_rule_and_values(request)?;
    let mut values = submitted
        .values
        .into_iter()
        .map(|value| (command_value_name(&value).to_owned(), value))
        .collect::<BTreeMap<_, _>>();
    let executable = open_executable(Path::new(&rule.executable), package_root)?;
    let mut arguments = Vec::with_capacity(rule.arguments.len());
    let mut approved_files = Vec::new();
    for template in &rule.arguments {
        match template {
            CommandArgument::Literal { value } => arguments.push(value.clone()),
            CommandArgument::BoundedInteger { name, .. } => {
                let CommandValue::Integer { value, .. } = take_value(&mut values, name)? else {
                    return Err(BrokerErrorCode::InvalidRequest);
                };
                arguments.push(value.to_string());
            }
            CommandArgument::FixedEnum { name, .. }
            | CommandArgument::BoundedText { name, .. }
            | CommandArgument::Url { name, .. } => {
                let value = take_value(&mut values, name)?;
                let text = match value {
                    CommandValue::FixedEnum { value, .. }
                    | CommandValue::Text { value, .. }
                    | CommandValue::Url { value, .. } => value,
                    _ => return Err(BrokerErrorCode::InvalidRequest),
                };
                arguments.push(text);
            }
            CommandArgument::ApprovedFile { name, mount } => {
                let CommandValue::ApprovedFile { path, .. } = take_value(&mut values, name)? else {
                    return Err(BrokerErrorCode::InvalidRequest);
                };
                let mount_root = request
                    .bindings
                    .filesystem_mounts
                    .get(mount)
                    .ok_or(BrokerErrorCode::OutOfScope)?;
                let root = crate::filesystem::open_bound_mount(mount_root)?;
                let file = open_approved_file(root.as_raw_fd(), &path)?;
                arguments.push(format!("/proc/self/fd/{}", file.as_raw_fd()));
                approved_files.push(file);
            }
        }
    }
    let working_directory = rule
        .working_directory
        .as_deref()
        .map(Path::new)
        .map(open_directory)
        .transpose()?;
    Ok(PreparedCommand {
        rule,
        executable,
        arguments,
        approved_files,
        working_directory,
    })
}

fn spawn_command(
    prepared: &PreparedCommand,
    cgroup: &CommandCgroup,
    landlock: &CommandLandlock,
    seccomp: &CommandSeccomp,
) -> Result<(Child, libc::pid_t), BrokerErrorCode> {
    let executable_fd = prepared.executable.as_raw_fd();
    let approved_fds = prepared
        .approved_files
        .iter()
        .map(AsRawFd::as_raw_fd)
        .collect::<Vec<_>>();
    let working_directory = prepared.working_directory.as_ref().map(AsRawFd::as_raw_fd);
    let cgroup_processes = cgroup.processes.as_raw_fd();
    let landlock_ruleset = landlock.ruleset.as_raw_fd();
    let seccomp_filter = seccomp.filter.as_ptr() as usize;
    let seccomp_length =
        u16::try_from(seccomp.filter.len()).map_err(|_| BrokerErrorCode::Internal)?;
    let process_limit = current_user_process_limit()?;
    let address_space_limit = bounded_rlimit(libc::RLIMIT_AS, MAXIMUM_COMMAND_ADDRESS_SPACE_BYTES)?;
    let open_file_limit = bounded_rlimit(libc::RLIMIT_NOFILE, MAXIMUM_COMMAND_OPEN_FILES)?;
    let cpu_limit = bounded_rlimit(
        libc::RLIMIT_CPU,
        u64::from(prepared.rule.timeout_milliseconds)
            .div_ceil(1_000)
            .saturating_add(1),
    )?;
    let mut command = Command::new(format!("/proc/self/fd/{executable_fd}"));
    command
        .args(&prepared.arguments)
        .env_clear()
        .envs(&prepared.rule.environment)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // SAFETY: all calls in this closure are async-signal-safe and all captured
    // descriptors remain live until spawn returns.
    unsafe {
        command.pre_exec(move || {
            if libc::setsid() < 0 || libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            if libc::write(cgroup_processes, b"0".as_ptr().cast(), 1) != 1 {
                return Err(std::io::Error::last_os_error());
            }
            for (resource, limit) in [
                (libc::RLIMIT_AS, address_space_limit),
                (libc::RLIMIT_NOFILE, open_file_limit),
                (libc::RLIMIT_NPROC, process_limit),
                (libc::RLIMIT_CPU, cpu_limit),
                (libc::RLIMIT_CORE, 0),
            ] {
                let limit = libc::rlimit {
                    rlim_cur: limit,
                    rlim_max: limit,
                };
                if libc::setrlimit(resource, &limit) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
            }
            if clear_cloexec(executable_fd) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            for descriptor in &approved_fds {
                if clear_cloexec(*descriptor) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
            }
            if let Some(directory) = working_directory
                && libc::fchdir(directory) != 0
            {
                return Err(std::io::Error::last_os_error());
            }
            if libc::syscall(libc::SYS_landlock_restrict_self, landlock_ruleset, 0_u32) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            let program = libc::sock_fprog {
                len: seccomp_length,
                filter: seccomp_filter as *mut libc::sock_filter,
            };
            if libc::prctl(
                libc::PR_SET_SECCOMP,
                SECCOMP_MODE_FILTER,
                &program as *const libc::sock_fprog,
            ) != 0
            {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let child = command
        .spawn()
        .map_err(|_| BrokerErrorCode::BackendFailed)?;
    let process_group = libc::pid_t::try_from(child.id()).map_err(|_| BrokerErrorCode::Internal)?;
    Ok((child, process_group))
}

fn run_command(
    child: &mut Child,
    process_group: libc::pid_t,
    rule: CommandRule,
    cancelled: Arc<AtomicBool>,
    containment: Arc<ProcessContainment>,
    events: ResourceEventSink,
    _permit: ActivePermit,
) {
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let total = Arc::new(AtomicU64::new(0));
    let stdout_bytes = Arc::new(AtomicU64::new(0));
    let stderr_bytes = Arc::new(AtomicU64::new(0));
    let failure = Arc::new(Mutex::new(None));
    let stdout_worker = stdout
        .map(|stream| {
            spawn_output_reader(
                stream,
                false,
                rule.maximum_output_bytes,
                Arc::clone(&total),
                Arc::clone(&stdout_bytes),
                Arc::clone(&failure),
                Arc::clone(&containment),
                events.clone(),
            )
        })
        .transpose();
    let stderr_worker = stderr
        .map(|stream| {
            spawn_output_reader(
                stream,
                true,
                rule.maximum_output_bytes,
                Arc::clone(&total),
                Arc::clone(&stderr_bytes),
                Arc::clone(&failure),
                Arc::clone(&containment),
                events.clone(),
            )
        })
        .transpose();
    if stdout_worker.is_err() || stderr_worker.is_err() {
        record_failure(&failure, BrokerErrorCode::Unavailable);
        containment.kill_all();
    }
    let deadline = Instant::now()
        .checked_add(Duration::from_millis(u64::from(rule.timeout_milliseconds)))
        .unwrap_or_else(Instant::now);
    let status = monitor_process(
        child,
        process_group,
        &cancelled,
        deadline,
        &failure,
        &containment,
    );
    // The command is a single broker resource, not a daemon launcher. The
    // cgroup catches descendants even if they changed process group/session.
    containment.kill_all();
    containment.cgroup.wait_empty();
    if let Ok(Some(worker)) = stdout_worker {
        let _ = worker.join();
    }
    if let Ok(Some(worker)) = stderr_worker {
        let _ = worker.join();
    }
    let failure = failure.lock().ok().and_then(|failure| *failure);
    if let Some(error) = failure {
        events.finish(error);
        return;
    }
    let Ok(status) = status else {
        events.finish(BrokerErrorCode::BackendFailed);
        return;
    };
    let event = CommandEvent::Exited {
        exit_code: status.code(),
        signal: status.signal(),
        stdout_bytes: stdout_bytes.load(Ordering::Acquire),
        stderr_bytes: stderr_bytes.load(Ordering::Acquire),
    };
    match event.encode() {
        Ok(payload) => {
            let _ = events.complete(payload);
        }
        Err(_) => events.finish(BrokerErrorCode::Internal),
    }
}

#[allow(clippy::too_many_arguments)]
fn spawn_output_reader(
    stream: impl Read + Send + 'static,
    stderr: bool,
    maximum_output: u64,
    total: Arc<AtomicU64>,
    stream_total: Arc<AtomicU64>,
    failure: Arc<Mutex<Option<BrokerErrorCode>>>,
    containment: Arc<ProcessContainment>,
    events: ResourceEventSink,
) -> Result<thread::JoinHandle<()>, BrokerErrorCode> {
    thread::Builder::new()
        .name(
            if stderr {
                "touchbar-command-stderr"
            } else {
                "touchbar-command-stdout"
            }
            .into(),
        )
        .spawn(move || {
            read_output(
                stream,
                stderr,
                maximum_output,
                &total,
                &stream_total,
                &failure,
                &containment,
                &events,
            )
        })
        .map_err(|_| BrokerErrorCode::Unavailable)
}

#[allow(clippy::too_many_arguments)]
fn read_output(
    mut stream: impl Read,
    stderr: bool,
    maximum_output: u64,
    total: &AtomicU64,
    stream_total: &AtomicU64,
    failure: &Mutex<Option<BrokerErrorCode>>,
    containment: &ProcessContainment,
    events: &ResourceEventSink,
) {
    let mut buffer = vec![0; MAX_COMMAND_OUTPUT_CHUNK_BYTES];
    loop {
        let count = match stream.read(&mut buffer) {
            Ok(0) => return,
            Ok(count) => count,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => {
                record_failure(failure, BrokerErrorCode::BackendFailed);
                containment.kill_all();
                return;
            }
        };
        if !reserve_output(total, count as u64, maximum_output) {
            record_failure(failure, BrokerErrorCode::QuotaExceeded);
            containment.kill_all();
            return;
        }
        stream_total.fetch_add(count as u64, Ordering::AcqRel);
        let event = if stderr {
            CommandEvent::Stderr(buffer[..count].to_vec())
        } else {
            CommandEvent::Stdout(buffer[..count].to_vec())
        };
        let result = event
            .encode()
            .map_err(schema_error)
            .and_then(|payload| events.emit_buffered(payload));
        if let Err(error) = result {
            if !events.is_finished() {
                record_failure(failure, error);
            }
            containment.kill_all();
            return;
        }
    }
}

fn reserve_output(total: &AtomicU64, count: u64, maximum: u64) -> bool {
    total
        .fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
            current.checked_add(count).filter(|next| *next <= maximum)
        })
        .is_ok()
}

fn monitor_process(
    child: &mut Child,
    process_group: libc::pid_t,
    cancelled: &AtomicBool,
    deadline: Instant,
    failure: &Mutex<Option<BrokerErrorCode>>,
    containment: &ProcessContainment,
) -> std::io::Result<std::process::ExitStatus> {
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(status);
        }
        let error = if cancelled.load(Ordering::Acquire) {
            Some(BrokerErrorCode::Cancelled)
        } else if failure.lock().is_ok_and(|failure| failure.is_some()) {
            None
        } else if Instant::now() >= deadline {
            Some(BrokerErrorCode::Timeout)
        } else {
            thread::sleep(PROCESS_POLL_INTERVAL);
            continue;
        };
        if let Some(error) = error {
            record_failure(failure, error);
        }
        terminate_process_group(child, process_group, containment)?;
        return child.wait();
    }
}

fn terminate_process_group(
    child: &mut Child,
    process_group: libc::pid_t,
    containment: &ProcessContainment,
) -> std::io::Result<()> {
    kill_process_group(process_group, libc::SIGTERM);
    containment.signal_leader(libc::SIGTERM);
    let deadline = Instant::now() + TERMINATION_GRACE;
    while Instant::now() < deadline {
        if child.try_wait()?.is_some() {
            return Ok(());
        }
        thread::sleep(PROCESS_POLL_INTERVAL);
    }
    containment.kill_all();
    Ok(())
}

fn record_failure(failure: &Mutex<Option<BrokerErrorCode>>, error: BrokerErrorCode) {
    if let Ok(mut failure) = failure.lock()
        && failure.is_none()
    {
        *failure = Some(error);
    }
}

fn kill_process_group(process_group: libc::pid_t, signal: i32) {
    if process_group > 0 {
        // SAFETY: a negative PID targets only the command's dedicated session.
        unsafe {
            libc::kill(-process_group, signal);
        }
    }
}

fn blocked_command_syscalls() -> Vec<libc::c_long> {
    vec![
        libc::SYS_ptrace,
        libc::SYS_process_vm_readv,
        libc::SYS_process_vm_writev,
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
        libc::SYS_acct,
    ]
}

const fn bpf_statement(code: u16, value: u32) -> libc::sock_filter {
    libc::sock_filter {
        code,
        jt: 0,
        jf: 0,
        k: value,
    }
}

const fn bpf_jump(code: u16, value: u32, jt: u8, jf: u8) -> libc::sock_filter {
    libc::sock_filter {
        code,
        jt,
        jf,
        k: value,
    }
}

fn current_cgroup_directory() -> Result<PathBuf, BrokerErrorCode> {
    let membership =
        fs::read_to_string("/proc/self/cgroup").map_err(|_| BrokerErrorCode::Unavailable)?;
    let mut unified = None;
    for line in membership.lines() {
        let mut fields = line.splitn(3, ':');
        if fields.next() == Some("0") && fields.next() == Some("") {
            if unified.is_some() {
                return Err(BrokerErrorCode::Unavailable);
            }
            unified = fields.next();
        }
    }
    let relative = unified
        .and_then(|path| path.strip_prefix('/'))
        .ok_or(BrokerErrorCode::Unavailable)?;
    let root = Path::new("/sys/fs/cgroup");
    let directory = root.join(relative);
    let canonical_root = root
        .canonicalize()
        .map_err(|_| BrokerErrorCode::Unavailable)?;
    let canonical_directory = directory
        .canonicalize()
        .map_err(|_| BrokerErrorCode::Unavailable)?;
    if !canonical_directory.starts_with(&canonical_root) {
        return Err(BrokerErrorCode::Unavailable);
    }
    let metadata = fs::metadata(&canonical_directory).map_err(|_| BrokerErrorCode::Unavailable)?;
    let effective_user = unsafe { libc::geteuid() };
    if !metadata.is_dir() || metadata.uid() != effective_user || metadata.mode() & 0o022 != 0 {
        return Err(BrokerErrorCode::Unavailable);
    }
    Ok(canonical_directory)
}

fn add_landlock_children_except(
    ruleset: RawFd,
    directory: &Path,
    excluded: &[&str],
    directory_access: u64,
) -> Result<(), BrokerErrorCode> {
    let entries = fs::read_dir(directory).map_err(|_| BrokerErrorCode::Unavailable)?;
    for entry in entries {
        let entry = entry.map_err(|_| BrokerErrorCode::Unavailable)?;
        if excluded
            .iter()
            .any(|excluded| entry.file_name() == OsStr::new(excluded))
        {
            continue;
        }
        let path = entry.path();
        let metadata = fs::metadata(&path).map_err(|_| BrokerErrorCode::Unavailable)?;
        let access = if metadata.is_dir() {
            directory_access
        } else {
            directory_access
                & (FS_EXECUTE | FS_WRITE_FILE | FS_READ_FILE | FS_TRUNCATE | FS_IOCTL_DEV)
        };
        add_landlock_rule(ruleset, &path, access)?;
    }
    Ok(())
}

fn add_landlock_rule(
    ruleset: RawFd,
    path: &Path,
    allowed_access: u64,
) -> Result<(), BrokerErrorCode> {
    let descriptor = open_path(path, libc::O_PATH)?;
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
            0_u32,
        )
    } != 0
    {
        return Err(BrokerErrorCode::Unavailable);
    }
    Ok(())
}

fn open_cgroup_file(directory: &Path, name: &str, flags: i32) -> Result<OwnedFd, BrokerErrorCode> {
    let descriptor = open_path(&directory.join(name), flags | libc::O_NOFOLLOW)?;
    let metadata = descriptor_metadata(descriptor.as_raw_fd())?;
    let effective_user = unsafe { libc::geteuid() };
    if metadata.st_mode & libc::S_IFMT != libc::S_IFREG
        || metadata.st_uid != effective_user
        || metadata.st_mode & 0o022 != 0
    {
        return Err(BrokerErrorCode::Unavailable);
    }
    Ok(descriptor)
}

fn write_cgroup_setting(directory: &Path, name: &str, value: &[u8]) -> Result<(), BrokerErrorCode> {
    let descriptor = open_cgroup_file(directory, name, libc::O_WRONLY)?;
    write_all_descriptor(descriptor.as_raw_fd(), value)
}

fn write_optional_cgroup_setting(
    directory: &Path,
    name: &str,
    value: &[u8],
) -> Result<(), BrokerErrorCode> {
    match fs::symlink_metadata(directory.join(name)) {
        Ok(_) => write_cgroup_setting(directory, name, value),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(_) => Err(BrokerErrorCode::Unavailable),
    }
}

fn write_descriptor(descriptor: RawFd, value: &[u8]) {
    let _ = write_all_descriptor(descriptor, value);
}

fn write_all_descriptor(descriptor: RawFd, mut value: &[u8]) -> Result<(), BrokerErrorCode> {
    while !value.is_empty() {
        // SAFETY: descriptor is live and value is readable for its declared length.
        let count = unsafe { libc::write(descriptor, value.as_ptr().cast(), value.len()) };
        if count < 0 {
            if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err(BrokerErrorCode::Unavailable);
        }
        if count == 0 {
            return Err(BrokerErrorCode::Unavailable);
        }
        value = &value[count as usize..];
    }
    Ok(())
}

fn open_pidfd(pid: u32) -> Result<OwnedFd, BrokerErrorCode> {
    let pid = libc::pid_t::try_from(pid).map_err(|_| BrokerErrorCode::Internal)?;
    // SAFETY: pidfd_open takes no pointer arguments and returns a new descriptor.
    let descriptor = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0_u32) };
    let descriptor = i32::try_from(descriptor).map_err(|_| os_error())?;
    owned_descriptor(descriptor)
}

fn current_user_process_limit() -> Result<u64, BrokerErrorCode> {
    let effective_user = unsafe { libc::geteuid() };
    let mut tasks = 0_u64;
    let entries = fs::read_dir("/proc").map_err(|_| BrokerErrorCode::Unavailable)?;
    for entry in entries {
        let Ok(entry) = entry else {
            continue;
        };
        if !entry.file_name().as_bytes().iter().all(u8::is_ascii_digit) {
            continue;
        }
        if entry
            .metadata()
            .ok()
            .is_some_and(|value| value.uid() == effective_user)
        {
            let count = fs::read_dir(entry.path().join("task"))
                .ok()
                .map(|entries| entries.filter_map(Result::ok).count() as u64)
                .unwrap_or(1);
            tasks = tasks.saturating_add(count.max(1));
        }
    }
    // RLIMIT_NPROC counts tasks (threads), not merely process leaders.
    let desired = tasks.saturating_add(MAXIMUM_ADDITIONAL_USER_PROCESSES);
    bounded_rlimit(libc::RLIMIT_NPROC, desired)
}

fn bounded_rlimit(
    resource: libc::__rlimit_resource_t,
    desired: u64,
) -> Result<u64, BrokerErrorCode> {
    let mut existing = std::mem::MaybeUninit::<libc::rlimit>::uninit();
    // SAFETY: existing points to sufficient writable storage for getrlimit.
    if unsafe { libc::getrlimit(resource, existing.as_mut_ptr()) } != 0 {
        return Err(BrokerErrorCode::Unavailable);
    }
    // SAFETY: getrlimit succeeded.
    let existing = unsafe { existing.assume_init() };
    Ok(desired.min(existing.rlim_max))
}

fn command_scope(request: &BackendRequest) -> Result<&CommandRunScope, BrokerErrorCode> {
    match &request.authorized_scope {
        CapabilityScope::CommandRun(scope) => Ok(scope),
        _ => Err(BrokerErrorCode::InvalidRequest),
    }
}

fn command_value_name(value: &CommandValue) -> &str {
    match value {
        CommandValue::Integer { name, .. }
        | CommandValue::FixedEnum { name, .. }
        | CommandValue::Text { name, .. }
        | CommandValue::ApprovedFile { name, .. }
        | CommandValue::Url { name, .. } => name,
    }
}

fn take_value(
    values: &mut BTreeMap<String, CommandValue>,
    name: &str,
) -> Result<CommandValue, BrokerErrorCode> {
    values.remove(name).ok_or(BrokerErrorCode::InvalidRequest)
}

fn valid_relative_path(value: &str) -> bool {
    !value.is_empty()
        && !Path::new(value).is_absolute()
        && Path::new(value)
            .components()
            .all(|component| matches!(component, Component::Normal(_)))
}

fn open_executable(path: &Path, package_root: &Path) -> Result<OwnedFd, BrokerErrorCode> {
    if !path.is_absolute()
        || path
            .components()
            .any(|part| !matches!(part, Component::RootDir | Component::Normal(_)))
    {
        return Err(BrokerErrorCode::OutOfScope);
    }
    let descriptor = open_absolute_no_magic(path, libc::O_PATH)?;
    let metadata = descriptor_metadata(descriptor.as_raw_fd())?;
    let effective_user = unsafe { libc::geteuid() };
    if metadata.st_mode & libc::S_IFMT != libc::S_IFREG
        || metadata.st_mode & 0o111 == 0
        || metadata.st_mode & 0o022 != 0
        || metadata.st_nlink != 1
        || (metadata.st_uid != 0 && metadata.st_uid != effective_user)
    {
        return Err(BrokerErrorCode::OutOfScope);
    }
    let resolved = fs::read_link(format!("/proc/self/fd/{}", descriptor.as_raw_fd()))
        .map_err(|_| BrokerErrorCode::BackendFailed)?;
    if resolved.starts_with(package_root) {
        return Err(BrokerErrorCode::OutOfScope);
    }
    Ok(descriptor)
}

fn open_directory(path: &Path) -> Result<OwnedFd, BrokerErrorCode> {
    let descriptor = open_path(path, libc::O_PATH | libc::O_DIRECTORY | libc::O_NOFOLLOW)?;
    let metadata = descriptor_metadata(descriptor.as_raw_fd())?;
    if metadata.st_mode & libc::S_IFMT != libc::S_IFDIR {
        return Err(BrokerErrorCode::OutOfScope);
    }
    Ok(descriptor)
}

fn open_path(path: &Path, flags: i32) -> Result<OwnedFd, BrokerErrorCode> {
    let path = c_string(path.as_os_str())?;
    // SAFETY: path is nul-terminated and flags require no mode argument.
    let descriptor = unsafe { libc::open(path.as_ptr(), flags | libc::O_CLOEXEC) };
    owned_descriptor(descriptor)
}

fn open_absolute_no_magic(path: &Path, flags: i32) -> Result<OwnedFd, BrokerErrorCode> {
    let path = c_string(path.as_os_str())?;
    let how = OpenHow {
        flags: (flags | libc::O_CLOEXEC) as u64,
        mode: 0,
        // Ordinary distribution symlinks such as /usr/bin/sh are safe after
        // descriptor pinning. Kernel magic links are not ordinary files and
        // could otherwise smuggle one of the supervisor's own descriptors.
        resolve: RESOLVE_NO_MAGICLINKS,
    };
    // SAFETY: syscall arguments point to initialized storage of the declared size.
    let descriptor = unsafe {
        libc::syscall(
            libc::SYS_openat2,
            libc::AT_FDCWD,
            path.as_ptr(),
            &how,
            std::mem::size_of::<OpenHow>(),
        )
    };
    let descriptor = i32::try_from(descriptor).map_err(|_| os_error())?;
    owned_descriptor(descriptor)
}

fn open_approved_file(root: RawFd, path: &str) -> Result<OwnedFd, BrokerErrorCode> {
    let path = CString::new(path).map_err(|_| BrokerErrorCode::InvalidRequest)?;
    let how = OpenHow {
        flags: (libc::O_PATH | libc::O_CLOEXEC | libc::O_NOFOLLOW) as u64,
        mode: 0,
        resolve: RESOLVE_NO_XDEV | RESOLVE_NO_MAGICLINKS | RESOLVE_NO_SYMLINKS | RESOLVE_BENEATH,
    };
    // SAFETY: syscall arguments point to initialized storage of the declared size.
    let descriptor = unsafe {
        libc::syscall(
            libc::SYS_openat2,
            root,
            path.as_ptr(),
            &how,
            std::mem::size_of::<OpenHow>(),
        )
    };
    let descriptor = i32::try_from(descriptor).map_err(|_| os_error())?;
    let descriptor = owned_descriptor(descriptor)?;
    let metadata = descriptor_metadata(descriptor.as_raw_fd())?;
    if metadata.st_mode & libc::S_IFMT != libc::S_IFREG
        || metadata.st_nlink != 1
        || descriptor_filesystem_type(descriptor.as_raw_fd())? == CGROUP2_SUPER_MAGIC
    {
        return Err(BrokerErrorCode::OutOfScope);
    }
    Ok(descriptor)
}

fn descriptor_metadata(descriptor: RawFd) -> Result<libc::stat, BrokerErrorCode> {
    let mut metadata = std::mem::MaybeUninit::<libc::stat>::uninit();
    // SAFETY: descriptor is live and metadata points to sufficient writable storage.
    if unsafe { libc::fstat(descriptor, metadata.as_mut_ptr()) } != 0 {
        return Err(os_error());
    }
    // SAFETY: fstat succeeded.
    Ok(unsafe { metadata.assume_init() })
}

fn descriptor_filesystem_type(descriptor: RawFd) -> Result<libc::c_long, BrokerErrorCode> {
    let mut statistics = std::mem::MaybeUninit::<libc::statfs>::uninit();
    // SAFETY: descriptor is live and statistics points to sufficient writable storage.
    if unsafe { libc::fstatfs(descriptor, statistics.as_mut_ptr()) } != 0 {
        return Err(os_error());
    }
    // SAFETY: fstatfs succeeded.
    Ok(unsafe { statistics.assume_init() }.f_type)
}

fn owned_descriptor(descriptor: i32) -> Result<OwnedFd, BrokerErrorCode> {
    if descriptor < 0 {
        Err(os_error())
    } else {
        // SAFETY: successful open returned a uniquely owned descriptor.
        Ok(unsafe { OwnedFd::from_raw_fd(descriptor) })
    }
}

fn c_string(value: &OsStr) -> Result<CString, BrokerErrorCode> {
    CString::new(value.as_bytes()).map_err(|_| BrokerErrorCode::InvalidRequest)
}

fn clear_cloexec(descriptor: RawFd) -> i32 {
    // SAFETY: fcntl does not dereference pointers for F_SETFD.
    unsafe { libc::fcntl(descriptor, libc::F_SETFD, 0) }
}

fn schema_error(error: SchemaError) -> BrokerErrorCode {
    match error {
        SchemaError::Malformed => BrokerErrorCode::InvalidRequest,
        SchemaError::LimitExceeded => BrokerErrorCode::QuotaExceeded,
    }
}

fn os_error() -> BrokerErrorCode {
    match std::io::Error::last_os_error().raw_os_error() {
        Some(libc::EXDEV) | Some(libc::ELOOP) | Some(libc::ENOENT) | Some(libc::ENOTDIR)
        | Some(libc::EACCES) | Some(libc::EPERM) => BrokerErrorCode::OutOfScope,
        Some(libc::ENOSYS) => BrokerErrorCode::Unsupported,
        _ => BrokerErrorCode::BackendFailed,
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::{BTreeMap, BTreeSet},
        fs,
        io::Read,
        os::unix::{ffi::OsStrExt, fs::PermissionsExt},
        path::{Path, PathBuf},
        sync::{Arc, atomic::Ordering},
        thread,
        time::{Duration, Instant},
    };

    use semver::Version;
    use tempfile::tempdir;
    use touchbar_broker_schema::{CommandEvent, CommandOpened, CommandRunRequest, CommandValue};
    use touchbar_package::GithubSource;
    use touchbar_policy::{
        CapabilityId, CapabilityScope, CommandArgument, CommandRule, CommandRunScope,
        FilesystemMountBinding, PackageInstance, Provenance, RuntimeKind,
    };
    use touchbar_protocol::broker_ipc::{BrokerErrorCode, BrokerResult};

    use crate::{
        ActivationLedger, BackendRequest, ConnectionIdentity, HostEvent, HostEventQueue,
        ResourceBackend, ResourceManager,
    };

    use super::{
        CommandCgroup, CommandLandlock, CommandRunBackend, CommandSeccomp,
        current_cgroup_directory, prepare_command, spawn_command,
    };

    fn rule(executable: &str, arguments: Vec<CommandArgument>) -> CommandRule {
        CommandRule {
            id: "test-command".into(),
            executable: executable.into(),
            arguments,
            environment: BTreeMap::new(),
            working_directory: None,
            maximum_output_bytes: 64 * 1024,
            timeout_milliseconds: 2_000,
        }
    }

    fn request(
        rule: CommandRule,
        values: Vec<CommandValue>,
        mounts: BTreeMap<String, std::path::PathBuf>,
        maximum_parallel_processes: u8,
    ) -> BackendRequest {
        let filesystem_mounts = mounts
            .into_iter()
            .map(|(label, path)| (label, FilesystemMountBinding::from_directory(path).unwrap()))
            .collect();
        BackendRequest {
            identity: ConnectionIdentity {
                instance_id: 1,
                package: PackageInstance {
                    source: GithubSource::new("alice", "command-test").unwrap(),
                    version: Version::new(1, 0, 0),
                    digest: format!("sha256:{}", "a".repeat(64)),
                    provenance: Provenance::VerifiedRelease,
                    runtime: RuntimeKind::Component,
                },
            },
            request_id: 1,
            capability: CapabilityId::CommandRunV1,
            authorized_scope: CapabilityScope::CommandRun(CommandRunScope {
                commands: BTreeSet::from([rule]),
                maximum_parallel_processes,
            }),
            bindings: touchbar_policy::GrantBindings {
                filesystem_mounts,
                ..Default::default()
            },
            activation: None,
            operation: "run".into(),
            payload: CommandRunRequest {
                command_id: "test-command".into(),
                values,
            }
            .encode()
            .unwrap(),
        }
    }

    fn run_resource(
        backend: Arc<CommandRunBackend>,
        request: &BackendRequest,
    ) -> (CommandOpened, Vec<BrokerResult>) {
        let mut resources = ResourceManager::new(1).unwrap();
        resources.register(CapabilityId::CommandRunV1, backend);
        let limits = resources.limits(request).unwrap();
        let response = resources
            .open(1, request, limits, &mut ActivationLedger::new(8), 0)
            .unwrap();
        let opened = CommandOpened::decode(&response).unwrap();
        let mut queue = HostEventQueue::new(8, 256);
        let mut results = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(3);
        while resources.resource_count() != 0 && Instant::now() < deadline {
            resources.pump(&mut queue, 1);
            while let Some(event) = queue.pop() {
                if let HostEvent::ResourceEvent { result, .. } = event {
                    results.push(result);
                }
            }
            thread::sleep(Duration::from_millis(2));
        }
        resources.pump(&mut queue, 1);
        while let Some(event) = queue.pop() {
            if let HostEvent::ResourceEvent { result, .. } = event {
                results.push(result);
            }
        }
        assert_eq!(resources.resource_count(), 0, "command did not terminate");
        (opened, results)
    }

    fn successful_payloads(results: &[BrokerResult]) -> Vec<&[u8]> {
        results
            .iter()
            .filter_map(|result| match result {
                BrokerResult::Success { payload } => Some(payload.as_slice()),
                BrokerResult::Error(_) => None,
            })
            .collect()
    }

    #[test]
    fn metacharacters_are_literal_and_environment_starts_empty() {
        let package = tempdir().unwrap();
        let submitted = "$(touch /tmp/not-executed); $HOME * ' quoted";
        let printf_request = request(
            rule(
                "/usr/bin/printf",
                vec![
                    CommandArgument::Literal {
                        value: "[%s]".into(),
                    },
                    CommandArgument::BoundedText {
                        name: "text".into(),
                        maximum_bytes: 256,
                    },
                ],
            ),
            vec![CommandValue::Text {
                name: "text".into(),
                value: submitted.into(),
            }],
            BTreeMap::new(),
            1,
        );
        let (_, results) = run_resource(
            Arc::new(CommandRunBackend::new(package.path().to_owned())),
            &printf_request,
        );
        let events = successful_payloads(&results)
            .into_iter()
            .map(|payload| CommandEvent::decode(payload).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(
            events,
            [
                CommandEvent::Stdout(format!("[{submitted}]").into_bytes()),
                CommandEvent::Exited {
                    exit_code: Some(0),
                    signal: None,
                    stdout_bytes: (submitted.len() + 2) as u64,
                    stderr_bytes: 0,
                },
            ]
        );

        let mut env_rule = rule("/usr/bin/env", Vec::new());
        env_rule.environment = BTreeMap::from([("SAFE_VALUE".into(), "fixed".into())]);
        let (_, results) = run_resource(
            Arc::new(CommandRunBackend::new(package.path().to_owned())),
            &request(env_rule, Vec::new(), BTreeMap::new(), 1),
        );
        let output = successful_payloads(&results)
            .into_iter()
            .filter_map(|payload| match CommandEvent::decode(payload).unwrap() {
                CommandEvent::Stdout(bytes) => Some(bytes),
                _ => None,
            })
            .flatten()
            .collect::<Vec<_>>();
        assert_eq!(output, b"SAFE_VALUE=fixed\n");
    }

    #[test]
    fn typed_slots_reject_missing_extra_wrong_and_out_of_scope_values() {
        let package = tempdir().unwrap();
        let backend = CommandRunBackend::new(package.path().to_owned());
        let template = rule(
            "/usr/bin/true",
            vec![CommandArgument::BoundedInteger {
                name: "level".into(),
                minimum: 0,
                maximum: 10,
            }],
        );
        for (values, expected) in [
            (Vec::new(), BrokerErrorCode::InvalidRequest),
            (
                vec![CommandValue::Text {
                    name: "level".into(),
                    value: "3".into(),
                }],
                BrokerErrorCode::InvalidRequest,
            ),
            (
                vec![CommandValue::Integer {
                    name: "level".into(),
                    value: 11,
                }],
                BrokerErrorCode::OutOfScope,
            ),
            (
                vec![
                    CommandValue::Integer {
                        name: "level".into(),
                        value: 3,
                    },
                    CommandValue::Text {
                        name: "extra".into(),
                        value: "bad".into(),
                    },
                ],
                BrokerErrorCode::InvalidRequest,
            ),
        ] {
            let request = request(template.clone(), values, BTreeMap::new(), 1);
            assert_eq!(
                backend.authorize(&request, &mut ActivationLedger::new(8), 0),
                Err(expected)
            );
        }
    }

    #[test]
    fn approved_files_are_descriptor_pinned_and_cannot_traverse() {
        let package = tempdir().unwrap();
        let approved = tempdir().unwrap();
        fs::write(approved.path().join("selected"), b"original").unwrap();
        let template = rule(
            "/usr/bin/cat",
            vec![CommandArgument::ApprovedFile {
                name: "file".into(),
                mount: "document".into(),
            }],
        );
        let mounts = BTreeMap::from([("document".into(), approved.path().to_owned())]);
        let original_request = request(
            template.clone(),
            vec![CommandValue::ApprovedFile {
                name: "file".into(),
                path: "selected".into(),
            }],
            mounts.clone(),
            1,
        );
        let prepared = prepare_command(&original_request, package.path()).unwrap();
        fs::rename(
            approved.path().join("selected"),
            approved.path().join("old"),
        )
        .unwrap();
        fs::write(approved.path().join("selected"), b"replacement").unwrap();
        let cgroup = CommandCgroup::create(91).unwrap();
        let landlock = CommandLandlock::create().unwrap();
        let seccomp = CommandSeccomp::create();
        let (mut child, _) = spawn_command(&prepared, &cgroup, &landlock, &seccomp).unwrap();
        let mut output = Vec::new();
        child
            .stdout
            .take()
            .unwrap()
            .read_to_end(&mut output)
            .unwrap();
        assert!(child.wait().unwrap().success());
        cgroup.kill_all();
        cgroup.wait_empty();
        assert_eq!(output, b"original");

        for path in ["../secret", "/etc/passwd", ".", "nested/../secret"] {
            let request = request(
                template.clone(),
                vec![CommandValue::ApprovedFile {
                    name: "file".into(),
                    path: path.into(),
                }],
                mounts.clone(),
                1,
            );
            assert_eq!(
                prepare_command(&request, package.path()).unwrap_err_code(),
                BrokerErrorCode::OutOfScope
            );
        }

        let cgroup_request = request(
            template,
            vec![CommandValue::ApprovedFile {
                name: "file".into(),
                path: "cgroup.procs".into(),
            }],
            BTreeMap::from([("document".into(), current_cgroup_directory().unwrap())]),
            1,
        );
        assert_eq!(
            prepare_command(&cgroup_request, package.path()).unwrap_err_code(),
            BrokerErrorCode::OutOfScope
        );
    }

    #[test]
    fn package_owned_and_replaceable_executables_are_rejected_or_pinned() {
        let package = tempdir().unwrap();
        let packaged = package.path().join("plugin-command");
        fs::write(&packaged, b"#!/usr/bin/sh\necho bad\n").unwrap();
        fs::set_permissions(&packaged, fs::Permissions::from_mode(0o755)).unwrap();
        let packaged_request = request(
            rule(packaged.to_str().unwrap(), Vec::new()),
            Vec::new(),
            BTreeMap::new(),
            1,
        );
        assert_eq!(
            prepare_command(&packaged_request, package.path()).unwrap_err_code(),
            BrokerErrorCode::OutOfScope
        );

        let external = tempdir().unwrap();
        let executable = external.path().join("printf");
        fs::copy("/usr/bin/printf", &executable).unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o755)).unwrap();
        let request = request(
            rule(
                executable.to_str().unwrap(),
                vec![CommandArgument::Literal {
                    value: "pinned".into(),
                }],
            ),
            Vec::new(),
            BTreeMap::new(),
            1,
        );
        let prepared = prepare_command(&request, package.path()).unwrap();
        fs::rename(&executable, external.path().join("old")).unwrap();
        fs::write(&executable, b"#!/usr/bin/sh\necho replaced\n").unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o755)).unwrap();
        let cgroup = CommandCgroup::create(92).unwrap();
        let landlock = CommandLandlock::create().unwrap();
        let seccomp = CommandSeccomp::create();
        let (mut child, _) = spawn_command(&prepared, &cgroup, &landlock, &seccomp).unwrap();
        let mut output = Vec::new();
        child
            .stdout
            .take()
            .unwrap()
            .read_to_end(&mut output)
            .unwrap();
        assert!(child.wait().unwrap().success());
        cgroup.kill_all();
        cgroup.wait_empty();
        assert_eq!(output, b"pinned");
    }

    #[test]
    fn timeout_output_and_parallel_process_quotas_fail_closed() {
        let package = tempdir().unwrap();
        let backend = Arc::new(CommandRunBackend::new(package.path().to_owned()));

        let mut timeout_rule = rule(
            "/usr/bin/sleep",
            vec![CommandArgument::Literal { value: "10".into() }],
        );
        timeout_rule.timeout_milliseconds = 20;
        let (_, results) = run_resource(
            Arc::clone(&backend),
            &request(timeout_rule, Vec::new(), BTreeMap::new(), 1),
        );
        assert_eq!(
            results.last(),
            Some(&BrokerResult::Error(BrokerErrorCode::Timeout))
        );

        let mut output_rule = rule("/usr/bin/yes", Vec::new());
        output_rule.maximum_output_bytes = 100;
        let (_, results) = run_resource(
            Arc::clone(&backend),
            &request(output_rule, Vec::new(), BTreeMap::new(), 1),
        );
        assert_eq!(
            results.last(),
            Some(&BrokerResult::Error(BrokerErrorCode::QuotaExceeded))
        );

        let long = request(
            rule(
                "/usr/bin/sleep",
                vec![CommandArgument::Literal { value: "10".into() }],
            ),
            Vec::new(),
            BTreeMap::new(),
            1,
        );
        let mut resources = ResourceManager::new(8).unwrap();
        resources.register(CapabilityId::CommandRunV1, backend);
        let limits = resources.limits(&long).unwrap();
        resources
            .open(1, &long, limits, &mut ActivationLedger::new(8), 0)
            .unwrap();
        assert_eq!(
            resources.open(2, &long, limits, &mut ActivationLedger::new(8), 0),
            Err(BrokerErrorCode::QuotaExceeded)
        );
        assert!(resources.close(1));
    }

    #[test]
    fn cgroup_terminates_descendants_which_escape_the_commands_process_group() {
        let package = tempdir().unwrap();
        let state = tempdir().unwrap();
        let pid_file = state.path().join("child-pid");
        let parent_procs = current_cgroup_directory().unwrap().join("cgroup.procs");
        let script = format!(
            "if printf 0 > '{}'; then printf ESCAPED; else printf CONTAINED; fi; \
             /usr/bin/setsid /usr/bin/sleep 10 & child=$!; printf '%s' \"$child\" > '{}'; wait",
            parent_procs.display(),
            pid_file.display()
        );
        let mut template = rule(
            "/usr/bin/sh",
            vec![
                CommandArgument::Literal { value: "-c".into() },
                CommandArgument::Literal { value: script },
            ],
        );
        template.timeout_milliseconds = 500;
        let (_, results) = run_resource(
            Arc::new(CommandRunBackend::new(package.path().to_owned())),
            &request(template, Vec::new(), BTreeMap::new(), 1),
        );
        assert_eq!(
            results.last(),
            Some(&BrokerResult::Error(BrokerErrorCode::Timeout))
        );
        let stdout = successful_payloads(&results)
            .into_iter()
            .filter_map(|payload| match CommandEvent::decode(payload).unwrap() {
                CommandEvent::Stdout(bytes) => Some(bytes),
                _ => None,
            })
            .flatten()
            .collect::<Vec<_>>();
        assert_eq!(stdout, b"CONTAINED");
        let child_pid = fs::read_to_string(pid_file)
            .unwrap()
            .parse::<libc::pid_t>()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(1);
        loop {
            // SAFETY: signal zero performs a liveness check without changing the process.
            let alive = unsafe { libc::kill(child_pid, 0) } == 0;
            if !alive {
                break;
            }
            if Instant::now() >= deadline {
                // Avoid leaking the test child if the assertion fails.
                unsafe {
                    libc::kill(child_pid, libc::SIGKILL);
                }
                panic!("command descendant survived timeout");
            }
            thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn command_child_cannot_reopen_cgroup_or_use_kernel_escape_syscalls() {
        const CHILD: &str = "TOUCHBAR_COMMAND_CONFINEMENT_CHILD";
        const CGROUP_PROCS: &str = "TOUCHBAR_PARENT_CGROUP_PROCS";
        if std::env::var_os(CHILD).is_some() {
            let cgroup_procs = std::env::var_os(CGROUP_PROCS).unwrap();
            let error = fs::OpenOptions::new()
                .write(true)
                .open(cgroup_procs)
                .unwrap_err();
            assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
            // SAFETY: these probes have no pointer arguments. The command
            // seccomp filter must return EPERM before either operation runs.
            assert_eq!(unsafe { libc::unshare(libc::CLONE_NEWNS) }, -1);
            assert_eq!(
                std::io::Error::last_os_error().raw_os_error(),
                Some(libc::EPERM)
            );
            assert_eq!(unsafe { libc::ptrace(libc::PTRACE_TRACEME, 0, 0, 0) }, -1);
            assert_eq!(
                std::io::Error::last_os_error().raw_os_error(),
                Some(libc::EPERM)
            );
            let mut limit = std::mem::MaybeUninit::<libc::rlimit>::uninit();
            // SAFETY: limit points to sufficient writable storage.
            assert_eq!(
                unsafe { libc::getrlimit(libc::RLIMIT_AS, limit.as_mut_ptr()) },
                0
            );
            // SAFETY: getrlimit succeeded.
            let limit = unsafe { limit.assume_init() };
            assert!(limit.rlim_max <= super::MAXIMUM_COMMAND_ADDRESS_SPACE_BYTES);
            println!("COMMAND_CONFINEMENT_PROBED");
            return;
        }

        let package = tempdir().unwrap();
        let executable = std::env::current_exe().unwrap();
        let test_name =
            "command::tests::command_child_cannot_reopen_cgroup_or_use_kernel_escape_syscalls";
        let mut template = rule(
            executable.to_str().unwrap(),
            vec![
                CommandArgument::Literal {
                    value: "--exact".into(),
                },
                CommandArgument::Literal {
                    value: test_name.into(),
                },
                CommandArgument::Literal {
                    value: "--nocapture".into(),
                },
            ],
        );
        template.timeout_milliseconds = 3_000;
        template.maximum_output_bytes = 256 * 1024;
        template.environment = BTreeMap::from([
            (CHILD.into(), "1".into()),
            (
                CGROUP_PROCS.into(),
                current_cgroup_directory()
                    .unwrap()
                    .join("cgroup.procs")
                    .to_string_lossy()
                    .into_owned(),
            ),
        ]);
        let (_, results) = run_resource(
            Arc::new(CommandRunBackend::new(package.path().to_owned())),
            &request(template, Vec::new(), BTreeMap::new(), 1),
        );
        assert!(
            results.iter().any(|result| {
                let BrokerResult::Success { payload } = result else {
                    return false;
                };
                matches!(
                    CommandEvent::decode(payload),
                    Ok(CommandEvent::Stdout(bytes))
                        if String::from_utf8_lossy(&bytes).contains("COMMAND_CONFINEMENT_PROBED")
                )
            }),
            "child confinement probe did not complete: {results:?}"
        );
        assert!(matches!(
            results.last(),
            Some(BrokerResult::Success { payload })
                if matches!(
                    CommandEvent::decode(payload),
                    Ok(CommandEvent::Exited { exit_code: Some(0), signal: None, .. })
                )
        ));
    }

    #[test]
    #[ignore = "release-gate command fork, memory-pressure, and reclamation campaign"]
    fn security_campaign_command_pressure_is_bounded_and_reclaimable() {
        const MODE: &str = "TOUCHBAR_COMMAND_CAMPAIGN_MODE";
        const PID_FILE: &str = "TOUCHBAR_COMMAND_CAMPAIGN_PID_FILE";
        const TEST_NAME: &str =
            "command::tests::security_campaign_command_pressure_is_bounded_and_reclaimable";

        match std::env::var(MODE).as_deref() {
            Ok("memory") => {
                let mut allocation = vec![0_u8; 32 * 1024 * 1024];
                for page in allocation.chunks_mut(4096) {
                    page[0] = 0xa5;
                }
                std::hint::black_box(&allocation);

                let oversized =
                    usize::try_from(super::MAXIMUM_COMMAND_ADDRESS_SPACE_BYTES.saturating_mul(2))
                        .unwrap();
                // SAFETY: this reserves no backing pages. RLIMIT_AS must reject
                // a mapping larger than the complete command address-space cap.
                let mapping = unsafe {
                    libc::mmap(
                        std::ptr::null_mut(),
                        oversized,
                        libc::PROT_NONE,
                        libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                        -1,
                        0,
                    )
                };
                if mapping != libc::MAP_FAILED {
                    unsafe {
                        libc::munmap(mapping, oversized);
                    }
                    panic!("command exceeded its address-space limit");
                }
                println!("COMMAND_MEMORY_PRESSURE_OK");
                return;
            }
            Ok("fork") => {
                let mut pids = vec![unsafe { libc::getpid() }];
                for _ in 0..128 {
                    // SAFETY: fork is the behavior under test. The child uses
                    // only async-signal-safe libc calls until cgroup teardown.
                    let pid = unsafe { libc::fork() };
                    if pid == 0 {
                        loop {
                            unsafe {
                                libc::pause();
                            }
                        }
                    }
                    if pid < 0 {
                        break;
                    }
                    pids.push(pid);
                }
                let body = pids
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join("\n");
                fs::write(std::env::var_os(PID_FILE).unwrap(), body).unwrap();
                println!("COMMAND_FORK_PRESSURE_READY children={}", pids.len() - 1);
                loop {
                    unsafe {
                        libc::pause();
                    }
                }
            }
            Ok(other) => panic!("unexpected command campaign mode {other}"),
            Err(_) => {}
        }

        let package = tempdir().unwrap();
        let state = tempdir().unwrap();
        let executable = std::env::current_exe().unwrap();
        let baseline_cgroups = command_campaign_cgroups();
        let baseline_threads = command_campaign_thread_count();
        let backend = Arc::new(CommandRunBackend::new(package.path().to_owned()));

        for _ in 0..12 {
            let mut template = rule(
                executable.to_str().unwrap(),
                vec![
                    CommandArgument::Literal {
                        value: "--exact".into(),
                    },
                    CommandArgument::Literal {
                        value: TEST_NAME.into(),
                    },
                    CommandArgument::Literal {
                        value: "--ignored".into(),
                    },
                    CommandArgument::Literal {
                        value: "--nocapture".into(),
                    },
                ],
            );
            template.timeout_milliseconds = 3_000;
            template.maximum_output_bytes = 256 * 1024;
            template.environment = BTreeMap::from([(MODE.into(), "memory".into())]);
            let (_, results) = run_resource(
                Arc::clone(&backend),
                &request(template, Vec::new(), BTreeMap::new(), 1),
            );
            assert!(results.iter().any(|result| {
                let BrokerResult::Success { payload } = result else {
                    return false;
                };
                matches!(
                    CommandEvent::decode(payload),
                    Ok(CommandEvent::Stdout(bytes))
                        if String::from_utf8_lossy(&bytes)
                            .contains("COMMAND_MEMORY_PRESSURE_OK")
                )
            }));
            assert!(matches!(
                results.last(),
                Some(BrokerResult::Success { payload })
                    if matches!(
                        CommandEvent::decode(payload),
                        Ok(CommandEvent::Exited {
                            exit_code: Some(0),
                            signal: None,
                            ..
                        })
                    )
            ));
        }

        let mut resources = ResourceManager::new(8).unwrap();
        resources.register(CapabilityId::CommandRunV1, backend.clone());
        let mut queue = HostEventQueue::new(16, 512);
        for cycle in 0..6_u64 {
            let pid_file = state.path().join(format!("fork-{cycle}.pids"));
            let mut template = rule(
                executable.to_str().unwrap(),
                vec![
                    CommandArgument::Literal {
                        value: "--exact".into(),
                    },
                    CommandArgument::Literal {
                        value: TEST_NAME.into(),
                    },
                    CommandArgument::Literal {
                        value: "--ignored".into(),
                    },
                    CommandArgument::Literal {
                        value: "--nocapture".into(),
                    },
                ],
            );
            template.timeout_milliseconds = 10_000;
            template.maximum_output_bytes = 256 * 1024;
            template.environment = BTreeMap::from([
                (MODE.into(), "fork".into()),
                (PID_FILE.into(), pid_file.to_string_lossy().into_owned()),
            ]);
            let command = request(template, Vec::new(), BTreeMap::new(), 1);
            let resource_id = cycle + 1;
            let limits = resources.limits(&command).unwrap();
            resources
                .open(
                    resource_id,
                    &command,
                    limits,
                    &mut ActivationLedger::new(8),
                    0,
                )
                .unwrap();

            let deadline = Instant::now() + Duration::from_secs(3);
            while (!pid_file.is_file()
                || fs::read_to_string(&pid_file).unwrap_or_default().is_empty())
                && Instant::now() < deadline
            {
                resources.pump(&mut queue, 1);
                while queue.pop().is_some() {}
                thread::sleep(Duration::from_millis(5));
            }
            let pids = fs::read_to_string(&pid_file)
                .expect("fork-pressure child did not publish its process set")
                .lines()
                .map(|line| line.parse::<libc::pid_t>().unwrap())
                .collect::<Vec<_>>();
            assert!(pids.len() > 1, "fork pressure created no descendants");
            assert!(
                pids.len() <= 33,
                "command escaped its 33-task cgroup limit: {pids:?}"
            );

            let active_cgroups = command_campaign_cgroups()
                .difference(&baseline_cgroups)
                .cloned()
                .collect::<Vec<_>>();
            assert_eq!(active_cgroups.len(), 1);
            let cgroup = &active_cgroups[0];
            assert_eq!(
                fs::read_to_string(cgroup.join("cgroup.max.depth"))
                    .unwrap()
                    .trim(),
                "0"
            );
            assert_eq!(
                fs::read_to_string(cgroup.join("cgroup.max.descendants"))
                    .unwrap()
                    .trim(),
                "0"
            );
            if cgroup.join("pids.max").is_file() {
                assert_eq!(
                    fs::read_to_string(cgroup.join("pids.max")).unwrap().trim(),
                    "33"
                );
            }
            if cgroup.join("memory.max").is_file() {
                assert_eq!(
                    fs::read_to_string(cgroup.join("memory.max"))
                        .unwrap()
                        .trim(),
                    super::MAXIMUM_COMMAND_ADDRESS_SPACE_BYTES.to_string()
                );
            }
            if cgroup.join("memory.swap.max").is_file() {
                assert_eq!(
                    fs::read_to_string(cgroup.join("memory.swap.max"))
                        .unwrap()
                        .trim(),
                    "0"
                );
            }

            assert!(resources.close(resource_id));
            let deadline = Instant::now() + Duration::from_secs(3);
            while Instant::now() < deadline
                && (backend.active.load(Ordering::Acquire) != 0
                    || command_campaign_cgroups() != baseline_cgroups
                    || pids
                        .iter()
                        .any(|pid| Path::new(&format!("/proc/{pid}")).exists()))
            {
                thread::sleep(Duration::from_millis(5));
            }
            assert_eq!(backend.active.load(Ordering::Acquire), 0);
            assert_eq!(command_campaign_cgroups(), baseline_cgroups);
            assert!(
                pids.iter()
                    .all(|pid| !Path::new(&format!("/proc/{pid}")).exists()),
                "cancelled command left a live descendant: {pids:?}"
            );
        }

        let deadline = Instant::now() + Duration::from_secs(2);
        while command_campaign_thread_count() > baseline_threads && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(5));
        }
        assert!(command_campaign_thread_count() <= baseline_threads);
        assert_eq!(command_campaign_cgroups(), baseline_cgroups);
    }

    fn command_campaign_cgroups() -> BTreeSet<PathBuf> {
        let prefix = format!("touchbar-command-{}-", std::process::id());
        fs::read_dir(current_cgroup_directory().unwrap())
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| entry.file_name().as_bytes().starts_with(prefix.as_bytes()))
            .map(|entry| entry.path())
            .collect()
    }

    fn command_campaign_thread_count() -> usize {
        fs::read_dir("/proc/self/task").unwrap().count()
    }

    trait ResultError {
        fn unwrap_err_code(self) -> BrokerErrorCode;
    }

    impl<T> ResultError for Result<T, BrokerErrorCode> {
        fn unwrap_err_code(self) -> BrokerErrorCode {
            match self {
                Ok(_) => panic!("operation unexpectedly succeeded"),
                Err(error) => error,
            }
        }
    }
}
