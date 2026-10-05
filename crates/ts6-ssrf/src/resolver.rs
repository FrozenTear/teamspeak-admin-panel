//! DNS resolution surface for the SSRF validator.
//!
//! Spec §6.7.2 + §9.3: the validator MUST resolve hostnames before deciding
//! whether to allow an outbound request, but if resolution fails (NXDOMAIN /
//! timeout) the request proceeds and the downstream HTTP/FFmpeg call is left
//! to fail naturally.
//!
//! The trait abstraction lets tests swap a deterministic [`MockResolver`] in
//! place of [`HickoryResolver`]; production wires the latter into app state.

use std::collections::HashMap;
use std::net::IpAddr;

#[derive(Debug, thiserror::Error)]
pub enum ResolveError {
    #[error("name not found")]
    NotFound,
    #[error("DNS lookup failed: {0}")]
    Other(String),
}

#[async_trait::async_trait]
pub trait Resolver: Send + Sync {
    /// Resolve `host` to a (possibly empty) list of IPs. Returning `Ok(vec![])`
    /// is treated by the caller the same as a DNS failure (allow per spec §9.3).
    async fn resolve(&self, host: &str) -> Result<Vec<IpAddr>, ResolveError>;
}

/// Production resolver backed by `hickory-resolver` and the system's DNS config.
///
/// Holds a `TokioResolver` and reuses it across calls. Cheap to clone
/// (the resolver is internally shared).
///
/// Feature-gated behind `hickory` so consumers with their own resolver (the
/// sibling-workspace `ts6-media-sidecar` uses `tokio::net::lookup_host`) can
/// drop the dep entirely.
#[cfg(feature = "hickory")]
pub struct HickoryResolver {
    inner: std::sync::Arc<hickory_resolver::TokioResolver>,
}

#[cfg(feature = "hickory")]
impl HickoryResolver {
    /// Build a resolver from the system's `/etc/resolv.conf` (or the platform
    /// equivalent). Falls back to Google's public resolvers if system config
    /// is not readable, so this is safe inside containers that omit resolv.conf.
    pub fn from_system() -> Result<Self, ResolveError> {
        let resolver = match hickory_resolver::TokioResolver::builder_tokio() {
            Ok(builder) => builder
                .build()
                .map_err(|e| ResolveError::Other(e.to_string()))?,
            Err(_) => {
                let builder = hickory_resolver::TokioResolver::builder_with_config(
                    hickory_resolver::config::ResolverConfig::udp_and_tcp(
                        &hickory_resolver::config::GOOGLE,
                    ),
                    hickory_resolver::net::runtime::TokioRuntimeProvider::default(),
                );
                builder
                    .build()
                    .map_err(|e| ResolveError::Other(e.to_string()))?
            }
        };
        Ok(Self {
            inner: std::sync::Arc::new(resolver),
        })
    }
}

#[cfg(feature = "hickory")]
#[async_trait::async_trait]
impl Resolver for HickoryResolver {
    async fn resolve(&self, host: &str) -> Result<Vec<IpAddr>, ResolveError> {
        match self.inner.lookup_ip(host).await {
            Ok(rec) => Ok(rec.iter().collect()),
            Err(e) => match e {
                hickory_resolver::net::NetError::Dns(
                    hickory_resolver::net::DnsError::NoRecordsFound(_),
                ) => Err(ResolveError::NotFound),
                other => Err(ResolveError::Other(other.to_string())),
            },
        }
    }
}

/// Deterministic resolver for tests. Two terminal states per host:
/// - mapped to a `Vec<IpAddr>` (resolves to those IPs).
/// - mapped to `None` (DNS failure / NXDOMAIN).
///
/// Hosts not present in the table are treated as `Err(ResolveError::NotFound)`.
#[derive(Default, Clone)]
pub struct MockResolver {
    table: HashMap<String, Option<Vec<IpAddr>>>,
}

impl MockResolver {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with(mut self, host: &str, ips: Vec<IpAddr>) -> Self {
        self.table.insert(host.to_ascii_lowercase(), Some(ips));
        self
    }

    pub fn nxdomain(mut self, host: &str) -> Self {
        self.table.insert(host.to_ascii_lowercase(), None);
        self
    }
}

#[async_trait::async_trait]
impl Resolver for MockResolver {
    async fn resolve(&self, host: &str) -> Result<Vec<IpAddr>, ResolveError> {
        match self.table.get(&host.to_ascii_lowercase()) {
            Some(Some(ips)) => Ok(ips.clone()),
            Some(None) => Err(ResolveError::NotFound),
            None => Err(ResolveError::NotFound),
        }
    }
}
