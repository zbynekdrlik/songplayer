//! #255 item C, the live part: the process-wide guard (the refused set +
//! one refusal counter per listener), the axum 0.8 `Listener` wrapper that
//! drops a refused connection before a byte is read, and the task that
//! keeps the set current. The facade's own accept loop asks the same guard
//! (`remote::serve_guarded`).

use std::net::{Ipv4Addr, SocketAddr};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, RwLock};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::broadcast;
use tracing::{info, warn};

use super::{Cidr, ENV_DANTE_SUBNET, dante_ips, env_subnets, refused_set, refuses, warn_due};

/// How often the refresh task re-reads the Dante outputs and the
/// interfaces.
const REFRESH_EVERY: Duration = Duration::from_secs(10);

/// Which listener a connection arrived on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Which {
    /// HTTP `:8920` (the dashboard + API).
    Http,
    /// The remote obs-websocket facade (`:4456`).
    Remote,
}

impl Which {
    fn label(self) -> &'static str {
        match self {
            Which::Http => "http",
            Which::Remote => "remote",
        }
    }
}

/// `dante_refusals` on `GET /api/v1/status`: connections refused since the
/// start, per listener.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct DanteRefusals {
    pub http: u64,
    pub remote: u64,
}

#[derive(Default)]
struct Refusals {
    count: AtomicU64,
    last_warn: Mutex<Option<Instant>>,
}

/// The refused set + the counters.
#[derive(Default)]
pub struct DanteGuard {
    set: RwLock<Vec<Cidr>>,
    http: Refusals,
    remote: Refusals,
}

impl DanteGuard {
    pub fn new(set: Vec<Cidr>) -> Self {
        Self {
            set: RwLock::new(set),
            ..Self::default()
        }
    }

    /// Replace the set; `true` when it changed.
    pub fn set_subnets(&self, set: Vec<Cidr>) -> bool {
        let mut now = self.set.write().unwrap_or_else(|p| p.into_inner());
        let changed = *now != set;
        *now = set;
        changed
    }

    pub fn subnets(&self) -> Vec<Cidr> {
        self.set.read().unwrap_or_else(|p| p.into_inner()).clone()
    }

    fn side(&self, which: Which) -> &Refusals {
        match which {
            Which::Http => &self.http,
            Which::Remote => &self.remote,
        }
    }

    /// Whether to refuse a connection accepted on `which` (its own local
    /// address, its peer): counted, and logged at most once per minute per
    /// listener. An unreadable local address is served.
    pub fn refuse(
        &self,
        which: Which,
        local: std::io::Result<SocketAddr>,
        peer: SocketAddr,
    ) -> bool {
        let Ok(local) = local else {
            return false;
        };
        if !refuses(&self.subnets(), local.ip()) {
            return false;
        }
        let side = self.side(which);
        side.count.fetch_add(1, Ordering::Relaxed);
        let now = Instant::now();
        let mut last = side.last_warn.lock().unwrap_or_else(|p| p.into_inner());
        if warn_due(*last, now) {
            *last = Some(now);
            warn!(
                listener = which.label(),
                %peer,
                %local,
                refused = side.count.load(Ordering::Relaxed),
                "dante guard: refused a connection that arrived on the Dante network"
            );
        }
        true
    }

    pub fn refusals(&self) -> DanteRefusals {
        DanteRefusals {
            http: self.http.count.load(Ordering::Relaxed),
            remote: self.remote.count.load(Ordering::Relaxed),
        }
    }
}

/// The process-wide guard both listeners ask (empty until the refresh
/// task's first pass).
pub fn global() -> &'static Arc<DanteGuard> {
    static GUARD: OnceLock<Arc<DanteGuard>> = OnceLock::new();
    GUARD.get_or_init(|| Arc::new(DanteGuard::default()))
}

/// A `TcpListener` whose `accept` drops (unread, unanswered) every
/// connection the guard refuses and returns the next one it serves.
pub struct GuardedListener {
    inner: TcpListener,
    guard: Arc<DanteGuard>,
    which: Which,
}

impl GuardedListener {
    pub fn new(inner: TcpListener, guard: Arc<DanteGuard>, which: Which) -> Self {
        Self {
            inner,
            guard,
            which,
        }
    }
}

impl axum::serve::Listener for GuardedListener {
    type Io = TcpStream;
    type Addr = SocketAddr;

    async fn accept(&mut self) -> (TcpStream, SocketAddr) {
        loop {
            let (stream, peer) = axum::serve::Listener::accept(&mut self.inner).await;
            if !self.guard.refuse(self.which, stream.local_addr(), peer) {
                return (stream, peer);
            }
        }
    }

    fn local_addr(&self) -> std::io::Result<SocketAddr> {
        self.inner.local_addr()
    }
}

/// Start the refresh task (`DANTE_SUBNET` read once, here).
#[cfg_attr(test, mutants::skip)] // glue: env + DB + interfaces; the set is `refused_set`
pub fn start(pool: SqlitePool, shutdown: broadcast::Receiver<()>) {
    let env = match env_subnets(std::env::var(ENV_DANTE_SUBNET).ok().as_deref()) {
        Ok(env) => env,
        Err(e) => {
            warn!(error = %e, "dante guard: {ENV_DANTE_SUBNET} ignored");
            Vec::new()
        }
    };
    tokio::spawn(run(pool, env, shutdown));
}

#[cfg_attr(test, mutants::skip)] // glue around `refresh`
async fn run(pool: SqlitePool, env: Vec<Cidr>, mut shutdown: broadcast::Receiver<()>) {
    loop {
        refresh(&pool, &env).await;
        tokio::select! {
            _ = shutdown.recv() => break,
            _ = tokio::time::sleep(REFRESH_EVERY) => {}
        }
    }
}

#[cfg_attr(test, mutants::skip)] // glue: reads, then the tested `refused_set`
async fn refresh(pool: &SqlitePool, env: &[Cidr]) {
    let entries = match crate::playback::audio_out_config::load(pool).await {
        Ok(settings) => settings.entries,
        Err(e) => {
            warn!(%e, "dante guard: reading the audio outputs failed; the set is kept");
            return;
        }
    };
    let ifaces: Vec<(Ipv4Addr, u8)> = match if_addrs::get_if_addrs() {
        Ok(list) => list
            .into_iter()
            .filter_map(|i| match i.addr {
                if_addrs::IfAddr::V4(v4) => Some((v4.ip, v4.prefixlen)),
                if_addrs::IfAddr::V6(_) => None,
            })
            .collect(),
        Err(e) => {
            warn!(%e, "dante guard: listing the interfaces failed; the set is kept");
            return;
        }
    };
    let set = refused_set(env, &dante_ips(&entries), &ifaces);
    if global().set_subnets(set.clone()) {
        let shown: Vec<String> = set.iter().map(Cidr::to_string).collect();
        info!(subnets = ?shown, "dante guard: refusing connections that arrive on these subnets");
    }
}

#[cfg(test)]
#[path = "dante_guard_live_tests.rs"]
mod tests;
