//! GitHub Issues sink. Uses the workspace `reqwest` (rustls) client —
//! same TLS posture as WebQuery / the sidecar. octocrab was considered
//! and skipped so we do not pull a second HTTP stack or `default-tls`.

use reqwest::StatusCode;
use serde::Deserialize;
use serde_json::json;

use crate::bug_reports::{BugReportSink, CreatedIssue, IssueDraft, SinkError};
use crate::config::BugReportsGithubConfig;

const GITHUB_API: &str = "https://api.github.com";
const USER_AGENT: &str = "ts6-manager-server-bug-reports";

/// Always-off sink used when token/repo are unset or `repo` is not `owner/name`.
#[derive(Debug, Default, Clone, Copy)]
pub struct UnconfiguredSink;

#[async_trait::async_trait]
impl BugReportSink for UnconfiguredSink {
    fn is_configured(&self) -> bool {
        false
    }

    async fn create_issue(&self, _draft: IssueDraft) -> Result<CreatedIssue, SinkError> {
        Err(SinkError::Unconfigured)
    }
}

/// Private-repo Issues writer. Token is held in memory only; never logged.
#[derive(Debug, Clone)]
pub struct GitHubIssueSink {
    token: String,
    owner: String,
    repo: String,
    labels: Vec<String>,
    api_base: String,
}

impl GitHubIssueSink {
    pub fn try_from_config(cfg: &BugReportsGithubConfig) -> Option<Self> {
        Self::try_new(
            cfg.token.clone()?,
            cfg.repo.as_deref()?,
            cfg.labels.clone(),
            GITHUB_API,
        )
    }

    fn try_new(
        token: String,
        repo: &str,
        labels: Vec<String>,
        api_base: impl Into<String>,
    ) -> Option<Self> {
        let token = token.trim();
        let repo = repo.trim();
        if token.is_empty() || repo.is_empty() {
            return None;
        }
        let (owner, name) = split_owner_repo(repo)?;
        Some(Self {
            token: token.to_string(),
            owner,
            repo: name,
            labels,
            api_base: api_base.into(),
        })
    }

    /// Test helper: point the client at a loopback mock instead of api.github.com.
    #[cfg(test)]
    pub fn for_test(token: &str, repo: &str, api_base: &str, labels: Vec<String>) -> Option<Self> {
        Self::try_new(token.to_string(), repo, labels, api_base)
    }
}

fn split_owner_repo(repo: &str) -> Option<(String, String)> {
    let (owner, name) = repo.split_once('/')?;
    let owner = owner.trim();
    let name = name.trim();
    if owner.is_empty() || name.is_empty() || name.contains('/') {
        return None;
    }
    Some((owner.to_string(), name.to_string()))
}

#[derive(Debug, Deserialize)]
struct GitHubIssueResponse {
    html_url: String,
    number: i64,
}

#[async_trait::async_trait]
impl BugReportSink for GitHubIssueSink {
    fn is_configured(&self) -> bool {
        true
    }

    async fn create_issue(&self, draft: IssueDraft) -> Result<CreatedIssue, SinkError> {
        let url = format!(
            "{}/repos/{}/{}/issues",
            self.api_base, self.owner, self.repo
        );
        let mut body = json!({
            "title": draft.title,
            "body": draft.body,
        });
        if !self.labels.is_empty() {
            body["labels"] = json!(self.labels);
        }

        let client = reqwest::Client::builder()
            .user_agent(USER_AGENT)
            .build()
            .map_err(|e| {
                tracing::warn!(error = %e, "bug-reports: failed to build GitHub HTTP client");
                SinkError::Upstream
            })?;

        let resp = client
            .post(url)
            .header("Authorization", format!("Bearer {}", self.token))
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28")
            .json(&body)
            .send()
            .await
            .map_err(|e| {
                tracing::warn!(error = %e, "bug-reports: GitHub request failed");
                SinkError::Upstream
            })?;

        let status = resp.status();
        if status != StatusCode::CREATED && status != StatusCode::OK {
            tracing::warn!(
                status = %status,
                owner = %self.owner,
                repo = %self.repo,
                "bug-reports: GitHub rejected issue create"
            );
            return Err(SinkError::Upstream);
        }

        let parsed: GitHubIssueResponse = resp.json().await.map_err(|e| {
            tracing::warn!(error = %e, "bug-reports: GitHub response was not a valid issue");
            SinkError::Upstream
        })?;

        Ok(CreatedIssue {
            html_url: parsed.html_url,
            number: parsed.number,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::BugReportsGithubConfig;

    #[test]
    fn unset_token_or_repo_is_unconfigured() {
        assert!(
            GitHubIssueSink::try_from_config(&BugReportsGithubConfig {
                token: None,
                repo: Some("o/r".into()),
                labels: Vec::new(),
            })
            .is_none()
        );
        assert!(
            GitHubIssueSink::try_from_config(&BugReportsGithubConfig {
                token: Some("tok".into()),
                repo: None,
                labels: Vec::new(),
            })
            .is_none()
        );
    }

    #[test]
    fn malformed_repo_is_unconfigured() {
        assert!(
            GitHubIssueSink::try_new("tok".into(), "noslash", Vec::new(), GITHUB_API).is_none()
        );
        assert!(GitHubIssueSink::try_new("tok".into(), "a/b/c", Vec::new(), GITHUB_API).is_none());
        assert!(GitHubIssueSink::try_new("tok".into(), "/repo", Vec::new(), GITHUB_API).is_none());
    }

    #[test]
    fn owner_repo_splits() {
        let sink = GitHubIssueSink::try_new(
            "tok".into(),
            "FrozenTear/teamspeak-admin-panel",
            vec!["bug-report".into()],
            GITHUB_API,
        )
        .unwrap();
        assert_eq!(sink.owner, "FrozenTear");
        assert_eq!(sink.repo, "teamspeak-admin-panel");
        assert_eq!(sink.labels, ["bug-report"]);
    }

    #[tokio::test]
    async fn create_issue_posts_to_mock_github() {
        use axum::Router;
        use axum::extract::Json;
        use axum::http::HeaderMap;
        use axum::routing::post;
        use serde_json::Value;
        use std::net::SocketAddr;
        use std::sync::{Arc, Mutex};

        let captured: Arc<Mutex<Option<(HeaderMap, Value)>>> = Arc::new(Mutex::new(None));
        let cap = captured.clone();
        let app = Router::new().route(
            "/repos/FrozenTear/teamspeak-admin-panel/issues",
            post(move |headers: HeaderMap, Json(body): Json<Value>| {
                let cap = cap.clone();
                async move {
                    *cap.lock().unwrap() = Some((headers, body));
                    (
                        StatusCode::CREATED,
                        Json(json!({
                            "html_url": "https://github.com/FrozenTear/teamspeak-admin-panel/issues/12",
                            "number": 12
                        })),
                    )
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0)))
            .await
            .unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let sink = GitHubIssueSink::for_test(
            "test-token",
            "FrozenTear/teamspeak-admin-panel",
            &format!("http://{addr}"),
            vec!["bug-report".into()],
        )
        .unwrap();
        let created = sink
            .create_issue(IssueDraft {
                title: "[bug-report] /logs".into(),
                body: "hello".into(),
            })
            .await
            .unwrap();
        assert_eq!(created.number, 12);
        assert_eq!(
            created.html_url,
            "https://github.com/FrozenTear/teamspeak-admin-panel/issues/12"
        );

        let (headers, body) = captured.lock().unwrap().take().unwrap();
        assert_eq!(
            headers.get("authorization").unwrap().to_str().unwrap(),
            "Bearer test-token"
        );
        assert_eq!(body["title"], "[bug-report] /logs");
        assert_eq!(body["body"], "hello");
        assert_eq!(body["labels"][0], "bug-report");
    }
}
