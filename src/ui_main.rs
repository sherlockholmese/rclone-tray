use rclone_tray::{backend, config, control, ipc, tray};
mod style;
mod ui;
use gtk::{gio, glib, prelude::*};

fn main() -> glib::ExitCode {
    let app = gtk::Application::builder()
        .application_id("io.github.rclone_tray.Settings")
        .flags(gio::ApplicationFlags::HANDLES_COMMAND_LINE)
        .build();
    app.connect_startup(|_| gtk::Window::set_default_icon_name("drive-harddisk"));
    app.connect_activate(|app| {
        if let Some(window) = app.windows().first() {
            window.present();
            return;
        }
        let result = config::Paths::new()
            .and_then(|paths| ipc::snapshot(&ipc::socket_path(&paths)).map(|state| (paths, state)));
        match result {
            Ok((paths, state)) => ui::build(app, paths, state),
            Err(error) => {
                let window = gtk::ApplicationWindow::builder()
                    .application(app)
                    .title("Rclone Tray — connection error")
                    .default_width(520)
                    .default_height(180)
                    .build();
                let label = gtk::Label::new(Some(&format!("{error}\n\nStart rclone-tray first.")));
                label.set_wrap(true);
                label.set_margin_start(24);
                label.set_margin_end(24);
                window.set_child(Some(&label));
                window.present();
            }
        }
    });
    app.connect_command_line(|app, command| {
        app.activate();
        if let Some(action) = command.arguments().get(1).and_then(|arg| arg.to_str()) {
            app.activate_action("dispatch", Some(&action.to_variant()));
        }
        glib::ExitCode::SUCCESS
    });
    app.run()
}
