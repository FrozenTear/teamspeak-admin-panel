//! Page components for the operator SPA. One module per route per
//! `study-documents/ts6-manager-impl-plan.md` §3.12.
//!
//! PURA-14 ships `login` and a tiny placeholder dashboard so post-login
//! redirect has somewhere to land. The remaining 23 routes are pulled in
//! by sibling PURA-5 children.

pub mod active_server;
mod admin;
mod bans;
mod channels;
mod clients;
mod dashboard_placeholder;
#[cfg(debug_assertions)]
mod dev_video_player;
mod flows;
mod home;
mod login;
mod logs;
mod music_bots;
mod not_found;
mod public_widget;
mod server_edit;
mod server_info;
mod servers_index;
mod settings;
mod setup;
mod video_sources;
mod widgets;

pub use admin::AdminUsersPage;
pub use bans::BansPage;
pub use channels::ChannelsPage;
pub use clients::ClientsPage;
pub use dashboard_placeholder::DashboardPlaceholder;
#[cfg(debug_assertions)]
pub use dev_video_player::DevVideoPlayerPage;
pub use flows::{FlowDetailPage, FlowEditPage, FlowFormPage, FlowsListPage};
pub use home::Home;
pub use login::LoginPage;
pub use logs::LogsPage;
pub use music_bots::{
    BotDetailPage, BotsIndexPage, MusicLibraryPage, MusicPlaylistsPage, RadioStationsPage,
};
pub use not_found::NotFoundPage;
pub use public_widget::PublicWidgetPage;
pub use server_edit::ServerEditPage;
pub use server_info::ServerInfoPage;
pub use servers_index::ServersIndexPage;
pub use settings::SettingsPage;
pub use setup::SetupPage;
pub use video_sources::VideoSourcesPage;
pub use widgets::WidgetsPage;
