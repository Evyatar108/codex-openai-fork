// Windows Job Object helpers for confining tool-exec child process trees.
//
// Rationale: on Windows, pipe handles for a child's stdout/stderr are
// inheritable. When the child is a shell (bash, cmd.exe) that spawns long-
// running grandchildren (cargo, rustc, link.exe, ...), those grandchildren
// inherit the pipe write ends. When the immediate child exits but
// grandchildren are still running, the pipe write ends stay open, and the
// tokio `spawn_blocking` tasks reading the pipe (`tokio::io::blocking` ->
// `ReadFile`) never see EOF. On runtime shutdown, `BlockingPool::Drop`
// waits on those tasks forever and `codex-core.exe` hangs indefinitely.
//
// Fix: create a Job Object with `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` for
// each tool-exec child. Spawn the child CREATE_SUSPENDED, assign it to the
// job before any instruction runs, then resume it. A background
// `spawn_blocking` watcher holds its own duplicated handle to the child
// and waits for exit; once the child exits, the watcher drops the job
// handle. Closing the job handle causes the OS to `TerminateProcess`
// every descendant still in the job, which closes their pipe handles,
// lets the `ReadFile` blocking tasks EOF, and lets runtime shutdown
// complete.
//
// This mechanism complements (not duplicates) the Unix
// `prctl(PR_SET_PDEATHSIG)` path in `spawn.rs`: prctl kills the immediate
// child when the codex-core process dies; JobObject kills grandchildren
// when the immediate child dies.
#![cfg(windows)]

use std::io;
use std::mem;
use std::ptr;

use windows_sys::Win32::Foundation::CloseHandle;
use windows_sys::Win32::Foundation::DuplicateHandle;
use windows_sys::Win32::Foundation::DUPLICATE_SAME_ACCESS;
use windows_sys::Win32::Foundation::FALSE;
use windows_sys::Win32::Foundation::HANDLE;
use windows_sys::Win32::System::JobObjects::AssignProcessToJobObject;
use windows_sys::Win32::System::JobObjects::CreateJobObjectW;
use windows_sys::Win32::System::JobObjects::JobObjectExtendedLimitInformation;
use windows_sys::Win32::System::JobObjects::SetInformationJobObject;
use windows_sys::Win32::System::JobObjects::JOBOBJECT_EXTENDED_LIMIT_INFORMATION;
use windows_sys::Win32::System::JobObjects::JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
use windows_sys::Win32::System::Threading::GetCurrentProcess;
use windows_sys::Win32::System::Threading::WaitForSingleObject;
use windows_sys::Win32::System::Threading::INFINITE;
use windows_sys::Win32::System::Threading::PROCESS_SYNCHRONIZE;

// `NtResumeProcess` resumes every thread previously suspended via
// `CREATE_SUSPENDED`. It is exported by `ntdll.dll` and has been stable for
// decades even though it is not in the public Win32 SDK headers. Using it
// avoids the need to enumerate the child's threads to find the primary one.
#[link(name = "ntdll")]
unsafe extern "system" {
    fn NtResumeProcess(ProcessHandle: HANDLE) -> i32;
}

/// Owned Job Object handle. Dropping this handle closes the job, which for
/// a job created via [`create_kill_on_close_job`] terminates every process
/// still in the job.
pub struct JobHandle(HANDLE);

// SAFETY: a Windows `HANDLE` is a process-wide identifier that is safe to
// transfer (move) between threads. We do not share the handle across
// threads by reference — only `Send` is needed to move ownership of the
// handle from the spawn thread into the watcher task's closure. `Sync`
// would permit shared `&JobHandle` across threads, which this type does
// not need and deliberately does not support.
unsafe impl Send for JobHandle {}

impl Drop for JobHandle {
    fn drop(&mut self) {
        if self.0 != 0 {
            // SAFETY: we own the handle and are closing it exactly once.
            let _ = unsafe { CloseHandle(self.0) };
        }
    }
}

/// Owned Windows process handle opened with `PROCESS_SYNCHRONIZE`.
/// Used by the background watcher to wait on child exit without racing
/// against PID recycling.
struct ProcessHandle(HANDLE);

// SAFETY: same rationale as `JobHandle` — move-only across threads.
unsafe impl Send for ProcessHandle {}

impl Drop for ProcessHandle {
    fn drop(&mut self) {
        if self.0 != 0 {
            // SAFETY: we own the handle and are closing it exactly once.
            let _ = unsafe { CloseHandle(self.0) };
        }
    }
}

/// Creates a Job Object configured with `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`.
///
/// When the last handle to the returned job is closed (including by
/// dropping the returned [`JobHandle`]), every process still in the job is
/// terminated by the OS.
pub fn create_kill_on_close_job() -> io::Result<JobHandle> {
    // SAFETY: `CreateJobObjectW` is a well-documented Win32 API. Passing
    // null for both parameters creates an unnamed, default-security job.
    let job = unsafe { CreateJobObjectW(ptr::null(), ptr::null()) };
    if job == 0 {
        return Err(io::Error::last_os_error());
    }
    let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { mem::zeroed() };
    info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
    // SAFETY: `info` is a fully-initialized, stack-owned value of the
    // correct type; we pass its size in bytes as required by
    // `SetInformationJobObject`.
    let ok = unsafe {
        SetInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            &info as *const _ as *const _,
            mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        )
    };
    if ok == 0 {
        let err = io::Error::last_os_error();
        // SAFETY: we just created this handle and have not transferred
        // ownership; closing it here is the correct cleanup path.
        let _ = unsafe { CloseHandle(job) };
        return Err(err);
    }
    Ok(JobHandle(job))
}

/// Assigns `process_handle` to `job`. On Windows 8+ nested jobs are
/// supported, so this generally succeeds even when `codex-core` itself is
/// already inside an outer job (WSL, Docker, some CI runners). On older
/// systems or jobs with incompatible limits this can fail with
/// `ERROR_ACCESS_DENIED`; callers should treat failure as non-fatal and
/// fall back to legacy spawn behavior (see `spawn.rs`).
pub fn assign_process_to_job(job: &JobHandle, process_handle: HANDLE) -> io::Result<()> {
    // SAFETY: both handles are valid and owned by the caller for the
    // duration of this call.
    let ok = unsafe { AssignProcessToJobObject(job.0, process_handle) };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Resumes every thread of the process referenced by `process_handle`.
///
/// Used to start a process spawned with `CREATE_SUSPENDED` after it has
/// been added to a Job Object, so grandchildren cannot race ahead of the
/// job assignment. The input process must currently be suspended via
/// `CREATE_SUSPENDED`; if individual threads were suspended separately,
/// `NtResumeProcess` decrements each suspend count by one and may not
/// actually resume them.
pub fn resume_process(process_handle: HANDLE) -> io::Result<()> {
    // SAFETY: `NtResumeProcess` takes a process handle and returns an
    // `NTSTATUS`. Negative values are failures; the bit pattern is not
    // a Win32 error code, so we surface the raw status rather than
    // pretending it is.
    let status = unsafe { NtResumeProcess(process_handle) };
    if status < 0 {
        return Err(io::Error::other(format!(
            "NtResumeProcess failed: NTSTATUS {status:#010x}"
        )));
    }
    Ok(())
}

/// Duplicates `source_handle` (which must be a handle in the current
/// process) into a new handle owned by the returned `ProcessHandle`, with
/// `PROCESS_SYNCHRONIZE` access. Used so the watcher task can reference
/// the child process independently of `tokio::process::Child`'s own
/// handle, avoiding PID-recycling races when `kill_on_drop` closes
/// Child's handle before the watcher runs.
fn duplicate_for_wait(source_handle: HANDLE) -> io::Result<ProcessHandle> {
    let current = unsafe { GetCurrentProcess() };
    let mut dup: HANDLE = 0;
    // SAFETY: `GetCurrentProcess` returns a pseudo-handle valid for both
    // source and target; `source_handle` is owned by the caller; `dup` is
    // a stack-allocated out-parameter.
    let ok = unsafe {
        DuplicateHandle(
            current,
            source_handle,
            current,
            &mut dup,
            PROCESS_SYNCHRONIZE,
            FALSE,
            0,
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(ProcessHandle(dup))
}

/// Spawns a background blocking task that waits for the child referenced
/// by `source_handle` to exit and then drops `job`. Closing the job (via
/// drop) terminates every descendant still in the job, which closes their
/// stdio pipe handles and unblocks any `tokio::io::blocking` readers.
///
/// `source_handle` is duplicated on the calling thread before being moved
/// into the watcher, so the watcher's wait is unaffected by
/// `tokio::process::Child` closing its own handle (e.g. via `kill_on_drop`).
pub fn close_job_on_child_exit(source_handle: HANDLE, job: JobHandle) -> io::Result<()> {
    let owned = duplicate_for_wait(source_handle)?;
    tokio::task::spawn_blocking(move || {
        // SAFETY: `owned.0` is a valid process handle with
        // `PROCESS_SYNCHRONIZE`; `INFINITE` is a documented wait timeout.
        unsafe { WaitForSingleObject(owned.0, INFINITE) };
        drop(owned);
        drop(job);
    });
    Ok(())
}
