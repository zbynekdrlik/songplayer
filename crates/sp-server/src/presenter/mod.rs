//! HTTP push to the Presenter stage-display API. Used by the playback
//! engine's line-change hook (T2.4) to inform band singers what line is
//! sung and what comes next on their stage displays, independently of
//! whatever the audience wall shows.
//!
//! Prod host: http://10.77.9.205/api/stage
//! Dev host:  http://10.77.8.134:8080/api/stage

pub mod payload;

pub use payload::PresenterPayload;
