//! #210 part 2: the `vban-output` thread as an MMCSS "Pro Audio" thread.
//! Design record: #210 comment 5916097259 (Approach 1, item 1).
//!
//! On the box the VBAN thread was held off for 11–20 ms on a fixed 10 s grid
//! (finding 5915907311). It ran at `THREAD_PRIORITY_TIME_CRITICAL`: 15 in a
//! NORMAL_PRIORITY_CLASS process, the top of the normal band, and the same
//! level the NDI runtime's own threads can take (runtime 6.3.2 imports
//! `SetThreadPriority` and never touches MMCSS, lane check 5916282660). The
//! Multimedia Class Scheduler (MMCSS) is how Windows schedules real-time
//! audio: a thread in the task "Pro Audio" (a subkey of
//! `HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Multimedia\SystemProfile\Tasks`)
//! runs in the real-time band — 26 on the box, 27 at `AVRT_PRIORITY_HIGH` —
//! above every normal-band thread. OBS registers its audio threads the same
//! way (task "Audio", `libobs/media-io/audio-io.c`).
//!
//! [`MmcssTask::join`] joins the calling thread (`AvSetMmThreadCharacteristicsW`,
//! then `AvSetMmThreadPriority(AVRT_PRIORITY_HIGH)`) and its drop leaves the
//! task again (`AvRevertMmThreadCharacteristics`). What the calls came to is
//! the pure [`MmcssOutcome`]; [`MmcssOutcome::needs_fallback`] decides when the
//! thread keeps `TIME_CRITICAL` instead. `join_pro_audio` (Windows) is the one
//! call a thread makes: join, log the outcome, fall back when refused. Off
//! Windows `join` is a stub that is always refused, so this module and its
//! tests build on Linux. The avrt calls live in windows-sys's
//! `Win32::System::Threading` (feature `Win32_System_Threading`).

/// The MMCSS task the VBAN sender joins.
pub const MMCSS_PRO_AUDIO: &str = "Pro Audio";

/// What joining an MMCSS task came to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MmcssOutcome {
    /// In the task at `AVRT_PRIORITY_HIGH`.
    High { task_index: u32 },
    /// In the task, but `AvSetMmThreadPriority` failed with `error`: the
    /// thread runs at the task's own priority, still MMCSS's real-time band.
    TaskOnly { task_index: u32, error: u32 },
    /// Not in the task: `AvSetMmThreadCharacteristicsW` failed with `error`
    /// (0 off Windows).
    Refused { error: u32 },
}

impl MmcssOutcome {
    /// The outcome of the two calls: `joined` = the task call returned a
    /// handle (with `task_index`), `priority_set` = the priority call
    /// returned TRUE (it runs only after a join), `error` = `GetLastError`
    /// after the call that failed, 0 when none did.
    pub fn of(joined: bool, priority_set: bool, task_index: u32, error: u32) -> Self {
        match (joined, priority_set) {
            (false, _) => Self::Refused { error },
            (true, true) => Self::High { task_index },
            (true, false) => Self::TaskOnly { task_index, error },
        }
    }

    /// The thread is not in the task, so it keeps the normal band's
    /// `THREAD_PRIORITY_TIME_CRITICAL` (the scheduling before part 2).
    pub fn needs_fallback(self) -> bool {
        matches!(self, Self::Refused { .. })
    }
}

/// The error codes Microsoft documents for `AvSetMmThreadCharacteristicsW`,
/// by name, for the fallback WARN; any other code is `"other"` (the WARN
/// carries the number too).
pub fn avrt_error_name(error: u32) -> &'static str {
    match error {
        1550 => "ERROR_INVALID_TASK_NAME",
        1551 => "ERROR_INVALID_TASK_INDEX",
        1314 => "ERROR_PRIVILEGE_NOT_HELD",
        _ => "other",
    }
}

/// `name` as the NUL-terminated UTF-16 string `AvSetMmThreadCharacteristicsW`
/// takes.
pub fn wide_nul(name: &str) -> Vec<u16> {
    name.encode_utf16().chain(std::iter::once(0)).collect()
}

/// The calling thread's MMCSS task membership: taken by
/// [`join`](Self::join), left again when dropped. Build it ON the thread it
/// registers and keep it there for the thread's life (on Windows it holds a
/// raw handle, so it cannot leave the thread).
pub struct MmcssTask {
    /// The task handle; null when the join was refused.
    #[cfg(windows)]
    handle: windows_sys::Win32::Foundation::HANDLE,
    outcome: MmcssOutcome,
}

impl MmcssTask {
    /// What the join came to.
    pub fn outcome(&self) -> MmcssOutcome {
        self.outcome
    }
}

#[cfg(windows)]
impl MmcssTask {
    /// Join the calling thread to the MMCSS task `task` at
    /// `AVRT_PRIORITY_HIGH`.
    ///
    /// mutants::skip — the avrt FFI; its decisions are the tested
    /// [`MmcssOutcome::of`] and [`wide_nul`], and the Windows test job runs
    /// the refused path for real.
    #[cfg_attr(test, mutants::skip)]
    pub fn join(task: &str) -> Self {
        use windows_sys::Win32::Foundation::GetLastError;
        use windows_sys::Win32::System::Threading::{
            AVRT_PRIORITY_HIGH, AvSetMmThreadCharacteristicsW, AvSetMmThreadPriority,
        };
        let name = wide_nul(task);
        let mut task_index: u32 = 0; // 0 on a thread's first call (documented)
        // SAFETY: `name` is a NUL-terminated UTF-16 string that outlives the
        // call; `task_index` is a live u32 the call reads and writes.
        let handle = unsafe { AvSetMmThreadCharacteristicsW(name.as_ptr(), &mut task_index) };
        if handle.is_null() {
            // SAFETY: a leaf call reading this thread's last-error value.
            let error = unsafe { GetLastError() };
            return Self {
                handle,
                outcome: MmcssOutcome::of(false, false, task_index, error),
            };
        }
        // SAFETY: `handle` is the task handle just returned for this thread.
        let priority_set = unsafe { AvSetMmThreadPriority(handle, AVRT_PRIORITY_HIGH) } != 0;
        let error = if priority_set {
            0
        } else {
            // SAFETY: as above, right after the call that failed.
            unsafe { GetLastError() }
        };
        Self {
            handle,
            outcome: MmcssOutcome::of(true, priority_set, task_index, error),
        }
    }
}

#[cfg(not(windows))]
impl MmcssTask {
    /// Off Windows there is no MMCSS: always [`MmcssOutcome::Refused`] with
    /// error 0 (the Linux stub, so the module and its tests build here).
    ///
    /// mutants::skip — a constant.
    #[cfg_attr(test, mutants::skip)]
    pub fn join(_task: &str) -> Self {
        Self {
            outcome: MmcssOutcome::Refused { error: 0 },
        }
    }
}

#[cfg(windows)]
impl Drop for MmcssTask {
    /// Leave the task (a refused join has nothing to leave).
    ///
    /// mutants::skip — the avrt FFI.
    #[cfg_attr(test, mutants::skip)]
    fn drop(&mut self) {
        use windows_sys::Win32::System::Threading::AvRevertMmThreadCharacteristics;
        if self.handle.is_null() {
            return;
        }
        // SAFETY: `handle` is this thread's live task handle from `join`,
        // reverted exactly once (the guard is neither Clone nor Send).
        if unsafe { AvRevertMmThreadCharacteristics(self.handle) } == 0 {
            tracing::warn!("mmcss: AvRevertMmThreadCharacteristics failed");
        }
    }
}

/// Give the calling thread real-time audio scheduling: the MMCSS task
/// [`MMCSS_PRO_AUDIO`] at `AVRT_PRIORITY_HIGH`; when MMCSS refuses it, the
/// normal band's `THREAD_PRIORITY_TIME_CRITICAL`, with a WARN naming the
/// error. Keep the returned guard alive for the thread's life. Every
/// real-time sender can take it (design record: shared benefit).
///
/// mutants::skip — the Windows thread setup; the fallback decision is the
/// tested [`MmcssOutcome::needs_fallback`].
#[cfg(windows)]
#[cfg_attr(test, mutants::skip)]
pub fn join_pro_audio(thread: &str) -> MmcssTask {
    let task = MmcssTask::join(MMCSS_PRO_AUDIO);
    log_outcome(thread, task.outcome());
    if task.outcome().needs_fallback() {
        raise_thread_priority(thread);
    }
    task
}

/// Raise the calling thread to `THREAD_PRIORITY_TIME_CRITICAL` so a heavy
/// child's CPU/memory burst cannot delay a grid slot: the NDI input's grid
/// thread (`ndi-input`), and the #210 VBAN sender when MMCSS refuses it
/// ([`join_pro_audio`]'s fallback). `thread` labels the log line. (#221
/// lane 3 moved it here from `pipeline_audio.rs`, deleted with the
/// per-playlist NDI senders' wall-clock audio emitter.)
///
/// mutants::skip — a Windows thread call; logging only besides it.
#[cfg(windows)]
#[cfg_attr(test, mutants::skip)]
pub(crate) fn raise_thread_priority(thread: &str) {
    use windows_sys::Win32::System::Threading::{
        GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_TIME_CRITICAL,
    };
    // SAFETY: GetCurrentThread returns a pseudo-handle valid for the calling
    // thread; SetThreadPriority is a leaf call with primitive args.
    let ok = unsafe { SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_TIME_CRITICAL) };
    if ok == 0 {
        tracing::warn!(thread, "SetThreadPriority(TIME_CRITICAL) failed");
    } else {
        tracing::info!(thread, "thread priority = TIME_CRITICAL");
    }
}

/// The one log line of a join. Logging only.
#[cfg(windows)]
#[cfg_attr(test, mutants::skip)]
fn log_outcome(thread: &str, outcome: MmcssOutcome) {
    match outcome {
        MmcssOutcome::High { task_index } => tracing::info!(
            thread,
            task_index,
            "mmcss: the thread runs as an MMCSS \"Pro Audio\" thread at AVRT_PRIORITY_HIGH"
        ),
        MmcssOutcome::TaskOnly { task_index, error } => tracing::warn!(
            thread,
            task_index,
            error,
            error_name = avrt_error_name(error),
            "mmcss: the thread joined MMCSS \"Pro Audio\", but AvSetMmThreadPriority(HIGH) failed — it runs at the task's own priority"
        ),
        MmcssOutcome::Refused { error } => tracing::warn!(
            thread,
            error,
            error_name = avrt_error_name(error),
            "mmcss: MMCSS \"Pro Audio\" refused the thread — it falls back to THREAD_PRIORITY_TIME_CRITICAL"
        ),
    }
}

#[cfg(test)]
#[path = "mmcss_tests.rs"]
mod tests;
