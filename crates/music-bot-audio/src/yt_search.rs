//! THE-933 — YouTube Data API v3 fast search path.
//!
//! Replaces the `ytsearch1:` yt-dlp organic-search path (18–30 s from
//! datacenter IPs) with a direct API call (~300 ms) when `YOUTUBE_API_KEY`
//! is present. The caller receives the watch URL and passes it to the
//! normal warm yt-dlp resolver, so the media-URL extraction path is
//! unchanged.

use std::time::Duration;

use anyhow::Context;
use serde::Deserialize;
use tracing::instrument;

const SEARCH_TIMEOUT: Duration = Duration::from_secs(5);
const SEARCH_URL: &str = "https://www.googleapis.com/youtube/v3/search";

#[derive(Deserialize)]
struct SearchResponse {
    items: Vec<SearchItem>,
}

#[derive(Deserialize)]
struct SearchItem {
    id: VideoId,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct VideoId {
    video_id: String,
}

/// Call the YouTube Data API v3 and return the first matching watch URL.
///
/// Errors propagate to the caller, which should fall back to `ytsearch1:`.
/// The 5 s timeout is ~16× the healthy API latency (~300 ms) and well
/// below the worst-case organic-search stall (~30 s).
#[instrument(skip(api_key), fields(query))]
pub async fn search_youtube_api(query: &str, api_key: &str) -> anyhow::Result<String> {
    let client = reqwest::Client::builder()
        .timeout(SEARCH_TIMEOUT)
        .build()
        .context("build reqwest client")?;

    let resp = client
        .get(SEARCH_URL)
        .query(&[
            ("part", "id"),
            ("type", "video"),
            ("maxResults", "1"),
            ("q", query),
            ("key", api_key),
        ])
        .send()
        .await
        .context("YouTube search request")?
        .error_for_status()
        .context("YouTube search HTTP error")?;

    let body: SearchResponse = resp.json().await.context("parse YouTube search response")?;

    let video_id = body
        .items
        .into_iter()
        .next()
        .map(|item| item.id.video_id)
        .context("YouTube search returned no results")?;

    Ok(format!("https://www.youtube.com/watch?v={video_id}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Verify that a well-formed API response maps to the correct watch URL.
    /// Tests JSON parsing directly — no HTTP round-trip, no external dependency.
    #[test]
    fn mock_response_yields_watch_url() {
        let payload = r#"{"items":[{"id":{"videoId":"dQw4w9WgXcQ"}}]}"#;
        let body: SearchResponse = serde_json::from_str(payload).unwrap();
        let video_id = body.items.into_iter().next().unwrap().id.video_id;
        let url = format!("https://www.youtube.com/watch?v={video_id}");
        assert_eq!(url, "https://www.youtube.com/watch?v=dQw4w9WgXcQ");
    }

    /// Verify that an empty `items` array propagates an error (no results).
    #[test]
    fn empty_items_is_error() {
        let body: SearchResponse = serde_json::from_str(r#"{"items":[]}"#).unwrap();
        let result = body
            .items
            .into_iter()
            .next()
            .map(|item| item.id.video_id)
            .ok_or_else(|| anyhow::anyhow!("no results"));
        assert!(result.is_err());
    }
}
