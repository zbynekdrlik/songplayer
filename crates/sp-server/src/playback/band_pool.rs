//! Persistent row-band workers for the `SP-program` sender (#223 follow-up,
//! design record 5973498519).
//!
//! The fused kernel (`nv12_mix.rs`) paints a picture in K row bands. Until
//! this pool, every painted picture started K − 1 scoped threads
//! (`std::thread::scope`) and joined them: ~150 thread starts a second with a
//! 1440p song on program, and two rounds per fade boundary. On Windows every
//! thread start and exit runs each loaded DLL's thread attach / detach under
//! the loader lock (NDI, Media Foundation, WebView2).
//!
//! A [`BandPool`] starts its K − 1 workers ONCE (the `SP-program` sender
//! builds it with its `ProgramOutput`) and feeds each [`BandPool::run`] its
//! bands: band 0 on the calling thread, band `i` on worker `i`, a thread
//! named `<name>-<i>` (`program-mix-1` … in production). `run` returns only
//! once every band is painted, so a picture is complete when it returns, as
//! it was with the scoped threads. Dropping the pool closes every worker's
//! queue and joins the worker (RAII).
//!
//! A band's painter borrows the caller's buffers, which a persistent thread
//! cannot hold by type. So `run` hands each worker a pointer to the painter
//! and does not return, nor unwind, before every worker has dropped it (the
//! SAFETY notes on [`Job`] and [`Bands`]). This is the scoped-pool pattern,
//! in the few lines below.
//!
//! A band that panics on a worker is caught there, so the worker lives on
//! for the next picture, and its panic is resumed on the calling thread once
//! every band is done, as `std::thread::scope` did. (A release build aborts
//! on any panic: `panic = "abort"`.)

use std::any::Any;
use std::panic::{self, AssertUnwindSafe};
use std::sync::Arc;
#[cfg(test)]
use std::sync::Weak;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::{self, JoinHandle};

use tracing::warn;

/// A painter's band, as a worker gets it: a pointer to the caller's painter
/// with its lifetime erased, and the function that calls it.
///
/// SAFETY: a `Job` is made only in [`BandPool::run`], from a painter that
/// lives for the whole call. Every copy of it travels in a [`Task`], and a
/// worker drops the task (and with it the task's report sender) only after
/// its last use of the job. `run` neither returns nor unwinds before every
/// such sender is gone ([`Bands`]), so the painter outlives every call made
/// through the pointer. Several threads may call it at once: the painter is
/// `Sync` (`run`'s bound).
#[derive(Clone, Copy)]
struct Job {
    painter: *const (),
    call: unsafe fn(*const (), usize),
}

// SAFETY: the pointee is a `Sync` painter (`run`'s bound), so handing the
// pointer to another thread shares a `&F` across threads, which `Sync`
// allows; its lifetime is the `Job` SAFETY note's.
unsafe impl Send for Job {}

/// Call the `F` that `painter` points to for `band`.
///
/// SAFETY: `painter` must point to a live `F` (the `Job` SAFETY note).
unsafe fn call<F: Fn(usize) + Sync>(painter: *const (), band: usize) {
    // SAFETY: the caller's contract.
    let paint = unsafe { &*painter.cast::<F>() };
    paint(band);
}

impl Job {
    fn new<F: Fn(usize) + Sync>(paint: &F) -> Self {
        Self {
            painter: (paint as *const F).cast(),
            call: call::<F>,
        }
    }

    /// Paint `band`.
    ///
    /// SAFETY: only while the painter lives (the `Job` SAFETY note).
    unsafe fn paint(self, band: usize) {
        // SAFETY: the caller's contract.
        unsafe { (self.call)(self.painter, band) }
    }
}

/// A worker's report of one band: the panic it caught, if any.
type Report = Option<Box<dyn Any + Send>>;

/// One band handed to a worker, and where to report it.
struct Task {
    job: Job,
    band: usize,
    report: Sender<Report>,
}

/// The bands one `run` handed to the workers, and the wait for them: on the
/// normal path ([`Bands::wait`], which also collects the reports) and, when
/// the calling thread unwinds (its own band panicked), on drop.
///
/// SAFETY (of [`Job`]): either way the call does not leave before every
/// task's report sender is gone, that is before every worker has dropped its
/// copy of the job.
struct Bands {
    /// Cloned into every task; dropped by the wait, so the wait ends when the
    /// last task is gone.
    report: Option<Sender<Report>>,
    reports: Receiver<Report>,
}

impl Bands {
    fn new() -> Self {
        let (report, reports) = mpsc::channel();
        Self {
            report: Some(report),
            reports,
        }
    }

    /// A task for `band` of `job` (until the wait).
    fn task(&self, job: Job, band: usize) -> Option<Task> {
        let report = self.report.clone()?;
        Some(Task { job, band, report })
    }

    /// Wait until every task is gone (reported and dropped by its worker, or
    /// dropped unsent): how many bands the workers painted, and the first
    /// panic one of them caught.
    fn wait(&mut self) -> (usize, Report) {
        self.report = None;
        let mut painted = 0;
        let mut caught = None;
        for report in self.reports.iter() {
            painted += 1;
            caught = caught.or(report);
        }
        (painted, caught)
    }
}

impl Drop for Bands {
    /// The calling thread unwinds with tasks still out: wait for them first,
    /// or a worker would paint into buffers the unwinding frees.
    fn drop(&mut self) {
        self.wait();
    }
}

/// A worker: its queue, and its thread.
struct Worker {
    tasks: Sender<Task>,
    handle: JoinHandle<()>,
}

/// A worker's loop: paint every band it is handed and report it, until its
/// queue closes (the pool was dropped). `_alive` lives as long as the
/// thread's loop (the pool's token, read by the tests).
fn work(tasks: Receiver<Task>, _alive: Arc<()>) {
    for task in tasks {
        // SAFETY: `run` waits for this task's report sender, which this
        // iteration drops after the job's last use (`Job`).
        let caught = panic::catch_unwind(AssertUnwindSafe(|| unsafe { task.job.paint(task.band) }));
        // A send fails only once the run's receiver is gone, and a run keeps
        // it until every task is dropped: nothing to do about it here.
        let _ = task.report.send(caught.err());
    }
}

/// Start the worker of `band`, the thread `<name>-<band>`. One that cannot
/// start is WARNed: its band is painted on the calling thread instead.
fn spawn_worker(name: &str, band: usize, alive: &Arc<()>) -> Option<Worker> {
    let (tasks, queue) = mpsc::channel();
    let alive = Arc::clone(alive);
    let started = thread::Builder::new()
        .name(format!("{name}-{band}"))
        .spawn(move || work(queue, alive));
    match started {
        Ok(handle) => Some(Worker { tasks, handle }),
        Err(e) => {
            warn!(
                %e,
                pool = name,
                band,
                "band pool: a worker thread did not start — its band is painted on the calling thread"
            );
            None
        }
    }
}

/// K row bands, painted at once: band 0 on the calling thread, bands 1 .. K
/// on persistent workers (see the module doc).
pub struct BandPool {
    bands: usize,
    /// The worker of band `i` at `i − 1`; `None` = it did not start.
    workers: Vec<Option<Worker>>,
    /// The token every worker holds for its thread's life.
    #[cfg(test)]
    alive: Weak<()>,
}

impl BandPool {
    /// A pool of `bands` row bands (at least 1): it starts `bands − 1`
    /// workers, the threads `<name>-1` … `<name>-<bands − 1>`.
    pub fn new(name: &str, bands: usize) -> Self {
        let bands = bands.max(1);
        let alive = Arc::new(());
        let workers = (1..bands)
            .map(|band| spawn_worker(name, band, &alive))
            .collect();
        Self {
            bands,
            workers,
            #[cfg(test)]
            alive: Arc::downgrade(&alive),
        }
    }

    /// How many bands a picture is painted in.
    pub fn bands(&self) -> usize {
        self.bands
    }

    /// How many workers started (one per band past the first, unless a
    /// thread could not start).
    pub fn workers(&self) -> usize {
        self.workers.iter().flatten().count()
    }

    /// The worker of `band` (from 1), when it started.
    fn worker(&self, band: usize) -> Option<&Worker> {
        self.workers.get(band - 1)?.as_ref()
    }

    /// Paint bands `0 .. bands()` with `paint`: band 0 on the calling
    /// thread, band `i` on worker `i` (on the calling thread when it did not
    /// start), all at once. Returns once EVERY band is painted: how many
    /// threads painted. A band that panicked on a worker panics here, once
    /// every band is done.
    pub fn run<F: Fn(usize) + Sync>(&self, paint: &F) -> usize {
        let job = Job::new(paint);
        let mut bands = Bands::new();
        for band in 1..self.bands {
            let sent = bands
                .task(job, band)
                .zip(self.worker(band))
                .map(|(task, worker)| worker.tasks.send(task));
            match sent {
                Some(Ok(())) => {}
                _ => paint(band),
            }
        }
        paint(0);
        let (helpers, caught) = bands.wait();
        if let Some(payload) = caught {
            panic::resume_unwind(payload);
        }
        1 + helpers
    }

    /// How many workers are still running: they all hold the pool's token.
    #[cfg(test)]
    pub(crate) fn alive(&self) -> Weak<()> {
        self.alive.clone()
    }
}

impl Drop for BandPool {
    /// Close every worker's queue, then wait for each worker to end: no
    /// worker outlives its pool.
    fn drop(&mut self) {
        let handles: Vec<JoinHandle<()>> = self
            .workers
            .drain(..)
            .flatten()
            .map(|worker| worker.handle)
            .collect();
        for handle in handles {
            if handle.join().is_err() {
                warn!("band pool: a worker thread ended in a panic");
            }
        }
    }
}

#[cfg(test)]
#[path = "band_pool_tests.rs"]
mod tests;
