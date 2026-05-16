//! Server-rendered public report / appeal web forms — `GET|POST
//! /moderation/{report,appeal}` (PURA-309, workstream `9.2-public-form`
//! of [PURA-269](/PURA/issues/PURA-269) §9).
//!
//! These are the **human-facing** half of the Phase 9.2 surface: plain
//! HTML pages a reporter or an appealing subject actually opens. The
//! `/api/public/moderation/*` JSON handlers ([`super::reports`],
//! [`super::appeals`]) own the logic; this module is a thin presentation
//! layer that renders a form and re-submits into those handlers.
//!
//! ## Why server-rendered (not the Dioxus SPA)
//!
//! The brief (§7, §10 Q2) cuts the public form as a server-rendered page:
//! the SPA's multi-megabyte WASM bundle is wasted on a four-field form,
//! and every byte of client code is attack surface on an *unauthenticated*
//! page. So each page is one HTML document with an inline `<style>` and a
//! native `<form method="post">` — **no JavaScript at all**. That keeps
//! the page inside a strict CSP (`script-src` needs no nonce here;
//! `style-src` permits the inline sheet; `form-action 'self'` permits the
//! native POST) and means the form works with scripting disabled.
//!
//! ## Routes
//!
//! - `GET  /moderation/report` — the report form. The poke-delivered link
//!   carries `?token=…&serverConfigId=…&virtualServerId=…`; without a
//!   token it shows the "request a report link" step.
//! - `POST /moderation/report` — branches on the `token` field: empty ⇒
//!   request-report-link, present ⇒ file the report.
//! - `GET  /moderation/appeal?token=…` — server-renders the redacted case
//!   view plus the appeal form.
//! - `POST /moderation/appeal` — files the appeal.
//!
//! CAPTCHA / proof-of-work is a per-server config stub
//! ([`super::FLAG_CAPTCHA_ENABLED`]): when enabled the forms render a
//! verification placeholder, but the challenge itself is deferred per plan
//! §7 — it is not enforced.

use axum::Form;
use axum::extract::{Extension, Query, State};
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Response};
use serde::Deserialize;
use ts6_manager_shared::moderation as wire;

use crate::app_state::AppState;

use super::appeals::{self, TokenQuery};
use super::reports;
use super::{
    ClientIp, FLAG_APPEALS_ENABLED, FLAG_CAPTCHA_ENABLED, FLAG_REPORTS_ENABLED, MAX_TEXT_LEN,
    flag_enabled,
};

// ---------------------------------------------------------------------
// Request shapes — query strings on the GET pages, form bodies on POST
// ---------------------------------------------------------------------

/// Query string on `GET /moderation/report`. Every field is optional: the
/// stable report URL may be opened with no context (info page), with the
/// server scope only (request-link step), or with all three carried by the
/// poke-delivered link (the report form itself).
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct ReportFormQuery {
    #[serde(default)]
    token: Option<String>,
    #[serde(default)]
    server_config_id: Option<i64>,
    #[serde(default)]
    virtual_server_id: Option<i64>,
}

/// Query string on `GET /moderation/appeal`.
#[derive(Debug, Default, Deserialize)]
pub(super) struct AppealFormQuery {
    #[serde(default)]
    token: Option<String>,
}

/// `POST /moderation/report` form body. An empty `token` is the
/// request-report-link step; a non-empty `token` is a report submission.
/// The hidden `server_config_id` / `virtual_server_id` are always present
/// (rendered into both form variants).
#[derive(Debug, Deserialize)]
pub(super) struct ReportSubmitForm {
    #[serde(default)]
    token: String,
    server_config_id: i64,
    virtual_server_id: i64,
    #[serde(default)]
    uid: String,
    #[serde(default)]
    subject: String,
    #[serde(default)]
    category: String,
    #[serde(default)]
    statement: String,
    #[serde(default)]
    evidence_url: String,
}

/// `POST /moderation/appeal` form body.
#[derive(Debug, Deserialize)]
pub(super) struct AppealSubmitForm {
    #[serde(default)]
    token: String,
    #[serde(default)]
    statement: String,
}

// ---------------------------------------------------------------------
// Report form
// ---------------------------------------------------------------------

/// `GET /moderation/report` — the report form page.
pub(super) async fn report_form(
    State(state): State<AppState>,
    Query(q): Query<ReportFormQuery>,
) -> Response {
    if !flag_enabled(&state, FLAG_REPORTS_ENABLED).await {
        return not_found_page();
    }
    let captcha = flag_enabled(&state, FLAG_CAPTCHA_ENABLED).await;

    match (q.token, q.server_config_id, q.virtual_server_id) {
        // The poke-delivered link: token + server scope ⇒ the report form.
        (Some(token), Some(scid), Some(vsid)) if !token.is_empty() => html_page(
            StatusCode::OK,
            "File a report",
            &render_report_form(&token, scid, vsid, captcha),
        ),
        // Server scope but no token ⇒ the request-a-link step.
        (_, Some(scid), Some(vsid)) => html_page(
            StatusCode::OK,
            "Request a report link",
            &render_request_link_form(scid, vsid, captcha),
        ),
        // No context at all — explain how to reach a usable form.
        _ => html_page(
            StatusCode::OK,
            "Report a user",
            "<div class=\"notice\"><p>To file a report, open the report link \
             for your TeamSpeak server. Your server operator publishes that \
             link, or you can request it from within TeamSpeak while \
             connected.</p></div>",
        ),
    }
}

/// `POST /moderation/report` — request a report link, or file a report.
pub(super) async fn report_submit(
    State(state): State<AppState>,
    Extension(ClientIp(client_ip)): Extension<ClientIp>,
    Form(f): Form<ReportSubmitForm>,
) -> Response {
    if !flag_enabled(&state, FLAG_REPORTS_ENABLED).await {
        return not_found_page();
    }

    if f.token.trim().is_empty() {
        // --- request-report-link step --------------------------------
        let back = Back::report_request(f.server_config_id, f.virtual_server_id);
        let req = wire::RequestReportLinkRequest {
            server_config_id: f.server_config_id,
            virtual_server_id: f.virtual_server_id,
            uid: f.uid.trim().to_string(),
        };
        match reports::request_report_link(State(state), axum::Json(req)).await {
            Ok(_) => success_page(
                "Check your TeamSpeak client",
                "<p>A single-use report link has been sent to the TeamSpeak \
                 client holding that Unique ID. Open it to continue.</p>\
                 <p class=\"muted\">The link is valid for 15 minutes. You must \
                 be connected to the server to receive it.</p>",
            ),
            Err(resp) => error_page(
                resp.status(),
                "That TeamSpeak client is not connected. Open this form while \
                 connected to the server, then request the link again.",
                Some(back),
            ),
        }
    } else {
        // --- file-the-report step ------------------------------------
        let back = Back::report_form(&f.token, f.server_config_id, f.virtual_server_id);
        let evidence = f.evidence_url.trim();
        let req = wire::PublicReportRequest {
            token: f.token.clone(),
            server_config_id: f.server_config_id,
            virtual_server_id: f.virtual_server_id,
            subject_uid_or_nickname: f.subject,
            category: f.category,
            statement: f.statement,
            evidence_url: (!evidence.is_empty()).then(|| evidence.to_string()),
        };
        match reports::submit(
            State(state),
            Extension(ClientIp(client_ip)),
            axum::Json(req),
        )
        .await
        {
            Ok((_, axum::Json(accepted))) => success_page(
                "Report submitted",
                &format!(
                    "<p>Your report has been submitted to the server's \
                     moderators (reference #{}).</p><p class=\"muted\">A \
                     moderator will review it. No further action is needed \
                     from you.</p>",
                    accepted.id
                ),
            ),
            Err(resp) => error_page(resp.status(), "This report could not be filed.", Some(back)),
        }
    }
}

// ---------------------------------------------------------------------
// Appeal form
// ---------------------------------------------------------------------

/// `GET /moderation/appeal?token=…` — the redacted case view + appeal form.
pub(super) async fn appeal_form(
    State(state): State<AppState>,
    Query(q): Query<AppealFormQuery>,
) -> Response {
    if !flag_enabled(&state, FLAG_APPEALS_ENABLED).await {
        return not_found_page();
    }
    let captcha = flag_enabled(&state, FLAG_CAPTCHA_ENABLED).await;

    let token = match q.token {
        Some(t) if !t.is_empty() => t,
        _ => {
            return html_page(
                StatusCode::OK,
                "Appeal a moderation action",
                "<div class=\"notice\"><p>To appeal a moderation action, open \
                 the appeal link included in the message you received when the \
                 action was taken.</p></div>",
            );
        }
    };

    match appeals::view_redacted_case(
        State(state),
        Query(TokenQuery {
            token: token.clone(),
        }),
    )
    .await
    {
        Ok(axum::Json(case)) => html_page(
            StatusCode::OK,
            "Appeal a moderation action",
            &render_appeal_page(&token, &case, captcha),
        ),
        Err(resp) => error_page(resp.status(), "This case cannot be viewed.", None),
    }
}

/// `POST /moderation/appeal` — file an appeal.
pub(super) async fn appeal_submit(
    State(state): State<AppState>,
    Extension(ClientIp(client_ip)): Extension<ClientIp>,
    Form(f): Form<AppealSubmitForm>,
) -> Response {
    if !flag_enabled(&state, FLAG_APPEALS_ENABLED).await {
        return not_found_page();
    }
    let back = Back::appeal_form(&f.token);
    let req = wire::PublicAppealRequest {
        token: f.token.clone(),
        statement: f.statement,
    };
    match appeals::submit(
        State(state),
        Extension(ClientIp(client_ip)),
        axum::Json(req),
    )
    .await
    {
        Ok((_, axum::Json(accepted))) => success_page(
            "Appeal submitted",
            &format!(
                "<p>Your appeal has been submitted (reference #{}).</p>\
                 <p class=\"muted\">A moderator will review it and decide \
                 whether to uphold or overturn the action.</p>",
                accepted.id
            ),
        ),
        Err(resp) => error_page(
            resp.status(),
            "An appeal is already on file for this case, or the case is no \
             longer in an appealable state.",
            Some(back),
        ),
    }
}

// ---------------------------------------------------------------------
// HTML rendering
// ---------------------------------------------------------------------

/// Minimal inline stylesheet. Inline `<style>` is permitted by the
/// `style-src 'unsafe-inline'` CSP directive ([`crate::web::csp_nonce`]);
/// the page loads no external CSS so it is fully self-contained.
const STYLE: &str = "\
*{box-sizing:border-box}\
body{margin:0;font-family:system-ui,-apple-system,'Segoe UI',Roboto,sans-serif;\
background:#0f1115;color:#e6e8ec;line-height:1.55}\
.wrap{max-width:34rem;margin:0 auto;padding:2.5rem 1.25rem}\
h1{font-size:1.4rem;margin:0 0 1rem}h2{font-size:1.05rem;margin:1.6rem 0 .5rem}\
p{margin:.6rem 0}\
.card{background:#1a1d24;border:1px solid #2b2f3a;border-radius:.6rem;padding:1.25rem;margin:1rem 0}\
label{display:block;font-weight:600;margin:.9rem 0 .3rem;font-size:.9rem}\
input,select,textarea{width:100%;padding:.55rem .65rem;background:#0f1115;\
color:#e6e8ec;border:1px solid #2b2f3a;border-radius:.4rem;font:inherit}\
textarea{min-height:8rem;resize:vertical}\
input:focus,select:focus,textarea:focus{outline:2px solid #4c8dff;outline-offset:1px}\
button{margin-top:1.2rem;padding:.6rem 1.1rem;background:#4c8dff;color:#fff;\
border:0;border-radius:.4rem;font:inherit;font-weight:600;cursor:pointer}\
button:hover{background:#3b7af0}\
.muted{color:#9aa0ab;font-size:.88rem}\
.notice{background:#22262f;border-left:3px solid #4c8dff;padding:.7rem .9rem;\
border-radius:.3rem;margin:1rem 0}\
.notice.err{border-left-color:#ff6b6b}.notice.ok{border-left-color:#3ecf8e}\
.timeline{list-style:none;padding:0;margin:.5rem 0}\
.timeline li{border:1px solid #2b2f3a;border-radius:.4rem;padding:.6rem .8rem;margin:.5rem 0}\
.kind{font-weight:700}.when{color:#9aa0ab;font-size:.82rem}\
a{color:#4c8dff}";

/// Wrap page `body` HTML in the shared document scaffold. `title` is a
/// trusted static string; `body` must already be escaped by its builder.
fn page(title: &str, body: &str) -> String {
    format!(
        "<!DOCTYPE html><html lang=\"en\"><head><meta charset=\"utf-8\">\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\
         <meta name=\"robots\" content=\"noindex, nofollow\">\
         <title>{title}</title><style>{STYLE}</style></head>\
         <body><main class=\"wrap\"><h1>{title}</h1>{body}</main></body></html>"
    )
}

/// Build an HTML [`Response`] with `status`.
fn html_page(status: StatusCode, title: &str, body: &str) -> Response {
    (status, Html(page(title, body))).into_response()
}

/// `404` — used both for a disabled kill-switch (must be indistinguishable
/// from a route that does not exist, brief §4.1) and a genuinely missing
/// page.
fn not_found_page() -> Response {
    html_page(
        StatusCode::NOT_FOUND,
        "Not found",
        "<div class=\"notice\"><p>This page is not available.</p></div>",
    )
}

/// Success page — a green-flagged confirmation notice.
fn success_page(title: &str, detail_html: &str) -> Response {
    html_page(
        StatusCode::OK,
        title,
        &format!("<div class=\"notice ok\">{detail_html}</div>"),
    )
}

/// Error page keyed on the upstream handler's status code. `conflict_msg`
/// is the flow-specific text for a `409` (its meaning differs per form);
/// it is ignored for every other status. `back`, when set, offers a link
/// to retry the form without losing the URL context.
fn error_page(status: StatusCode, conflict_msg: &str, back: Option<Back>) -> Response {
    let (heading, detail): (&str, &str) = match status {
        StatusCode::BAD_REQUEST => (
            "Submission rejected",
            "We couldn't accept your submission. Go back, check the form, and try again.",
        ),
        StatusCode::FORBIDDEN => (
            "Invalid or expired link",
            "This link is invalid or has expired. These links are single-use — request a new one.",
        ),
        StatusCode::NOT_FOUND => ("Not available", "This page is not available."),
        StatusCode::CONFLICT => ("Couldn't continue", conflict_msg),
        StatusCode::TOO_MANY_REQUESTS => (
            "Too many requests",
            "You've made too many requests. Please wait a while before trying again.",
        ),
        StatusCode::BAD_GATEWAY => (
            "Server unreachable",
            "The TeamSpeak server could not be reached right now. Please try again shortly.",
        ),
        _ => (
            "Something went wrong",
            "An unexpected error occurred. Please try again later.",
        ),
    };
    let mut body = format!("<div class=\"notice err\"><p>{}</p></div>", esc(detail));
    if let Some(b) = back {
        body.push_str(&format!(
            "<p><a href=\"{}\">{}</a></p>",
            esc(&b.href),
            esc(b.label)
        ));
    }
    html_page(status, heading, &body)
}

/// A retry link back to the form the user just submitted.
struct Back {
    href: String,
    label: &'static str,
}

impl Back {
    fn report_request(scid: i64, vsid: i64) -> Self {
        Back {
            href: format!("/moderation/report?serverConfigId={scid}&virtualServerId={vsid}"),
            label: "← Back to the form",
        }
    }
    fn report_form(token: &str, scid: i64, vsid: i64) -> Self {
        Back {
            href: format!(
                "/moderation/report?token={token}&serverConfigId={scid}&virtualServerId={vsid}"
            ),
            label: "← Back to the report form",
        }
    }
    fn appeal_form(token: &str) -> Self {
        Back {
            href: format!("/moderation/appeal?token={token}"),
            label: "← Back to the appeal form",
        }
    }
}

/// The CAPTCHA / proof-of-work placeholder (brief §4.6). Rendered only
/// when [`FLAG_CAPTCHA_ENABLED`] is on; the challenge itself is deferred
/// per plan §7, so this is a visible stub, not an enforced gate.
fn captcha_block(enabled: bool) -> &'static str {
    if enabled {
        "<div class=\"notice\" role=\"note\"><strong>Verification</strong>\
         <p class=\"muted\">This server has additional verification enabled. \
         The proof-of-work challenge is not yet available — if your \
         submission is not accepted, contact the server operator.</p></div>"
    } else {
        ""
    }
}

/// The request-a-report-link step (no token yet).
fn render_request_link_form(scid: i64, vsid: i64, captcha: bool) -> String {
    format!(
        "<p>To file a report we send a single-use link to your TeamSpeak \
         client. Enter the Unique ID of your current connection.</p>\
         <form method=\"post\" action=\"/moderation/report\" class=\"card\">\
         <input type=\"hidden\" name=\"token\" value=\"\">\
         <input type=\"hidden\" name=\"server_config_id\" value=\"{scid}\">\
         <input type=\"hidden\" name=\"virtual_server_id\" value=\"{vsid}\">\
         <label for=\"uid\">Your TeamSpeak Unique ID</label>\
         <input id=\"uid\" name=\"uid\" type=\"text\" maxlength=\"64\" required \
         autocomplete=\"off\" placeholder=\"e.g. abcdEFGH1234...=\">\
         <p class=\"muted\">You must be connected to the server — the link is \
         delivered to your client by the server, never shown here.</p>\
         {captcha}<button type=\"submit\">Send me a report link</button></form>",
        captcha = captcha_block(captcha),
    )
}

/// The report form proper (a verified token is present).
fn render_report_form(token: &str, scid: i64, vsid: i64, captcha: bool) -> String {
    let mut options = String::new();
    for &c in reports::CATEGORIES {
        options.push_str(&format!(
            "<option value=\"{}\">{}</option>",
            esc(c),
            esc(&title_case(c))
        ));
    }
    format!(
        "<p>Your identity is confirmed by the link you opened. Tell the \
         moderators what happened — plain text only.</p>\
         <form method=\"post\" action=\"/moderation/report\" class=\"card\">\
         <input type=\"hidden\" name=\"token\" value=\"{token}\">\
         <input type=\"hidden\" name=\"server_config_id\" value=\"{scid}\">\
         <input type=\"hidden\" name=\"virtual_server_id\" value=\"{vsid}\">\
         <label for=\"subject\">Who are you reporting?</label>\
         <input id=\"subject\" name=\"subject\" type=\"text\" maxlength=\"{maxlen}\" \
         required placeholder=\"TeamSpeak Unique ID or nickname\">\
         <label for=\"category\">Category</label>\
         <select id=\"category\" name=\"category\" required>{options}</select>\
         <label for=\"statement\">What happened?</label>\
         <textarea id=\"statement\" name=\"statement\" maxlength=\"{maxlen}\" \
         required placeholder=\"Describe the issue.\"></textarea>\
         <label for=\"evidence_url\">Evidence link <span class=\"muted\">\
         (optional)</span></label>\
         <input id=\"evidence_url\" name=\"evidence_url\" type=\"url\" \
         maxlength=\"{urllen}\" placeholder=\"https://...\">\
         {captcha}<button type=\"submit\">Submit report</button></form>",
        token = esc(token),
        maxlen = MAX_TEXT_LEN,
        urllen = reports::MAX_URL_LEN,
        captcha = captcha_block(captcha),
    )
}

/// The appeal page: the redacted case view, then the appeal form (or a
/// notice when the case is no longer appealable).
fn render_appeal_page(token: &str, case: &wire::RedactedCase, captcha: bool) -> String {
    let mut timeline = String::new();
    for a in &case.timeline {
        timeline.push_str(&format!(
            "<li><span class=\"kind\">{}</span> — {}<br>\
             <span class=\"when\">{}</span></li>",
            esc(&title_case(&a.action_kind)),
            esc(&a.reason),
            esc(&a.created_at.format("%Y-%m-%d %H:%M UTC").to_string()),
        ));
    }
    if timeline.is_empty() {
        timeline = "<li class=\"muted\">No actions recorded.</li>".to_string();
    }

    let mut html = format!(
        "<p class=\"muted\">Case #{case_id} &middot; opened {opened}</p>\
         <div class=\"card\"><h2>Action taken</h2><p>{reason}</p>\
         <h2>Timeline</h2><ul class=\"timeline\">{timeline}</ul></div>",
        case_id = case.case_id,
        opened = esc(&case.opened_at.format("%Y-%m-%d %H:%M UTC").to_string()),
        reason = esc(&case.reason),
    );

    if case.appealable {
        html.push_str(&format!(
            "<h2>Submit an appeal</h2>\
             <p>If you believe this action was a mistake, explain why below. \
             You may appeal a case once.</p>\
             <form method=\"post\" action=\"/moderation/appeal\" class=\"card\">\
             <input type=\"hidden\" name=\"token\" value=\"{token}\">\
             <label for=\"statement\">Why should this action be \
             reconsidered?</label>\
             <textarea id=\"statement\" name=\"statement\" maxlength=\"{maxlen}\" \
             required placeholder=\"Explain your appeal. Plain text only.\">\
             </textarea>{captcha}\
             <button type=\"submit\">Submit appeal</button></form>",
            token = esc(token),
            maxlen = MAX_TEXT_LEN,
            captcha = captcha_block(captcha),
        ));
    } else {
        html.push_str(
            "<div class=\"notice\"><p>This case can no longer be appealed — \
             an appeal is already on file, or the case has been closed.</p>\
             </div>",
        );
    }
    html
}

/// HTML-escape a string for safe interpolation into element text or a
/// double-quoted attribute. External text (case reasons, action kinds)
/// MUST pass through here before it reaches the page (brief §6 hook 5).
fn esc(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 8);
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}

/// Capitalise the first character and replace `_` with a space — turns a
/// wire token like `ban_ip` into the display label `Ban ip`.
fn title_case(s: &str) -> String {
    let spaced = s.replace('_', " ");
    let mut chars = spaced.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}
