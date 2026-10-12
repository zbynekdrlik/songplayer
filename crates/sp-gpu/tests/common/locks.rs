//! #243: the named-mutex helpers of the Spout lock tests (`tests/spout.rs`,
//! `tests/spout_list.rs`): a mutex opened by name, one left abandoned, one
//! another thread keeps, what another thread meets when it waits, and
//! (#243 follow-up) one whose holder dies holding it.

use std::ffi::c_void;
use std::sync::mpsc;
use std::thread::JoinHandle;
use std::time::Duration;

use sp_gpu::{ListSentinel, SentinelEnd};
use windows::Win32::Foundation::{CloseHandle, FALSE, HANDLE, WAIT_ABANDONED, WAIT_OBJECT_0};
use windows::Win32::System::Threading::{CreateMutexW, ReleaseMutex, WaitForSingleObject};
use windows::core::HSTRING;

/// #243: the named mutex `name`, opened (or made, as Spout's `CreateMutexA`
/// does), closed when dropped.
pub struct TestMutex(pub HANDLE);

impl TestMutex {
    pub fn open(name: &str) -> Self {
        let mutex = unsafe { CreateMutexW(None, FALSE, &HSTRING::from(name)) }
            .unwrap_or_else(|e| panic!("the mutex {name}: {e}"));
        Self(mutex)
    }
}

impl Drop for TestMutex {
    fn drop(&mut self) {
        let _ = unsafe { CloseHandle(self.0) };
    }
}

/// Whether a wait's result means the waiter took the mutex.
pub fn took(wait: u32) -> bool {
    wait == WAIT_OBJECT_0.0 || wait == WAIT_ABANDONED.0
}

/// #243: leave the mutex `name` ABANDONED, as a program that died holding a
/// Spout lock leaves it: a helper thread takes it and ends without
/// releasing it. The returned handle keeps the mutex alive.
pub fn abandon(name: &str) -> TestMutex {
    let mutex = TestMutex::open(name);
    // A HANDLE is not Send: the thread gets the number.
    let raw = mutex.0.0 as isize;
    let wait = std::thread::spawn(move || {
        unsafe { WaitForSingleObject(HANDLE(raw as *mut c_void), 5_000) }.0
    })
    .join()
    .expect("the thread that dies holding the lock");
    assert!(took(wait), "it took the lock: {wait:#x}");
    mutex
}

/// Releases its mutex as often as THIS thread holds it, when dropped: a
/// failing test never leaves its thread holding a Spout lock for the next.
pub struct GiveBack(pub TestMutex);

impl Drop for GiveBack {
    fn drop(&mut self) {
        for _ in 0..64 {
            if unsafe { ReleaseMutex((self.0).0) }.is_err() {
                break;
            }
        }
    }
}

/// What another thread meets when it waits up to 300 ms for the mutex
/// `name`: `WAIT_OBJECT_0` = free (given back at once), `WAIT_TIMEOUT` = a
/// thread keeps it, `WAIT_ABANDONED` = left abandoned (nothing took it).
pub fn wait_elsewhere(name: &str) -> u32 {
    let name = name.to_owned();
    std::thread::spawn(move || {
        let mutex = TestMutex::open(&name);
        let wait = unsafe { WaitForSingleObject(mutex.0, 300) }.0;
        if took(wait) {
            unsafe { ReleaseMutex(mutex.0) }.expect("give it back");
        }
        wait
    })
    .join()
    .expect("the waiting thread")
}

/// #243: another thread of this process holds the mutex `name` until this
/// value is dropped, as a program that keeps a Spout lock does.
pub struct Hold {
    release: Option<mpsc::Sender<()>>,
    thread: Option<JoinHandle<()>>,
}

impl Hold {
    pub fn take(name: &str) -> Self {
        let name = name.to_owned();
        let (taken, taken_rx) = mpsc::channel();
        let (release, release_rx) = mpsc::channel::<()>();
        let thread = std::thread::spawn(move || {
            let mutex = TestMutex::open(&name);
            let wait = unsafe { WaitForSingleObject(mutex.0, 5_000) }.0;
            taken.send(wait).expect("the test waits for the take");
            let _ = release_rx.recv();
            if took(wait) {
                unsafe { ReleaseMutex(mutex.0) }.expect("release the held lock");
            }
        });
        let wait = taken_rx
            .recv_timeout(Duration::from_secs(10))
            .expect("the holding thread took its lock");
        assert!(took(wait), "the lock was taken: {wait:#x}");
        Self {
            release: Some(release),
            thread: Some(thread),
        }
    }
}

impl Drop for Hold {
    fn drop(&mut self) {
        drop(self.release.take());
        if let Some(thread) = self.thread.take()
            && let Err(panic) = thread.join()
            && !std::thread::panicking()
        {
            // The holding thread failed (its release): the test fails too.
            std::panic::resume_unwind(panic);
        }
    }
}

/// How a refusal names this test process as a lock's holder, lowercased.
pub fn this_process_holder() -> String {
    let exe = std::env::current_exe().expect("the test's exe");
    let file = exe.file_name().expect("its file name").to_string_lossy();
    format!("{file} (pid {}, this process)", std::process::id()).to_ascii_lowercase()
}

/// #243 follow-up: a thread of this process that holds the mutex `name`
/// until [`Doomed::die`], then ENDS without releasing it, as a program a
/// deploy's `taskkill /F` kills mid-lock does.
pub struct Doomed {
    die: Option<mpsc::Sender<()>>,
    thread: Option<JoinHandle<()>>,
}

impl Doomed {
    pub fn take(name: &str) -> Self {
        let name = name.to_owned();
        let (taken, taken_rx) = mpsc::channel();
        let (die, die_rx) = mpsc::channel::<()>();
        let thread = std::thread::spawn(move || {
            let mutex = TestMutex::open(&name);
            let wait = unsafe { WaitForSingleObject(mutex.0, 5_000) }.0;
            taken.send(wait).expect("the test waits for the take");
            let _ = die_rx.recv();
            // It ends holding the lock: Windows marks it abandoned.
        });
        let wait = taken_rx
            .recv_timeout(Duration::from_secs(10))
            .expect("the doomed thread took its lock");
        assert!(took(wait), "the lock was taken: {wait:#x}");
        Self {
            die: Some(die),
            thread: Some(thread),
        }
    }

    /// The thread ends, still holding the lock.
    pub fn die(mut self) {
        drop(self.die.take());
        if let Some(thread) = self.thread.take() {
            thread.join().expect("the doomed thread");
        }
    }
}

impl Drop for Doomed {
    fn drop(&mut self) {
        drop(self.die.take());
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// The sentinel's end, waited for up to 10 s (`None`: it did not end).
pub fn end_of(sentinel: &ListSentinel) -> Option<SentinelEnd> {
    for _ in 0..1_000 {
        if let Some(end) = sentinel.ended() {
            return Some(end);
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    None
}
