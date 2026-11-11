//! On-demand settings window. The tray backend keeps running when this process exits.
use gpui_kit::{App, KeyBinding};
use rclone_tray::{config::Paths, ipc, tray::Action};

mod editor;
mod style;
mod ui;

fn main() {
    let action = std::env::args()
        .nth(1)
        .and_then(|arg| serde_json::from_str::<Action>(&arg).ok())
        .unwrap_or(Action::Show);
    let paths = match Paths::new() {
        Ok(paths) => paths,
        Err(error) => {
            eprintln!("Cannot open Rclone Tray: {error}");
            std::process::exit(1);
        }
    };
    let message = serde_json::to_string(&action).unwrap_or_default();
    let incoming = match ipc::claim_ui(&paths, &message) {
        Ok(ipc::Instance::Forwarded) => return,
        Ok(ipc::Instance::Primary(requests, guard)) => Some((requests, guard)),
        Err(error) => {
            eprintln!("Settings window is not single-instance this time: {error}");
            None
        }
    };
    gpui_kit::application()
        .with_assets(gpui_kit::assets::Assets)
        .run(move |cx: &mut App| {
            gpui_kit::init(cx);
            ui::init(cx);
            cx.bind_keys([
                KeyBinding::new("ctrl-n", ui::AddMount, Some(ui::CONTEXT)),
                KeyBinding::new("f5", ui::Refresh, Some(ui::CONTEXT)),
                KeyBinding::new("ctrl-q", ui::Quit, Some(ui::CONTEXT)),
                KeyBinding::new("escape", ui::Back, Some(ui::CONTEXT)),
                KeyBinding::new("alt-left", ui::Back, Some(ui::CONTEXT)),
            ]);
            cx.on_window_closed(|cx, _| {
                if cx.windows().is_empty() {
                    cx.quit();
                }
            })
            .detach();
            ui::open(cx, paths, incoming, action);
        });
}
