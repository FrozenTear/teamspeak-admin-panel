//! Catch-all 404 surface for unknown URLs.
//!
//! Wired into the [`Route`](crate::ui::routes::Route) enum as the final
//! variant with `#[route("/:..segments")]` so any path the router cannot
//! otherwise match lands here instead of throwing a `ParseRouteError`. Per
//! [PURA-213](/PURA/issues/PURA-213), the raw `Routable` parse-error string
//! was leaking through the default dioxus-core error path on the production
//! image — release builds do not suppress it, the only contract that
//! prevents the leak is an explicit catch-all in the route table.
//!
//! Lives outside `AppShell` so an anonymous visitor who typos a URL gets
//! a friendly page instead of an auth bounce to `/login?next=<bad-path>`.
//! Authenticated operators land here just the same; the "Back to home" CTA
//! routes them to `/` which `Home` redirects to `/dashboard`, so the
//! chrome is one click away regardless of session.

use dioxus::prelude::*;

#[component]
pub fn NotFoundPage(segments: Vec<String>) -> Element {
    // Reconstruct the attempted path for the diagnostic line. `segments` is
    // already URL-decoded by the router; joining with `/` recovers the
    // shape the operator typed (minus query string, which is fine for a
    // human-readable nudge — the goal is "you tried /foo, here's home",
    // not exact echo).
    let attempted = if segments.is_empty() {
        "/".to_string()
    } else {
        format!("/{}", segments.join("/"))
    };

    rsx! {
        main { class: "main not-found", tabindex: "0",
            div { class: "empty",
                div { class: "icon", "?" }
                h3 { "Page not found" }
                p {
                    "We could not find a page at "
                    code { "{attempted}" }
                    ". The link may be out of date, or the URL was typed by hand."
                }
                div { class: "actions",
                    a { class: "btn btn-primary", href: "/", "Back to dashboard" }
                }
            }
        }
    }
}
