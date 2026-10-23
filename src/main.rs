mod backend;
mod config;
mod control;
mod style;
mod tray;
mod ui;

use gtk::prelude::*;

fn main() -> gtk::glib::ExitCode {
    let background = std::env::args().any(|a| a == "--background");
    let app = gtk::Application::builder()
        .application_id("io.github.rclone_tray.App")
        .build();
    app.connect_startup(|_| gtk::Window::set_default_icon_name("drive-harddisk"));
    app.connect_activate(move |app| {
        if let Some(window) = app.windows().first() { window.present(); return; }
        let result = config::Paths::new().and_then(|paths| paths.load().map(|config| (paths, config)));
        match result {
            Ok((paths, config)) => ui::build(app, paths, config, background),
            Err(error) => {
                let window = gtk::ApplicationWindow::builder().application(app).title("Rclone Tray — configuration error")
                    .default_width(520).default_height(180).build();
                let label = gtk::Label::new(Some(&format!("{error}\n\nThe configuration has not been changed. Fix it and restart the application.")));
                label.set_wrap(true); label.set_selectable(true); label.set_margin_start(24); label.set_margin_end(24);
                window.set_child(Some(&label)); window.present();
            }
        }
    });
    // --background is ours, not a GTK command-line option.
    app.run_with_args::<&str>(&[])
}
