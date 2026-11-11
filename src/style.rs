//! The settings window's design roles. Colors come from the active shadcn-style theme, so
//! light and dark stay in step; screens use these roles instead of raw values.
use gpui_kit::component::ActiveTheme as _;
use gpui_kit::{App, Hsla, Pixels, px};
use rclone_tray::view::Tone;

/// Horizontal inset shared by headers, rows, tabs, and panes: one strong left edge.
pub const INSET: Pixels = px(16.);
/// Height of a mount row. Fixed, so status changes never move neighbouring rows.
pub const MOUNT_ROW: Pixels = px(60.);
/// Height of a queue row in the virtualized transfer lists.
pub const QUEUE_ROW: Pixels = px(52.);
/// The narrowest a mount's name gets in its row. Live facts give way before crowding it further.
pub const NAME_MIN: Pixels = px(96.);
/// The information-bearing progress rail along a row's lower edge.
pub const RAIL: Pixels = px(2.);

pub fn muted(cx: &App) -> Hsla {
    cx.theme().muted_foreground
}

pub fn rule(cx: &App) -> Hsla {
    cx.theme().border
}

pub fn hover(cx: &App) -> Hsla {
    cx.theme().list_hover
}

pub fn rail(cx: &App) -> Hsla {
    cx.theme().primary
}

/// Text color for a status. Quiet states stay neutral; only exceptions take a hue.
pub fn tone(tone: Tone, cx: &App) -> Hsla {
    match tone {
        Tone::Idle | Tone::Ok | Tone::Busy => cx.theme().muted_foreground,
        Tone::Warning => cx.theme().warning,
        Tone::Danger => cx.theme().danger,
    }
}

/// Paths keep their meaningful end visible: `…/Cloud/Work`, not `/home/you/Clo…`.
pub fn path<E: gpui_kit::Styled>(element: E) -> E {
    element
        .overflow_hidden()
        .whitespace_nowrap()
        .text_overflow(gpui_kit::TextOverflow::TruncateStart("…".into()))
}
