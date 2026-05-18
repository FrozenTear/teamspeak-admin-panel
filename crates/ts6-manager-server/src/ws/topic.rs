//! WS hub topics — PURA-70.
//!
//! Topics are the unit of fan-out and authorisation. Wire format is the
//! string `server:{id}:{kind}` where `id` is the `server_connection.id`
//! and `kind` is one of `clients` / `channels` / `logs` / `widget`.
//!
//! Each topic carries an [`AuthRequirement`]: `JwtUser` for the three
//! operator-facing topics, `WidgetToken` for the public widget topic. The
//! requirement informs both the handshake (which credential type opens the
//! connection) and the per-subscribe ACL run by [`super::hub::Hub`].
//!
//! Spec deviation: [`Topic`] is a Phase 2 (D-WS) addition. Spec §8.3 says
//! the WS is push-only with no client-to-server protocol; PURA-66 elected
//! to add explicit topic subscriptions instead — see
//! `study-documents/ts6-manager-impl-deviations.md` for the rationale.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

/// All recognised topic kinds. The wire string is the lower-case variant
/// name (`clients`, `channels`, `logs`, `widget`, `video_sources`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TopicKind {
    Clients,
    Channels,
    Logs,
    Widget,
    /// PURA-144 (WS-6) — operator-facing live status of video sources
    /// published through the MoQ sidecar. Auth = JWT user; per-server
    /// fan-out keyed on the same `serverConfigId` the rest of the
    /// operator topics use.
    VideoSources,
    /// PURA-373 (PURA-369 moderation completion) — operator-facing live
    /// fan-out for server-group / channel-group / token / message
    /// mutations. Auth = JWT user; per-server keyed like the other
    /// operator topics.
    Moderation,
}

impl TopicKind {
    pub fn as_str(self) -> &'static str {
        match self {
            TopicKind::Clients => "clients",
            TopicKind::Channels => "channels",
            TopicKind::Logs => "logs",
            TopicKind::Widget => "widget",
            TopicKind::VideoSources => "video_sources",
            TopicKind::Moderation => "moderation",
        }
    }

    pub fn auth_requirement(self) -> AuthRequirement {
        match self {
            TopicKind::Clients
            | TopicKind::Channels
            | TopicKind::Logs
            | TopicKind::VideoSources
            | TopicKind::Moderation => AuthRequirement::JwtUser,
            TopicKind::Widget => AuthRequirement::WidgetToken,
        }
    }
}

/// What kind of credential a connection must present to subscribe to a
/// topic. The two paths are exclusive — a JWT-authenticated connection
/// cannot subscribe to a widget topic and vice versa, even if the IDs
/// would line up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthRequirement {
    JwtUser,
    WidgetToken,
}

/// A parsed topic identifier. `server_id` is the `server_connection.id`
/// (the public §7 `configId`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Topic {
    pub server_id: i64,
    pub kind: TopicKind,
}

impl Topic {
    pub fn new(server_id: i64, kind: TopicKind) -> Self {
        Self { server_id, kind }
    }

    pub fn auth_requirement(&self) -> AuthRequirement {
        self.kind.auth_requirement()
    }
}

impl fmt::Display for Topic {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "server:{}:{}", self.server_id, self.kind.as_str())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TopicParseError {
    /// The topic string did not match `server:{id}:{kind}`.
    Malformed,
    /// The id segment was not a base-10 i64.
    BadId,
    /// The kind segment was not one of the recognised values.
    UnknownKind,
}

impl fmt::Display for TopicParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TopicParseError::Malformed => f.write_str("topic must be `server:<id>:<kind>`"),
            TopicParseError::BadId => f.write_str("topic id must be an integer"),
            TopicParseError::UnknownKind => {
                f.write_str("topic kind must be clients/channels/logs/widget")
            }
        }
    }
}

impl FromStr for Topic {
    type Err = TopicParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let mut parts = s.splitn(3, ':');
        let head = parts.next().ok_or(TopicParseError::Malformed)?;
        let id_str = parts.next().ok_or(TopicParseError::Malformed)?;
        let kind_str = parts.next().ok_or(TopicParseError::Malformed)?;
        // Reject a trailing fourth segment by checking the iterator
        // exhausted before we returned. `splitn(3, _)` keeps everything
        // after the second colon together, so `server:1:clients:foo`
        // would surface as kind=`clients:foo` here — explicitly reject.
        if head != "server" {
            return Err(TopicParseError::Malformed);
        }
        if kind_str.contains(':') {
            return Err(TopicParseError::Malformed);
        }
        let server_id: i64 = id_str.parse().map_err(|_| TopicParseError::BadId)?;
        let kind = match kind_str {
            "clients" => TopicKind::Clients,
            "channels" => TopicKind::Channels,
            "logs" => TopicKind::Logs,
            "widget" => TopicKind::Widget,
            "video_sources" => TopicKind::VideoSources,
            "moderation" => TopicKind::Moderation,
            _ => return Err(TopicParseError::UnknownKind),
        };
        Ok(Topic { server_id, kind })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        for kind in [
            TopicKind::Clients,
            TopicKind::Channels,
            TopicKind::Logs,
            TopicKind::Widget,
            TopicKind::VideoSources,
            TopicKind::Moderation,
        ] {
            let t = Topic::new(7, kind);
            let s = t.to_string();
            assert_eq!(s.parse::<Topic>().unwrap(), t, "round-trip failed for {s}");
        }
    }

    #[test]
    fn rejects_malformed() {
        assert_eq!("".parse::<Topic>(), Err(TopicParseError::Malformed));
        assert_eq!("server:1".parse::<Topic>(), Err(TopicParseError::Malformed));
        assert_eq!(
            "user:1:clients".parse::<Topic>(),
            Err(TopicParseError::Malformed)
        );
        assert_eq!(
            "server:1:clients:extra".parse::<Topic>(),
            Err(TopicParseError::Malformed)
        );
    }

    #[test]
    fn rejects_bad_id() {
        assert_eq!(
            "server:abc:clients".parse::<Topic>(),
            Err(TopicParseError::BadId)
        );
    }

    #[test]
    fn rejects_unknown_kind() {
        assert_eq!(
            "server:1:bots".parse::<Topic>(),
            Err(TopicParseError::UnknownKind)
        );
    }

    #[test]
    fn auth_requirements() {
        assert_eq!(
            TopicKind::Clients.auth_requirement(),
            AuthRequirement::JwtUser
        );
        assert_eq!(
            TopicKind::Channels.auth_requirement(),
            AuthRequirement::JwtUser
        );
        assert_eq!(TopicKind::Logs.auth_requirement(), AuthRequirement::JwtUser);
        assert_eq!(
            TopicKind::Widget.auth_requirement(),
            AuthRequirement::WidgetToken
        );
    }
}
