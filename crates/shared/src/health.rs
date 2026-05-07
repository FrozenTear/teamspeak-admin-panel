use serde::{Deserialize, Serialize};

/// Response body of `GET /health`.
///
/// Phase 0 surface — currently only carries a status string. Future fields (uptime,
/// build hash, dependency probes) land here when the server starts gating on them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Health {
    pub status: HealthStatus,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HealthStatus {
    Ok,
    Degraded,
    Unhealthy,
}
