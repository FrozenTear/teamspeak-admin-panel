//! `/` route — placeholder dashboard.
//!
//! Real dashboard data depends on `GET /api/servers/:configId/vs/:sid/dashboard`
//! (§7.19) which is not yet built; a separate PURA-5 child wires that endpoint
//! to live channel/client counts. For now the page renders a friendly empty
//! state inside the AppShell so the operator sees the chrome and the post-
//! login redirect has a target. Auth gating + logout live in `AppShell` /
//! `Header`; this component is rendered only when a session exists.

use dioxus::prelude::*;

use crate::client::dioxus::use_session;
use crate::client::store::AuthState;
use crate::ui::components::{Banner, BannerVariant};

#[component]
pub fn DashboardPlaceholder() -> Element {
    let session = use_session();
    let user = match &*session.state.read() {
        AuthState::Authenticated { user, .. } => user.clone(),
        // AppShell already redirects on Anonymous; render nothing as a guard
        // for the brief frame between state change and effect firing.
        AuthState::Anonymous => return rsx! { "" },
    };

    rsx! {
        div { class: "crumb", "Dashboard" }
        h1 { "Welcome, {user.display_name}" }
        section { class: "stack-md",
            Banner { variant: BannerVariant::Info, title: "Dashboard placeholder",
                "Live dashboard data lands in a follow-up PURA-5 child once the "
                "`/api/servers/:configId/vs/:sid/dashboard` endpoint is staffed. "
                "Until then the chrome and authentication paths are testable."
            }
            p { "Signed in as @{user.username} ({user.role})." }
        }
    }
}
