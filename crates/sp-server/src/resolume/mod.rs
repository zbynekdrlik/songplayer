//! Resolume Arena integration with per-host workers and A/B crossfade.

pub mod driver;
pub mod handlers;

use std::collections::HashMap;

use tokio::sync::{broadcast, mpsc};
use tracing::{info, warn};

use crate::resolume::driver::HostDriver;

/// Commands sent to per-host Resolume workers.
#[derive(Debug, Clone)]
pub enum ResolumeCommand {
    UpdateTitle {
        playlist_id: i64,
        song: String,
        artist: String,
    },
    ClearTitle {
        playlist_id: i64,
    },
    RefreshMapping,
    Shutdown,
}

/// Registry managing per-host Resolume workers.
pub struct ResolumeRegistry {
    hosts: HashMap<i64, mpsc::Sender<ResolumeCommand>>,
}

impl ResolumeRegistry {
    pub fn new() -> Self {
        Self {
            hosts: HashMap::new(),
        }
    }

    /// Start a worker for a host. Spawns a background task and stores the
    /// command channel sender.
    pub fn add_host(
        &mut self,
        host_id: i64,
        host: String,
        port: u16,
        shutdown: broadcast::Receiver<()>,
    ) {
        let (tx, rx) = mpsc::channel::<ResolumeCommand>(64);
        let driver = HostDriver::new(host.clone(), port);

        tokio::spawn(async move {
            driver.run(rx, shutdown).await;
        });

        info!(host_id, %host, port, "added Resolume host worker");
        self.hosts.insert(host_id, tx);
    }

    /// Remove a host worker. Sends a shutdown command before dropping the
    /// channel.
    pub fn remove_host(&mut self, host_id: i64) {
        if let Some(tx) = self.hosts.remove(&host_id) {
            let _ = tx.try_send(ResolumeCommand::Shutdown);
            info!(host_id, "removed Resolume host worker");
        }
    }

    /// Send a command to a specific host.
    pub async fn send(&self, host_id: i64, cmd: ResolumeCommand) -> Result<(), anyhow::Error> {
        let tx = self
            .hosts
            .get(&host_id)
            .ok_or_else(|| anyhow::anyhow!("no Resolume host with id {host_id}"))?;
        tx.send(cmd)
            .await
            .map_err(|_| anyhow::anyhow!("Resolume host {host_id} channel closed"))
    }

    /// Broadcast a command to all hosts.
    pub async fn broadcast(&self, cmd: ResolumeCommand) {
        for (&host_id, tx) in &self.hosts {
            if let Err(e) = tx.send(cmd.clone()).await {
                warn!(host_id, %e, "failed to send broadcast to Resolume host");
            }
        }
    }
}

impl Default for ResolumeRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_new_is_empty() {
        let registry = ResolumeRegistry::new();
        assert!(registry.hosts.is_empty());
    }

    #[test]
    fn registry_default_is_empty() {
        let registry = ResolumeRegistry::default();
        assert!(registry.hosts.is_empty());
    }

    #[tokio::test]
    async fn registry_add_remove() {
        let (shutdown_tx, _) = broadcast::channel::<()>(1);
        let mut registry = ResolumeRegistry::new();

        registry.add_host(1, "192.168.1.10".to_string(), 8080, shutdown_tx.subscribe());
        registry.add_host(2, "192.168.1.11".to_string(), 8080, shutdown_tx.subscribe());

        assert_eq!(registry.hosts.len(), 2);
        assert!(registry.hosts.contains_key(&1));
        assert!(registry.hosts.contains_key(&2));

        registry.remove_host(1);
        assert_eq!(registry.hosts.len(), 1);
        assert!(!registry.hosts.contains_key(&1));
        assert!(registry.hosts.contains_key(&2));

        // Removing non-existent host is a no-op.
        registry.remove_host(999);
        assert_eq!(registry.hosts.len(), 1);

        // Clean up.
        let _ = shutdown_tx.send(());
    }

    #[tokio::test]
    async fn registry_send_to_missing_host_errors() {
        let registry = ResolumeRegistry::new();
        let result = registry.send(42, ResolumeCommand::RefreshMapping).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("42"));
    }

    #[tokio::test]
    async fn registry_send_to_existing_host() {
        let (shutdown_tx, _) = broadcast::channel::<()>(1);
        let mut registry = ResolumeRegistry::new();

        registry.add_host(1, "127.0.0.1".to_string(), 8080, shutdown_tx.subscribe());

        let result = registry.send(1, ResolumeCommand::RefreshMapping).await;
        assert!(result.is_ok());

        let _ = shutdown_tx.send(());
    }

    #[tokio::test]
    async fn registry_broadcast_sends_to_all() {
        let (shutdown_tx, _) = broadcast::channel::<()>(1);
        let mut registry = ResolumeRegistry::new();

        registry.add_host(1, "127.0.0.1".to_string(), 8080, shutdown_tx.subscribe());
        registry.add_host(2, "127.0.0.1".to_string(), 8081, shutdown_tx.subscribe());

        // Broadcast should not panic or error even with no real Resolume server.
        registry.broadcast(ResolumeCommand::RefreshMapping).await;

        let _ = shutdown_tx.send(());
    }
}
