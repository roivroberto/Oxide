use std::io;
use std::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command, ExitStatus, Stdio};
use std::time::Instant;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct CachedIoError {
    kind: io::ErrorKind,
    raw_os_error: Option<i32>,
}

impl CachedIoError {
    fn capture(error: &io::Error) -> Self {
        Self {
            kind: error.kind(),
            raw_os_error: error.raw_os_error(),
        }
    }

    pub(crate) fn kind(self) -> io::ErrorKind {
        self.kind
    }

    #[allow(dead_code)]
    pub(crate) fn raw_os_error(self) -> Option<i32> {
        self.raw_os_error
    }

    pub(crate) fn to_error(self) -> io::Error {
        self.raw_os_error
            .map_or_else(|| io::Error::from(self.kind), io::Error::from_raw_os_error)
    }
}

#[derive(Clone, Copy, Debug)]
enum TerminationState {
    NotAttempted,
    Succeeded,
    Failed(CachedIoError),
}

#[derive(Clone, Copy, Debug)]
enum ReapState {
    NotAttempted,
    Reaped(ExitStatus),
    Failed(CachedIoError),
}

pub(crate) struct ContainedPipedChild {
    process: ContainedChild,
    stdin: ChildStdin,
    stdout: ChildStdout,
    stderr: ChildStderr,
}

pub(crate) struct ContainedChild {
    child: Child,
    job: platform::ProcessJob,
    termination: TerminationState,
    reap: ReapState,
}

pub(crate) enum ContainedSpawnAttempt {
    Ready(ContainedPipedChild),
    FailedNoOwner {
        cause: CachedIoError,
    },
    FailedWithOwner {
        cause: CachedIoError,
        partial: PartialContainedChild,
    },
}

pub(crate) enum PartialContainedChild {
    Unassigned {
        child: Child,
        job: platform::ProcessJob,
    },
    Assigned {
        process: ContainedChild,
        stdin: Option<ChildStdin>,
        stdout: Option<ChildStdout>,
        stderr: Option<ChildStderr>,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PartialCleanupFailure {
    Terminate,
    Reap,
    #[cfg(windows)]
    VerifyTreeEmpty,
}

trait LaunchOps {
    fn create_job(&mut self) -> io::Result<platform::ProcessJob>;
    fn prepare_command(&mut self, command: &mut Command);
    fn assign(&mut self, job: &platform::ProcessJob, child: &Child) -> io::Result<()>;
    fn resume(&mut self, job: &platform::ProcessJob, child: &Child) -> io::Result<()>;
    fn take_stdin(&mut self, child: &mut Child) -> Option<ChildStdin>;
    fn take_stdout(&mut self, child: &mut Child) -> Option<ChildStdout>;
    fn take_stderr(&mut self, child: &mut Child) -> Option<ChildStderr>;
}

struct SystemLaunchOps;

impl LaunchOps for SystemLaunchOps {
    fn create_job(&mut self) -> io::Result<platform::ProcessJob> {
        platform::ProcessJob::create()
    }

    fn prepare_command(&mut self, command: &mut Command) {
        platform::ProcessJob::prepare_command(command);
    }

    fn assign(&mut self, job: &platform::ProcessJob, child: &Child) -> io::Result<()> {
        job.assign(child)
    }

    fn resume(&mut self, job: &platform::ProcessJob, child: &Child) -> io::Result<()> {
        job.resume(child)
    }

    fn take_stdin(&mut self, child: &mut Child) -> Option<ChildStdin> {
        child.stdin.take()
    }

    fn take_stdout(&mut self, child: &mut Child) -> Option<ChildStdout> {
        child.stdout.take()
    }

    fn take_stderr(&mut self, child: &mut Child) -> Option<ChildStderr> {
        child.stderr.take()
    }
}

impl ContainedPipedChild {
    pub(crate) fn spawn(command: Command) -> ContainedSpawnAttempt {
        Self::spawn_with_ops(command, &mut SystemLaunchOps)
    }

    fn spawn_with_ops(mut command: Command, ops: &mut impl LaunchOps) -> ContainedSpawnAttempt {
        let job = match ops.create_job() {
            Ok(job) => job,
            Err(cause) => {
                return ContainedSpawnAttempt::FailedNoOwner {
                    cause: CachedIoError::capture(&cause),
                };
            }
        };
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        ops.prepare_command(&mut command);
        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(cause) => {
                return ContainedSpawnAttempt::FailedNoOwner {
                    cause: CachedIoError::capture(&cause),
                };
            }
        };
        if let Err(cause) = ops.assign(&job, &child) {
            return ContainedSpawnAttempt::FailedWithOwner {
                cause: CachedIoError::capture(&cause),
                partial: PartialContainedChild::Unassigned { child, job },
            };
        }
        if let Err(cause) = ops.resume(&job, &child) {
            let stdin = ops.take_stdin(&mut child);
            let stdout = ops.take_stdout(&mut child);
            let stderr = ops.take_stderr(&mut child);
            return ContainedSpawnAttempt::FailedWithOwner {
                cause: CachedIoError::capture(&cause),
                partial: PartialContainedChild::Assigned {
                    process: ContainedChild::new(child, job),
                    stdin,
                    stdout,
                    stderr,
                },
            };
        }

        let stdin = ops.take_stdin(&mut child);
        let stdout = ops.take_stdout(&mut child);
        let stderr = ops.take_stderr(&mut child);
        let process = ContainedChild::new(child, job);
        match (stdin, stdout, stderr) {
            (Some(stdin), Some(stdout), Some(stderr)) => ContainedSpawnAttempt::Ready(Self {
                process,
                stdin,
                stdout,
                stderr,
            }),
            (stdin, stdout, stderr) => ContainedSpawnAttempt::FailedWithOwner {
                cause: CachedIoError::capture(&io::Error::from(io::ErrorKind::BrokenPipe)),
                partial: PartialContainedChild::Assigned {
                    process,
                    stdin,
                    stdout,
                    stderr,
                },
            },
        }
    }

    pub(crate) fn spawn_for_debugger(command: Command) -> io::Result<Self> {
        match Self::spawn(command) {
            ContainedSpawnAttempt::Ready(child) => Ok(child),
            ContainedSpawnAttempt::FailedNoOwner { cause } => Err(cause.to_error()),
            ContainedSpawnAttempt::FailedWithOwner { cause, partial } => {
                let cleanup = partial.cleanup_for_debugger();
                Err(effective_spawn_error(cause.to_error(), cleanup))
            }
        }
    }

    pub(crate) fn into_parts(self) -> (ContainedChild, ChildStdin, ChildStdout, ChildStderr) {
        (self.process, self.stdin, self.stdout, self.stderr)
    }
}

impl ContainedChild {
    fn new(child: Child, job: platform::ProcessJob) -> Self {
        Self {
            child,
            job,
            termination: TerminationState::NotAttempted,
            reap: ReapState::NotAttempted,
        }
    }

    pub(crate) fn try_wait_root(&mut self) -> io::Result<Option<ExitStatus>> {
        self.try_wait_root_with(&mut SystemProcessOps)
    }

    fn try_wait_root_with(&mut self, ops: &mut impl ProcessOps) -> io::Result<Option<ExitStatus>> {
        match self.reap {
            ReapState::Reaped(status) => return Ok(Some(status)),
            ReapState::Failed(error) => return Err(error.to_error()),
            ReapState::NotAttempted => {}
        }
        match ops.try_wait_root(&mut self.child) {
            Ok(Some(status)) => {
                self.reap = ReapState::Reaped(status);
                Ok(Some(status))
            }
            result => result,
        }
    }

    pub(crate) fn terminate_tree_once(&mut self) -> Result<(), CachedIoError> {
        self.terminate_tree_once_with(&mut SystemProcessOps)
    }

    fn terminate_tree_once_with(&mut self, ops: &mut impl ProcessOps) -> Result<(), CachedIoError> {
        match self.termination {
            TerminationState::Succeeded => return Ok(()),
            TerminationState::Failed(error) => return Err(error),
            TerminationState::NotAttempted => {}
        }
        match ops.terminate_tree(&mut self.job, &mut self.child) {
            Ok(()) => {
                self.termination = TerminationState::Succeeded;
                Ok(())
            }
            Err(error) => {
                let cached = CachedIoError::capture(&error);
                self.termination = TerminationState::Failed(cached);
                Err(cached)
            }
        }
    }

    #[allow(dead_code)]
    pub(crate) fn wait_root_until(
        &mut self,
        deadline: Instant,
    ) -> Result<ExitStatus, CachedIoError> {
        self.wait_root_until_with(deadline, &mut SystemProcessOps)
    }

    fn wait_root_until_with(
        &mut self,
        deadline: Instant,
        ops: &mut impl ProcessOps,
    ) -> Result<ExitStatus, CachedIoError> {
        match self.reap {
            ReapState::Reaped(status) => return Ok(status),
            ReapState::Failed(error) => return Err(error),
            ReapState::NotAttempted => {}
        }
        match ops.wait_root_until(&mut self.child, deadline) {
            Ok(status) => {
                self.reap = ReapState::Reaped(status);
                Ok(status)
            }
            Err(error) => {
                let cached = CachedIoError::capture(&error);
                self.reap = ReapState::Failed(cached);
                Err(cached)
            }
        }
    }

    pub(crate) fn wait_root_for_debugger(&mut self) -> io::Result<ExitStatus> {
        match self.reap {
            ReapState::Reaped(status) => return Ok(status),
            ReapState::Failed(error) => return Err(error.to_error()),
            ReapState::NotAttempted => {}
        }
        match SystemProcessOps.wait_root_for_debugger(&mut self.child) {
            Ok(status) => {
                self.reap = ReapState::Reaped(status);
                Ok(status)
            }
            Err(error) => {
                self.reap = ReapState::Failed(CachedIoError::capture(&error));
                Err(error)
            }
        }
    }

    pub(crate) fn cleanup_for_debugger(&mut self) -> io::Result<()> {
        cleanup_for_debugger(self)
    }

    #[cfg(windows)]
    #[allow(dead_code)]
    pub(crate) fn active_processes(&self) -> Result<u32, CachedIoError> {
        self.job
            .active_processes()
            .map_err(|error| CachedIoError::capture(&error))
    }
}

trait ProcessOps {
    fn try_wait_root(&mut self, child: &mut Child) -> io::Result<Option<ExitStatus>>;
    fn terminate_tree(
        &mut self,
        job: &mut platform::ProcessJob,
        child: &mut Child,
    ) -> io::Result<()>;
    fn wait_root_until(&mut self, child: &mut Child, deadline: Instant) -> io::Result<ExitStatus>;
    fn wait_root_for_debugger(&mut self, child: &mut Child) -> io::Result<ExitStatus>;
}

struct SystemProcessOps;

impl ProcessOps for SystemProcessOps {
    fn try_wait_root(&mut self, child: &mut Child) -> io::Result<Option<ExitStatus>> {
        child.try_wait()
    }

    fn terminate_tree(
        &mut self,
        job: &mut platform::ProcessJob,
        child: &mut Child,
    ) -> io::Result<()> {
        job.terminate(child)
    }

    fn wait_root_until(&mut self, child: &mut Child, deadline: Instant) -> io::Result<ExitStatus> {
        platform::wait_until(child, deadline)
    }

    fn wait_root_for_debugger(&mut self, child: &mut Child) -> io::Result<ExitStatus> {
        platform::wait_for_termination(child)
    }
}

impl PartialContainedChild {
    pub(crate) fn cleanup_for_debugger(self) -> io::Result<()> {
        self.cleanup_for_debugger_with(&mut SystemPartialCleanupOps)
    }

    fn cleanup_for_debugger_with(self, ops: &mut impl PartialCleanupOps) -> io::Result<()> {
        match self {
            Self::Unassigned {
                mut child,
                job: _job,
            } => {
                let termination = ops.terminate_unassigned(&mut child);
                let reap = ops.reap_unassigned(&mut child);
                termination.and(reap)
            }
            Self::Assigned {
                mut process,
                stdin,
                stdout,
                stderr,
            } => {
                let termination = ops.terminate_assigned(&mut process);
                let reap = ops.reap_assigned(&mut process);
                drop((stdin, stdout, stderr));
                termination.and(reap)
            }
        }
    }

    pub(crate) fn cleanup_for_language_until(
        &mut self,
        deadline: Instant,
    ) -> Result<(), PartialCleanupFailure> {
        match self {
            Self::Unassigned { child, job: _job } => {
                drop((child.stdin.take(), child.stdout.take(), child.stderr.take()));
                let termination = child.kill().map_err(|error| error.kind());
                let reap = platform::wait_until(child, deadline);
                if termination
                    .as_ref()
                    .is_err_and(|kind| *kind != io::ErrorKind::InvalidInput)
                {
                    Err(PartialCleanupFailure::Terminate)
                } else if reap.is_err() {
                    Err(PartialCleanupFailure::Reap)
                } else {
                    Ok(())
                }
            }
            Self::Assigned {
                process,
                stdin,
                stdout,
                stderr,
            } => {
                drop((stdin.take(), stdout.take(), stderr.take()));
                let termination = process.terminate_tree_once();
                let reap = process.wait_root_until(deadline);
                if termination
                    .as_ref()
                    .is_err_and(|error| error.kind() != io::ErrorKind::InvalidInput)
                {
                    return Err(PartialCleanupFailure::Terminate);
                }
                if reap.is_err() {
                    return Err(PartialCleanupFailure::Reap);
                }
                #[cfg(windows)]
                loop {
                    match process.active_processes() {
                        Ok(0) => break,
                        Ok(_) if Instant::now() < deadline => std::thread::park_timeout(
                            deadline
                                .saturating_duration_since(Instant::now())
                                .min(std::time::Duration::from_millis(1)),
                        ),
                        Ok(_) | Err(_) => {
                            return Err(PartialCleanupFailure::VerifyTreeEmpty);
                        }
                    }
                }
                Ok(())
            }
        }
    }

    #[cfg(all(test, unix))]
    pub(crate) fn unassigned_for_test(mut command: Command) -> io::Result<Self> {
        let job = platform::ProcessJob::create()?;
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        platform::ProcessJob::prepare_command(&mut command);
        let child = command.spawn()?;
        Ok(Self::Unassigned { child, job })
    }

    #[cfg(all(test, unix))]
    pub(crate) fn root_id(&self) -> u32 {
        match self {
            Self::Unassigned { child, .. } => child.id(),
            Self::Assigned { process, .. } => process.child.id(),
        }
    }
}

trait PartialCleanupOps {
    fn terminate_unassigned(&mut self, child: &mut Child) -> io::Result<()>;
    fn reap_unassigned(&mut self, child: &mut Child) -> io::Result<()>;
    fn terminate_assigned(&mut self, process: &mut ContainedChild) -> io::Result<()>;
    fn reap_assigned(&mut self, process: &mut ContainedChild) -> io::Result<()>;
}

struct SystemPartialCleanupOps;

impl PartialCleanupOps for SystemPartialCleanupOps {
    fn terminate_unassigned(&mut self, child: &mut Child) -> io::Result<()> {
        child.kill()
    }

    fn reap_unassigned(&mut self, child: &mut Child) -> io::Result<()> {
        platform::wait_for_termination(child).map(|_| ())
    }

    fn terminate_assigned(&mut self, process: &mut ContainedChild) -> io::Result<()> {
        process.terminate_for_debugger()
    }

    fn reap_assigned(&mut self, process: &mut ContainedChild) -> io::Result<()> {
        process.reap_for_debugger()
    }
}

trait DebuggerCleanup {
    fn terminate_for_debugger(&mut self) -> io::Result<()>;
    fn reap_for_debugger(&mut self) -> io::Result<()>;
}

impl DebuggerCleanup for ContainedChild {
    fn terminate_for_debugger(&mut self) -> io::Result<()> {
        self.terminate_tree_once().map_err(CachedIoError::to_error)
    }

    fn reap_for_debugger(&mut self) -> io::Result<()> {
        self.wait_root_for_debugger().map(|_| ())
    }
}

fn cleanup_for_debugger(value: &mut impl DebuggerCleanup) -> io::Result<()> {
    let termination = value.terminate_for_debugger();
    let reap = value.reap_for_debugger();
    termination.and(reap)
}

fn effective_spawn_error(original: io::Error, cleanup: io::Result<()>) -> io::Error {
    cleanup.err().unwrap_or(original)
}

#[cfg(not(windows))]
mod platform {
    use std::io;
    use std::process::{Child, Command, ExitStatus};
    use std::thread;
    use std::time::{Duration, Instant};

    pub(crate) struct ProcessJob;

    impl ProcessJob {
        pub(super) fn create() -> io::Result<Self> {
            Ok(Self)
        }

        pub(super) fn assign(&self, _child: &Child) -> io::Result<()> {
            Ok(())
        }

        pub(super) fn prepare_command(_command: &mut Command) {}

        pub(super) fn resume(&self, _child: &Child) -> io::Result<()> {
            Ok(())
        }

        pub(super) fn terminate(&mut self, child: &mut Child) -> io::Result<()> {
            child.kill()
        }
    }

    pub(super) fn wait_for_termination(child: &mut Child) -> io::Result<ExitStatus> {
        child.wait()
    }

    pub(super) fn wait_until(child: &mut Child, deadline: Instant) -> io::Result<ExitStatus> {
        loop {
            if let Some(status) = child.try_wait()? {
                return Ok(status);
            }
            let now = Instant::now();
            if now >= deadline {
                return Err(io::Error::from(io::ErrorKind::TimedOut));
            }
            thread::park_timeout(
                deadline
                    .saturating_duration_since(now)
                    .min(Duration::from_millis(1)),
            );
        }
    }
}

#[cfg(windows)]
mod platform {
    use std::ffi::c_void;
    use std::io;
    use std::mem::size_of;
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
    use std::os::windows::process::CommandExt;
    use std::process::{Child, Command, ExitStatus};
    use std::ptr;
    use std::time::Instant;

    use windows_sys::Win32::Foundation::{
        ERROR_NO_MORE_FILES, INVALID_HANDLE_VALUE, WAIT_FAILED, WAIT_OBJECT_0, WAIT_TIMEOUT,
    };
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, TH32CS_SNAPTHREAD, THREADENTRY32, Thread32First, Thread32Next,
    };
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        JOBOBJECT_BASIC_ACCOUNTING_INFORMATION, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JobObjectBasicAccountingInformation, JobObjectExtendedLimitInformation,
        QueryInformationJobObject, SetInformationJobObject, TerminateJobObject,
    };
    use windows_sys::Win32::System::Threading::{
        CREATE_NO_WINDOW, CREATE_SUSPENDED, GetProcessIdOfThread, OpenThread, ResumeThread,
        THREAD_QUERY_LIMITED_INFORMATION, THREAD_SUSPEND_RESUME, WaitForSingleObject,
    };

    const TERMINATION_WAIT_MILLIS: u32 = 5_000;

    pub(crate) struct ProcessJob {
        handle: Option<OwnedHandle>,
    }

    impl ProcessJob {
        pub(super) fn create() -> io::Result<Self> {
            // SAFETY: Null security attributes and name request a new anonymous job object.
            let raw_handle = unsafe { CreateJobObjectW(ptr::null(), ptr::null()) };
            if raw_handle.is_null() {
                return Err(io::Error::last_os_error());
            }
            // SAFETY: CreateJobObjectW returned a new owned handle, checked non-null above.
            let job = Self {
                handle: Some(unsafe { OwnedHandle::from_raw_handle(raw_handle) }),
            };
            let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
            limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            // SAFETY: The handle and initialized information remain valid for the call.
            let configured = unsafe {
                SetInformationJobObject(
                    job.raw_handle(),
                    JobObjectExtendedLimitInformation,
                    ptr::from_ref(&limits).cast::<c_void>(),
                    size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                )
            };
            if configured == 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(job)
        }

        pub(super) fn assign(&self, child: &Child) -> io::Result<()> {
            // SAFETY: Both handles are live for this call.
            let assigned = unsafe {
                AssignProcessToJobObject(self.raw_handle(), child.as_raw_handle().cast())
            };
            if assigned == 0 {
                Err(io::Error::last_os_error())
            } else {
                Ok(())
            }
        }

        pub(super) fn prepare_command(command: &mut Command) {
            command.creation_flags(CREATE_SUSPENDED | CREATE_NO_WINDOW);
        }

        pub(super) fn resume(&self, child: &Child) -> io::Result<()> {
            // SAFETY: The snapshot handle is checked before ownership is assumed.
            let snapshot_raw = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) };
            if snapshot_raw == INVALID_HANDLE_VALUE || snapshot_raw.is_null() {
                return Err(io::Error::last_os_error());
            }
            // SAFETY: CreateToolhelp32Snapshot returned a new owned handle.
            let snapshot = unsafe { OwnedHandle::from_raw_handle(snapshot_raw) };
            let mut entry = THREADENTRY32 {
                dwSize: size_of::<THREADENTRY32>() as u32,
                ..THREADENTRY32::default()
            };
            // SAFETY: entry is writable and its size field is initialized.
            if unsafe { Thread32First(snapshot.as_raw_handle(), &mut entry) } == 0 {
                return Err(io::Error::last_os_error());
            }

            let mut thread_handles = Vec::new();
            loop {
                if entry.th32OwnerProcessID == child.id() {
                    // SAFETY: The ID is from the live snapshot and the result is checked.
                    let thread_raw = unsafe {
                        OpenThread(
                            THREAD_SUSPEND_RESUME | THREAD_QUERY_LIMITED_INFORMATION,
                            0,
                            entry.th32ThreadID,
                        )
                    };
                    if thread_raw.is_null() {
                        return Err(io::Error::last_os_error());
                    }
                    // SAFETY: OpenThread returned a new owned handle.
                    let thread = unsafe { OwnedHandle::from_raw_handle(thread_raw) };
                    // SAFETY: The opened handle has query rights.
                    let owner = unsafe { GetProcessIdOfThread(thread.as_raw_handle()) };
                    if owner == 0 {
                        return Err(io::Error::last_os_error());
                    }
                    if owner != child.id() {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "contained thread ownership changed during suspended launch",
                        ));
                    }
                    thread_handles.push(thread);
                }

                // SAFETY: entry remains valid writable storage for the next record.
                if unsafe { Thread32Next(snapshot.as_raw_handle(), &mut entry) } == 0 {
                    let error = io::Error::last_os_error();
                    if error.raw_os_error() == Some(ERROR_NO_MORE_FILES as i32) {
                        break;
                    }
                    return Err(error);
                }
            }

            if thread_handles.len() != 1 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "suspended child must have exactly one primary thread",
                ));
            }
            // SAFETY: This zero-timeout wait only probes the live process handle.
            let process_wait = unsafe { WaitForSingleObject(child.as_raw_handle().cast(), 0) };
            if process_wait == WAIT_FAILED {
                return Err(io::Error::last_os_error());
            }
            if process_wait != WAIT_TIMEOUT {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "suspended child exited before primary-thread resume",
                ));
            }
            // SAFETY: The sole thread belongs to the still-suspended child.
            let previous = unsafe { ResumeThread(thread_handles[0].as_raw_handle()) };
            if previous == u32::MAX {
                return Err(io::Error::last_os_error());
            }
            if previous != 1 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "contained primary thread had an unexpected suspend count",
                ));
            }
            Ok(())
        }

        pub(super) fn terminate(&mut self, child: &mut Child) -> io::Result<()> {
            // SAFETY: The handle is a live Job Object handle.
            let terminated = unsafe { TerminateJobObject(self.raw_handle(), 1) };
            if terminated == 0 {
                let error = io::Error::last_os_error();
                drop(self.handle.take());
                let _ = child.kill();
                Err(error)
            } else {
                Ok(())
            }
        }

        pub(super) fn active_processes(&self) -> io::Result<u32> {
            let Some(handle) = self.handle.as_ref() else {
                return Err(io::Error::from(io::ErrorKind::BrokenPipe));
            };
            let mut accounting = JOBOBJECT_BASIC_ACCOUNTING_INFORMATION::default();
            let mut returned = 0;
            // SAFETY: The live Job handle and initialized output storage remain valid for the
            // duration of this fixed-size accounting query.
            let queried = unsafe {
                QueryInformationJobObject(
                    handle.as_raw_handle(),
                    JobObjectBasicAccountingInformation,
                    ptr::from_mut(&mut accounting).cast::<c_void>(),
                    size_of::<JOBOBJECT_BASIC_ACCOUNTING_INFORMATION>() as u32,
                    &mut returned,
                )
            };
            if queried == 0 {
                Err(io::Error::last_os_error())
            } else {
                Ok(accounting.ActiveProcesses)
            }
        }

        fn raw_handle(&self) -> *mut c_void {
            self.handle
                .as_ref()
                .expect("contained Job Object handle remains live")
                .as_raw_handle()
        }
    }

    pub(super) fn wait_for_termination(child: &mut Child) -> io::Result<ExitStatus> {
        // SAFETY: Child owns the exact process handle and the wait is bounded on Windows.
        match unsafe { WaitForSingleObject(child.as_raw_handle().cast(), TERMINATION_WAIT_MILLIS) }
        {
            WAIT_OBJECT_0 => child.wait(),
            WAIT_TIMEOUT => Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "contained child did not exit after termination",
            )),
            WAIT_FAILED => Err(io::Error::last_os_error()),
            result => Err(io::Error::other(format!(
                "unexpected contained-child termination wait result: {result}"
            ))),
        }
    }

    pub(super) fn wait_until(child: &mut Child, deadline: Instant) -> io::Result<ExitStatus> {
        let now = Instant::now();
        if now >= deadline {
            return Err(io::Error::from(io::ErrorKind::TimedOut));
        }
        let remaining = deadline.saturating_duration_since(now);
        let millis = remaining
            .as_millis()
            .saturating_add(if remaining.subsec_nanos().is_multiple_of(1_000_000) {
                0
            } else {
                1
            })
            .min(u128::from(u32::MAX)) as u32;
        // SAFETY: Child owns the exact live process handle and the timeout is derived from the
        // caller's single absolute deadline.
        match unsafe { WaitForSingleObject(child.as_raw_handle().cast(), millis) } {
            WAIT_OBJECT_0 => child.wait(),
            WAIT_TIMEOUT => Err(io::Error::from(io::ErrorKind::TimedOut)),
            WAIT_FAILED => Err(io::Error::last_os_error()),
            result => Err(io::Error::other(format!(
                "unexpected contained-child deadline wait result: {result}"
            ))),
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use std::io::{BufRead, BufReader, Read, Write};
        use std::process::{Command, Stdio};
        use std::thread;
        use std::time::{Duration, Instant};
        use windows_sys::Win32::Foundation::{WAIT_OBJECT_0, WAIT_TIMEOUT};
        use windows_sys::Win32::System::JobObjects::{IsProcessInJob, QueryInformationJobObject};
        use windows_sys::Win32::System::Threading::{
            OpenProcess, PROCESS_SYNCHRONIZE, WaitForSingleObject,
        };

        #[test]
        fn job_is_configured_to_kill_all_members_when_its_handle_closes() {
            let job = ProcessJob::create().expect("create process job");
            let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
            let mut returned = 0;
            // SAFETY: The output pointer and size describe writable storage of the class queried.
            let queried = unsafe {
                QueryInformationJobObject(
                    job.raw_handle(),
                    JobObjectExtendedLimitInformation,
                    ptr::from_mut(&mut limits).cast::<c_void>(),
                    size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                    &mut returned,
                )
            };

            assert_ne!(queried, 0, "{}", io::Error::last_os_error());
            assert_eq!(
                limits.BasicLimitInformation.LimitFlags,
                JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE
            );
        }

        #[test]
        fn active_process_accounting_tracks_an_assigned_root_while_job_is_live() {
            let mut job = ProcessJob::create().expect("create process job");
            assert_eq!(job.active_processes().expect("query empty job"), 0);
            let mut command = Command::new("cmd.exe");
            command
                .args(["/D", "/C", "exit 0"])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            ProcessJob::prepare_command(&mut command);
            let mut child = command.spawn().expect("spawn accounting fixture");
            if let Err(error) = job.assign(&child) {
                let _ = child.kill();
                let _ = child.wait();
                panic!("assign accounting fixture: {error}");
            }

            assert_eq!(job.active_processes().expect("query assigned job"), 1);
            job.terminate(&mut child)
                .expect("terminate accounting fixture");
            child.wait().expect("reap accounting fixture");
        }

        #[test]
        fn closing_the_job_terminates_an_assigned_root_process() {
            let job = ProcessJob::create().expect("create process job");
            let mut child = Command::new("cmd.exe")
                .args(["/D", "/C", "ping.exe -n 30 127.0.0.1"])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .expect("spawn contained process");
            if let Err(error) = job.assign(&child) {
                let _ = child.kill();
                let _ = child.wait();
                panic!("assign contained process: {error}");
            }
            thread::sleep(Duration::from_millis(100));
            assert!(
                child.try_wait().expect("probe contained process").is_none(),
                "contained process fixture exited before the job handle closed"
            );

            drop(job);
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                match child.try_wait() {
                    Ok(Some(_)) => break,
                    Ok(None) if Instant::now() < deadline => {
                        thread::sleep(Duration::from_millis(10));
                    }
                    Ok(None) => {
                        let _ = child.kill();
                        let _ = child.wait();
                        panic!("contained process survived the closed job handle");
                    }
                    Err(error) => panic!("wait for contained process: {error}"),
                }
            }
        }

        #[test]
        fn forced_termination_reaps_a_descendant_that_inherits_the_childs_pipes() {
            let mut job = ProcessJob::create().expect("create process job");
            let fixture = r#"$null = [Console]::In.ReadLine(); $p = Start-Process -FilePath "$env:SystemRoot\System32\ping.exe" -ArgumentList "-n","30","127.0.0.1" -NoNewWindow -PassThru; [Console]::Out.WriteLine($p.Id); [Console]::Out.Flush(); $p.WaitForExit()"#;
            let mut child = Command::new("powershell.exe")
                .args([
                    "-NoLogo",
                    "-NoProfile",
                    "-NonInteractive",
                    "-Command",
                    fixture,
                ])
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn()
                .expect("spawn synchronized process-tree fixture");
            if let Err(error) = job.assign(&child) {
                let _ = child.kill();
                let _ = child.wait();
                panic!("assign process-tree fixture: {error}");
            }

            let mut stdin = child.stdin.take().expect("fixture stdin");
            let mut stdout = BufReader::new(child.stdout.take().expect("fixture stdout"));
            writeln!(stdin).expect("release process-tree fixture");
            drop(stdin);
            let descendant_pid = loop {
                let mut line = String::new();
                let bytes = stdout.read_line(&mut line).expect("read fixture output");
                assert_ne!(
                    bytes, 0,
                    "fixture exited before reporting its descendant PID"
                );
                if let Ok(pid) = line.trim().parse::<u32>() {
                    break pid;
                }
            };
            // SAFETY: The PID was reported by the live fixture. The returned handle is checked
            // before converting it to an owned handle.
            let descendant_raw = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, 0, descendant_pid) };
            if descendant_raw.is_null() {
                let _ = job.terminate(&mut child);
                let _ = child.wait();
                panic!("open descendant process: {}", io::Error::last_os_error());
            }
            // SAFETY: OpenProcess returned a new owned handle, checked non-null above.
            let descendant = unsafe { OwnedHandle::from_raw_handle(descendant_raw) };
            // SAFETY: The descendant process handle is live and the zero timeout only probes it.
            assert_eq!(
                unsafe { WaitForSingleObject(descendant.as_raw_handle(), 0) },
                WAIT_TIMEOUT,
                "descendant exited before forced termination"
            );

            job.terminate(&mut child).expect("terminate process job");
            child.wait().expect("reap process-tree fixture");
            // SAFETY: The descendant process handle remains live until this wait completes.
            let descendant_wait = unsafe { WaitForSingleObject(descendant.as_raw_handle(), 5_000) };
            assert_eq!(
                descendant_wait, WAIT_OBJECT_0,
                "descendant survived forced Job Object termination"
            );
            drop(stdout);
        }

        #[test]
        fn spawned_process_remains_suspended_until_job_assignment_completes() {
            let job = ProcessJob::create().expect("create process job");
            let mut command = Command::new("cmd.exe");
            command
                .args(["/D", "/C", "echo suspended-child-ready"])
                .stdout(Stdio::piped());
            ProcessJob::prepare_command(&mut command);
            let mut child = command.spawn().expect("spawn suspended fixture");
            let mut stdout = child.stdout.take().expect("suspended fixture stdout");
            let (output_tx, output_rx) = std::sync::mpsc::sync_channel(1);
            let reader = thread::spawn(move || {
                let mut output = String::new();
                let result = stdout.read_to_string(&mut output).map(|_| output);
                let _ = output_tx.send(result);
            });

            assert!(matches!(
                output_rx.recv_timeout(Duration::from_millis(100)),
                Err(std::sync::mpsc::RecvTimeoutError::Timeout)
            ));
            if let Err(error) = job.assign(&child) {
                let _ = child.kill();
                let _ = child.wait();
                panic!("assign suspended fixture: {error}");
            }
            let mut assigned = 0;
            // SAFETY: Both handles remain live, and assigned points to writable BOOL storage.
            let queried = unsafe {
                IsProcessInJob(
                    child.as_raw_handle().cast(),
                    job.raw_handle(),
                    &mut assigned,
                )
            };
            assert_ne!(queried, 0, "{}", io::Error::last_os_error());
            assert_ne!(assigned, 0, "fixture was resumed outside the process job");
            assert!(matches!(
                output_rx.recv_timeout(Duration::from_millis(100)),
                Err(std::sync::mpsc::RecvTimeoutError::Timeout)
            ));
            job.resume(&child).expect("resume assigned fixture");
            let output = output_rx
                .recv_timeout(Duration::from_secs(5))
                .expect("fixture produced output after resume")
                .expect("read resumed fixture output");
            assert!(output.contains("suspended-child-ready"));
            reader.join().expect("join suspended fixture reader");
            assert!(child.wait().expect("reap resumed fixture").success());
        }

        #[test]
        fn descendant_holding_stdout_is_terminated_after_the_root_is_reaped() {
            let mut job = ProcessJob::create().expect("create process job");
            let fixture = r#"$p = Start-Process -FilePath "$env:SystemRoot\System32\ping.exe" -ArgumentList "-n","30","127.0.0.1" -NoNewWindow -PassThru; [Console]::Out.WriteLine($p.Id); [Console]::Out.Flush()"#;
            let mut command = Command::new("powershell.exe");
            command
                .args([
                    "-NoLogo",
                    "-NoProfile",
                    "-NonInteractive",
                    "-Command",
                    fixture,
                ])
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::null());
            ProcessJob::prepare_command(&mut command);
            let mut child = command.spawn().expect("spawn root-exit fixture");
            if let Err(error) = job.assign(&child) {
                let _ = child.kill();
                let _ = child.wait();
                panic!("assign root-exit fixture: {error}");
            }
            job.resume(&child).expect("resume root-exit fixture");

            let mut stdout = BufReader::new(child.stdout.take().expect("fixture stdout"));
            let descendant_pid = loop {
                let mut line = String::new();
                let bytes = stdout.read_line(&mut line).expect("read fixture output");
                assert_ne!(bytes, 0, "fixture exited before reporting descendant PID");
                if let Ok(pid) = line.trim().parse::<u32>() {
                    break pid;
                }
            };
            // SAFETY: The PID was reported by the live fixture. The returned handle is checked
            // before converting it to an owned handle.
            let descendant_raw = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, 0, descendant_pid) };
            if descendant_raw.is_null() {
                let error = io::Error::last_os_error();
                let _ = job.terminate(&mut child);
                let _ = child.wait();
                panic!("open descendant process: {error}");
            }
            // SAFETY: OpenProcess returned a new owned handle, checked non-null above.
            let descendant = unsafe { OwnedHandle::from_raw_handle(descendant_raw) };
            child.wait().expect("reap root process");

            let (eof_tx, eof_rx) = std::sync::mpsc::sync_channel(1);
            let reader = thread::spawn(move || {
                let mut retained = Vec::new();
                let result = stdout.read_to_end(&mut retained);
                let _ = eof_tx.send(result);
            });
            assert!(matches!(
                eof_rx.recv_timeout(Duration::from_millis(150)),
                Err(std::sync::mpsc::RecvTimeoutError::Timeout)
            ));

            job.terminate(&mut child)
                .expect("terminate descendants after root reap");
            eof_rx
                .recv_timeout(Duration::from_secs(5))
                .expect("stdout reader reached EOF")
                .expect("read descendant output");
            reader.join().expect("join stdout reader");
            // SAFETY: The descendant process handle remains live until this wait completes.
            assert_eq!(
                unsafe { WaitForSingleObject(descendant.as_raw_handle(), 5_000) },
                WAIT_OBJECT_0,
                "descendant survived termination after root reap"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    #[cfg(unix)]
    use std::io::{Read, Write};
    #[cfg(unix)]
    use std::process::Command;
    #[cfg(unix)]
    use std::time::{Duration, Instant};

    #[cfg(unix)]
    use super::{
        ContainedChild, ContainedPipedChild, ContainedSpawnAttempt, LaunchOps, PartialCleanupOps,
        PartialContainedChild, ProcessOps,
    };
    use super::{DebuggerCleanup, cleanup_for_debugger, effective_spawn_error};

    #[cfg(unix)]
    #[derive(Default)]
    struct FaultLaunchOps {
        assign_error: Option<std::io::ErrorKind>,
        resume_error: Option<std::io::ErrorKind>,
        missing_pipe: Option<&'static str>,
        assigned: usize,
        resumed: usize,
    }

    #[cfg(unix)]
    impl LaunchOps for FaultLaunchOps {
        fn create_job(&mut self) -> std::io::Result<super::platform::ProcessJob> {
            super::platform::ProcessJob::create()
        }

        fn prepare_command(&mut self, command: &mut Command) {
            super::platform::ProcessJob::prepare_command(command);
        }

        fn assign(
            &mut self,
            job: &super::platform::ProcessJob,
            child: &std::process::Child,
        ) -> std::io::Result<()> {
            self.assigned += 1;
            self.assign_error
                .map_or_else(|| job.assign(child), |kind| Err(std::io::Error::from(kind)))
        }

        fn resume(
            &mut self,
            job: &super::platform::ProcessJob,
            child: &std::process::Child,
        ) -> std::io::Result<()> {
            self.resumed += 1;
            self.resume_error
                .map_or_else(|| job.resume(child), |kind| Err(std::io::Error::from(kind)))
        }

        fn take_stdin(
            &mut self,
            child: &mut std::process::Child,
        ) -> Option<std::process::ChildStdin> {
            let pipe = child.stdin.take();
            (self.missing_pipe != Some("stdin"))
                .then_some(pipe)
                .flatten()
        }

        fn take_stdout(
            &mut self,
            child: &mut std::process::Child,
        ) -> Option<std::process::ChildStdout> {
            let pipe = child.stdout.take();
            (self.missing_pipe != Some("stdout"))
                .then_some(pipe)
                .flatten()
        }

        fn take_stderr(
            &mut self,
            child: &mut std::process::Child,
        ) -> Option<std::process::ChildStderr> {
            let pipe = child.stderr.take();
            (self.missing_pipe != Some("stderr"))
                .then_some(pipe)
                .flatten()
        }
    }

    #[cfg(unix)]
    #[derive(Default)]
    struct FaultProcessOps {
        terminations: usize,
        reaps: usize,
        termination_error: Option<std::io::ErrorKind>,
        reap_error: Option<std::io::ErrorKind>,
        reap_status: Option<std::process::ExitStatus>,
    }

    #[cfg(unix)]
    impl ProcessOps for FaultProcessOps {
        fn try_wait_root(
            &mut self,
            child: &mut std::process::Child,
        ) -> std::io::Result<Option<std::process::ExitStatus>> {
            child.try_wait()
        }

        fn terminate_tree(
            &mut self,
            _job: &mut super::platform::ProcessJob,
            _child: &mut std::process::Child,
        ) -> std::io::Result<()> {
            self.terminations += 1;
            self.termination_error
                .map_or(Ok(()), |kind| Err(std::io::Error::from(kind)))
        }

        fn wait_root_until(
            &mut self,
            _child: &mut std::process::Child,
            _deadline: Instant,
        ) -> std::io::Result<std::process::ExitStatus> {
            self.reaps += 1;
            if let Some(kind) = self.reap_error {
                Err(std::io::Error::from(kind))
            } else if let Some(status) = self.reap_status {
                Ok(status)
            } else {
                Err(std::io::Error::from(std::io::ErrorKind::TimedOut))
            }
        }

        fn wait_root_for_debugger(
            &mut self,
            child: &mut std::process::Child,
        ) -> std::io::Result<std::process::ExitStatus> {
            child.wait()
        }
    }

    #[cfg(unix)]
    #[derive(Default)]
    struct CountingPartialCleanup {
        terminations: usize,
        reaps: usize,
        termination_error: Option<std::io::ErrorKind>,
        reap_error: Option<std::io::ErrorKind>,
    }

    #[cfg(unix)]
    impl PartialCleanupOps for CountingPartialCleanup {
        fn terminate_unassigned(&mut self, child: &mut std::process::Child) -> std::io::Result<()> {
            self.terminations += 1;
            let actual = child.kill();
            self.termination_error
                .map_or(actual, |kind| Err(std::io::Error::from(kind)))
        }

        fn reap_unassigned(&mut self, child: &mut std::process::Child) -> std::io::Result<()> {
            self.reaps += 1;
            let actual = child.wait().map(|_| ());
            self.reap_error
                .map_or(actual, |kind| Err(std::io::Error::from(kind)))
        }

        fn terminate_assigned(&mut self, process: &mut ContainedChild) -> std::io::Result<()> {
            self.terminations += 1;
            let actual = process.child.kill();
            self.termination_error
                .map_or(actual, |kind| Err(std::io::Error::from(kind)))
        }

        fn reap_assigned(&mut self, process: &mut ContainedChild) -> std::io::Result<()> {
            self.reaps += 1;
            let actual = process.child.wait().map(|_| ());
            self.reap_error
                .map_or(actual, |kind| Err(std::io::Error::from(kind)))
        }
    }

    #[derive(Default)]
    struct FakeCleanup {
        terminated: usize,
        reaped: usize,
        termination_error: Option<std::io::ErrorKind>,
        reap_error: Option<std::io::ErrorKind>,
    }

    impl DebuggerCleanup for FakeCleanup {
        fn terminate_for_debugger(&mut self) -> std::io::Result<()> {
            self.terminated += 1;
            self.termination_error
                .map_or(Ok(()), |kind| Err(std::io::Error::from(kind)))
        }

        fn reap_for_debugger(&mut self) -> std::io::Result<()> {
            self.reaped += 1;
            self.reap_error
                .map_or(Ok(()), |kind| Err(std::io::Error::from(kind)))
        }
    }

    #[test]
    fn debugger_cleanup_reaps_after_termination_failure_and_keeps_first_error() {
        let mut cleanup = FakeCleanup {
            termination_error: Some(std::io::ErrorKind::PermissionDenied),
            reap_error: Some(std::io::ErrorKind::TimedOut),
            ..FakeCleanup::default()
        };

        let error = cleanup_for_debugger(&mut cleanup).unwrap_err();

        assert_eq!(cleanup.terminated, 1);
        assert_eq!(cleanup.reaped, 1);
        assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
    }

    #[test]
    fn debugger_spawn_error_uses_cleanup_error_only_when_cleanup_failed() {
        let original = std::io::Error::from(std::io::ErrorKind::InvalidData);
        let unchanged = effective_spawn_error(original, Ok(()));
        assert_eq!(unchanged.kind(), std::io::ErrorKind::InvalidData);

        let original = std::io::Error::from(std::io::ErrorKind::InvalidData);
        let overridden = effective_spawn_error(
            original,
            Err(std::io::Error::from(std::io::ErrorKind::TimedOut)),
        );
        assert_eq!(overridden.kind(), std::io::ErrorKind::TimedOut);
    }

    #[test]
    fn launch_error_precedence_is_termination_then_reap_then_original() {
        let original = || std::io::Error::from(std::io::ErrorKind::InvalidData);
        let cleanup_error = |termination_error, reap_error| {
            let mut cleanup = FakeCleanup {
                termination_error,
                reap_error,
                ..FakeCleanup::default()
            };
            cleanup_for_debugger(&mut cleanup)
        };

        let error = effective_spawn_error(
            original(),
            cleanup_error(
                Some(std::io::ErrorKind::PermissionDenied),
                Some(std::io::ErrorKind::TimedOut),
            ),
        );
        assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
        let error = effective_spawn_error(
            original(),
            cleanup_error(None, Some(std::io::ErrorKind::TimedOut)),
        );
        assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
        let error = effective_spawn_error(original(), cleanup_error(None, None));
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
    }

    #[cfg(unix)]
    fn long_running_command() -> Command {
        let mut command = Command::new("sh");
        command.args(["-c", "exec sleep 30"]);
        command
    }

    #[cfg(unix)]
    fn unassigned_partial_failure() -> (super::CachedIoError, PartialContainedChild) {
        let mut ops = FaultLaunchOps {
            assign_error: Some(std::io::ErrorKind::InvalidData),
            ..FaultLaunchOps::default()
        };
        let ContainedSpawnAttempt::FailedWithOwner { cause, partial } =
            ContainedPipedChild::spawn_with_ops(long_running_command(), &mut ops)
        else {
            panic!("assignment failure did not retain ownership");
        };
        assert!(matches!(partial, PartialContainedChild::Unassigned { .. }));
        assert_eq!(ops.resumed, 0);
        (cause, partial)
    }

    #[cfg(unix)]
    fn assigned_partial_failure() -> (super::CachedIoError, PartialContainedChild) {
        let mut ops = FaultLaunchOps {
            resume_error: Some(std::io::ErrorKind::InvalidData),
            ..FaultLaunchOps::default()
        };
        let ContainedSpawnAttempt::FailedWithOwner { cause, partial } =
            ContainedPipedChild::spawn_with_ops(long_running_command(), &mut ops)
        else {
            panic!("resume failure did not retain ownership");
        };
        assert!(matches!(partial, PartialContainedChild::Assigned { .. }));
        (cause, partial)
    }

    #[cfg(unix)]
    fn assert_partial_cleanup_error_precedence(
        mut make_partial: impl FnMut() -> (super::CachedIoError, PartialContainedChild),
    ) {
        for (termination_error, reap_error, expected) in [
            (
                Some(std::io::ErrorKind::PermissionDenied),
                Some(std::io::ErrorKind::TimedOut),
                std::io::ErrorKind::PermissionDenied,
            ),
            (
                None,
                Some(std::io::ErrorKind::TimedOut),
                std::io::ErrorKind::TimedOut,
            ),
            (None, None, std::io::ErrorKind::InvalidData),
        ] {
            let (cause, partial) = make_partial();
            let mut cleanup = CountingPartialCleanup {
                termination_error,
                reap_error,
                ..CountingPartialCleanup::default()
            };

            let cleanup_result = partial.cleanup_for_debugger_with(&mut cleanup);
            let error = effective_spawn_error(cause.to_error(), cleanup_result);

            assert_eq!(cleanup.terminations, 1);
            assert_eq!(cleanup.reaps, 1);
            assert_eq!(error.kind(), expected);
        }
    }

    #[cfg(unix)]
    #[test]
    fn unassigned_partial_cleanup_attempts_both_stages_with_launch_error_precedence() {
        assert_partial_cleanup_error_precedence(unassigned_partial_failure);
    }

    #[cfg(unix)]
    #[test]
    fn assigned_partial_cleanup_attempts_both_stages_with_launch_error_precedence() {
        assert_partial_cleanup_error_precedence(assigned_partial_failure);
    }

    #[cfg(unix)]
    #[test]
    fn strict_partial_cleanup_honors_an_expired_deadline_without_consuming_the_owner() {
        let (_cause, mut partial) = unassigned_partial_failure();
        let started = Instant::now();

        let error = partial
            .cleanup_for_language_until(Instant::now())
            .expect_err("expired cleanup deadline must fail");

        assert_eq!(error, super::PartialCleanupFailure::Reap);
        assert!(started.elapsed() < Duration::from_secs(1));
        let _ = partial.cleanup_for_debugger();
    }

    #[cfg(unix)]
    #[test]
    fn strict_assigned_partial_cleanup_closes_every_pipe_before_reaping() {
        let (_cause, mut partial) = assigned_partial_failure();

        let _ = partial.cleanup_for_language_until(Instant::now());

        let PartialContainedChild::Assigned {
            stdin,
            stdout,
            stderr,
            ..
        } = &partial
        else {
            panic!("resume failure was not assigned");
        };
        assert!(stdin.is_none());
        assert!(stdout.is_none());
        assert!(stderr.is_none());
        let _ = partial.cleanup_for_debugger();
    }

    #[cfg(unix)]
    #[test]
    fn assignment_failure_never_resumes_and_returns_unassigned_owner() {
        let mut ops = FaultLaunchOps {
            assign_error: Some(std::io::ErrorKind::PermissionDenied),
            ..FaultLaunchOps::default()
        };

        let attempt = ContainedPipedChild::spawn_with_ops(long_running_command(), &mut ops);

        let ContainedSpawnAttempt::FailedWithOwner { cause, partial } = attempt else {
            panic!("assignment failure did not retain ownership");
        };
        assert_eq!(cause.kind(), std::io::ErrorKind::PermissionDenied);
        assert!(matches!(partial, PartialContainedChild::Unassigned { .. }));
        assert_eq!(ops.assigned, 1);
        assert_eq!(ops.resumed, 0);
        let mut cleanup = CountingPartialCleanup::default();
        partial
            .cleanup_for_debugger_with(&mut cleanup)
            .expect("clean failed launch");
        assert_eq!(cleanup.terminations, 1);
        assert_eq!(cleanup.reaps, 1);
    }

    #[cfg(unix)]
    #[test]
    fn resume_failure_returns_assigned_owner_with_all_remaining_pipes() {
        let mut ops = FaultLaunchOps {
            resume_error: Some(std::io::ErrorKind::InvalidData),
            ..FaultLaunchOps::default()
        };

        let attempt = ContainedPipedChild::spawn_with_ops(long_running_command(), &mut ops);

        let ContainedSpawnAttempt::FailedWithOwner { cause, partial } = attempt else {
            panic!("resume failure did not retain ownership");
        };
        assert_eq!(cause.kind(), std::io::ErrorKind::InvalidData);
        let PartialContainedChild::Assigned {
            stdin,
            stdout,
            stderr,
            ..
        } = &partial
        else {
            panic!("resume failure was not assigned");
        };
        assert!(stdin.is_some());
        assert!(stdout.is_some());
        assert!(stderr.is_some());
        assert_eq!(ops.resumed, 1);
        let mut cleanup = CountingPartialCleanup::default();
        partial
            .cleanup_for_debugger_with(&mut cleanup)
            .expect("clean failed launch");
        assert_eq!(cleanup.terminations, 1);
        assert_eq!(cleanup.reaps, 1);
    }

    #[cfg(unix)]
    #[test]
    fn each_missing_pipe_retains_the_exact_other_pipe_owners() {
        for missing in ["stdin", "stdout", "stderr"] {
            let mut ops = FaultLaunchOps {
                missing_pipe: Some(missing),
                ..FaultLaunchOps::default()
            };
            let attempt = ContainedPipedChild::spawn_with_ops(long_running_command(), &mut ops);
            let ContainedSpawnAttempt::FailedWithOwner { partial, .. } = attempt else {
                panic!("missing {missing} did not retain ownership");
            };
            let PartialContainedChild::Assigned {
                stdin,
                stdout,
                stderr,
                ..
            } = &partial
            else {
                panic!("missing {missing} was not assigned");
            };
            assert_eq!(stdin.is_some(), missing != "stdin");
            assert_eq!(stdout.is_some(), missing != "stdout");
            assert_eq!(stderr.is_some(), missing != "stderr");
            partial.cleanup_for_debugger().expect("clean failed launch");
        }
    }

    #[cfg(unix)]
    fn contained_fixture() -> ContainedChild {
        let ContainedSpawnAttempt::Ready(child) =
            ContainedPipedChild::spawn(long_running_command())
        else {
            panic!("spawn fixture");
        };
        let (process, stdin, stdout, stderr) = child.into_parts();
        drop((stdin, stdout, stderr));
        process
    }

    #[cfg(unix)]
    #[test]
    fn failed_terminate_is_cached_and_touches_the_platform_once() {
        let mut process = contained_fixture();
        let mut ops = FaultProcessOps {
            termination_error: Some(std::io::ErrorKind::PermissionDenied),
            ..FaultProcessOps::default()
        };

        let first = process.terminate_tree_once_with(&mut ops).unwrap_err();
        let second = process.terminate_tree_once_with(&mut ops).unwrap_err();

        assert_eq!(first.kind(), std::io::ErrorKind::PermissionDenied);
        assert_eq!(second, first);
        assert_eq!(ops.terminations, 1);
        let _ = process.child.kill();
        let _ = process.child.wait();
    }

    #[cfg(unix)]
    #[test]
    fn successful_terminate_is_cached_and_touches_the_platform_once() {
        let mut process = contained_fixture();
        let mut ops = FaultProcessOps::default();

        process
            .terminate_tree_once_with(&mut ops)
            .expect("first termination");
        process
            .terminate_tree_once_with(&mut ops)
            .expect("cached termination");

        assert_eq!(ops.terminations, 1);
        let _ = process.child.kill();
        let _ = process.child.wait();
    }

    #[cfg(unix)]
    #[test]
    fn successful_deadline_reap_is_cached_and_touches_the_platform_once() {
        use std::os::unix::process::ExitStatusExt;

        let mut process = contained_fixture();
        let mut ops = FaultProcessOps {
            reap_status: Some(std::process::ExitStatus::from_raw(7 << 8)),
            ..FaultProcessOps::default()
        };
        let deadline = Instant::now() + Duration::from_secs(1);

        let first = process
            .wait_root_until_with(deadline, &mut ops)
            .expect("first reap");
        let second = process
            .wait_root_until_with(deadline, &mut ops)
            .expect("cached reap");

        assert_eq!(first.code(), Some(7));
        assert_eq!(second.code(), Some(7));
        assert_eq!(ops.reaps, 1);
        let _ = process.child.kill();
        let _ = process.child.wait();
    }

    #[cfg(unix)]
    #[test]
    fn failed_deadline_reap_is_typed_bounded_and_cached() {
        let mut process = contained_fixture();
        let mut ops = FaultProcessOps {
            reap_error: Some(std::io::ErrorKind::TimedOut),
            ..FaultProcessOps::default()
        };
        let started = Instant::now();
        let deadline = started + Duration::from_millis(20);

        let first = process
            .wait_root_until_with(deadline, &mut ops)
            .unwrap_err();
        let second = process
            .wait_root_until_with(deadline, &mut ops)
            .unwrap_err();

        assert_eq!(first.kind(), std::io::ErrorKind::TimedOut);
        assert_eq!(second, first);
        assert!(started.elapsed() < Duration::from_secs(1));
        assert_eq!(ops.reaps, 1);
        let _ = process.child.kill();
        let _ = process.child.wait();
    }

    #[cfg(unix)]
    #[test]
    fn production_expired_deadline_reap_is_typed_bounded_and_cached() {
        let mut process = contained_fixture();
        assert!(
            process
                .child
                .try_wait()
                .expect("probe live deadline fixture")
                .is_none()
        );
        let deadline = Instant::now()
            .checked_sub(Duration::from_secs(1))
            .expect("construct expired deadline");
        let started = Instant::now();

        let first = process.wait_root_until(deadline).unwrap_err();

        assert_eq!(first.kind(), std::io::ErrorKind::TimedOut);
        assert!(started.elapsed() < Duration::from_secs(1));
        process.child.kill().expect("kill deadline fixture");
        process.child.wait().expect("reap deadline fixture");

        let second = process.wait_root_until(deadline).unwrap_err();
        assert_eq!(second, first);
    }

    #[cfg(unix)]
    #[test]
    fn successful_spawn_returns_one_process_owner_and_all_three_pipes() {
        let mut command = Command::new("sh");
        command.args(["-c", "IFS= read -r line; printf '%s' \"$line\""]);

        let ContainedSpawnAttempt::Ready(child) = ContainedPipedChild::spawn(command) else {
            panic!("contained child did not launch");
        };
        let (mut process, mut stdin, mut stdout, stderr) = child.into_parts();
        writeln!(stdin, "oxide").expect("write child stdin");
        drop(stdin);
        drop(stderr);
        let mut output = String::new();
        stdout
            .read_to_string(&mut output)
            .expect("read child stdout");
        let status = process
            .wait_root_for_debugger()
            .expect("reap contained root");

        assert_eq!(output, "oxide");
        assert!(status.success());
    }

    #[cfg(unix)]
    #[test]
    fn successful_root_reap_is_cached() {
        let mut command = Command::new("sh");
        command.args(["-c", "exit 7"]);
        let ContainedSpawnAttempt::Ready(child) = ContainedPipedChild::spawn(command) else {
            panic!("contained child did not launch");
        };
        let (mut process, stdin, stdout, stderr) = child.into_parts();
        drop((stdin, stdout, stderr));

        let first = process.wait_root_for_debugger().expect("first root reap");
        let second = process.wait_root_for_debugger().expect("cached root reap");

        assert_eq!(first.code(), Some(7));
        assert_eq!(second.code(), Some(7));
    }
}
