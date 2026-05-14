//! [`ts6_ssrf::Resolver`] impl backed by `tokio::net::lookup_host`
//! (= `getaddrinfo` on glibc). Avoids pulling `hickory-resolver` into
//! the sidecar workspace — the main workspace's parking_lot graph is
//! distinct (moq-native pins `deadlock_detection`), so we keep the
//! transitive dep surface minimal here.
//!
//! `lookup_host("host:port")` is the only API tokio exposes for DNS;
//! we pass a sentinel `:0` port and discard it, then collect the IPs.

use std::net::IpAddr;

use async_trait::async_trait;
use ts6_ssrf::{ResolveError, Resolver};

/// System-DNS resolver. Stateless; clone freely.
#[derive(Debug, Default, Clone)]
pub struct GaiResolver;

impl GaiResolver {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl Resolver for GaiResolver {
    async fn resolve(&self, host: &str) -> Result<Vec<IpAddr>, ResolveError> {
        // `host:0` is the cheapest way to ask getaddrinfo for IPs only —
        // the port is required by `lookup_host`'s SocketAddr return shape.
        let target = format!("{host}:0");
        match tokio::net::lookup_host(target).await {
            Ok(iter) => Ok(iter.map(|sa| sa.ip()).collect()),
            Err(e) => {
                use std::io::ErrorKind;
                match e.kind() {
                    ErrorKind::NotFound | ErrorKind::AddrNotAvailable => {
                        Err(ResolveError::NotFound)
                    }
                    _ => Err(ResolveError::Other(e.to_string())),
                }
            }
        }
    }
}
