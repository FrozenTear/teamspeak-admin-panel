//! Compile-time mirrors of the design tokens defined in `assets/tokens.css`.
//!
//! Per `study-documents/design-system/tokens.md` §10, these constants exist for
//! Rust-side calculations (canvas math, server-rendered SVG, layout breakpoints
//! consumed by `cfg!`-style code paths). All visual styling consumes the CSS
//! custom properties — never these constants — so this module is intentionally
//! a small, hand-curated subset, not an exhaustive mirror.

/// 4-pt spacing scale (px).
pub const SPACE_0: u32 = 0;
pub const SPACE_1: u32 = 2;
pub const SPACE_2: u32 = 4;
pub const SPACE_3: u32 = 8;
pub const SPACE_4: u32 = 12;
pub const SPACE_5: u32 = 16;
pub const SPACE_6: u32 = 24;
pub const SPACE_7: u32 = 32;
pub const SPACE_8: u32 = 48;
pub const SPACE_9: u32 = 64;
pub const SPACE_10: u32 = 96;

/// Border-radius scale (px).
pub const RADIUS_SM: u32 = 2;
pub const RADIUS_MD: u32 = 6;
pub const RADIUS_LG: u32 = 10;
pub const RADIUS_XL: u32 = 16;

/// Motion durations (ms).
pub const MOTION_INSTANT_MS: u32 = 0;
pub const MOTION_FAST_MS: u32 = 100;
pub const MOTION_NORMAL_MS: u32 = 180;
pub const MOTION_SLOW_MS: u32 = 280;
pub const MOTION_SLOWER_MS: u32 = 420;

/// Layout primitives (px).
pub const SIDEBAR_WIDTH_EXPANDED: u32 = 240;
pub const SIDEBAR_WIDTH_COLLAPSED: u32 = 64;
pub const HEADER_HEIGHT: u32 = 56;
pub const CONTENT_MAX_WIDTH: u32 = 1280;
pub const TOUCH_MIN: u32 = 40;
pub const TOUCH_COMFORTABLE: u32 = 44;

/// Responsive breakpoints (px). Mirrors `--bp-*` from tokens.md §9.
pub const BREAKPOINT_SM: u32 = 640;
pub const BREAKPOINT_MD: u32 = 768;
pub const BREAKPOINT_LG: u32 = 1024;
pub const BREAKPOINT_XL: u32 = 1280;
pub const BREAKPOINT_2XL: u32 = 1600;
