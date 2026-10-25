use gio::prelude::*;
use ksni::blocking::TrayMethods;
use rclone_tray::{
    backend::{self, Event, Request, Snapshot},
    config::Paths,
    ipc,
    notifications::{Notice, Notifications},
    tray::{Action, AppTray},
};
use std::{
    process::Command,
    sync::{Arc, Mutex, mpsc},
    time::Duration,
};

fn show(action: Action) {
    let result = std::env::current_exe().and_then(|exe| {
        Command::new(exe.with_file_name("rclone-tray-ui"))
            .arg(serde_json::to_string(&action).unwrap())
            .spawn()
    });
    match result {
        Ok(mut child) => {
            std::thread::spawn(move || {
                let _ = child.wait();
            });
        }
        Err(error) => {
            eprintln!("Cannot open settings (install rclone-tray-ui beside rclone-tray): {error}")
        }
    }
}

fn main() -> glib::ExitCode {
    let app = gio::Application::new(
        Some("io.github.rclone_tray.App"),
        gio::ApplicationFlags::HANDLES_COMMAND_LINE,
    );
    app.add_main_option(
        "background",
        glib::Char(0),
        glib::OptionFlags::NONE,
        glib::OptionArg::None,
        "Start without settings",
        None,
    );
    app.connect_startup(|app| {
        let result = (|| -> Result<_, String> {
            let paths = Paths::new()?;
            let config = paths.load()?;
            let state = Arc::new(Mutex::new(Snapshot {
                config: config.clone(),
                remotes: vec![],
                statuses: Default::default(),
                activity: Default::default(),
                message: "Discovering rclone remotes…".into(),
                quitting: false,
            }));
            // Lock and bind before starting mounts, so a second backend cannot own them.
            let (requests, receiver) = mpsc::channel();
            let server =
                ipc::Server::start(ipc::socket_path(&paths), state.clone(), requests.clone())
                    .map_err(|e| e.to_string())?;
            let (backend, events) = backend::start(paths, config);
            let forwarding = backend.clone();
            std::thread::spawn(move || {
                while let Ok(request) = receiver.recv() {
                    if forwarding.send(request).is_err() {
                        break;
                    }
                }
            });
            Ok((state, backend, events, server))
        })();
        let (state, requests, events, server) = match result {
            Ok(started) => started,
            Err(error) => {
                eprintln!("Cannot start Rclone Tray: {error}");
                app.quit();
                return;
            }
        };
        let show_action = gio::SimpleAction::new("show", None);
        show_action.connect_activate(|_, _| show(Action::Show));
        app.add_action(&show_action);
        let (actions, action_rx) = mpsc::channel();
        let tray = AppTray {
            state: state.lock().unwrap().clone(),
            actions,
        }
        .spawn();
        let tray = match tray {
            Ok(tray) => Some(tray),
            Err(error) => {
                eprintln!("Tray unavailable: {error}");
                show(Action::Show);
                None
            }
        };
        let application = app.clone();
        let hold = app.hold();
        let mut notifications = Notifications::default();
        glib::timeout_add_local(Duration::from_millis(100), move || {
            let _lifetime = (&hold, &server);
            while let Ok(event) = events.try_recv() {
                match event {
                    Event::State(snapshot) => {
                        for notice in notifications.update(&snapshot) {
                            notice.send(&application);
                        }
                        if let Some(tray) = &tray {
                            tray.update(|t| t.state = snapshot.clone());
                        }
                        *state.lock().unwrap() = snapshot;
                    }
                    Event::Problem(problem) => Notice::problem(&problem).send(&application),
                    Event::Quit => {
                        if let Some(tray) = &tray {
                            let _ = tray.shutdown();
                        }
                        application.quit();
                        return glib::ControlFlow::Break;
                    }
                }
            }
            while let Ok(action) = action_rx.try_recv() {
                match action {
                    Action::Mount(id) => {
                        let _ = requests.send(Request::Mount(id));
                    }
                    Action::Open(id) => {
                        if let Some(profile) = state
                            .lock()
                            .unwrap()
                            .config
                            .profiles
                            .iter()
                            .find(|p| p.id == id)
                            && let Err(error) = gio::AppInfo::launch_default_for_uri(
                                &gio::File::for_path(&profile.mountpoint).uri(),
                                gio::AppLaunchContext::NONE,
                            )
                        {
                            Notice::problem(&error.to_string()).send(&application);
                        }
                    }
                    action => show(action), // Confirmation dialogs and queues belong to the on-demand UI.
                }
            }
            glib::ControlFlow::Continue
        });
    });
    app.connect_command_line(|app, command| {
        if !app.has_action("show") {
            return glib::ExitCode::FAILURE;
        }
        if !command.options_dict().contains("background") {
            show(Action::Show);
        }
        glib::ExitCode::SUCCESS
    });
    app.run()
}
