//! A relaunch with no push in between (#217 addendum 3). Arena gives every
//! clip and text param a new id on each relaunch. A relaunch quicker than
//! three failed probes never opens the breaker, and while SongPlayer pushes
//! nothing no request answers 404, so the map kept the dead ids until the
//! 300 s TTL refresh, which fires no RecoveryEvent. So after a liveness
//! failure the breaker did not see, the driver asks Arena for ONE mapped
//! param (~100 bytes, not the ~14 MB composition). Split from `driver.rs` for
//! the 1000-line cap; a child module of `driver`, so it reads the driver's
//! private state.

use std::time::Instant;

use tracing::{debug, warn};

use super::HostDriver;

impl HostDriver {
    /// Called on a failing→ok liveness flip:
    /// `GET /api/v1/parameter/by-id/{id}` for the first mapped SongPlayer
    /// text param. A 404 means the map is stale: it opens the not-ready
    /// episode, so this tick's `decide` refreshes on the NotReady path (then
    /// every 2 s until the clips are mapped, and the ready map fires the
    /// RecoveryEvent). Any other answer leaves the map alone. An open episode
    /// refreshes anyway, so it is not probed; a breaker close is one (it
    /// evicted the map and opened an episode, the design's "breaker did not
    /// open" case).
    pub(super) async fn probe_stale_map(&mut self, now: Instant) {
        if self.not_ready_since.is_some() {
            return;
        }
        let Some(param_id) = self.probe_param_id() else {
            return;
        };
        match self.fetch_param_status(param_id).await {
            Ok(status) if status == reqwest::StatusCode::NOT_FOUND => {
                warn!(
                    host = %self.host,
                    param_id,
                    "Resolume answered 404 for a mapped param after a liveness failure — the clip map is stale (Arena re-ids its clips on relaunch), refreshing it"
                );
                self.not_ready_since = Some(now);
            }
            Ok(status) => debug!(
                host = %self.host,
                param_id,
                %status,
                "Resolume param probe after a liveness failure — the clip map is still valid"
            ),
            Err(e) => debug!(
                host = %self.host,
                param_id,
                %e,
                "Resolume param probe failed — keeping the clip map"
            ),
        }
    }

    /// The text param of the first mapped SongPlayer clip, in
    /// `SONGPLAYER_TOKENS` order.
    fn probe_param_id(&self) -> Option<i64> {
        crate::resolume::SONGPLAYER_TOKENS
            .iter()
            .find_map(|token| {
                self.clip_mapping
                    .get(*token)
                    .and_then(|clips| clips.first())
            })
            .map(|clip| clip.text_param_id)
    }

    /// `GET /api/v1/parameter/by-id/{param_id}`: only the status matters.
    async fn fetch_param_status(
        &mut self,
        param_id: i64,
    ) -> Result<reqwest::StatusCode, anyhow::Error> {
        let ep = self.endpoint().await?;
        let url = format!("{}/api/v1/parameter/by-id/{param_id}", ep.base_url);
        let req = self.client.get(&url);
        Ok(Self::apply_host_header(req, &ep).send().await?.status())
    }
}
