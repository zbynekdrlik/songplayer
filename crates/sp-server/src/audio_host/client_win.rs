//! #233: SongPlayer's Windows glue of the ASIO host (out of the mutation
//! gate, `.cargo/mutants.toml`: process, job and pipe calls no Linux runner
//! can make; every decision is in the Linux-tested `client.rs`,
//! `launch.rs`, `restart.rs`).
//!
//! - [`start_remote`] is the outputs task's ASIO starter: it registers the
//!   output with the process-wide [`HostClient`], starts the supervisor once
//!   (on the runtime the outputs task runs on) and the output's forwarder
//!   thread (`asio-fwd-<id>`).
//! - [`launch`] starts one host: `sp-audio-host.exe` next to SongPlayer's
//!   exe (missing = an `Err` that names it, the outputs read waiting), a new
//!   pipe (the only instance, local clients only), the process with no
//!   window and no inherited stdio, put at once into a Job object with
//!   KILL_ON_JOB_CLOSE (it dies with SongPlayer, even on a `taskkill /F`),
//!   then the host's connect, bounded at 10 s. Its guard ([`HostProcess`])
//!   ends the process when the session is over.

use std::os::windows::io::AsRawHandle;
use std::os::windows::process::CommandExt;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use tokio::io::{ReadHalf, WriteHalf};
use tokio::net::windows::named_pipe::{NamedPipeServer, ServerOptions};
use tracing::{error, info};

use crate::audio_host::client::{HostClient, run_forwarder, supervise};
use crate::audio_host::launch::{host_exe_beside, missing_exe, pipe_name};
use crate::playback::asio_out::AsioOut;

/// `CREATE_NO_WINDOW`: the host shows no console.
const CREATE_NO_WINDOW: u32 = 0x0800_0000;
/// How long the host has to open its pipe.
const CONNECT_WAIT: Duration = Duration::from_secs(10);

/// The process-wide client (one host per SongPlayer).
pub fn client() -> &'static Arc<HostClient> {
    static CLIENT: OnceLock<Arc<HostClient>> = OnceLock::new();
    CLIENT.get_or_init(|| {
        Arc::new(HostClient::new(|| {
            crate::playback::fleet_shift::global().slots()
        }))
    })
}

/// The outputs task's ASIO starter (module doc).
pub fn start_remote(out: Arc<AsioOut>, id: String) {
    let client = client().clone();
    start_supervisor(&client);
    let handle = client.attach(out.clone());
    let forwarded = out.clone();
    let spawned = std::thread::Builder::new()
        .name(format!("asio-fwd-{id}"))
        .spawn(move || run_forwarder(&client, handle, &forwarded));
    if let Err(e) = spawned {
        error!(%e, id = %id, "asio output: spawning the forwarder thread failed");
        self::client().detach(handle);
        out.set_start_error(format!(
            "the ASIO forwarder did not start: spawning it failed: {e}"
        ));
    }
}

/// The supervisor, started once, on the current runtime.
fn start_supervisor(client: &Arc<HostClient>) {
    static STARTED: OnceLock<()> = OnceLock::new();
    STARTED.get_or_init(|| match tokio::runtime::Handle::try_current() {
        Ok(rt) => {
            info!("asio host: supervisor started - ASIO outputs run in sp-audio-host.exe");
            rt.spawn(supervise(client.clone(), launch));
        }
        Err(e) => error!(%e, "asio host: no runtime to supervise the ASIO host on"),
    });
}

/// A started host: its process and its kill-on-close job. Dropping it ends
/// the process.
pub struct HostProcess {
    child: Child,
    job: isize,
}

impl Drop for HostProcess {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        // SAFETY: `job` is a Job object handle this guard alone owns.
        unsafe {
            windows_sys::Win32::Foundation::CloseHandle(
                self.job as windows_sys::Win32::Foundation::HANDLE,
            );
        }
    }
}

/// Start one host (module doc).
pub async fn launch() -> Result<
    (
        ReadHalf<NamedPipeServer>,
        WriteHalf<NamedPipeServer>,
        HostProcess,
    ),
    String,
> {
    static STARTS: AtomicU64 = AtomicU64::new(0);
    let exe = std::env::current_exe().map_err(|e| format!("SongPlayer's own exe path: {e}"))?;
    let host = host_exe_beside(&exe);
    if !host.is_file() {
        return Err(missing_exe(&host));
    }
    let name = pipe_name(
        std::process::id(),
        STARTS.fetch_add(1, Ordering::Relaxed) + 1,
    );
    let server = ServerOptions::new()
        .first_pipe_instance(true)
        .reject_remote_clients(true)
        .max_instances(1)
        .create(&name)
        .map_err(|e| format!("creating the pipe {name} failed: {e}"))?;
    let child = Command::new(&host)
        .arg("--pipe")
        .arg(&name)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()
        .map_err(|e| format!("starting {} failed: {e}", host.display()))?;
    let pid = child.id();
    let process = match kill_with_songplayer(&child) {
        Ok(job) => HostProcess { child, job },
        Err(why) => {
            let mut child = child;
            let _ = child.kill();
            let _ = child.wait();
            return Err(why);
        }
    };
    info!(pid, pipe = %name, "asio host: started sp-audio-host.exe");
    match tokio::time::timeout(CONNECT_WAIT, server.connect()).await {
        Ok(Ok(())) => {}
        Ok(Err(e)) => return Err(format!("the host's pipe failed: {e}")),
        Err(_) => return Err("the host did not open its pipe within 10 s".into()),
    }
    let (reader, writer) = tokio::io::split(server);
    Ok((reader, writer, process))
}

/// Put `child` into a new Job object that kills it when its last handle
/// closes: when SongPlayer exits, however it exits.
fn kill_with_songplayer(child: &Child) -> Result<isize, String> {
    use core::ffi::c_void;
    use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, HANDLE};
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
        SetInformationJobObject,
    };
    // SAFETY: the job handle is null-checked and closed on every failure;
    // the limit struct is zeroed, then its one flag set, and passed with its
    // own size; the child's handle is valid while `child` lives.
    unsafe {
        let job: HANDLE = CreateJobObjectW(std::ptr::null(), std::ptr::null());
        if job.is_null() {
            return Err(format!(
                "creating the host's job failed ({})",
                GetLastError()
            ));
        }
        let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        if SetInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            &info as *const _ as *const c_void,
            std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        ) == 0
        {
            let err = GetLastError();
            CloseHandle(job);
            return Err(format!("setting the host's job failed ({err})"));
        }
        if AssignProcessToJobObject(job, child.as_raw_handle().cast()) == 0 {
            let err = GetLastError();
            CloseHandle(job);
            return Err(format!("putting the host into its job failed ({err})"));
        }
        Ok(job as isize)
    }
}
