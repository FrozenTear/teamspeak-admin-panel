//! Typed database-error boundary — R8 / D8 mapping.
//!
//! ## Why this module exists
//!
//! Impl-plan §6 R8 calls for three named SQLite-full boundaries to be mapped
//! onto the SurrealDB Rust client per the D8 deviation:
//!
//! | Boundary | Spec intent (SQLite-full §4.4 / §16.2) | SurrealDB v3 source |
//! |---|---|---|
//! | **Write failure** | The write was rejected — constraint, schema, validation, NotAllowed, AlreadyExists. | `ErrorDetails::{NotAllowed, AlreadyExists, Validation}`, query layer rejecting `CREATE`/`UPDATE`/`UPSERT`. |
//! | **Transaction conflict** | Concurrent transactions collided; the caller may retry the whole operation. | `ErrorDetails::Query(Some(QueryError::TransactionConflict))` (wire code `-32009`). |
//! | **Capacity pressure** | Backing store is out of capacity (disk-full, KV quota, refused growth). | SurrealKV / kvstore IO error surfaced as `ErrorDetails::Internal` whose message text contains a capacity signal (`disk full`, `no space left on device`, `out of memory`, etc.) per the surrealkv/IO layer. Heuristic by necessity — SurrealDB v3 has no dedicated capacity variant — but captured behind one classify function so the heuristic lives in exactly one place. |
//!
//! ## What this module does NOT do
//!
//! - It does not change the repo function signatures. Repos continue to
//!   return `anyhow::Result<T>`; classification is opt-in on the calling
//!   side. This keeps the diff small and prevents inadvertent surface-area
//!   leakage at call sites that already log the underlying `anyhow::Error`
//!   to `tracing::warn!` and return a static-string HTTP response.
//! - It does not touch the wire-error envelope (`crate::routes::control::ErrorBody`).
//!   Callers that opt into classification get a typed [`DbBoundary`] plus a
//!   ready-made `(StatusCode, &'static str)` translation via
//!   [`DbBoundary::http_response`] so the wire-error body remains free of
//!   any internal SurrealDB message text.
//!
//! ## Surface-area leakage hardening
//!
//! Every code path in this module that turns a [`DbBoundary`] into an HTTP
//! response uses *static strings* for the response body. Internal SurrealDB
//! `message()` text never crosses the HTTP boundary. The full underlying
//! error is preserved on the [`DbError::source`] anyhow chain so the call
//! site can still `tracing::warn!(error = %e, …)` it into structured logs.

use std::borrow::Cow;

use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Serialize;

use crate::routes::control::ErrorBody;

/// Named storage-full boundary the impl-plan calls for. Stable enum — every
/// SurrealDB error category we care about classifies into exactly one of
/// these.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DbBoundary {
    /// The write was rejected by the backend. Non-retryable from the
    /// caller's perspective — the operation will keep failing until the
    /// input changes.
    ///
    /// HTTP: `500 Internal Server Error` (default). Routes that have a
    /// more specific contract (`409 Conflict` for unique-key collisions,
    /// `400 Bad Request` for validation) MUST still translate through
    /// [`DbBoundary::http_response`] with their own status override and
    /// MUST NOT surface the underlying SurrealDB message text.
    WriteFailure,

    /// Concurrent transactions collided. The caller MAY retry the same
    /// operation; the surrounding repo function is responsible for the
    /// retry policy.
    ///
    /// HTTP: `409 Conflict` with a generic "Conflicting concurrent
    /// update; please retry." body — the body is intentionally
    /// retry-positive so clients can backoff and re-issue without a
    /// human in the loop.
    TransactionConflict,

    /// The backing store has run out of capacity (disk full, KV quota,
    /// refused growth). Non-retryable until the operator clears space.
    ///
    /// HTTP: `507 Insufficient Storage` (RFC 4918 §11.5). This is the
    /// closest standard mapping for "the server understood the request
    /// but the persistent store is full." Clients that don't understand
    /// 507 will treat it as 5xx and back off, which is also correct.
    CapacityPressure,

    /// Resource not found. Translated separately because route handlers
    /// generally want a 404 here, not a 500.
    NotFound,

    /// Anything we couldn't classify. Treated as an internal error.
    Other,
}

impl DbBoundary {
    /// Static HTTP status + body pair. Routes that want a richer body
    /// (e.g. with a `code`) should call [`Self::status`] and build their
    /// own `ErrorBody`, but the default body never includes any
    /// underlying SurrealDB message text.
    pub fn http_response(self) -> Response {
        let (status, message, code) = self.status_message();
        let body = ErrorBody {
            error: message.into_owned(),
            code: None,
            details: None,
        };
        (status, BoundaryJson { body, _code: code }).into_response()
    }

    /// HTTP status this boundary maps to. Exposed so callers can short-
    /// circuit before assembling the body (e.g. when emitting a custom
    /// envelope alongside a WS-error event).
    #[allow(dead_code)]
    pub fn status(self) -> StatusCode {
        self.status_message().0
    }

    /// Internal — single source of truth for the boundary → wire pair.
    fn status_message(self) -> (StatusCode, Cow<'static, str>, &'static str) {
        match self {
            Self::WriteFailure => (
                StatusCode::INTERNAL_SERVER_ERROR,
                Cow::Borrowed("Internal server error"),
                "db_write_failure",
            ),
            Self::TransactionConflict => (
                StatusCode::CONFLICT,
                Cow::Borrowed("Conflicting concurrent update; please retry."),
                "db_transaction_conflict",
            ),
            Self::CapacityPressure => (
                StatusCode::INSUFFICIENT_STORAGE,
                Cow::Borrowed("Persistent store is full; please contact the operator."),
                "db_capacity_pressure",
            ),
            Self::NotFound => (
                StatusCode::NOT_FOUND,
                Cow::Borrowed("Not found"),
                "db_not_found",
            ),
            Self::Other => (
                StatusCode::INTERNAL_SERVER_ERROR,
                Cow::Borrowed("Internal server error"),
                "db_other",
            ),
        }
    }
}

/// Wrapper so the response body carries a `code` discriminator for ops
/// dashboards / log correlation without exposing message text. The
/// underlying body type stays `ErrorBody` so the wire shape matches
/// every other route handler.
#[derive(Debug, Serialize)]
struct BoundaryJson {
    #[serde(flatten)]
    body: ErrorBody,
    /// Stable discriminator string; never surfaced to the operator-facing
    /// `error` field but logged + serialised so log scrapers can bucket.
    #[serde(rename = "code")]
    _code: &'static str,
}

impl IntoResponse for BoundaryJson {
    fn into_response(self) -> Response {
        Json(self).into_response()
    }
}

/// Typed boundary error with an `anyhow` source preserved for tracing.
#[derive(Debug)]
pub struct DbError {
    pub boundary: DbBoundary,
    pub source: anyhow::Error,
}

impl DbError {
    /// Construct a [`DbError`] from a pre-classified boundary + anyhow
    /// source. Tests use this for synthetic-boundary error-injection;
    /// production call sites should prefer [`ClassifyDbResult::classify_db`]
    /// which dispatches the boundary automatically.
    #[allow(dead_code)]
    pub fn new(boundary: DbBoundary, source: anyhow::Error) -> Self {
        Self { boundary, source }
    }
}

impl std::fmt::Display for DbError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Display intentionally omits the underlying message — call sites
        // log the full chain via `error = %self.source`. The Display impl
        // is what would land in `format!("{}", err)`, and we don't want
        // any caller accidentally substituting that into a response body.
        match self.boundary {
            DbBoundary::WriteFailure => f.write_str("database write failure"),
            DbBoundary::TransactionConflict => f.write_str("database transaction conflict"),
            DbBoundary::CapacityPressure => f.write_str("database capacity pressure"),
            DbBoundary::NotFound => f.write_str("database not found"),
            DbBoundary::Other => f.write_str("database error"),
        }
    }
}

impl std::error::Error for DbError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.source
            .source()
            .or_else(|| Some(self.source.as_ref() as &(dyn std::error::Error + 'static)))
    }
}

impl IntoResponse for DbError {
    fn into_response(self) -> Response {
        // Log the full underlying chain for ops, but only the boundary's
        // static-string body crosses the wire.
        tracing::warn!(
            boundary = ?self.boundary,
            error = %self.source,
            "DB boundary error returned to client"
        );
        self.boundary.http_response()
    }
}

/// Classify an `anyhow::Error` chain into the named storage-full boundary.
///
/// Walks the source chain looking for a `surrealdb::Error`. If one is
/// found, [`classify_surrealdb`] dispatches on its typed details. If the
/// chain does not contain a SurrealDB error, the classification falls back
/// to [`DbBoundary::Other`] — repos that wrap non-DB failures (e.g. IO
/// errors during migration loading) end up here.
pub fn classify(err: &anyhow::Error) -> DbBoundary {
    for cause in err.chain() {
        if let Some(surreal) = cause.downcast_ref::<surrealdb::Error>() {
            return classify_surrealdb(surreal);
        }
    }
    // No SurrealDB in the chain — heuristic-classify by message text so
    // unwrapped capacity-pressure IO errors (the most operationally
    // important case for an embedded backend) still classify correctly.
    if message_indicates_capacity_pressure(&err.to_string()) {
        return DbBoundary::CapacityPressure;
    }
    DbBoundary::Other
}

/// Classify a SurrealDB error directly. Public so the sidecar / WS layer
/// can reuse the same mapping for IO errors that originate inside an axum
/// extractor before they get wrapped in `anyhow`.
pub fn classify_surrealdb(err: &surrealdb::Error) -> DbBoundary {
    use surrealdb::types::{ErrorDetails, QueryError};

    match err.details() {
        // Spec mapping: transaction conflict is retryable. Wire code
        // `-32009` per surrealdb-types/src/error.rs:27.
        ErrorDetails::Query(Some(QueryError::TransactionConflict)) => {
            DbBoundary::TransactionConflict
        }
        // Spec mapping: timeouts and cancellations are also conflict-class
        // for the purposes of the upstream retry policy — the caller may
        // try again with fresh state. Folded into the conflict boundary
        // so the wire status stays `409` (retry-positive) instead of `500`.
        ErrorDetails::Query(Some(QueryError::TimedOut { .. })) => DbBoundary::TransactionConflict,
        ErrorDetails::Query(Some(QueryError::Cancelled)) => DbBoundary::TransactionConflict,

        // Spec mapping: write-failure family. Anything the backend
        // rejected on validation / not-allowed / already-exists grounds
        // is a hard write rejection.
        ErrorDetails::Validation(_)
        | ErrorDetails::NotAllowed(_)
        | ErrorDetails::AlreadyExists(_)
        | ErrorDetails::Configuration(_)
        | ErrorDetails::Serialization(_) => DbBoundary::WriteFailure,

        // Not-found is its own boundary so HTTP 404s map naturally.
        ErrorDetails::NotFound(_) => DbBoundary::NotFound,

        // Connection-state errors usually mean the storage backend is
        // unreachable. From an operator's perspective that's closer to
        // capacity-pressure (system unavailable, retry won't help) than
        // a generic internal error — but only when the message text
        // clearly signals it. Otherwise fall through to Other so a
        // transient client-side error doesn't get a 507.
        ErrorDetails::Connection(_) | ErrorDetails::Internal => {
            if message_indicates_capacity_pressure(err.message()) {
                DbBoundary::CapacityPressure
            } else {
                DbBoundary::Other
            }
        }

        // Thrown user-level errors (THROW in SurrealQL) and any future
        // variant we haven't pattern-matched stay in Other.
        ErrorDetails::Thrown => DbBoundary::Other,
        _ => DbBoundary::Other,
    }
}

/// Heuristic — does the error message contain a recognised
/// capacity-pressure signal from SurrealKV / the underlying IO layer? Kept
/// case-insensitive, prefix-free, and centralised so the moment SurrealDB
/// upstream adds a typed variant we can swap this for a `matches!` check.
///
/// Patterns derived from:
/// - POSIX errno strings (`ENOSPC` → "no space left on device").
/// - SurrealKV embedded backend IO errors (`out of memory`, `quota
///   exceeded`, `db is full`).
/// - The SQLite legacy contract our spec inherits (`SQLITE_FULL` →
///   "database or disk is full").
fn message_indicates_capacity_pressure(msg: &str) -> bool {
    let lower = msg.to_ascii_lowercase();
    [
        "no space left on device",
        "disk full",
        "database or disk is full",
        "db is full",
        "out of memory",
        "quota exceeded",
        "storage full",
        "insufficient storage",
        "enospc",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
}

/// `anyhow::Result` → `Result<T, DbError>` translation. Repos keep their
/// `anyhow::Result` signatures; call sites that want typed boundary
/// handling opt in via this trait, which classifies the error chain and
/// preserves the underlying anyhow source.
pub trait ClassifyDbResult<T> {
    fn classify_db(self) -> Result<T, DbError>;
}

impl<T> ClassifyDbResult<T> for anyhow::Result<T> {
    fn classify_db(self) -> Result<T, DbError> {
        self.map_err(|source| {
            let boundary = classify(&source);
            DbError { boundary, source }
        })
    }
}

#[cfg(test)]
mod tests {
    //! R8 boundary-classification tests. PURA-161.
    //!
    //! These act as the impl-plan's "error-injection tests on each
    //! boundary": each test drives the classifier to exactly one
    //! [`DbBoundary`] using either a real SurrealDB error variant or a
    //! synthesised one via the wire-friendly constructors in
    //! `surrealdb::types::Error`.
    //!
    //! The classifier MUST stay deterministic on these inputs forever —
    //! if a SurrealDB upgrade renames a variant, fix the classifier
    //! before these tests start passing again, not the tests.
    use super::*;
    use surrealdb::types::{
        AlreadyExistsError, ConfigurationError, ConnectionError, Error as SurrealError,
        NotFoundError, QueryError, SerializationError, ValidationError,
    };

    fn boundary_of(err: SurrealError) -> DbBoundary {
        classify_surrealdb(&err)
    }

    #[test]
    fn transaction_conflict_classifies_as_transaction_conflict() {
        let err = SurrealError::query(
            "transaction conflict".into(),
            QueryError::TransactionConflict,
        );
        assert_eq!(boundary_of(err), DbBoundary::TransactionConflict);
    }

    #[test]
    fn query_timeout_classifies_as_transaction_conflict() {
        // Retry-positive: the wire response stays 409 with the
        // retry-positive copy so a client can backoff.
        let err = SurrealError::query(
            "query timed out".into(),
            QueryError::TimedOut {
                duration: std::time::Duration::from_secs(1),
            },
        );
        assert_eq!(boundary_of(err), DbBoundary::TransactionConflict);
    }

    #[test]
    fn validation_errors_classify_as_write_failure() {
        let err = SurrealError::validation("bad params".into(), ValidationError::InvalidParams);
        assert_eq!(boundary_of(err), DbBoundary::WriteFailure);
    }

    #[test]
    fn already_exists_classifies_as_write_failure() {
        let err = SurrealError::already_exists(
            "duplicate".into(),
            AlreadyExistsError::Record {
                id: "user:42".into(),
            },
        );
        assert_eq!(boundary_of(err), DbBoundary::WriteFailure);
    }

    #[test]
    fn not_found_classifies_as_not_found() {
        let err = SurrealError::not_found(
            "missing".into(),
            NotFoundError::Record {
                id: "user:99".into(),
            },
        );
        assert_eq!(boundary_of(err), DbBoundary::NotFound);
    }

    #[test]
    fn serialization_classifies_as_write_failure() {
        let err =
            SurrealError::serialization("bad payload".into(), SerializationError::Deserialization);
        assert_eq!(boundary_of(err), DbBoundary::WriteFailure);
    }

    #[test]
    fn configuration_classifies_as_write_failure() {
        let err = SurrealError::configuration(
            "bad config".into(),
            ConfigurationError::BadLiveQueryConfig,
        );
        assert_eq!(boundary_of(err), DbBoundary::WriteFailure);
    }

    #[test]
    fn capacity_pressure_keywords_promote_internal_to_capacity_boundary() {
        for msg in [
            "io error: No space left on device",
            "surrealkv: disk full",
            "out of memory while reserving 4 MiB",
            "quota exceeded for namespace ts6",
            "ENOSPC",
        ] {
            let err = SurrealError::internal(msg.into());
            assert_eq!(
                classify_surrealdb(&err),
                DbBoundary::CapacityPressure,
                "{msg:?} must classify as capacity pressure"
            );
        }
    }

    #[test]
    fn generic_internal_falls_through_to_other() {
        let err = SurrealError::internal("something went sideways".into());
        assert_eq!(classify_surrealdb(&err), DbBoundary::Other);
    }

    #[test]
    fn connection_failed_without_capacity_signal_falls_through_to_other() {
        let err = SurrealError::connection(
            "could not reach the server".into(),
            ConnectionError::ConnectionFailed,
        );
        assert_eq!(classify_surrealdb(&err), DbBoundary::Other);
    }

    #[test]
    fn connection_failed_with_capacity_signal_promotes_to_capacity_boundary() {
        let err = SurrealError::connection(
            "could not write: no space left on device".into(),
            ConnectionError::ConnectionFailed,
        );
        assert_eq!(classify_surrealdb(&err), DbBoundary::CapacityPressure);
    }

    #[test]
    fn classify_walks_anyhow_chain_to_find_surreal_error() {
        let surreal = SurrealError::query("retry me".into(), QueryError::TransactionConflict);
        let wrapped: anyhow::Error =
            anyhow::Error::new(surreal).context("user insert query failed");
        assert_eq!(classify(&wrapped), DbBoundary::TransactionConflict);
    }

    #[test]
    fn classify_falls_back_to_message_heuristic_when_no_surreal_error() {
        let err = anyhow::anyhow!("io error: No space left on device");
        assert_eq!(classify(&err), DbBoundary::CapacityPressure);
    }

    #[test]
    fn classify_db_extension_trait_threads_boundary_through_result() {
        let result: anyhow::Result<()> = Err(anyhow::Error::new(SurrealError::query(
            "x".into(),
            QueryError::TransactionConflict,
        )));
        let typed = result.classify_db().unwrap_err();
        assert_eq!(typed.boundary, DbBoundary::TransactionConflict);
        // Source is preserved for tracing.
        assert!(typed.source.to_string().contains('x'));
    }

    #[test]
    fn http_response_bodies_contain_no_underlying_message_text() {
        // R8 surface-area leakage: the boundary's HTTP body MUST be the
        // pre-baked static string; the SurrealDB message text MUST NOT
        // appear in the response body. We invoke the classifier on a
        // bespoke message and then inspect the constructed response.
        let secret_marker = "leaky_marker_DO_NOT_LEAK_42";
        let err = SurrealError::internal(format!("io error: {secret_marker}; disk full"));
        let typed = DbError::new(classify_surrealdb(&err), anyhow::Error::new(err));
        let resp = typed.into_response();
        assert_eq!(resp.status(), StatusCode::INSUFFICIENT_STORAGE);
        // The body is rendered through `BoundaryJson` → static string;
        // the secret marker must not appear in the serialised body.
        let body_bytes = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async { axum::body::to_bytes(resp.into_body(), 4096).await.unwrap() });
        let body_str = std::str::from_utf8(&body_bytes).unwrap();
        assert!(
            !body_str.contains(secret_marker),
            "underlying SurrealDB message text leaked into HTTP body: {body_str}"
        );
        assert!(
            body_str.contains("Persistent store is full"),
            "static body string missing from {body_str}"
        );
    }

    #[test]
    fn write_failure_response_returns_500_with_static_body() {
        let err = SurrealError::validation("nope".into(), ValidationError::InvalidParams);
        let typed = DbError::new(classify_surrealdb(&err), anyhow::Error::new(err));
        let resp = typed.into_response();
        assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
    }

    #[test]
    fn transaction_conflict_response_returns_409() {
        let err = SurrealError::query("conflict".into(), QueryError::TransactionConflict);
        let typed = DbError::new(classify_surrealdb(&err), anyhow::Error::new(err));
        let resp = typed.into_response();
        assert_eq!(resp.status(), StatusCode::CONFLICT);
    }

    #[test]
    fn capacity_pressure_response_returns_507() {
        let err = SurrealError::internal("disk full".into());
        let typed = DbError::new(classify_surrealdb(&err), anyhow::Error::new(err));
        let resp = typed.into_response();
        assert_eq!(resp.status(), StatusCode::INSUFFICIENT_STORAGE);
    }
}
