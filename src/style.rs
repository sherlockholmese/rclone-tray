//! The settings window's design roles. Colors come from the active shadcn-style theme, so
//! light and dark stay in step; screens use these roles instead of raw values.
use gpui_kit::component::ActiveTheme as _;
use gpui_kit::{App, Hsla, Pixels, px};
use rclone_tray::view::Tone;

/// Horizontal inset shared by headers, rows, tabs, and panes: one strong left edge.
pub const INSET: Pixels = px(16.);
/// Width of the mount sidebar, shared by its title-bar segment so the two read as one column.
pub const SIDEBAR: Pixels = px(224.);
/// Left edge of the sidebar's icons. The wordmark above them starts on the same line.
pub const SIDEBAR_INSET: Pixels = px(20.);
/// Height of a queue row in the virtualized transfer lists.
pub const QUEUE_ROW: Pixels = px(52.);
/// Space between the window edge and the lifted content plate.
pub const PLATE_GAP: Pixels = px(8.);
/// The information-bearing progress rail along a row's lower edge.
pub const RAIL: Pixels = px(2.);

/// The window behind the content plate. The sidebar sits directly on it.
pub fn canvas(cx: &App) -> Hsla {
    cx.theme().sidebar
}

/// The lifted content plate. In light mode white on the off-white canvas, carried by its
/// shadow. In dark mode shadows vanish, so the plate steps up in lightness instead.
pub fn plate(cx: &App) -> Hsla {
    let theme = cx.theme();
    if theme.is_dark() {
        theme.background.blend(theme.foreground.opacity(0.04))
    } else {
        theme.background
    }
}

pub fn muted(cx: &App) -> Hsla {
    cx.theme().muted_foreground
}

pub fn rule(cx: &App) -> Hsla {
    cx.theme().border
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
