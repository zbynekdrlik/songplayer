//! #233: `GET /api/v1/audio/asio-drivers` — the ASIO drivers this box has
//! registered (HKLM\SOFTWARE\ASIO, read only; no driver is loaded), for the
//! dashboard's driver list of an ASIO output. Empty off Windows, and empty
//! when the key is absent; a read that failed otherwise is a 500 naming why
//! (#233 release review), never an empty list.

use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Serialize;

/// The answer (no `Default`: an empty list is only ever the box's own).
#[derive(Debug, Serialize)]
pub struct AsioDrivers {
    pub drivers: Vec<String>,
}

pub async fn get_asio_drivers() -> Response {
    drivers_answer(registered_drivers().await)
}

/// The answer to a read of the driver list.
pub(crate) fn drivers_answer(read: Result<Vec<String>, String>) -> Response {
    let drivers = read.unwrap_or_default();
    (StatusCode::OK, Json(AsioDrivers { drivers })).into_response()
}

/// The registry read (`asio_win::list_drivers`, tested on the Windows job),
/// on the blocking pool.
#[cfg(windows)]
#[cfg_attr(test, mutants::skip)] // Windows-only; dead on the Linux mutation runner
async fn registered_drivers() -> Result<Vec<String>, String> {
    tokio::task::spawn_blocking(crate::playback::asio_win::list_drivers)
        .await
        .unwrap_or_else(|e| Err(format!("the read's task failed: {e}")))
}

/// No ASIO off Windows.
#[cfg(not(windows))]
#[cfg_attr(test, mutants::skip)] // the empty list: its `vec![]` mutant is the same list
async fn registered_drivers() -> Result<Vec<String>, String> {
    Ok(Vec::new())
}

#[cfg(test)]
#[path = "audio_tests.rs"]
mod tests;
