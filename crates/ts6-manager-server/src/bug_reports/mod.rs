//! Operator bug-report sink — private GitHub Issues via the existing
//! rustls `reqwest` client.
//!
//! Product choice (this draft): no Sentry crate, no browser SDK. Contabo
//! enables the sink later by setting `BUG_REPORTS_GITHUB_TOKEN` +
//! `BUG_REPORTS_GITHUB_REPO` without a boot crash — missing config is a
//! 503 from the route, not a `Config::load` failure.
//!
//! [`BugReportSink`] is a trait so route tests inject a recording mock
//! instead of talking to api.github.com.

mod github;
mod markdown;

pub use github::{GitHubIssueSink, UnconfiguredSink};
pub use markdown::{IssueDraft, Reporter, build_issue};

use std::sync::Arc;

use crate::config::BugReportsGithubConfig;

/// Handle stored on [`crate::app_state::AppState`]. Cheap to clone.
pub type BugReportSinkHandle = Arc<dyn BugReportSink>;

/// Result of a successful sink write.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreatedIssue {
    pub html_url: String,
    pub number: i64,
}

/// Why the sink refused or failed a write.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SinkError {
    Unconfigured,
    Upstream,
}

#[async_trait::async_trait]
pub trait BugReportSink: Send + Sync {
    fn is_configured(&self) -> bool;
    async fn create_issue(&self, draft: IssueDraft) -> Result<CreatedIssue, SinkError>;
}

/// Build the production sink from boot config. Token/repo unset or a
/// malformed `owner/name` repo → [`UnconfiguredSink`] (503 at request time).
pub fn sink_from_config(cfg: &BugReportsGithubConfig) -> BugReportSinkHandle {
    match GitHubIssueSink::try_from_config(cfg) {
        Some(sink) => Arc::new(sink),
        None => unconfigured_sink(),
    }
}

/// Test / default handle: every `create_issue` is [`SinkError::Unconfigured`].
pub fn unconfigured_sink() -> BugReportSinkHandle {
    Arc::new(UnconfiguredSink)
}

/// In-memory sink for route tests. Records every draft; returns a canned issue.
#[derive(Debug, Clone)]
pub struct RecordingSink {
    pub issues: Arc<std::sync::Mutex<Vec<IssueDraft>>>,
    pub issue_url: String,
    pub issue_number: i64,
}

impl RecordingSink {
    pub fn new(issue_url: impl Into<String>, issue_number: i64) -> Self {
        Self {
            issues: Arc::new(std::sync::Mutex::new(Vec::new())),
            issue_url: issue_url.into(),
            issue_number,
        }
    }

    pub fn handle(self) -> BugReportSinkHandle {
        Arc::new(self)
    }
}

#[async_trait::async_trait]
impl BugReportSink for RecordingSink {
    fn is_configured(&self) -> bool {
        true
    }

    async fn create_issue(&self, draft: IssueDraft) -> Result<CreatedIssue, SinkError> {
        self.issues
            .lock()
            .expect("recording sink mutex")
            .push(draft);
        Ok(CreatedIssue {
            html_url: self.issue_url.clone(),
            number: self.issue_number,
        })
    }
}
