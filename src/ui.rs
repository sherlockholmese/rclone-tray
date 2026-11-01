use crate::{
    backend::{self, Event, Request, Snapshot, Status},
    config::{Paths, Profile},
    control::Activity,
    ipc,
    style::{self, FIELD_GAP, GAP, GROUP_GAP, INSET, ROW_GAP},
    tray::Action,
    view::{self, Primary, QueueRow, Tone},
};
use gtk::{gio, glib, prelude::*};
use std::{
    cell::{Cell, RefCell},
    collections::HashMap,
    path::PathBuf,
    rc::Rc,
    sync::mpsc,
    time::Duration,
};

const LIST_PAGE: &str = "list";
const DETAIL_PAGE: &str = "detail";
const UPLOADS: &str = "uploads";
const DOWNLOADS: &str = "downloads";
const LOG: &str = "log";
// Bounds natural width, so long prose wraps instead of asking the window to grow.
const MEASURE: i32 = 60;

type Sample = Result<Activity, String>;

fn label(text: &str, classes: &[&str]) -> gtk::Label {
    let label = gtk::Label::new(Some(text));
    label.set_xalign(0.0);
    for class in classes {
        label.add_css_class(class);
    }
    label
}

fn wrapped(classes: &[&str]) -> gtk::Label {
    let label = label("", classes);
    label.set_wrap(true);
    label.set_wrap_mode(gtk::pango::WrapMode::WordChar);
    label.set_max_width_chars(MEASURE);
    selectable(&label);
    label.set_visible(false);
    label
}

/// Copyable by pointer without becoming a Tab stop that selects itself on focus.
fn selectable(label: &gtk::Label) {
    label.set_selectable(true);
    label.set_focusable(false);
}

fn set_tone(widget: &impl IsA<gtk::Widget>, tone: Tone) {
    for other in [
        Tone::Idle,
        Tone::Ok,
        Tone::Busy,
        Tone::Warning,
        Tone::Danger,
    ] {
        widget.remove_css_class(other.css_class());
    }
    widget.add_css_class(tone.css_class());
}

fn seam() -> gtk::ProgressBar {
    let bar = gtk::ProgressBar::new();
    bar.add_css_class("rclone-seam");
    bar
}

/// A state mark that differs by shape, so color is never the only signal.
struct Lamp {
    root: gtk::Box,
    dot: gtk::Box,
    spinner: gtk::Spinner,
    icon: gtk::Image,
}

impl Lamp {
    fn new() -> Self {
        let root = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        root.set_size_request(16, 16);
        root.set_hexpand(false);
        root.set_valign(gtk::Align::Center);
        root.set_halign(gtk::Align::Center);
        let dot = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        dot.add_css_class("rclone-dot");
        dot.set_halign(gtk::Align::Center);
        dot.set_valign(gtk::Align::Center);
        dot.set_hexpand(true);
        let spinner = gtk::Spinner::new();
        spinner.set_hexpand(true);
        let icon = gtk::Image::from_icon_name("dialog-warning-symbolic");
        icon.set_pixel_size(16);
        root.append(&dot);
        root.append(&spinner);
        root.append(&icon);
        Self {
            root,
            dot,
            spinner,
            icon,
        }
    }

    fn set(&self, tone: Tone) {
        self.dot.set_visible(matches!(tone, Tone::Ok | Tone::Idle));
        self.spinner.set_visible(tone == Tone::Busy);
        self.spinner.set_spinning(tone == Tone::Busy);
        self.icon
            .set_visible(matches!(tone, Tone::Warning | Tone::Danger));
        set_tone(&self.dot, tone);
        set_tone(&self.icon, tone);
    }
}

struct MountRow {
    root: gtk::ListBoxRow,
    profile: Profile,
    status: Rc<RefCell<Status>>,
    lamp: Lamp,
    state: gtk::Label,
    path: gtk::Label,
    action: gtk::Button,
    progress: gtk::ProgressBar,
}

impl MountRow {
    fn new(
        profile: &Profile,
        status: &Status,
        actions: &mpsc::Sender<Action>,
        widths: &gtk::SizeGroup,
    ) -> Self {
        let id = profile.id;
        let line = gtk::Box::new(gtk::Orientation::Horizontal, ROW_GAP);
        style::inset(&line, ROW_GAP, INSET);
        let lamp = Lamp::new();
        line.append(&lamp.root);
        let identity = gtk::Box::new(gtk::Orientation::Vertical, FIELD_GAP);
        identity.set_hexpand(true);
        identity.set_valign(gtk::Align::Center);
        let name = label(&profile.name, &["rclone-name"]);
        name.set_ellipsize(gtk::pango::EllipsizeMode::End);
        name.set_tooltip_text(Some(&profile.name));
        identity.append(&name);
        let path = label("", &["rclone-muted"]);
        path.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
        path.set_tooltip_text(Some(&profile.mountpoint.to_string_lossy()));
        identity.append(&path);
        line.append(&identity);
        let state = label("", &["rclone-numeric"]);
        state.set_xalign(1.0);
        state.set_ellipsize(gtk::pango::EllipsizeMode::End);
        state.set_valign(gtk::Align::Center);
        line.append(&state);
        let current = Rc::new(RefCell::new(status.clone()));
        let action = gtk::Button::new();
        action.set_valign(gtk::Align::Center);
        widths.add_widget(&action);
        let live = current.clone();
        let tx = actions.clone();
        action.connect_clicked(move |_| {
            let _ = tx.send(match view::primary(&live.borrow()) {
                Primary::Open | Primary::Unavailable => Action::Open(id),
                Primary::Mount => Action::Mount(id),
                Primary::Cancel => Action::Unmount(id),
            });
        });
        line.append(&action);
        let chevron = gtk::Image::from_icon_name("go-next-symbolic");
        chevron.add_css_class("rclone-muted");
        line.append(&chevron);
        let progress = seam();
        let body = gtk::Box::new(gtk::Orientation::Vertical, 0);
        body.append(&line);
        body.append(&progress);
        let root = gtk::ListBoxRow::new();
        root.set_child(Some(&body));
        root.set_action_name(Some("win.details"));
        root.set_action_target_value(Some(&id.to_variant()));
        Self {
            root,
            profile: profile.clone(),
            status: current,
            lamp,
            state,
            path,
            action,
            progress,
        }
    }

    fn update(&self, status: &Status, sample: Option<&Sample>, home: Option<&std::path::Path>) {
        *self.status.borrow_mut() = status.clone();
        let (text, tone) = view::activity_line(status, sample);
        self.lamp.set(tone);
        self.lamp.root.set_tooltip_text(Some(&text));
        let note = view::ledger_note(status, &text);
        self.state.set_text(note);
        self.state.set_visible(!note.is_empty());
        set_tone(&self.state, tone);
        self.state.set_tooltip_text(view::status_hint(status));
        let path = view::short_path(&self.profile.mountpoint, home);
        self.path.set_text(&path);
        if *status == Status::Unmounted {
            self.root.add_css_class("rclone-unmounted");
        } else {
            self.root.remove_css_class("rclone-unmounted");
        }
        let primary = view::primary(status);
        self.action.set_label(match primary {
            Primary::Open | Primary::Unavailable => "Open",
            Primary::Mount => "Mount",
            Primary::Cancel => "Cancel…",
        });
        self.action.set_sensitive(primary != Primary::Unavailable);
        self.action.set_tooltip_text(Some(match primary {
            Primary::Open | Primary::Unavailable => "Open folder",
            Primary::Mount => "Mount",
            Primary::Cancel => "Cancel mount",
        }));
        self.progress
            .set_fraction(view::progress(status, sample).unwrap_or(0.0));
    }
}

struct QueuePane {
    page: gtk::StackPage,
    title: &'static str,
    model: gio::ListStore,
    heading: gtk::Box,
    scroll: gtk::ScrolledWindow,
    notice: gtk::Label,
    hint: gtk::Label,
}

impl QueuePane {
    fn new(panes: &gtk::Stack, id: &str, title: &'static str) -> Self {
        let model = gio::ListStore::new::<glib::BoxedAnyObject>();
        let factory = gtk::SignalListItemFactory::new();
        factory.connect_setup(|_, item| {
            let item = item.downcast_ref::<gtk::ListItem>().unwrap();
            let text = gtk::Box::new(gtk::Orientation::Vertical, FIELD_GAP);
            style::inset(&text, GAP, INSET);
            let name = label("", &[]);
            name.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
            text.append(&name);
            let detail = label("", &["rclone-muted", "rclone-numeric"]);
            detail.set_ellipsize(gtk::pango::EllipsizeMode::End);
            text.append(&detail);
            let row = gtk::Box::new(gtk::Orientation::Vertical, 0);
            row.append(&text);
            row.append(&seam());
            item.set_child(Some(&row));
        });
        factory.connect_bind(|_, item| {
            let item = item.downcast_ref::<gtk::ListItem>().unwrap();
            let object = item
                .item()
                .and_downcast::<glib::BoxedAnyObject>()
                .expect("queue rows hold QueueRow");
            let row = object.borrow::<QueueRow>();
            let container = item.child().unwrap();
            let text = container.first_child().unwrap();
            let name = text.first_child().and_downcast::<gtk::Label>().unwrap();
            let detail = name.next_sibling().and_downcast::<gtk::Label>().unwrap();
            let bar = text
                .next_sibling()
                .and_downcast::<gtk::ProgressBar>()
                .unwrap();
            name.set_text(&row.name);
            name.set_tooltip_text(Some(&row.name));
            detail.set_text(&row.detail);
            bar.set_fraction(row.progress);
        });
        let list = gtk::ListView::new(
            Some(gtk::NoSelection::new(Some(model.clone()))),
            Some(factory),
        );
        list.add_css_class("rclone-ledger");
        let scroll = gtk::ScrolledWindow::builder()
            .vexpand(true)
            .hscrollbar_policy(gtk::PolicyType::Never)
            .child(&list)
            .build();
        let heading = gtk::Box::new(gtk::Orientation::Vertical, FIELD_GAP);
        style::inset(&heading, ROW_GAP, INSET);
        let notice = label("", &["rclone-numeric"]);
        notice.set_wrap(true);
        heading.append(&notice);
        let hint = wrapped(&["rclone-muted"]);
        heading.append(&hint);
        let page = gtk::Box::new(gtk::Orientation::Vertical, 0);
        page.append(&heading);
        page.append(&scroll);
        page.set_vexpand(true);
        Self {
            page: panes.add_titled(&page, Some(id), title),
            title,
            model,
            heading,
            scroll,
            notice,
            hint,
        }
    }

    fn update(&self, pane: &view::QueuePane) {
        self.notice.set_text(&pane.notice);
        self.hint.set_text(pane.hint.as_deref().unwrap_or(""));
        self.hint.set_visible(pane.hint.is_some());
        // With nothing to list, the message is the pane's content, not a caption.
        let empty = pane.rows.is_empty();
        self.scroll.set_visible(!empty);
        self.heading.set_vexpand(empty);
        self.heading.set_valign(if empty {
            gtk::Align::Center
        } else {
            gtk::Align::Fill
        });
        for text in [&self.notice, &self.hint] {
            text.set_halign(if empty {
                gtk::Align::Center
            } else {
                gtk::Align::Start
            });
            text.set_xalign(if empty { 0.5 } else { 0.0 });
            text.set_justify(if empty {
                gtk::Justification::Center
            } else {
                gtk::Justification::Left
            });
        }
        let title = match pane.rows.len() {
            0 => self.title.to_string(),
            count => format!("{} ({count})", self.title),
        };
        if self.page.title().as_deref() != Some(title.as_str()) {
            self.page.set_title(&title);
        }
        set_queue_rows(&self.model, &pane.rows);
    }
}

struct Detail {
    id: Rc<Cell<Option<u64>>>,
    status: Rc<RefCell<Status>>,
    title: gtk::Label,
    subtitle: gtk::Label,
    problem: gtk::Label,
    folder: gtk::Label,
    meta: gtk::Label,
    open: gtk::Button,
    mount: gtk::Button,
    unmount: gtk::Button,
    edit: gtk::Button,
    remove: gtk::Button,
    panes: gtk::Stack,
    uploads: QueuePane,
    downloads: QueuePane,
    log: gtk::TextView,
    root: gtk::Box,
}

impl Detail {
    fn new(actions: &mpsc::Sender<Action>) -> Self {
        let id = Rc::new(Cell::new(None::<u64>));
        let status = Rc::new(RefCell::new(Status::Unmounted));
        let summary = gtk::Box::new(gtk::Orientation::Vertical, GROUP_GAP);
        style::inset(&summary, GROUP_GAP, INSET);
        let problem = wrapped(&[]);
        summary.append(&problem);
        let lead = gtk::Box::new(gtk::Orientation::Horizontal, GROUP_GAP);
        let identity = gtk::Box::new(gtk::Orientation::Vertical, FIELD_GAP);
        identity.set_hexpand(true);
        identity.set_valign(gtk::Align::Center);
        let folder = label("", &["rclone-headline"]);
        folder.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
        selectable(&folder);
        identity.append(&folder);
        let meta = label("", &["rclone-muted", "rclone-numeric"]);
        meta.set_wrap(true);
        meta.set_wrap_mode(gtk::pango::WrapMode::WordChar);
        meta.set_max_width_chars(MEASURE);
        selectable(&meta);
        identity.append(&meta);
        lead.append(&identity);
        // Quiet actions first, so the main one ends on the same edge as the list's buttons.
        let buttons = gtk::Box::new(gtk::Orientation::Horizontal, GAP);
        buttons.set_valign(gtk::Align::Center);
        let button = |text: &str| {
            let button = gtk::Button::with_label(text);
            buttons.append(&button);
            button
        };
        let remove = button("Remove…");
        let edit = button("Edit…");
        let unmount = button("Unmount…");
        let mount = button("Mount");
        let open = button("Open folder");
        open.add_css_class("suggested-action");
        lead.append(&buttons);
        summary.append(&lead);

        let send = |button: &gtk::Button, action: fn(u64, &Status) -> Action| {
            let (tx, id, status) = (actions.clone(), id.clone(), status.clone());
            button.connect_clicked(move |_| {
                if let Some(id) = id.get() {
                    let _ = tx.send(action(id, &status.borrow()));
                }
            });
        };
        send(&open, |id, _| Action::Open(id));
        send(&unmount, |id, _| Action::Unmount(id));
        send(&mount, |id, status| match view::primary(status) {
            Primary::Cancel => Action::Unmount(id),
            _ => Action::Mount(id),
        });

        let panes = gtk::Stack::new();
        panes.set_vexpand(true);
        panes.set_transition_type(gtk::StackTransitionType::Crossfade);
        panes.set_transition_duration(120);
        let uploads = QueuePane::new(&panes, UPLOADS, "Uploads");
        let downloads = QueuePane::new(&panes, DOWNLOADS, "Downloads");
        let log = gtk::TextView::new();
        log.set_editable(false);
        log.set_cursor_visible(false);
        log.set_monospace(true);
        log.set_wrap_mode(gtk::WrapMode::WordChar);
        log.add_css_class("rclone-log");
        log.set_left_margin(INSET);
        log.set_right_margin(INSET);
        log.set_top_margin(GAP);
        log.set_bottom_margin(GAP);
        let scroll = gtk::ScrolledWindow::builder()
            .vexpand(true)
            .hscrollbar_policy(gtk::PolicyType::Never)
            .child(&log)
            .build();
        panes.add_titled(&scroll, Some(LOG), "Log");
        let switcher = gtk::StackSwitcher::new();
        switcher.set_stack(Some(&panes));
        switcher.remove_css_class("linked");
        switcher.add_css_class("rclone-tabs");
        switcher.set_halign(gtk::Align::Start);
        switcher.set_margin_start(INSET - ROW_GAP);
        let tabs = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        tabs.add_css_class("rclone-tabbar");
        tabs.append(&switcher);
        let root = gtk::Box::new(gtk::Orientation::Vertical, 0);
        root.append(&summary);
        root.append(&tabs);
        root.append(&panes);
        let title = label("", &["title"]);
        title.set_xalign(0.5);
        title.set_ellipsize(gtk::pango::EllipsizeMode::End);
        let subtitle = label("", &["subtitle"]);
        subtitle.set_xalign(0.5);
        subtitle.set_ellipsize(gtk::pango::EllipsizeMode::End);
        Self {
            id,
            status,
            title,
            subtitle,
            problem,
            folder,
            meta,
            open,
            mount,
            unmount,
            edit,
            remove,
            panes,
            uploads,
            downloads,
            log,
            root,
        }
    }

    fn update(&self, snapshot: &Snapshot, home: Option<&std::path::Path>) {
        let Some(id) = self.id.get() else { return };
        let profile = snapshot.config.profiles.iter().find(|p| p.id == id);
        let status = snapshot.statuses.get(&id).unwrap_or(&Status::Unmounted);
        let sample = snapshot.activity.get(&id);
        *self.status.borrow_mut() = status.clone();
        let (state, tone) = view::activity_line(status, sample);
        self.title.set_text(profile.map_or("", |p| &p.name));
        self.subtitle.set_text(&state);
        let problem = view::cache_warning(sample).or(view::status_hint(status));
        self.problem.set_text(problem.unwrap_or(""));
        self.problem.set_visible(problem.is_some());
        set_tone(&self.problem, tone);
        if let Some(profile) = profile {
            let folder = view::short_path(&profile.mountpoint, home);
            self.folder
                .set_tooltip_text((folder.chars().count() > 40).then_some(folder.as_str()));
            self.folder.set_text(&folder);
            self.meta.set_text(&view::detail_line(profile, sample));
        }
        let primary = view::primary(status);
        self.open.set_visible(primary == Primary::Open);
        self.mount.set_visible(primary != Primary::Open);
        self.mount.set_sensitive(primary != Primary::Unavailable);
        self.mount.set_label(match primary {
            Primary::Cancel => "Cancel mount…",
            Primary::Unavailable => "Unmounting…",
            _ => "Mount",
        });
        if primary == Primary::Mount {
            self.mount.add_css_class("suggested-action");
        } else {
            self.mount.remove_css_class("suggested-action");
        }
        self.unmount.set_visible(*status == Status::Mounted);
        self.edit.set_visible(!status.active());
        self.remove.set_visible(!status.active());
        let queues = view::queues(snapshot, id);
        self.uploads.update(&queues.uploads);
        self.downloads.update(&queues.downloads);
    }

    fn load_log(&self, paths: &Paths) {
        if let Some(id) = self.id.get() {
            let buffer = self.log.buffer();
            buffer.set_text(&read_log(paths, id));
            // The newest lines are the useful ones. Wait for layout, then reveal them.
            let view = self.log.downgrade();
            glib::idle_add_local_once(move || {
                if let Some(view) = view.upgrade() {
                    let mut end = view.buffer().end_iter();
                    view.scroll_to_iter(&mut end, 0.0, false, 0.0, 0.0);
                }
            });
        }
    }
}

struct Ui {
    window: gtk::ApplicationWindow,
    pages: gtk::Stack,
    back: gtk::Button,
    add: gtk::Button,
    menu: gtk::MenuButton,
    title: gtk::Label,
    subtitle: gtk::Label,
    banner: gtk::Box,
    message: gtk::Label,
    cancel_quit: gtk::Button,
    empty: gtk::Box,
    scroll: gtk::ScrolledWindow,
    list: gtk::ListBox,
    rows: RefCell<HashMap<u64, MountRow>>,
    widths: gtk::SizeGroup,
    detail: Detail,
    state: Rc<RefCell<Snapshot>>,
    paths: Paths,
    home: PathBuf,
    requests: mpsc::Sender<Request>,
    actions: mpsc::Sender<Action>,
}

impl Ui {
    fn say(&self, text: &str) {
        self.message.set_text(text);
        self.banner.set_visible(!text.is_empty());
    }

    fn refresh_header(&self) {
        let on_detail = self.detail.id.get().is_some();
        let state = self.state.borrow();
        let overview = view::overview(&state);
        if !on_detail {
            self.title.set_text(&overview.headline);
            self.subtitle.set_text(&overview.detail);
            self.subtitle.set_visible(!overview.detail.is_empty());
        } else {
            self.title.set_text(&self.detail.title.text());
            self.subtitle.set_text(&self.detail.subtitle.text());
            self.subtitle.set_visible(true);
        }
        self.back.set_visible(on_detail);
        self.add.set_visible(!on_detail);
        self.menu.set_visible(!on_detail);
    }

    fn apply(&self, snapshot: Snapshot) {
        self.say(&snapshot.message);
        self.cancel_quit.set_visible(snapshot.quitting);
        if snapshot.quitting && snapshot.message.is_empty() {
            self.say("Quitting. Waiting for uploads and unmounts to finish.");
        }
        let quitting = snapshot.quitting;
        self.add.set_sensitive(!quitting);
        for action in ["add", "refresh", "quit"] {
            if let Some(action) = self
                .window
                .lookup_action(action)
                .and_downcast::<gio::SimpleAction>()
            {
                action.set_enabled(!quitting);
            }
        }
        let empty = snapshot.config.profiles.is_empty();
        self.empty.set_visible(empty);
        self.scroll.set_visible(!empty);
        self.render(&snapshot);
        self.detail.update(&snapshot, Some(&self.home));
        if let Some(id) = self.detail.id.get()
            && !snapshot.config.profiles.iter().any(|p| p.id == id)
        {
            self.close_detail(false);
        }
        *self.state.borrow_mut() = snapshot;
        self.refresh_header();
    }

    fn render(&self, state: &Snapshot) {
        let mut rows = self.rows.borrow_mut();
        rows.retain(|id, row| {
            let keep = state
                .config
                .profiles
                .iter()
                .any(|p| p.id == *id && p == &row.profile);
            if !keep {
                self.list.remove(&row.root);
            }
            keep
        });
        for (position, profile) in state.config.profiles.iter().enumerate() {
            let status = state
                .statuses
                .get(&profile.id)
                .unwrap_or(&Status::Unmounted);
            let row = rows.entry(profile.id).or_insert_with(|| {
                let row = MountRow::new(profile, status, &self.actions, &self.widths);
                self.list.insert(&row.root, position as i32);
                row
            });
            if row.root.index() != position as i32 {
                self.list.remove(&row.root);
                self.list.insert(&row.root, position as i32);
            }
            row.update(status, state.activity.get(&profile.id), Some(&self.home));
        }
    }

    fn open_detail(&self, id: u64, pane: &str) {
        self.detail.id.set(Some(id));
        self.detail.panes.set_visible_child_name(pane);
        self.detail.update(&self.state.borrow(), Some(&self.home));
        // Hiding the focused row would hand focus to Back, and themes such as Breeze outline
        // any focused button. Tab still starts at the page, and Escape works from anywhere.
        gtk::prelude::GtkWindowExt::set_focus(&self.window, None::<&gtk::Widget>);
        self.pages
            .set_transition_type(gtk::StackTransitionType::SlideLeft);
        self.pages.set_visible_child_name(DETAIL_PAGE);
        self.refresh_header();
        if pane == LOG {
            self.detail.load_log(&self.paths);
        }
    }

    fn close_detail(&self, keyboard: bool) {
        let id = self.detail.id.take();
        gtk::prelude::GtkWindowExt::set_focus(&self.window, None::<&gtk::Widget>);
        self.pages
            .set_transition_type(gtk::StackTransitionType::SlideRight);
        self.pages.set_visible_child_name(LIST_PAGE);
        self.refresh_header();
        if keyboard
            && let Some(row) = id.and_then(|id| self.rows.borrow().get(&id).map(|r| r.root.clone()))
        {
            row.grab_focus();
        }
    }

    fn name(&self, id: u64) -> String {
        let state = self.state.borrow();
        let profile = state.config.profiles.iter().find(|p| p.id == id);
        profile.map_or("this mount".into(), |p| p.name.clone())
    }

    fn handle(&self, action: Action) {
        match action {
            Action::Show => self.window.present(),
            Action::Mount(id) => {
                let _ = self.requests.send(Request::Mount(id));
            }
            Action::Unmount(id) => {
                self.window.present();
                let tx = self.requests.clone();
                let status = self.state.borrow().statuses.get(&id).cloned();
                let cancel = status.as_ref().map(view::primary) == Some(Primary::Cancel);
                let name = self.name(id);
                confirm(
                    &self.window,
                    Confirm {
                        title: if cancel {
                            format!("Cancel mounting {name}?")
                        } else {
                            format!("Unmount {name}?")
                        },
                        detail: "Close open files first. Unmounting is blocked while uploads are pending or transfer status is unavailable. Cached files are kept.",
                        verb: if cancel { "Cancel mount" } else { "Unmount" },
                        back: if cancel { "Keep mounting" } else { "Cancel" },
                        destructive: false,
                    },
                    move || {
                        let _ = tx.send(Request::Unmount(id));
                    },
                );
            }
            Action::Open(id) => {
                if let Some(p) = self
                    .state
                    .borrow()
                    .config
                    .profiles
                    .iter()
                    .find(|p| p.id == id)
                {
                    open_uri(
                        &gio::File::for_path(&p.mountpoint).uri(),
                        &self.message,
                        &self.banner,
                    );
                }
            }
            Action::Queue(id) => {
                self.window.present();
                self.open_detail(id, UPLOADS);
            }
            Action::Log(id) => {
                self.window.present();
                self.open_detail(id, LOG);
            }
            Action::Quit => {
                self.window.present();
                if self.state.borrow().quitting {
                    let _ = self.requests.send(Request::CancelQuit);
                    return;
                }
                let tx = self.requests.clone();
                confirm(
                    &self.window,
                    Confirm {
                        title: "Quit Rclone Tray?".into(),
                        detail: "Waits for uploads, unmounts this app’s mounts, then quits. Close open files first. Cached files are kept.",
                        verb: "Quit",
                        back: "Cancel",
                        destructive: false,
                    },
                    move || {
                        let _ = tx.send(Request::Quit);
                    },
                );
            }
        }
    }
}

fn window_action(
    window: &gtk::ApplicationWindow,
    name: &str,
    run: impl Fn() + 'static,
) -> gio::SimpleAction {
    let action = gio::SimpleAction::new(name, None);
    action.connect_activate(move |_, _| run());
    window.add_action(&action);
    action
}

pub fn build(app: &gtk::Application, paths: Paths, snapshot: Snapshot) {
    style::install();
    let (requests, events) = ipc::connect(ipc::socket_path(&paths));
    let (actions, action_rx) = mpsc::channel();
    let state = Rc::new(RefCell::new(snapshot));
    let dispatch = gio::SimpleAction::new("dispatch", Some(glib::VariantTy::STRING));
    let dispatch_tx = actions.clone();
    dispatch.connect_activate(move |_, parameter| {
        if let Some(action) = parameter
            .and_then(|p| p.str())
            .and_then(|json| serde_json::from_str::<Action>(json).ok())
        {
            let _ = dispatch_tx.send(action);
        }
    });
    app.add_action(&dispatch);
    app.set_accels_for_action("win.add", &["<Control>n"]);
    app.set_accels_for_action("win.refresh", &["F5"]);
    app.set_accels_for_action("win.quit", &["<Control>q"]);
    let window = gtk::ApplicationWindow::builder()
        .application(app)
        .title("Rclone Tray")
        .default_width(640)
        .default_height(480)
        .build();

    let header = gtk::HeaderBar::new();
    let heading = gtk::Box::new(gtk::Orientation::Vertical, 0);
    heading.set_valign(gtk::Align::Center);
    let title = label("", &["title"]);
    title.set_xalign(0.5);
    title.set_ellipsize(gtk::pango::EllipsizeMode::End);
    heading.append(&title);
    let subtitle = label("", &["subtitle", "rclone-numeric"]);
    subtitle.set_xalign(0.5);
    subtitle.set_ellipsize(gtk::pango::EllipsizeMode::End);
    heading.append(&subtitle);
    header.set_title_widget(Some(&heading));
    let back = gtk::Button::from_icon_name("go-previous-symbolic");
    back.set_tooltip_text(Some("All mounts"));
    header.pack_start(&back);
    let add = gtk::Button::from_icon_name("list-add-symbolic");
    add.set_tooltip_text(Some("Add mount"));
    add.set_action_name(Some("win.add"));
    header.pack_start(&add);
    let menu_model = gio::Menu::new();
    menu_model.append(Some("Refresh remotes"), Some("win.refresh"));
    menu_model.append(Some("Start at login"), Some("win.autostart"));
    let quit_section = gio::Menu::new();
    quit_section.append(Some("Quit…"), Some("win.quit"));
    menu_model.append_section(None, &quit_section);
    let menu = gtk::MenuButton::new();
    menu.set_icon_name("open-menu-symbolic");
    menu.set_tooltip_text(Some("Menu"));
    menu.set_menu_model(Some(&menu_model));
    header.pack_end(&menu);
    window.set_titlebar(Some(&header));

    let banner_line = gtk::Box::new(gtk::Orientation::Horizontal, ROW_GAP);
    banner_line.add_css_class("rclone-banner");
    let banner_icon = gtk::Image::from_icon_name("dialog-warning-symbolic");
    banner_icon.add_css_class("rclone-warning");
    banner_icon.set_valign(gtk::Align::Start);
    banner_icon.set_margin_top(ROW_GAP);
    banner_icon.set_margin_start(INSET);
    banner_line.append(&banner_icon);
    let message = label("", &[]);
    message.set_wrap(true);
    message.set_wrap_mode(gtk::pango::WrapMode::WordChar);
    message.set_max_width_chars(MEASURE);
    selectable(&message);
    message.set_hexpand(true);
    message.set_margin_top(ROW_GAP);
    message.set_margin_bottom(ROW_GAP);
    message.set_margin_end(INSET);
    banner_line.append(&message);
    let cancel_quit = gtk::Button::with_label("Cancel quit");
    cancel_quit.set_valign(gtk::Align::Center);
    cancel_quit.set_margin_end(INSET);
    cancel_quit.set_visible(false);
    let tx = requests.clone();
    cancel_quit.connect_clicked(move |_| {
        let _ = tx.send(Request::CancelQuit);
    });
    banner_line.append(&cancel_quit);
    let banner = banner_line;
    banner.set_visible(false);

    let list = gtk::ListBox::new();
    list.add_css_class("rclone-ledger");
    list.set_selection_mode(gtk::SelectionMode::None);
    list.set_valign(gtk::Align::Start);
    let scroll = gtk::ScrolledWindow::builder()
        .vexpand(true)
        .hscrollbar_policy(gtk::PolicyType::Never)
        .child(&list)
        .build();
    let empty = gtk::Box::new(gtk::Orientation::Vertical, GROUP_GAP);
    empty.set_vexpand(true);
    empty.set_valign(gtk::Align::Center);
    empty.set_halign(gtk::Align::Center);
    let empty_title = label("No mounts yet", &["rclone-empty-title"]);
    empty_title.set_xalign(0.5);
    empty.append(&empty_title);
    let first = gtk::Button::with_label("Add mount");
    first.add_css_class("suggested-action");
    first.set_action_name(Some("win.add"));
    first.set_halign(gtk::Align::Center);
    empty.append(&first);
    let overview = gtk::Box::new(gtk::Orientation::Vertical, 0);
    overview.append(&scroll);
    overview.append(&empty);

    let detail = Detail::new(&actions);
    let pages = gtk::Stack::new();
    pages.set_transition_duration(180);
    pages.add_named(&overview, Some(LIST_PAGE));
    pages.add_named(&detail.root, Some(DETAIL_PAGE));
    pages.set_vexpand(true);
    let content = gtk::Box::new(gtk::Orientation::Vertical, 0);
    content.append(&banner);
    content.append(&pages);
    window.set_child(Some(&content));

    let home = glib::home_dir();
    let ui = Rc::new(Ui {
        window: window.clone(),
        pages,
        back: back.clone(),
        add,
        menu,
        title,
        subtitle,
        banner,
        message,
        cancel_quit,
        empty,
        scroll,
        list,
        rows: RefCell::new(HashMap::new()),
        widths: gtk::SizeGroup::new(gtk::SizeGroupMode::Horizontal),
        detail,
        state: state.clone(),
        paths: paths.clone(),
        home,
        requests: requests.clone(),
        actions: actions.clone(),
    });

    let details = gio::SimpleAction::new("details", Some(glib::VariantTy::UINT64));
    let weak = Rc::downgrade(&ui);
    details.connect_activate(move |_, parameter| {
        if let (Some(ui), Some(id)) = (weak.upgrade(), parameter.and_then(|p| p.get::<u64>())) {
            ui.open_detail(id, UPLOADS);
        }
    });
    window.add_action(&details);
    let weak = Rc::downgrade(&ui);
    let parent = window.clone();
    let tx = requests.clone();
    window_action(&window, "add", move || {
        if let Some(ui) = weak.upgrade() {
            editor(&parent, &ui.state.borrow(), None, tx.clone());
        }
    });
    let tx = requests.clone();
    window_action(&window, "refresh", move || {
        let _ = tx.send(Request::Refresh);
    });
    let tx = actions.clone();
    window_action(&window, "quit", move || {
        let _ = tx.send(Action::Quit);
    });
    let autostart =
        gio::SimpleAction::new_stateful("autostart", None, &paths.autostart.exists().to_variant());
    let weak = Rc::downgrade(&ui);
    autostart.connect_activate(move |action, _| {
        let enabled = !action
            .state()
            .and_then(|s| s.get::<bool>())
            .unwrap_or(false);
        let Some(ui) = weak.upgrade() else { return };
        match ui.paths.set_autostart(enabled) {
            Ok(()) => action.set_state(&enabled.to_variant()),
            Err(error) => ui.say(&format!("Cannot change login startup: {error}")),
        }
    });
    window.add_action(&autostart);

    let weak = Rc::downgrade(&ui);
    back.connect_clicked(move |_| {
        if let Some(ui) = weak.upgrade() {
            ui.close_detail(ui.window.gets_focus_visible());
        }
    });
    let weak = Rc::downgrade(&ui);
    let parent = window.clone();
    let tx = requests.clone();
    ui.detail.edit.connect_clicked(move |_| {
        let Some(ui) = weak.upgrade() else { return };
        let state = ui.state.borrow();
        let id = ui.detail.id.get();
        let profile = state.config.profiles.iter().find(|p| Some(p.id) == id);
        if let Some(profile) = profile {
            editor(&parent, &state, Some(profile.clone()), tx.clone());
        }
    });
    let weak = Rc::downgrade(&ui);
    let parent = window.clone();
    let tx = requests.clone();
    ui.detail.remove.connect_clicked(move |_| {
        let Some(ui) = weak.upgrade() else { return };
        let Some(id) = ui.detail.id.get() else { return };
        let tx = tx.clone();
        confirm(
            &parent,
            Confirm {
                title: format!("Remove {}?", ui.name(id)),
                detail: "Remote files and the local cache are not deleted.",
                verb: "Remove",
                back: "Cancel",
                destructive: true,
            },
            move || {
                let _ = tx.send(Request::Delete(id));
            },
        );
    });
    let weak = Rc::downgrade(&ui);
    ui.detail
        .panes
        .connect_visible_child_name_notify(move |panes| {
            if panes.visible_child_name().as_deref() == Some(LOG)
                && let Some(ui) = weak.upgrade()
            {
                ui.detail.load_log(&ui.paths);
            }
        });
    let keys = gtk::EventControllerKey::new();
    let weak = Rc::downgrade(&ui);
    keys.connect_key_pressed(move |_, key, _, modifiers| {
        let leave = key == gtk::gdk::Key::Escape
            || (key == gtk::gdk::Key::Left && modifiers == gtk::gdk::ModifierType::ALT_MASK);
        match weak.upgrade() {
            Some(ui) if leave && ui.detail.id.get().is_some() => {
                ui.close_detail(true);
                glib::Propagation::Stop
            }
            _ => glib::Propagation::Proceed,
        }
    });
    window.add_controller(keys);

    let application = app.clone();
    window.connect_close_request(move |_| {
        application.quit();
        glib::Propagation::Proceed
    });
    let first = state.borrow().clone();
    ui.apply(first);
    window.present();
    let application = app.clone();
    glib::timeout_add_local(Duration::from_millis(100), move || {
        while let Ok(event) = events.try_recv() {
            match event {
                Event::State(snapshot) => ui.apply(snapshot),
                Event::Problem(problem) => ui.say(&problem),
                Event::Quit => {
                    application.quit();
                    return glib::ControlFlow::Break;
                }
            }
        }
        while let Ok(action) = action_rx.try_recv() {
            ui.handle(action);
        }
        glib::ControlFlow::Continue
    });
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

fn field(
    form: &gtk::Box,
    text: &str,
    widget: &impl IsA<gtk::Widget>,
    target: &impl IsA<gtk::Widget>,
    hint: Option<&str>,
) {
    let title = label(text, &["heading"]);
    title.set_use_underline(true);
    title.set_mnemonic_widget(Some(target));
    let group = gtk::Box::new(gtk::Orientation::Vertical, FIELD_GAP);
    group.append(&title);
    group.append(widget);
    if let Some(hint) = hint {
        let hint = label(hint, &["rclone-muted"]);
        hint.set_wrap(true);
        group.append(&hint);
    }
    form.append(&group);
}

fn editor(
    parent: &gtk::ApplicationWindow,
    state: &Snapshot,
    profile: Option<Profile>,
    tx: mpsc::Sender<Request>,
) {
    let dialog = gtk::Window::builder()
        .title(if profile.is_some() {
            "Edit mount"
        } else {
            "Add mount"
        })
        .transient_for(parent)
        .modal(true)
        .default_width(480)
        .default_height(520)
        .build();
    let header = gtk::HeaderBar::new();
    header.set_show_title_buttons(false);
    let cancel = gtk::Button::with_label("Cancel");
    let d = dialog.clone();
    cancel.connect_clicked(move |_| d.close());
    header.pack_start(&cancel);
    let save = gtk::Button::with_label("Save");
    save.add_css_class("suggested-action");
    header.pack_end(&save);
    dialog.set_titlebar(Some(&header));
    dialog.set_default_widget(Some(&save));

    let form = gtk::Box::new(gtk::Orientation::Vertical, GROUP_GAP);
    style::inset(&form, INSET, INSET);
    let error = wrapped(&["rclone-danger"]);
    form.append(&error);
    let name = gtk::Entry::new();
    name.set_placeholder_text(Some("Work Drive"));
    // A remote that left rclone's config stays listed, so saving cannot silently switch to
    // another account. Validation then explains the problem.
    let mut choices = state.remotes.clone();
    if let Some(p) = &profile
        && !choices.contains(&p.remote)
    {
        choices.insert(0, p.remote.clone());
    }
    let names: Vec<_> = choices.iter().map(String::as_str).collect();
    let remote = gtk::DropDown::from_strings(&names);
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
        if let Some(index) = choices.iter().position(|r| *r == p.remote) {
            remote.set_selected(index as u32);
        }
    }
    field(&form, "_Name", &name, &name, None);
    field(&form, "_Remote", &remote, &remote, None);
    field(&form, "Remote _folder (optional)", &folder, &folder, None);
    field(&form, "_Mount folder", &mount_row, &mountpoint, None);
    field(
        &form,
        "_Cache target (GiB)",
        &cache,
        &cache,
        Some("A target, not a hard limit. Open files and pending uploads can exceed it."),
    );
    form.append(&auto);
    for entry in [&name, &folder, &mountpoint] {
        entry.set_activates_default(true);
    }
    let d = dialog.clone();
    let remotes = state.remotes.clone();
    let body = form.clone();
    save.connect_clicked(move |save| {
        let p = Profile {
            id: profile.as_ref().map_or(0, |p| p.id),
            name: name.text().trim().into(),
            remote: choices
                .get(remote.selected() as usize)
                .cloned()
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
        body.set_sensitive(false);
        error.set_visible(false);
        let dialog = d.downgrade();
        let body = body.clone();
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
            body.set_sensitive(true);
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
        .child(&form)
        .build();
    dialog.set_child(Some(&scroll));
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

struct Confirm {
    title: String,
    detail: &'static str,
    verb: &'static str,
    back: &'static str,
    destructive: bool,
}

fn confirm(parent: &gtk::ApplicationWindow, request: Confirm, yes: impl Fn() + 'static) {
    let dialog = gtk::MessageDialog::builder()
        .transient_for(parent)
        .modal(true)
        .text(&request.title)
        .secondary_text(request.detail)
        .build();
    dialog.add_button(request.back, gtk::ResponseType::Cancel);
    let accept = dialog.add_button(request.verb, gtk::ResponseType::Ok);
    if request.destructive {
        accept.add_css_class("destructive-action");
    }
    dialog.set_default_response(gtk::ResponseType::Cancel);
    dialog.connect_response(move |dialog, response| {
        if response == gtk::ResponseType::Ok {
            yes();
        }
        dialog.close();
    });
    dialog.present();
}

fn open_uri(uri: &str, message: &gtk::Label, banner: &gtk::Box) {
    let label = message.clone();
    let banner = banner.clone();
    gio::AppInfo::launch_default_for_uri_async(
        uri,
        None::<&gio::AppLaunchContext>,
        None::<&gio::Cancellable>,
        move |result| {
            if let Err(e) = result {
                label.set_text(&format!("Cannot open folder: {e}"));
                banner.set_visible(true);
            }
        },
    );
}

fn read_log(paths: &Paths, id: u64) -> String {
    use std::io::{Read, Seek, SeekFrom};
    let path = paths.logs.join(format!("{id}.log"));
    (|| {
        let mut file = std::fs::File::open(&path)?;
        let length = file.metadata()?.len();
        file.seek(SeekFrom::Start(length.saturating_sub(65536)))?;
        let mut bytes = vec![];
        file.read_to_end(&mut bytes)?;
        Ok::<_, std::io::Error>(String::from_utf8_lossy(&bytes).into_owned())
    })()
    .unwrap_or_else(|_| "No log yet. A log appears after the first mount attempt.".into())
}
