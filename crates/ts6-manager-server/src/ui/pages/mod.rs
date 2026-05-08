//! Page components for the operator SPA. One module per route per
//! `study-documents/ts6-manager-impl-plan.md` §3.12.
//!
//! PURA-14 ships `login` and a tiny placeholder dashboard so post-login
//! redirect has somewhere to land. The remaining 23 routes are pulled in
//! by sibling PURA-5 children.

mod dashboard_placeholder;
mod home;
mod login;
mod setup;

pub use dashboard_placeholder::DashboardPlaceholder;
pub use home::Home;
pub use login::LoginPage;
pub use setup::SetupPage;
