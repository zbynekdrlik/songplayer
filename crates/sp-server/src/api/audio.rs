//! #233: `GET /api/v1/audio/asio-drivers` — the ASIO drivers this box has
//! registered (HKLM\SOFTWARE\ASIO, read only; no driver is loaded), for the
//! dashboard's driver list of an ASIO output. Empty off Windows.

use axum::Json;
use serde::Serialize;

/// The answer (no `Default`: an empty list is only ever the box's own).
#[derive(Debug, Serialize)]
pub struct AsioDrivers {
    pub drivers: Vec<String>,
}

pub async fn get_asio_drivers() -> Json<AsioDrivers> {
    Json(AsioDrivers {
        drivers: registered_drivers().await,
    })
}

/// The registry read (`asio_win::list_drivers`, tested on the Windows job),
/// on the blocking pool.
#[cfg(windows)]
#[cfg_attr(test, mutants::skip)] // Windows-only; dead on the Linux mutation runner
async fn registered_drivers() -> Vec<String> {
    tokio::task::spawn_blocking(crate::playback::asio_win::list_drivers)
        .await
        .unwrap_or_else(|e| {
            tracing::warn!(%e, "listing the ASIO drivers failed");
            Vec::new()
        })
}

/// No ASIO off Windows.
#[cfg(not(windows))]
#[cfg_attr(test, mutants::skip)] // the empty list: its `vec![]` mutant is the same list
async fn registered_drivers() -> Vec<String> {
    Vec::new()
}

#[cfg(test)]
#[path = "audio_tests.rs"]
mod tests;
