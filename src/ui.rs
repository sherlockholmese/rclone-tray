use crate::{
    backend::{self, Event, Request, Snapshot, Status},
    config::{Config, Paths, Profile},
    control::{Activity, Transfer, byte_size},
    style::{self, CONTROL_HEIGHT, FIELD_GAP, GAP, GROUP_GAP, INSET},
    tray::{Action, AppTray},
};
use gtk::{gio, glib, prelude::*};
use ksni::blocking::TrayMethods;
use std::{cell::RefCell, collections::HashMap, path::PathBuf, rc::Rc, sync::mpsc, time::Duration};

struct MountCard {
    root: gtk::Frame,
    activity: gtk::Box,
    profile: Profile,
    status: Rc<RefCell<Status>>,
    badge: gtk::Label,
    detail: gtk::Label,
    mount: gtk::Button,
    open: gtk::Button,
    queue: gtk::Button,
    unmount: gtk::Button,
    edit: gtk::Button,
    delete: gtk::Button,
    sample: Option<Result<Activity, String>>,
}

impl MountCard {
    fn update(&mut self, status: &Status, sample: Option<&Result<Activity, String>>) {
        let changed = *self.status.borrow() != *status;
        if changed || self.sample.as_ref() != sample {
            render_activity(&self.activity, sample, status);
            self.sample = sample.cloned();
        }
        *self.status.borrow_mut() = status.clone();
        self.badge.set_text(status.label());
        self.badge.remove_css_class("error");
        self.badge.remove_css_class("rclone-hint");
        self.badge
            .add_css_class(if matches!(status, Status::Error(_)) {
                "error"
            } else {
                "rclone-hint"
            });
        let detail = match status {
            Status::Error(error) => error.as_str(),
            _ => "",
        };
        self.badge.set_tooltip_text(match status {
            Status::Recovering => {
                Some("Restoring cached uploads before the folder becomes available.")
            }
            Status::Waiting => Some("Startup has exceeded 30 seconds. View the log for details."),
            Status::External => Some("Managed by another session."),
            _ => None,
        });
        self.detail.set_text(detail);
        self.detail.set_visible(!detail.is_empty());
        self.mount.set_label(match status {
            Status::Mounted => "Unmount…",
            Status::Mounting | Status::Recovering | Status::Waiting => "Cancel mount…",
            Status::Unmounting => "Unmounting…",
            _ => "Mount",
        });
        self.mount
            .set_sensitive(!matches!(status, Status::Unmounting | Status::External));
        self.mount
            .set_visible(!matches!(status, Status::Mounted | Status::External));
        self.open
            .set_visible(matches!(status, Status::Mounted | Status::External));
        self.unmount.set_visible(*status == Status::Mounted);
        self.edit.set_visible(!status.active());
        self.delete.set_visible(!status.active());
        let queued = sample.and_then(|a| a.as_ref().ok()).and_then(|a| {
            a.uploads
                .as_ref()
                .ok()
                .map(|queue| queue.iter().filter(|item| !item.uploading).count() as u64)
                .or_else(|| a.cache.as_ref().ok().map(|cache| cache.uploads_queued))
        });
        self.queue.set_label(
            &queued
                .filter(|count| *count > 0)
                .map_or_else(|| "Queue…".into(), |count| format!("Queue ({count})…")),
        );
    }
}

pub fn build(app: &gtk::Application, paths: Paths, config: Config, background: bool) {
    style::install();
    let (requests, events) = backend::start(paths.clone(), config.clone());
    let (actions, action_rx) = mpsc::channel();
    let state = Rc::new(RefCell::new(Snapshot {
        config,
        remotes: vec![],
        statuses: Default::default(),
        activity: Default::default(),
        message: "Discovering rclone remotes…".into(),
        quitting: false,
    }));
    let window = gtk::ApplicationWindow::builder()
        .application(app)
        .title("Rclone Tray")
        .default_width(720)
        .default_height(480)
        .build();
    window.add_css_class("rclone-app");
    let header = gtk::HeaderBar::new();
    let title = gtk::Label::new(Some("Rclone Tray"));
    title.add_css_class("title");
    header.set_title_widget(Some(&title));
    let refresh = gtk::Button::from_icon_name("view-refresh-symbolic");
    refresh.set_margin_start(GAP);
    refresh.set_margin_end(GAP);
    refresh.set_tooltip_text(Some("Refresh configured rclone remotes"));
    let tx = requests.clone();
    refresh.connect_clicked(move |_| {
        let _ = tx.send(Request::Refresh);
    });
    header.pack_start(&refresh);
    let add = gtk::Button::with_label("Add mount");
    add.add_css_class("suggested-action");
    header.pack_end(&add);
    window.set_titlebar(Some(&header));

    let content = gtk::Box::new(gtk::Orientation::Vertical, GROUP_GAP);
    style::inset(&content, INSET);
    let message = gtk::Label::new(None);
    message.set_visible(false);
    message.set_wrap(true);
    message.set_xalign(0.0);
    message.set_selectable(true);
    content.append(&message);
    let list = gtk::Box::new(gtk::Orientation::Vertical, GROUP_GAP);
    let empty = gtk::Box::new(gtk::Orientation::Vertical, GAP);
    style::inset(&empty, GROUP_GAP);
    let empty_title = gtk::Label::new(Some("No mount profiles"));
    empty_title.set_xalign(0.0);
    empty_title.add_css_class("heading");
    empty.append(&empty_title);
    empty.set_visible(state.borrow().config.profiles.is_empty());
    let collection = gtk::Box::new(gtk::Orientation::Vertical, 0);
    collection.append(&empty);
    collection.append(&list);
    let scroll = gtk::ScrolledWindow::builder()
        .vexpand(true)
        .hscrollbar_policy(gtk::PolicyType::Never)
        .child(&collection)
        .build();
    content.append(&scroll);
    let startup = gtk::CheckButton::with_label("Start at login");
    startup.set_hexpand(true);
    startup.set_active(paths.autostart.exists());
    let paths_for_startup = paths.clone();
    let notice = message.clone();
    startup.connect_toggled(move |button| {
        if let Err(e) = paths_for_startup.set_autostart(button.is_active()) {
            notice.set_text(&format!("Cannot change login startup: {e}"));
            notice.set_visible(true);
        }
    });
    let footer = gtk::Box::new(gtk::Orientation::Horizontal, GAP);
    footer.append(&startup);
    let quit = gtk::Button::with_label("Quit…");
    let action_tx = actions.clone();
    quit.connect_clicked(move |_| {
        let _ = action_tx.send(Action::Quit);
    });
    footer.append(&quit);
    content.append(&footer);
    window.set_child(Some(&content));

    let editor_window = window.clone();
    let editor_state = state.clone();
    let tx = requests.clone();
    add.connect_clicked(move |_| editor(&editor_window, &editor_state.borrow(), None, tx.clone()));

    let tray = AppTray {
        state: state.borrow().clone(),
        actions: actions.clone(),
    }
    .spawn();
    let tray = match tray {
        Ok(handle) => Some(handle),
        Err(e) => {
            eprintln!("Tray unavailable: {e}. The settings window remains available.");
            None
        }
    };
    let tray_available = tray.is_some();
    window.connect_close_request(move |window| {
        // Without a tray host, retain a way to reach active mounts and Quit.
        if tray_available {
            window.set_visible(false);
        }
        glib::Propagation::Stop
    });
    if !background || !tray_available {
        window.present();
    }
    let hold = app.hold();
    let application = app.clone();
    let mut cards = HashMap::new();
    let mut queues = HashMap::<u64, QueueWindow>::new();
    glib::timeout_add_local(Duration::from_millis(100), move || {
        let _keep_alive = &hold;
        queues.retain(|_, queue| queue.window.is_visible());
        while let Ok(event) = events.try_recv() {
            match event {
                Event::State(snapshot) => {
                    message.set_text(&snapshot.message);
                    message.set_visible(!snapshot.message.is_empty());
                    quit.set_label(if snapshot.quitting {
                        "Cancel quit"
                    } else {
                        "Quit…"
                    });
                    add.set_sensitive(!snapshot.quitting);
                    refresh.set_sensitive(!snapshot.quitting);
                    empty.set_visible(snapshot.config.profiles.is_empty());
                    render(&list, &window, &snapshot, &requests, &actions, &mut cards);
                    for (&id, queue) in &queues {
                        queue.update(&snapshot, id);
                    }
                    if let Some(tray) = &tray {
                        tray.update(|t| t.state = snapshot.clone());
                    }
                    *state.borrow_mut() = snapshot;
                }
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
                Action::Show => window.present(),
                Action::Mount(id) => {
                    let _ = requests.send(Request::Mount(id));
                }
                Action::Unmount(id) => {
                    window.present();
                    let tx = requests.clone();
                    confirm(
                        &window,
                        "Unmount this profile?",
                        "Close open files first. Unmounting is blocked while uploads are pending or transfer status is unavailable. Cached files are retained.",
                        move || {
                            let _ = tx.send(Request::Unmount(id));
                        },
                    );
                }
                Action::Open(id) => {
                    if let Some(p) = state.borrow().config.profiles.iter().find(|p| p.id == id) {
                        open_uri(&gio::File::for_path(&p.mountpoint).uri(), &message);
                    }
                }
                Action::Queue(id) => {
                    let queue = queues
                        .entry(id)
                        .or_insert_with(|| QueueWindow::new(&window));
                    queue.update(&state.borrow(), id);
                    queue.window.present();
                }
                Action::Log(id) => show_log(&window, &paths, id),
                Action::Quit => {
                    window.present();
                    if state.borrow().quitting {
                        let _ = requests.send(Request::CancelQuit);
                        continue;
                    }
                    let tx = requests.clone();
                    confirm(
                        &window,
                        "Quit Rclone Tray?",
                        "Wait for uploads, unmount this app’s profiles, then quit. Close open files first. Cached files are retained.",
                        move || {
                            let _ = tx.send(Request::Quit);
                        },
                    );
                }
            }
        }
        glib::ControlFlow::Continue
    });
}

fn render(
    list: &gtk::Box,
    window: &gtk::ApplicationWindow,
    state: &Snapshot,
    requests: &mpsc::Sender<Request>,
    actions: &mpsc::Sender<Action>,
    cards: &mut HashMap<u64, MountCard>,
) {
    cards.retain(|id, card| {
        let keep = state.config.profiles.iter().any(|p| p.id == *id);
        if !keep {
            list.remove(&card.root);
        }
        keep
    });
    let mut previous: Option<gtk::Frame> = None;
    for profile in &state.config.profiles {
        let id = profile.id;
        let status = state.statuses.get(&id).unwrap_or(&Status::Unmounted);
        if let Some(card) = cards.get_mut(&id) {
            if card.profile == *profile {
                card.update(status, state.activity.get(&id));
                if card.root.prev_sibling().as_ref() != previous.as_ref().map(|p| p.upcast_ref()) {
                    list.reorder_child_after(&card.root, previous.as_ref());
                }
                previous = Some(card.root.clone());
                continue;
            }
            list.remove(&card.root);
        }
        let card = gtk::Box::new(gtk::Orientation::Vertical, GAP);
        style::inset(&card, GROUP_GAP);
        let top = gtk::Box::new(gtk::Orientation::Horizontal, GROUP_GAP);
        top.set_baseline_position(gtk::BaselinePosition::Center);
        let name = gtk::Label::new(Some(&profile.name));
        name.add_css_class("heading");
        name.set_xalign(0.0);
        name.set_valign(gtk::Align::Baseline);
        name.set_wrap(true);
        name.set_wrap_mode(gtk::pango::WrapMode::WordChar);
        name.set_hexpand(true);
        top.append(&name);
        let badge = gtk::Label::new(Some(status.label()));
        badge.set_wrap(true);
        badge.set_max_width_chars(18);
        badge.set_xalign(1.0);
        badge.set_valign(gtk::Align::Baseline);
        badge.add_css_class(if matches!(status, Status::Error(_)) {
            "error"
        } else {
            "rclone-hint"
        });
        top.append(&badge);
        let menu = gtk::MenuButton::new();
        menu.set_icon_name("view-more-symbolic");
        menu.add_css_class("flat");
        menu.set_tooltip_text(Some(&format!("Actions for {}", profile.name)));
        menu.set_valign(gtk::Align::Center);
        menu.set_size_request(CONTROL_HEIGHT, CONTROL_HEIGHT);
        let popover = gtk::Popover::new();
        menu.set_popover(Some(&popover));
        top.append(&menu);
        card.append(&top);
        let details = gtk::Label::new(Some(&format!(
            "{}:{} → {}",
            profile.remote,
            profile.folder,
            profile.mountpoint.display()
        )));
        details.set_xalign(0.0);
        details.set_wrap(true);
        details.set_wrap_mode(gtk::pango::WrapMode::WordChar);
        details.set_selectable(true);
        details.add_css_class("rclone-hint");
        card.append(&details);
        let detail = gtk::Label::new(None);
        detail.set_wrap(true);
        detail.set_xalign(0.0);
        detail.set_selectable(true);
        card.append(&detail);
        let activity = gtk::Box::new(gtk::Orientation::Vertical, GAP);
        render_activity(&activity, state.activity.get(&id), status);
        card.append(&activity);
        let buttons = gtk::Box::new(gtk::Orientation::Horizontal, GAP);
        let utilities = gtk::Box::new(gtk::Orientation::Vertical, FIELD_GAP);
        style::inset(&utilities, GAP);
        popover.set_child(Some(&utilities));
        let live_status = Rc::new(RefCell::new(status.clone()));
        let mount = gtk::Button::new();
        let current = live_status.clone();
        let tx = actions.clone();
        mount.connect_clicked(move |_| {
            let action = match *current.borrow() {
                Status::Mounted | Status::Mounting | Status::Recovering | Status::Waiting => {
                    Action::Unmount(id)
                }
                _ => Action::Mount(id),
            };
            let _ = tx.send(action);
        });
        buttons.append(&mount);
        let open = gtk::Button::with_label("Open folder");
        let tx = actions.clone();
        open.connect_clicked(move |_| {
            let _ = tx.send(Action::Open(id));
        });
        buttons.append(&open);
        let queue = gtk::Button::with_label("Queue…");
        let tx = actions.clone();
        queue.connect_clicked(move |_| {
            let _ = tx.send(Action::Queue(id));
        });
        buttons.append(&queue);
        let unmount = gtk::Button::with_label("Unmount…");
        unmount.add_css_class("flat");
        let tx = actions.clone();
        let close = popover.downgrade();
        unmount.connect_clicked(move |_| {
            if let Some(close) = close.upgrade() {
                close.popdown();
            }
            let _ = tx.send(Action::Unmount(id));
        });
        utilities.append(&unmount);
        let log = gtk::Button::with_label("View log");
        log.add_css_class("flat");
        let tx = actions.clone();
        let close = popover.downgrade();
        log.connect_clicked(move |_| {
            if let Some(close) = close.upgrade() {
                close.popdown();
            }
            let _ = tx.send(Action::Log(id));
        });
        utilities.append(&log);
        let edit = gtk::Button::with_label("Edit profile…");
        edit.add_css_class("flat");
        let parent = window.clone();
        let snapshot = state.clone();
        let p = profile.clone();
        let tx = requests.clone();
        let close = popover.downgrade();
        edit.connect_clicked(move |_| {
            if let Some(close) = close.upgrade() {
                close.popdown();
            }
            editor(&parent, &snapshot, Some(p.clone()), tx.clone());
        });
        utilities.append(&edit);
        let delete = gtk::Button::with_label("Delete profile…");
        delete.add_css_class("flat");
        delete.set_tooltip_text(Some(&format!(
            "Delete {} (keeps remote files and cache)",
            profile.name
        )));
        let parent = window.clone();
        let tx = requests.clone();
        let close = popover.downgrade();
        delete.connect_clicked(move |_| {
            if let Some(close) = close.upgrade() {
                close.popdown();
            }
            let tx = tx.clone();
            confirm(
                &parent,
                "Delete this mount profile?",
                "Remote files and local cache will not be deleted.",
                move || {
                    let _ = tx.send(Request::Delete(id));
                },
            );
        });
        utilities.append(&delete);
        let heights = gtk::SizeGroup::new(gtk::SizeGroupMode::Vertical);
        for button in [&mount, &open, &queue, &unmount, &log, &edit, &delete] {
            button.set_valign(gtk::Align::Center);
            button.set_height_request(CONTROL_HEIGHT);
            heights.add_widget(button);
        }
        buttons.set_halign(gtk::Align::Start);
        card.append(&buttons);
        let frame = gtk::Frame::new(None);
        frame.add_css_class("rclone-mount-card");
        frame.set_child(Some(&card));
        list.insert_child_after(&frame, previous.as_ref());
        previous = Some(frame.clone());
        let mut card = MountCard {
            root: frame,
            activity,
            profile: profile.clone(),
            status: live_status,
            badge,
            detail,
            mount,
            open,
            queue,
            unmount,
            edit,
            delete,
            sample: state.activity.get(&id).cloned(),
        };
        card.update(status, state.activity.get(&id));
        cards.insert(id, card);
    }
}

fn render_activity(
    container: &gtk::Box,
    sample: Option<&Result<Activity, String>>,
    status: &Status,
) {
    while let Some(child) = container.first_child() {
        container.remove(&child);
    }
    container.set_visible(matches!(
        status,
        Status::Mounting
            | Status::Recovering
            | Status::Waiting
            | Status::Mounted
            | Status::External
    ));
    if !container.is_visible() {
        return;
    }
    let notice = gtk::Label::new(None);
    notice.set_xalign(0.0);
    notice.set_wrap(true);
    notice.add_css_class("rclone-hint");
    container.append(&notice);
    if matches!(status, Status::External) {
        notice.set_text("Activity unavailable");
        notice.set_tooltip_text(Some("Managed by another session."));
        return;
    }
    let activity = match sample {
        Some(Ok(activity)) => activity,
        Some(Err(error)) => {
            notice.set_text("Activity unavailable");
            notice.set_tooltip_text(Some(error));
            return;
        }
        None => {
            notice.set_text("Connecting…");
            return;
        }
    };
    if let Ok(cache) = &activity.cache {
        notice.set_text(&format!(
            "{} - Cache {}",
            activity.rates().label(),
            byte_size(cache.bytes_used as f64)
        ));
        if cache.out_of_space || cache.errored_files > 0 {
            let warning = gtk::Label::new(Some(if cache.out_of_space {
                "Cache full - free disk space"
            } else {
                "Uploads need retry - view log"
            }));
            warning.set_xalign(0.0);
            warning.set_wrap(true);
            warning.add_css_class("error");
            container.append(&warning);
        }
    } else {
        notice.set_text(&format!("{} - Cache unavailable", activity.rates().label()));
    }
}

#[derive(Clone, PartialEq)]
struct QueueRow {
    name: String,
    detail: String,
    progress: Option<f64>,
}

struct QueueWindow {
    window: gtk::Window,
    uploads: gio::ListStore,
    downloads: gio::ListStore,
    upload_notice: gtk::Label,
    download_notice: gtk::Label,
}

impl QueueWindow {
    fn new(parent: &gtk::ApplicationWindow) -> Self {
        let window = gtk::Window::builder()
            .transient_for(parent)
            .default_width(640)
            .default_height(440)
            .build();
        window.add_css_class("rclone-app");
        window.set_titlebar(Some(&gtk::HeaderBar::new()));
        let tabs = gtk::Notebook::new();
        let (uploads, upload_notice) = queue_page(&tabs, "Uploads");
        let (downloads, download_notice) = queue_page(&tabs, "Downloads");
        download_notice.set_tooltip_text(Some("Downloads start when files are read. rclone exposes active downloads, not a waiting download queue."));
        window.set_child(Some(&tabs));
        let keys = gtk::EventControllerKey::new();
        let close = window.downgrade();
        keys.connect_key_pressed(move |_, key, _, _| {
            if key == gtk::gdk::Key::Escape {
                if let Some(window) = close.upgrade() {
                    window.close();
                }
                glib::Propagation::Stop
            } else {
                glib::Propagation::Proceed
            }
        });
        window.add_controller(keys);
        Self {
            window,
            uploads,
            downloads,
            upload_notice,
            download_notice,
        }
    }

    fn update(&self, snapshot: &Snapshot, id: u64) {
        let profile = snapshot.config.profiles.iter().find(|p| p.id == id);
        self.window.set_title(Some(&profile.map_or_else(
            || "Transfer queue".into(),
            |p| format!("{} — Transfer queue", p.name),
        )));
        let status = snapshot.statuses.get(&id).unwrap_or(&Status::Unmounted);
        let unavailable = if profile.is_none() {
            Some("Profile removed")
        } else if *status == Status::External {
            Some("Managed by another session")
        } else if !status.active() {
            Some("Not mounted")
        } else if *status == Status::Unmounting {
            Some("Unmounting…")
        } else {
            None
        };
        let activity = match (unavailable, snapshot.activity.get(&id)) {
            (None, Some(Ok(activity))) => activity,
            (message, sample) => {
                let message = message.unwrap_or(if matches!(sample, Some(Err(_))) {
                    "Queue unavailable - reconnecting…"
                } else {
                    "Loading…"
                });
                self.upload_notice.set_text(message);
                self.download_notice.set_text(message);
                set_queue_rows(&self.uploads, &[]);
                set_queue_rows(&self.downloads, &[]);
                return;
            }
        };
        let mut uploads = Vec::new();
        self.upload_notice.set_tooltip_text(None);
        match &activity.uploads {
            Ok(queue) => {
                let active = queue.iter().filter(|item| item.uploading).count();
                self.upload_notice.set_text(if queue.is_empty() {
                    "No queued uploads"
                } else {
                    ""
                });
                if !queue.is_empty() {
                    self.upload_notice.set_text(&format!(
                        "{active} uploading - {} queued",
                        queue.len() - active
                    ));
                }
                for item in queue {
                    if let Some(transfer) = activity
                        .transfers
                        .iter()
                        .find(|t| t.name == item.name && t.direction() == "Uploading")
                    {
                        uploads.push(transfer_queue_row(transfer));
                        continue;
                    }
                    let state = if item.uploading {
                        "Uploading".into()
                    } else if item.expiry > 0.0 {
                        format!("Queued - eligible in {:.0}s", item.expiry.ceil())
                    } else {
                        "Queued - waiting for slot".into()
                    };
                    let attempts = if item.tries > 0 {
                        format!(" - {} attempts", item.tries)
                    } else {
                        String::new()
                    };
                    let size = if item.size < 0 {
                        "Size unknown".into()
                    } else {
                        byte_size(item.size as f64)
                    };
                    uploads.push(QueueRow {
                        name: item.name.clone(),
                        detail: format!("{size} - {state}{attempts}"),
                        progress: (item.size >= 0 && !item.uploading).then_some(0.0),
                    });
                }
            }
            Err(error) => {
                self.upload_notice.set_text("Upload queue unavailable");
                self.upload_notice.set_tooltip_text(Some(error));
            }
        }
        // Startup uploads can be visible before the VFS queue is registered. Never hide them.
        for transfer in activity
            .transfers
            .iter()
            .filter(|t| t.direction() == "Uploading")
        {
            if !activity
                .uploads
                .as_ref()
                .is_ok_and(|queue| queue.iter().any(|item| item.name == transfer.name))
            {
                uploads.push(transfer_queue_row(transfer));
            }
        }
        let downloads: Vec<_> = activity
            .transfers
            .iter()
            .filter(|t| t.direction() == "Downloading")
            .map(transfer_queue_row)
            .collect();
        let unknown = activity
            .transfers
            .iter()
            .filter(|t| t.direction() == "Transferring")
            .count();
        self.download_notice.set_text(&format!(
            "{} downloading - on demand{}",
            downloads.len(),
            if unknown > 0 {
                format!(" - {unknown} unclassified")
            } else {
                String::new()
            }
        ));
        set_queue_rows(&self.uploads, &uploads);
        set_queue_rows(&self.downloads, &downloads);
    }
}

fn queue_page(tabs: &gtk::Notebook, title: &str) -> (gio::ListStore, gtk::Label) {
    let model = gio::ListStore::new::<glib::BoxedAnyObject>();
    let factory = gtk::SignalListItemFactory::new();
    factory.connect_setup(|_, item| {
        let row = gtk::Box::new(gtk::Orientation::Vertical, FIELD_GAP);
        style::inset(&row, GAP);
        let label = gtk::Label::new(None);
        label.set_xalign(0.0);
        label.set_wrap(true);
        label.set_wrap_mode(gtk::pango::WrapMode::WordChar);
        label.set_selectable(true);
        row.append(&label);
        row.append(&gtk::ProgressBar::new());
        let detail = gtk::Label::new(None);
        detail.set_xalign(0.0);
        detail.set_wrap(true);
        detail.add_css_class("rclone-hint");
        row.append(&detail);
        item.set_child(Some(&row));
    });
    factory.connect_bind(|_, item| {
        let object = item
            .item()
            .unwrap()
            .downcast::<glib::BoxedAnyObject>()
            .unwrap();
        let row = object.borrow::<QueueRow>();
        let container = item.child().unwrap();
        let label = container
            .first_child()
            .unwrap()
            .downcast::<gtk::Label>()
            .unwrap();
        let progress = label
            .next_sibling()
            .unwrap()
            .downcast::<gtk::ProgressBar>()
            .unwrap();
        let detail = progress
            .next_sibling()
            .unwrap()
            .downcast::<gtk::Label>()
            .unwrap();
        label.set_text(&row.name);
        progress.set_visible(row.progress.is_some());
        progress.set_fraction(row.progress.unwrap_or(0.0));
        progress.set_tooltip_text(Some(&row.name));
        detail.set_text(&row.detail);
    });
    let list = gtk::ListView::new(
        Some(gtk::NoSelection::new(Some(model.clone()))),
        Some(factory),
    );
    list.set_show_separators(true);
    let scroll = gtk::ScrolledWindow::builder()
        .vexpand(true)
        .hscrollbar_policy(gtk::PolicyType::Never)
        .child(&list)
        .build();
    let page = gtk::Box::new(gtk::Orientation::Vertical, GAP);
    style::inset(&page, GROUP_GAP);
    let notice = gtk::Label::new(None);
    notice.set_wrap(true);
    notice.set_xalign(0.0);
    notice.add_css_class("rclone-hint");
    page.append(&notice);
    page.append(&scroll);
    tabs.append_page(&page, Some(&gtk::Label::new(Some(title))));
    (model, notice)
}

fn set_queue_rows(model: &gio::ListStore, rows: &[QueueRow]) {
    // Replace only changed rows so progress updates do not reset the whole list.
    for (index, row) in rows.iter().enumerate() {
        let index = index as u32;
        let current = model
            .item(index)
            .map(|item| item.downcast::<glib::BoxedAnyObject>().unwrap());
        if current
            .as_ref()
            .is_some_and(|item| *item.borrow::<QueueRow>() == *row)
        {
            continue;
        }
        model.splice(
            index,
            u32::from(current.is_some()),
            &[glib::BoxedAnyObject::new(row.clone())],
        );
    }
    let count = rows.len() as u32;
    if model.n_items() > count {
        model.splice(
            count,
            model.n_items() - count,
            &[] as &[glib::BoxedAnyObject],
        );
    }
}

fn transfer_queue_row(transfer: &Transfer) -> QueueRow {
    let progress = if transfer.size > 0 {
        format!(
            "{} / {}",
            byte_size(transfer.bytes as f64),
            byte_size(transfer.size as f64)
        )
    } else {
        format!("{} transferred", byte_size(transfer.bytes as f64))
    };
    QueueRow {
        name: transfer.name.clone(),
        detail: format!(
            "{} - {progress} - {}/s",
            transfer.direction(),
            byte_size(transfer.speed_avg)
        ),
        progress: (transfer.size > 0)
            .then(|| (transfer.bytes as f64 / transfer.size as f64).clamp(0.0, 1.0)),
    }
}

fn editor(
    parent: &gtk::ApplicationWindow,
    state: &Snapshot,
    profile: Option<Profile>,
    tx: mpsc::Sender<Request>,
) {
    let dialog = gtk::Window::builder()
        .title(if profile.is_some() {
            "Edit mount profile"
        } else {
            "Add mount profile"
        })
        .transient_for(parent)
        .modal(true)
        .default_width(480)
        .default_height(560)
        .build();
    dialog.add_css_class("rclone-profile");
    let content = gtk::Box::new(gtk::Orientation::Vertical, GROUP_GAP);
    style::inset(&content, INSET);
    let name = gtk::Entry::new();
    name.set_placeholder_text(Some("Work Drive"));
    let remote = gtk::ComboBoxText::new();
    for name in &state.remotes {
        remote.append(Some(name), name);
    }
    if !state.remotes.is_empty() {
        remote.set_active(Some(0));
    }
    let folder = gtk::Entry::new();
    folder.set_placeholder_text(Some("Documents"));
    let mountpoint = gtk::Entry::new();
    mountpoint.set_placeholder_text(Some("/home/you/Cloud/Work"));
    mountpoint.set_hexpand(true);
    let mount_row = gtk::Box::new(gtk::Orientation::Horizontal, GAP);
    mount_row.append(&mountpoint);
    let browse = gtk::Button::with_label("Browse…");
    let chooser_parent = dialog.clone();
    let entry = mountpoint.clone();
    browse.connect_clicked(move |_| {
        let chooser = gtk::FileChooserNative::new(
            Some("Choose an empty mount directory"),
            Some(&chooser_parent),
            gtk::FileChooserAction::SelectFolder,
            Some("Choose"),
            Some("Cancel"),
        );
        let entry = entry.clone();
        chooser.connect_response(move |chooser, response| {
            if response == gtk::ResponseType::Accept
                && let Some(path) = chooser.file().and_then(|f| f.path())
            {
                entry.set_text(&path.to_string_lossy());
            }
            chooser.destroy();
        });
        chooser.show();
    });
    mount_row.append(&browse);
    let cache = gtk::SpinButton::with_range(1.0, 100000.0, 1.0);
    cache.set_value(10.0);
    let auto = gtk::CheckButton::with_label("Mount when Rclone Tray starts");
    if let Some(p) = &profile {
        name.set_text(&p.name);
        folder.set_text(&p.folder);
        mountpoint.set_text(&p.mountpoint.to_string_lossy());
        cache.set_value(p.cache_gib as f64);
        auto.set_active(p.mount_at_start);
        remote.set_active_id(Some(&p.remote));
    }
    for (text, widget, target) in [
        (
            "_Name",
            name.clone().upcast::<gtk::Widget>(),
            name.clone().upcast::<gtk::Widget>(),
        ),
        ("_Remote", remote.clone().upcast(), remote.clone().upcast()),
        (
            "Remote _folder (optional)",
            folder.clone().upcast(),
            folder.clone().upcast(),
        ),
        (
            "_Mountpoint",
            mount_row.upcast(),
            mountpoint.clone().upcast(),
        ),
        (
            "_Cache target (GiB)",
            cache.clone().upcast(),
            cache.clone().upcast(),
        ),
    ] {
        let label = gtk::Label::new(Some(text));
        label.set_xalign(0.0);
        label.add_css_class("heading");
        label.set_use_underline(true);
        label.set_mnemonic_widget(Some(&target));
        let field = gtk::Box::new(gtk::Orientation::Vertical, FIELD_GAP);
        field.append(&label);
        field.append(&widget);
        content.append(&field);
    }
    cache.set_tooltip_text(Some(
        "Target, not a hard limit. Open files and pending uploads can exceed it.",
    ));
    content.append(&auto);
    let error = gtk::Label::new(None);
    error.set_wrap(true);
    error.set_xalign(0.0);
    error.set_selectable(true);
    error.set_visible(false);
    error.add_css_class("error");
    content.append(&error);
    let buttons = gtk::Box::new(gtk::Orientation::Horizontal, GAP);
    buttons.set_halign(gtk::Align::End);
    buttons.set_margin_start(INSET);
    buttons.set_margin_end(INSET);
    buttons.set_margin_bottom(INSET);
    let cancel = gtk::Button::with_label("Cancel");
    let d = dialog.clone();
    cancel.connect_clicked(move |_| d.close());
    buttons.append(&cancel);
    let save = gtk::Button::with_label("Save profile");
    save.add_css_class("suggested-action");
    save.set_height_request(CONTROL_HEIGHT);
    cancel.set_height_request(CONTROL_HEIGHT);
    dialog.set_default_widget(Some(&save));
    for entry in [&name, &folder, &mountpoint] {
        entry.set_activates_default(true);
    }
    buttons.append(&save);
    let d = dialog.clone();
    let remotes = state.remotes.clone();
    let form = content.clone();
    save.connect_clicked(move |save| {
        let p = Profile {
            id: profile.as_ref().map_or(0, |p| p.id),
            name: name.text().trim().into(),
            remote: remote
                .active_id()
                .map(|id| id.to_string())
                .unwrap_or_default(),
            folder: folder.text().into(),
            mountpoint: PathBuf::from(mountpoint.text().as_str()),
            cache_gib: cache.value_as_int() as u32,
            mount_at_start: auto.is_active(),
        };
        if let Err(e) = backend::validate(&p, &remotes) {
            error.set_text(&e);
            error.set_visible(true);
            return;
        }
        let (saved, reply) = mpsc::channel();
        if tx.send(Request::Save(p, saved)).is_err() {
            error.set_text("Mount service unavailable. Restart the application.");
            error.set_visible(true);
            return;
        }
        save.set_sensitive(false);
        form.set_sensitive(false);
        error.set_visible(false);
        let dialog = d.downgrade();
        let form = form.clone();
        let save = save.clone();
        let error = error.clone();
        glib::timeout_add_local(Duration::from_millis(50), move || {
            let Some(dialog) = dialog.upgrade().filter(|d| d.is_visible()) else {
                return glib::ControlFlow::Break;
            };
            let result = match reply.try_recv() {
                Ok(result) => result,
                Err(mpsc::TryRecvError::Empty) => return glib::ControlFlow::Continue,
                Err(_) => Err("Mount service stopped. Restart the application.".into()),
            };
            form.set_sensitive(true);
            save.set_sensitive(true);
            match result {
                Ok(()) => dialog.close(),
                Err(message) => {
                    error.set_text(&message);
                    error.set_visible(true);
                }
            }
            glib::ControlFlow::Break
        });
    });
    let scroll = gtk::ScrolledWindow::builder()
        .vexpand(true)
        .hscrollbar_policy(gtk::PolicyType::Never)
        .child(&content)
        .build();
    let layout = gtk::Box::new(gtk::Orientation::Vertical, 0);
    layout.append(&scroll);
    layout.append(&buttons);
    dialog.set_child(Some(&layout));
    let keys = gtk::EventControllerKey::new();
    let close = dialog.downgrade();
    keys.connect_key_pressed(move |_, key, _, _| {
        if key == gtk::gdk::Key::Escape {
            if let Some(dialog) = close.upgrade() {
                dialog.close();
            }
            glib::Propagation::Stop
        } else {
            glib::Propagation::Proceed
        }
    });
    dialog.add_controller(keys);
    dialog.present();
}

fn confirm(parent: &gtk::ApplicationWindow, title: &str, detail: &str, yes: impl Fn() + 'static) {
    let dialog = gtk::MessageDialog::builder()
        .transient_for(parent)
        .modal(true)
        .text(title)
        .secondary_text(detail)
        .buttons(gtk::ButtonsType::OkCancel)
        .build();
    dialog.add_css_class("rclone-app");
    dialog.connect_response(move |dialog, response| {
        if response == gtk::ResponseType::Ok {
            yes();
        }
        dialog.close();
    });
    dialog.present();
}

fn open_uri(uri: &str, message: &gtk::Label) {
    let label = message.clone();
    gio::AppInfo::launch_default_for_uri_async(
        uri,
        None::<&gio::AppLaunchContext>,
        None::<&gio::Cancellable>,
        move |result| {
            if let Err(e) = result {
                label.set_text(&format!("Cannot open folder: {e}"));
                label.set_visible(true);
            }
        },
    );
}

fn show_log(parent: &gtk::ApplicationWindow, paths: &Paths, id: u64) {
    use std::io::{Read, Seek, SeekFrom};
    let path = paths.logs.join(format!("{id}.log"));
    let text = (|| {
        let mut file = std::fs::File::open(&path)?;
        let length = file.metadata()?.len();
        file.seek(SeekFrom::Start(length.saturating_sub(65536)))?;
        let mut bytes = vec![];
        file.read_to_end(&mut bytes)?;
        Ok::<_, std::io::Error>(String::from_utf8_lossy(&bytes).into_owned())
    })()
    .unwrap_or_else(|e| format!("No log available yet: {e}"));
    let window = gtk::Window::builder()
        .transient_for(parent)
        .title("Rclone log — latest 64 KiB (snapshot)")
        .default_width(800)
        .default_height(480)
        .build();
    window.add_css_class("rclone-log");
    let view = gtk::TextView::new();
    view.set_editable(false);
    view.set_monospace(true);
    view.set_wrap_mode(gtk::WrapMode::WordChar);
    view.buffer().set_text(&text);
    let scroll = gtk::ScrolledWindow::builder().child(&view).build();
    window.set_child(Some(&scroll));
    window.present();
}
