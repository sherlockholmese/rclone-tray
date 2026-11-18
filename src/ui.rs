//! The settings window: mounts in a sidebar, the selected one in the content pane.
use crate::{editor::Editor, style};
use gio::prelude::*;
use gpui_kit::component::{
    ActiveTheme as _, Collapsible, Disableable as _, Icon, IconName, InteractiveElementExt as _,
    Sizable as _, StyledExt as _, TITLE_BAR_HEIGHT, Theme, ThemeMode, TitleBar, WindowExt as _,
    button::{Button, ButtonVariants as _},
    dialog::{DialogButtonProps, DialogFooter},
    h_flex,
    menu::{ContextMenuExt as _, DropdownMenu as _, PopupMenu, PopupMenuItem},
    notification::Notification,
    scroll::{ScrollableElement as _, ScrollbarMode},
    sidebar::{Sidebar, SidebarItem},
    spinner::Spinner,
    switch::Switch,
    tab::{Tab, TabBar},
    v_flex,
};
use gpui_kit::{assets::IconName as AssetIcon, prelude::*, *};
use rclone_tray::{
    backend::{self, Event, Request, Snapshot, Status},
    config::{Config, Paths, Profile},
    control::byte_size,
    ipc::{self, InstanceGuard},
    tray::Action,
    view::{self, Primary, Tone},
};
use std::{
    collections::HashMap,
    path::PathBuf,
    rc::Rc,
    sync::mpsc::{self, Receiver, Sender, TryRecvError},
    time::Duration,
};

pub const CONTEXT: &str = "RcloneTray";
gpui_kit::actions!(
    rclone_tray,
    [AddMount, Refresh, Quit, SelectPrevious, SelectNext]
);

enum Link {
    Connected(Sender<Request>, Receiver<Event>),
    /// `error` is shown only after a start attempt fails; a stopped backend is the usual case.
    Offline {
        error: Option<String>,
        starting: bool,
    },
}

/// The window's appearance. Kept beside the profiles so it survives restarts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ThemeChoice {
    System,
    Light,
    Dark,
}

impl ThemeChoice {
    const ALL: [Self; 3] = [Self::System, Self::Light, Self::Dark];

    fn label(self) -> &'static str {
        match self {
            Self::System => "System theme",
            Self::Light => "Light theme",
            Self::Dark => "Dark theme",
        }
    }

    fn key(self) -> &'static str {
        match self {
            Self::System => "system",
            Self::Light => "light",
            Self::Dark => "dark",
        }
    }

    fn parse(text: &str) -> Self {
        Self::ALL
            .into_iter()
            .find(|choice| choice.key() == text.trim())
            .unwrap_or(Self::System)
    }

    fn file(paths: &Paths) -> PathBuf {
        paths.config.with_file_name("theme")
    }

    fn load(paths: &Paths) -> Self {
        std::fs::read_to_string(Self::file(paths)).map_or(Self::System, |text| Self::parse(&text))
    }

    fn save(self, paths: &Paths) -> Result<(), String> {
        let file = Self::file(paths);
        std::fs::create_dir_all(file.parent().unwrap()).map_err(|e| e.to_string())?;
        std::fs::write(file, format!("{}\n", self.key())).map_err(|e| e.to_string())
    }

    fn apply(self, window: &mut Window, cx: &mut App) {
        match self {
            Self::System => Theme::sync_system_appearance(Some(window), cx),
            Self::Light => Theme::change(ThemeMode::Light, Some(window), cx),
            Self::Dark => Theme::change(ThemeMode::Dark, Some(window), cx),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Pane {
    Uploads,
    Downloads,
    Log,
}

pub struct Shell {
    paths: Paths,
    home: Option<PathBuf>,
    state: Snapshot,
    link: Link,
    incoming: Option<Receiver<String>>,
    _guard: Option<InstanceGuard>,
    /// The mount shown in the content pane. Always one while any mount exists.
    selected: Option<u64>,
    pane: Pane,
    notice: Option<String>,
    log: SharedString,
    log_scroll: ScrollHandle,
    queue_scroll: UniformListScrollHandle,
    focus: FocusHandle,
    autostart: bool,
    theme: ThemeChoice,
    /// Measured cache size of unmounted profiles, by id. Mounted ones report it live.
    usage: HashMap<u64, u64>,
    /// Upload batch of each active mount, for overall progress that survives finished files.
    batches: HashMap<u64, view::UploadBatch>,
    clearing: Option<u64>,
    /// A press on the sidebar header that becomes a window move once the pointer moves.
    drag_armed: bool,
    editor: Option<WeakEntity<Editor>>,
    _appearance: Subscription,
}

fn empty_snapshot() -> Snapshot {
    Snapshot {
        config: Config::default(),
        remotes: vec![],
        statuses: Default::default(),
        activity: Default::default(),
        message: String::new(),
        quitting: false,
    }
}

/// App-wide presentation settings, applied once before the window opens.
pub fn init(cx: &mut App) {
    // Follow a desktop that hides scrollbars until scrolling. Otherwise keep them visible, so a
    // long queue or log shows at a glance that there is more and where you are in it.
    let mode = if cx.should_auto_hide_scrollbars() {
        ScrollbarMode::Scrolling
    } else {
        ScrollbarMode::Always
    };
    Theme::set_scrollbar_mode(mode, cx);
}

pub fn open(
    cx: &mut App,
    paths: Paths,
    incoming: Option<(Receiver<String>, InstanceGuard)>,
    action: Action,
) {
    let options = WindowOptions {
        window_bounds: Some(WindowBounds::centered(size(px(840.), px(560.)), cx)),
        // The sidebar plus a content pane that still fits a mount's title-bar actions.
        window_min_size: Some(size(px(600.), px(360.))),
        window_decorations: Some(WindowDecorations::Client),
        app_id: Some("io.github.rclone_tray.Settings".into()),
        titlebar: Some(TitlebarOptions {
            title: Some("Rclone Tray".into()),
            ..TitleBar::title_bar_options()
        }),
        ..TitleBar::window_options()
    };
    let opened = gpui_kit::open_window(options, cx, |window, cx| {
        cx.new(|cx| Shell::new(paths, incoming, action, window, cx))
    });
    if let Err(error) = opened {
        eprintln!("Cannot open the settings window: {error}");
        cx.quit();
    }
}

impl Shell {
    fn new(
        paths: Paths,
        incoming: Option<(Receiver<String>, InstanceGuard)>,
        action: Action,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let theme = ThemeChoice::load(&paths);
        theme.apply(window, cx);
        let appearance = cx.observe_window_appearance(window, |this: &mut Self, window, cx| {
            if this.theme == ThemeChoice::System {
                Theme::sync_system_appearance(Some(window), cx);
            }
        });
        let (incoming, guard) = match incoming {
            Some((rx, guard)) => (Some(rx), Some(guard)),
            None => (None, None),
        };
        let socket = ipc::socket_path(&paths);
        let (link, state) = match ipc::snapshot(&socket) {
            Ok(state) => {
                let (requests, events) = ipc::connect(socket);
                (Link::Connected(requests, events), state)
            }
            Err(_) => (
                Link::Offline {
                    error: None,
                    starting: false,
                },
                empty_snapshot(),
            ),
        };
        let focus = cx.focus_handle();
        window.focus(&focus, cx);
        let mut shell = Self {
            autostart: paths.autostart.exists(),
            theme,
            home: std::env::var_os("HOME").map(PathBuf::from),
            paths,
            state,
            link,
            incoming,
            _guard: guard,
            selected: None,
            pane: Pane::Uploads,
            notice: None,
            log: SharedString::default(),
            log_scroll: ScrollHandle::new(),
            queue_scroll: UniformListScrollHandle::new(),
            focus,
            usage: HashMap::new(),
            batches: HashMap::new(),
            clearing: None,
            drag_armed: false,
            editor: None,
            _appearance: appearance,
        };
        shell.reconcile_selection(window, cx);
        shell.handle(action, window, cx);
        cx.spawn_in(window, async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(100))
                    .await;
                if this
                    .update_in(cx, |this, window, cx| this.pump(window, cx))
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();
        shell
    }

    fn pump(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let mut actions = vec![];
        if let Some(incoming) = &self.incoming {
            while let Ok(line) = incoming.try_recv() {
                actions.push(serde_json::from_str(&line).unwrap_or(Action::Show));
            }
        }
        for action in actions {
            self.handle(action, window, cx);
        }
        let mut events = vec![];
        if let Link::Connected(_, receiver) = &self.link {
            loop {
                match receiver.try_recv() {
                    Ok(event) => events.push(event),
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => {
                        events.push(Event::Quit);
                        break;
                    }
                }
            }
        }
        for event in events {
            match event {
                Event::State(snapshot) => self.apply(snapshot, window, cx),
                Event::Problem(problem) => {
                    self.notice = Some(problem);
                    cx.notify();
                }
                Event::Quit => {
                    cx.quit();
                    return;
                }
            }
        }
    }

    fn apply(&mut self, snapshot: Snapshot, window: &mut Window, cx: &mut Context<Self>) {
        // A mount that just stopped has a cache worth measuring again.
        if let Some(id) = self.selected
            && self.status(id).active()
            && !snapshot.statuses.get(&id).is_some_and(Status::active)
        {
            self.measure(id, cx);
        }
        if snapshot.remotes != self.state.remotes
            && let Some(editor) = self.editor.as_ref().and_then(|e| e.upgrade())
        {
            let remotes = snapshot.remotes.clone();
            editor.update(cx, |editor, cx| editor.set_remotes(remotes, window, cx));
        }
        self.state = snapshot;
        let state = &self.state;
        let active = |id: &u64| state.statuses.get(id).is_some_and(Status::active);
        self.batches.retain(|id, _| active(id));
        for (id, sample) in &state.activity {
            if let Ok(activity) = sample
                && active(id)
            {
                self.batches.entry(*id).or_default().observe(activity);
            }
        }
        self.reconcile_selection(window, cx);
        cx.notify();
    }

    /// Keeps a mount selected while any exist: the current one, or the first after a removal.
    fn reconcile_selection(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let profiles = &self.state.config.profiles;
        let kept = self
            .selected
            .filter(|id| profiles.iter().any(|p| p.id == *id));
        let next = kept.or_else(|| profiles.first().map(|p| p.id));
        if next != self.selected {
            self.selected = None;
            if let Some(id) = next {
                self.select(id, None, window, cx);
            }
        }
    }

    fn step_selection(&mut self, offset: isize, window: &mut Window, cx: &mut Context<Self>) {
        let profiles = &self.state.config.profiles;
        let Some(index) = self
            .selected
            .and_then(|id| profiles.iter().position(|p| p.id == id))
        else {
            return;
        };
        let target = index.saturating_add_signed(offset).min(profiles.len() - 1);
        let id = profiles[target].id;
        self.select(id, None, window, cx);
    }

    fn send(&mut self, request: Request, cx: &mut Context<Self>) {
        match &self.link {
            Link::Connected(requests, _) => {
                if requests.send(request).is_err() {
                    self.notice = Some("Lost the connection to Rclone Tray.".into());
                }
            }
            Link::Offline { .. } => self.notice = Some("Rclone Tray isn’t running.".into()),
        }
        cx.notify();
    }

    fn profile(&self, id: u64) -> Option<&Profile> {
        self.state.config.profiles.iter().find(|p| p.id == id)
    }

    fn status(&self, id: u64) -> &Status {
        self.state.statuses.get(&id).unwrap_or(&Status::Unmounted)
    }

    fn handle(&mut self, action: Action, window: &mut Window, cx: &mut Context<Self>) {
        match action {
            Action::Show => window.activate_window(),
            Action::Mount(id) => self.send(Request::Mount(id), cx),
            Action::Unmount(id) => {
                window.activate_window();
                self.confirm_unmount(id, window, cx);
            }
            Action::Open(id) => self.open_folder(id, cx),
            Action::Queue(id) => {
                window.activate_window();
                self.select(id, Some(Pane::Uploads), window, cx);
            }
            Action::Log(id) => {
                window.activate_window();
                self.select(id, Some(Pane::Log), window, cx);
            }
            Action::Quit => {
                window.activate_window();
                self.quit(window, cx);
            }
            Action::QuitNow => self.send(Request::QuitNow, cx),
        }
    }

    /// Shows a mount. Without a `pane`, the open tab carries over, so you can compare mounts.
    fn select(&mut self, id: u64, pane: Option<Pane>, window: &mut Window, cx: &mut Context<Self>) {
        if self.profile(id).is_none() {
            return;
        }
        let changed = self.selected != Some(id);
        self.selected = Some(id);
        if changed {
            self.measure(id, cx);
            self.queue_scroll = UniformListScrollHandle::new();
        }
        let pane = pane.unwrap_or(self.pane);
        if changed || pane != self.pane {
            self.select_pane(pane, window, cx);
        }
        cx.notify();
    }

    fn select_pane(&mut self, pane: Pane, window: &mut Window, cx: &mut Context<Self>) {
        self.pane = pane;
        if pane == Pane::Log {
            self.load_log(window, cx);
        }
        cx.notify();
    }

    fn load_log(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(id) = self.selected else { return };
        self.log = read_log(&self.paths, id).into();
        // The newest lines are the useful ones. Reveal them once the text is laid out.
        cx.on_next_frame(window, |this, _, cx| {
            this.log_scroll.scroll_to_bottom();
            cx.notify();
        });
    }

    fn toggle(&mut self, id: u64, on: bool, window: &mut Window, cx: &mut Context<Self>) {
        if on {
            self.send(Request::Mount(id), cx);
        } else {
            self.confirm_unmount(id, window, cx);
        }
    }

    fn confirm_unmount(&mut self, id: u64, window: &mut Window, cx: &mut Context<Self>) {
        let Some(name) = self.profile(id).map(|p| p.name.clone()) else {
            return;
        };
        let cancel = view::primary(self.status(id)) == Primary::Cancel;
        let this = cx.entity().downgrade();
        confirm(
            window,
            cx,
            Confirm {
                title: if cancel {
                    format!("Stop mounting {name}?")
                } else {
                    format!("Unmount {name}?")
                },
                detail: "Close open files first. Cached files are kept.".into(),
                verb: if cancel { "Stop mounting" } else { "Unmount" },
                back: if cancel { "Keep mounting" } else { "Cancel" },
                danger: false,
            },
            move |_, cx| {
                let _ = this.update(cx, |this, cx| this.send(Request::Unmount(id), cx));
            },
        );
    }

    fn confirm_remove(&mut self, id: u64, window: &mut Window, cx: &mut Context<Self>) {
        let Some(name) = self.profile(id).map(|p| p.name.clone()) else {
            return;
        };
        let this = cx.entity().downgrade();
        confirm(
            window,
            cx,
            Confirm {
                title: format!("Remove {name}?"),
                detail: "Remote files and the local cache are kept.".into(),
                verb: "Remove",
                back: "Cancel",
                danger: true,
            },
            move |_, cx| {
                let _ = this.update(cx, |this, cx| this.send(Request::Delete(id), cx));
            },
        );
    }

    fn measure(&mut self, id: u64, cx: &mut Context<Self>) {
        let dir = self.paths.cache.join(id.to_string());
        let task = cx.background_spawn(async move { backend::cache_usage(&dir) });
        cx.spawn(async move |this, cx| {
            let used = task.await;
            let _ = this.update(cx, |this, cx| {
                this.usage.insert(id, used);
                cx.notify();
            });
        })
        .detach();
    }

    fn confirm_clear(&mut self, id: u64, window: &mut Window, cx: &mut Context<Self>) {
        let Some(name) = self.profile(id).map(|p| p.name.clone()) else {
            return;
        };
        let size = self.usage.get(&id).map(|bytes| byte_size(*bytes as f64));
        let this = cx.entity().downgrade();
        confirm(
            window,
            cx,
            Confirm {
                title: format!("Clear cache for {name}?"),
                detail: match size {
                    Some(size) => format!(
                        "Deletes {size} of local copies. Remote files are kept and download again when opened."
                    ),
                    None => "Deletes local copies. Remote files are kept and download again when opened.".into(),
                },
                verb: "Clear cache",
                back: "Cancel",
                danger: true,
            },
            move |window, cx| {
                let _ = this.update(cx, |this, cx| this.clear_cache(id, window, cx));
            },
        );
    }

    fn clear_cache(&mut self, id: u64, window: &mut Window, cx: &mut Context<Self>) {
        let (reply, result) = mpsc::channel();
        self.send(Request::ClearCache(id, reply), cx);
        self.clearing = Some(id);
        cx.spawn_in(window, async move |this, cx| {
            let result = loop {
                cx.background_executor()
                    .timer(Duration::from_millis(50))
                    .await;
                match result.try_recv() {
                    Ok(result) => break result,
                    Err(TryRecvError::Empty) => continue,
                    Err(TryRecvError::Disconnected) => {
                        break Err("Lost the connection to Rclone Tray.".into());
                    }
                }
            };
            let _ = this.update_in(cx, |this, window, cx| {
                this.clearing = None;
                match result {
                    Ok(freed) => {
                        this.usage.insert(id, 0);
                        window.push_notification(
                            Notification::success(format!("Cleared {}", byte_size(freed as f64))),
                            cx,
                        );
                    }
                    Err(error) => this.notice = Some(error),
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn quit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.state.quitting {
            self.send(Request::CancelQuit, cx);
            return;
        }
        if matches!(self.link, Link::Offline { .. }) {
            cx.quit();
            return;
        }
        let this = cx.entity().downgrade();
        confirm(
            window,
            cx,
            Confirm {
                title: "Quit Rclone Tray?".into(),
                detail: "Waits for uploads, then unmounts. Cached files are kept.".into(),
                verb: "Quit",
                back: "Cancel",
                danger: false,
            },
            move |_, cx| {
                let _ = this.update(cx, |this, cx| this.send(Request::Quit, cx));
            },
        );
    }

    fn open_folder(&mut self, id: u64, cx: &mut Context<Self>) {
        let Some(profile) = self.profile(id) else {
            return;
        };
        let uri = gio::File::for_path(&profile.mountpoint).uri().to_string();
        let task = cx.background_spawn(async move {
            gio::AppInfo::launch_default_for_uri(&uri, gio::AppLaunchContext::NONE)
                .map_err(|e| e.to_string())
        });
        cx.spawn(async move |this, cx| {
            if let Err(error) = task.await {
                let _ = this.update(cx, |this, cx| {
                    this.notice = Some(format!("Cannot open folder: {error}"));
                    cx.notify();
                });
            }
        })
        .detach();
    }

    fn edit(&mut self, profile: Option<Profile>, window: &mut Window, cx: &mut Context<Self>) {
        let Link::Connected(requests, _) = &self.link else {
            return;
        };
        if self.state.quitting {
            return;
        }
        let editor = Editor::open(
            &self.state.remotes,
            profile,
            self.home.clone(),
            requests.clone(),
            window,
            cx,
        );
        self.editor = Some(editor.downgrade());
    }

    fn set_autostart(&mut self, enabled: bool, cx: &mut Context<Self>) {
        match self.paths.set_autostart(enabled) {
            Ok(()) => self.autostart = enabled,
            Err(error) => self.notice = Some(format!("Cannot change login startup: {error}")),
        }
        cx.notify();
    }

    fn set_theme(&mut self, theme: ThemeChoice, window: &mut Window, cx: &mut Context<Self>) {
        self.theme = theme;
        theme.apply(window, cx);
        if let Err(error) = theme.save(&self.paths) {
            self.notice = Some(format!("Cannot remember the theme: {error}"));
        }
        cx.notify();
    }

    fn start_backend(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Link::Offline { starting, .. } = &mut self.link else {
            return;
        };
        *starting = true;
        cx.notify();
        let socket = ipc::socket_path(&self.paths);
        let task = cx.background_spawn(async move {
            let exe = std::env::current_exe()
                .map_err(|e| e.to_string())?
                .with_file_name("rclone-tray");
            let mut child = std::process::Command::new(exe)
                .arg("--background")
                .spawn()
                .map_err(|e| format!("Cannot start rclone-tray: {e}"))?;
            std::thread::spawn(move || child.wait());
            let mut last = String::new();
            for _ in 0..40 {
                std::thread::sleep(Duration::from_millis(250));
                match ipc::snapshot(&socket) {
                    Ok(state) => return Ok(state),
                    Err(error) => last = error,
                }
            }
            Err(last)
        });
        cx.spawn_in(window, async move |this, cx| {
            let result = task.await;
            let _ = this.update_in(cx, |this, window, cx| {
                match result {
                    Ok(state) => {
                        let (requests, events) = ipc::connect(ipc::socket_path(&this.paths));
                        this.link = Link::Connected(requests, events);
                        this.state = state;
                        this.reconcile_selection(window, cx);
                    }
                    Err(error) => {
                        this.link = Link::Offline {
                            error: Some(error),
                            starting: false,
                        }
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    // MARK: Actions bound to keys

    fn on_add(&mut self, _: &AddMount, window: &mut Window, cx: &mut Context<Self>) {
        self.edit(None, window, cx);
    }

    fn on_refresh(&mut self, _: &Refresh, _: &mut Window, cx: &mut Context<Self>) {
        if matches!(self.link, Link::Connected(..)) {
            self.send(Request::Refresh, cx);
        }
    }

    fn on_quit(&mut self, _: &Quit, window: &mut Window, cx: &mut Context<Self>) {
        self.quit(window, cx);
    }

    fn on_previous(&mut self, _: &SelectPrevious, window: &mut Window, cx: &mut Context<Self>) {
        self.step_selection(-1, window, cx);
    }

    fn on_next(&mut self, _: &SelectNext, window: &mut Window, cx: &mut Context<Self>) {
        self.step_selection(1, window, cx);
    }
}

// MARK: Rendering

impl Render for Shell {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let body = match (&self.link, self.selected) {
            (Link::Offline { error, starting }, _) => self
                .render_offline(error.clone(), *starting, cx)
                .into_any_element(),
            (_, Some(id)) => self.render_mount(id, window, cx).into_any_element(),
            (_, None) => self.render_empty(cx).into_any_element(),
        };
        let plate = v_flex()
            .size_full()
            .overflow_hidden()
            .bg(style::plate(cx))
            .rounded(cx.theme().radius_lg)
            .border_1()
            .border_color(style::rule(cx))
            .shadow_md()
            .child(self.render_title_bar(cx))
            .children(self.render_banners(cx))
            .child(body);
        h_flex()
            .id("shell")
            .key_context(CONTEXT)
            .track_focus(&self.focus)
            .on_action(cx.listener(Self::on_add))
            .on_action(cx.listener(Self::on_refresh))
            .on_action(cx.listener(Self::on_quit))
            .on_action(cx.listener(Self::on_previous))
            .on_action(cx.listener(Self::on_next))
            .size_full()
            .bg(style::canvas(cx))
            .text_color(cx.theme().foreground)
            .child(self.render_sidebar(cx))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .h_full()
                    .py(style::PLATE_GAP)
                    .pr(style::PLATE_GAP)
                    .child(plate),
            )
    }
}

/// What the menus of one mount may offer right now.
#[derive(Clone, Copy)]
struct MountMenu {
    id: u64,
    active: bool,
    mounted: bool,
    /// The switch's state cannot change: unmounting, mounted elsewhere, or quitting.
    locked: bool,
    cancel: bool,
    quitting: bool,
    cache_empty: bool,
    clearing: bool,
}

impl MountMenu {
    /// `full` adds the actions the content pane already shows, for the sidebar's context menu.
    fn build(self, menu: PopupMenu, this: WeakEntity<Shell>, full: bool) -> PopupMenu {
        let id = self.id;
        let changeable = !self.active && !self.quitting;
        let mut menu = menu;
        if full {
            let (open, toggle) = (this.clone(), this.clone());
            let active = self.active;
            menu = menu
                .item(
                    PopupMenuItem::new("Open folder")
                        .disabled(!self.mounted)
                        .on_click(move |_, _, cx| {
                            let _ = open.update(cx, |this, cx| this.open_folder(id, cx));
                        }),
                )
                .item(
                    PopupMenuItem::new(match (active, self.cancel) {
                        (false, _) => "Mount",
                        (true, true) => "Stop mounting…",
                        (true, false) => "Unmount…",
                    })
                    .disabled(self.locked)
                    .on_click(move |_, window, cx| {
                        let _ = toggle.update(cx, |this, cx| this.toggle(id, !active, window, cx));
                    }),
                )
                .separator();
        } else if self.active {
            menu = menu.label("Unmount to change");
        }
        let (edit, clear, remove) = (this.clone(), this.clone(), this);
        menu.item(PopupMenuItem::new("Edit…").disabled(!changeable).on_click(
            move |_, window, cx| {
                let _ = edit.update(cx, |this, cx| {
                    let profile = this.profile(id).cloned();
                    if profile.is_some() {
                        this.edit(profile, window, cx);
                    }
                });
            },
        ))
        .item(
            PopupMenuItem::new("Clear cache…")
                .disabled(!changeable || self.cache_empty || self.clearing)
                .on_click(move |_, window, cx| {
                    let _ = clear.update(cx, |this, cx| this.confirm_clear(id, window, cx));
                }),
        )
        .item(
            PopupMenuItem::new("Remove…")
                .disabled(!changeable)
                .on_click(move |_, window, cx| {
                    let _ = remove.update(cx, |this, cx| this.confirm_remove(id, window, cx));
                }),
        )
    }
}

impl Shell {
    fn mount_menu(&self, id: u64) -> MountMenu {
        let status = self.status(id);
        MountMenu {
            id,
            active: status.active(),
            mounted: view::primary(status) == Primary::Open,
            locked: matches!(status, Status::Unmounting | Status::External) || self.state.quitting,
            cancel: view::primary(status) == Primary::Cancel,
            quitting: self.state.quitting,
            cache_empty: self.usage.get(&id) == Some(&0),
            clearing: self.clearing.is_some(),
        }
    }

    /// The sidebar's header: app-wide actions. It drags the window like a title bar does.
    fn render_sidebar_header(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let connected = matches!(self.link, Link::Connected(..));
        let this = cx.entity().downgrade();
        let autostart = self.autostart;
        let current = self.theme;
        let settings = Button::new("settings")
            .ghost()
            .small()
            .icon(IconName::Settings)
            .tooltip("Settings")
            .dropdown_menu_with_anchor(Anchor::TopLeft, move |menu, _, _| {
                let (refresh, login, quit) = (this.clone(), this.clone(), this.clone());
                let mut menu = menu
                    .item(
                        PopupMenuItem::new("Refresh remotes").on_click(move |_, window, cx| {
                            let _ = refresh
                                .update(cx, |this, cx| this.on_refresh(&Refresh, window, cx));
                        }),
                    )
                    .item(
                        PopupMenuItem::new("Start at login")
                            .checked(autostart)
                            .on_click(move |_, _, cx| {
                                let _ =
                                    login.update(cx, |this, cx| this.set_autostart(!autostart, cx));
                            }),
                    )
                    .separator();
                for choice in ThemeChoice::ALL {
                    let this = this.clone();
                    menu = menu.item(
                        PopupMenuItem::new(choice.label())
                            .checked(choice == current)
                            .on_click(move |_, window, cx| {
                                let _ =
                                    this.update(cx, |this, cx| this.set_theme(choice, window, cx));
                            }),
                    );
                }
                menu.separator()
                    .item(PopupMenuItem::new("Quit…").on_click(move |_, window, cx| {
                        let _ = quit.update(cx, |this, cx| this.quit(window, cx));
                    }))
            });
        let add = Button::new("add")
            .ghost()
            .small()
            .icon(IconName::Plus)
            .tooltip("Add mount (Ctrl+N)")
            .disabled(!connected || self.state.quitting)
            .on_click(cx.listener(|this, _, window, cx| this.edit(None, window, cx)));
        // Sits level with the plate's title bar, which starts one plate gap down.
        h_flex()
            .id("sidebar-header")
            .flex_shrink_0()
            .h(TITLE_BAR_HEIGHT + style::PLATE_GAP)
            .pt(style::PLATE_GAP)
            .pl(style::SIDEBAR_INSET)
            .pr_2()
            .justify_between()
            .window_control_area(WindowControlArea::Drag)
            .on_double_click(|_, window, _| window.zoom_window())
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, _, _| this.drag_armed = true),
            )
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _, _, _| this.drag_armed = false),
            )
            .on_mouse_move(cx.listener(|this, _, window, _| {
                if std::mem::take(&mut this.drag_armed) {
                    window.start_window_move();
                }
            }))
            .child(div().text_sm().font_semibold().child("Rclone Tray"))
            .child(h_flex().gap_1().child(add).child(settings))
    }

    /// The plate's title bar: the selected mount, its switch, and its actions.
    fn render_title_bar(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let connected = matches!(self.link, Link::Connected(..));
        let selected = self
            .selected
            .filter(|_| connected)
            .and_then(|id| self.profile(id).cloned());
        let content = selected.map(|profile| {
            let id = profile.id;
            let status = self.status(id).clone();
            let menu = self.mount_menu(id);
            let shell = cx.entity().downgrade();
            let busy = matches!(
                status,
                Status::Mounting | Status::Recovering | Status::Unmounting
            );
            let switch = self.mount_switch(id, &status, &profile.name, self.state.quitting, cx);
            let muted = style::muted(cx);
            h_flex()
                .flex_1()
                .min_w_0()
                .h_full()
                .gap_3()
                .justify_between()
                .child(
                    div()
                        .min_w_0()
                        .truncate()
                        .text_sm()
                        .font_semibold()
                        .child(profile.name.clone()),
                )
                .child(
                    h_flex()
                        .flex_shrink_0()
                        .gap_2()
                        .when(busy, |this| {
                            this.child(Spinner::new().xsmall().color(muted))
                        })
                        .child(switch.mr_2())
                        .child(
                            Button::new("open")
                                .outline()
                                .small()
                                .icon(IconName::FolderOpen)
                                .label("Open folder")
                                .disabled(!menu.mounted)
                                .on_click(
                                    cx.listener(move |this, _, _, cx| this.open_folder(id, cx)),
                                ),
                        )
                        .child(
                            Button::new("mount-menu")
                                .ghost()
                                .small()
                                .icon(IconName::Ellipsis)
                                .tooltip("More")
                                .dropdown_menu_with_anchor(Anchor::TopRight, move |m, _, _| {
                                    menu.build(m, shell.clone(), false)
                                }),
                        ),
                )
        });
        TitleBar::new()
            .pl(style::INSET)
            .pr_1()
            .border_b_0()
            .bg(transparent_black())
            .children(content)
    }

    fn render_sidebar(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let items: Vec<_> = self
            .state
            .config
            .profiles
            .clone()
            .into_iter()
            .map(|profile| self.sidebar_item(profile, cx))
            .collect();
        v_flex()
            .flex_shrink_0()
            .w(style::SIDEBAR)
            .h_full()
            .child(self.render_sidebar_header(cx))
            .child(
                div().flex_1().min_h_0().child(
                    Sidebar::new("mounts")
                        .collapsible(false)
                        .w_full()
                        .border_r_0()
                        .children(items),
                ),
            )
    }

    /// A mount in the sidebar. The icon says whether it is mounted or needs you; the suffix
    /// carries the one live fact worth a glance.
    fn sidebar_item(&mut self, profile: Profile, cx: &mut Context<Self>) -> MountItem {
        let id = profile.id;
        let status = self.status(id).clone();
        let sample = self.state.activity.get(&id);
        let batch = self.batches.get(&id);
        let (now, rest, tone) = view::ledger_lines(&status, sample, batch);
        let icon = match (tone, status.active()) {
            (Tone::Warning | Tone::Danger, _) => {
                Icon::new(AssetIcon::CloudAlert).text_color(style::tone(tone, cx))
            }
            (_, true) => Icon::new(AssetIcon::Cloud),
            (_, false) => Icon::new(AssetIcon::CloudOff).text_color(style::muted(cx)),
        };
        let note = (status == Status::Mounted && tone == Tone::Ok)
            .then(|| now.into_iter().next().or_else(|| rest.into_iter().next()))
            .flatten();
        let menu = self.mount_menu(id);
        let this = cx.entity().downgrade();
        MountItem {
            name: profile.name.into(),
            icon,
            muted: !status.active(),
            selected: self.selected == Some(id),
            suffix: match note {
                _ if tone == Tone::Busy => Suffix::Busy,
                Some(note) => Suffix::Note(note.into()),
                None => Suffix::None,
            },
            on_click: Rc::new(cx.listener(move |this, _: &ClickEvent, window, cx| {
                this.select(id, None, window, cx)
            })),
            menu: Rc::new(move |m, _, _| menu.build(m, this.clone(), true)),
        }
    }

    fn render_banners(&mut self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let mut banners = vec![];
        if self.state.quitting {
            let text = if self.state.message.is_empty() {
                "Quitting…".to_string()
            } else {
                self.state.message.clone()
            };
            let waiting = !text.starts_with("Unmounting");
            banners.push(
                banner("quitting", &text, Tone::Busy, cx)
                    .when(waiting, |this| {
                        this.child(
                            Button::new("quit-now")
                                .outline()
                                .xsmall()
                                .label("Quit now")
                                .tooltip("Pending uploads resume the next time you mount")
                                .on_click(
                                    cx.listener(|this, _, _, cx| this.send(Request::QuitNow, cx)),
                                ),
                        )
                    })
                    .child(
                        Button::new("cancel-quit")
                            .outline()
                            .xsmall()
                            .label("Cancel quit")
                            .on_click(
                                cx.listener(|this, _, _, cx| this.send(Request::CancelQuit, cx)),
                            ),
                    )
                    .into_any_element(),
            );
        } else if !self.state.message.is_empty() {
            let tone = if self.state.message.ends_with('…') {
                Tone::Busy
            } else {
                Tone::Warning
            };
            banners.push(banner("message", &self.state.message, tone, cx).into_any_element());
        }
        if let Some(notice) = self.notice.clone() {
            banners.push(
                banner("notice", &notice, Tone::Danger, cx)
                    .child(
                        Button::new("dismiss")
                            .ghost()
                            .xsmall()
                            .icon(IconName::Close)
                            .tooltip("Dismiss")
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.notice = None;
                                cx.notify();
                            })),
                    )
                    .into_any_element(),
            );
        }
        banners
    }

    fn render_offline(
        &self,
        error: Option<String>,
        starting: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        v_flex()
            .flex_1()
            .items_center()
            .justify_center()
            .gap_4()
            .px(style::INSET)
            .child(
                Icon::new(IconName::HardDrive)
                    .size(px(32.))
                    .text_color(style::muted(cx)),
            )
            .child(
                v_flex()
                    .items_center()
                    .gap_1()
                    .child(
                        div()
                            .text_lg()
                            .font_semibold()
                            .child("Rclone Tray isn’t running"),
                    )
                    .children(error.map(|error| {
                        div()
                            .max_w(px(420.))
                            .text_sm()
                            .text_center()
                            .text_color(cx.theme().danger)
                            .child(error)
                    })),
            )
            .child(
                Button::new("start")
                    .primary()
                    .label("Start Rclone Tray")
                    .loading(starting)
                    .disabled(starting)
                    .on_click(cx.listener(|this, _, window, cx| this.start_backend(window, cx))),
            )
    }

    fn render_empty(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let discovering = self.state.message.ends_with('…');
        let no_remotes = self.state.remotes.is_empty() && !discovering;
        v_flex()
            .flex_1()
            .items_center()
            .justify_center()
            .gap_4()
            .px(style::INSET)
            .child(
                Icon::new(IconName::HardDrive)
                    .size(px(32.))
                    .text_color(style::muted(cx)),
            )
            .child(
                v_flex()
                    .items_center()
                    .gap_1()
                    .child(div().text_lg().font_semibold().child("No mounts yet"))
                    .when(no_remotes, |this| {
                        this.child(
                            div()
                                .text_sm()
                                .text_color(style::muted(cx))
                                .child("Add a remote with rclone config, then refresh."),
                        )
                    }),
            )
            .child(
                h_flex()
                    .gap_2()
                    .when(no_remotes, |this| {
                        this.child(
                            Button::new("refresh")
                                .outline()
                                .icon(IconName::RefreshCw)
                                .label("Refresh")
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.on_refresh(&Refresh, window, cx)
                                })),
                        )
                    })
                    .child(
                        Button::new("first")
                            .primary()
                            .icon(IconName::Plus)
                            .label("Add mount")
                            .disabled(no_remotes || self.state.quitting)
                            .on_click(
                                cx.listener(|this, _, window, cx| this.edit(None, window, cx)),
                            ),
                    ),
            )
    }

    fn mount_switch(
        &self,
        id: u64,
        status: &Status,
        name: &str,
        quitting: bool,
        cx: &mut Context<Self>,
    ) -> Switch {
        let locked = matches!(status, Status::Unmounting | Status::External) || quitting;
        let tooltip = match status {
            Status::External => "Mounted by another session".to_string(),
            Status::Unmounting => "Unmounting…".to_string(),
            _ if quitting => "Quitting…".to_string(),
            _ if status.active() => format!("Unmount {name}"),
            _ => format!("Mount {name}"),
        };
        // The label names the state the switch is in, so the toggle never stands alone.
        Switch::new(SharedString::from(format!("switch-{id}")))
            .label(status.label().to_string())
            .small()
            .checked(status.active())
            .disabled(locked)
            .tooltip(tooltip.clone())
            .accessibility_label(tooltip)
            .on_click(
                cx.listener(move |this, on: &bool, window, cx| this.toggle(id, *on, window, cx)),
            )
    }

    /// The selected mount: its switch and where it lives, then its queues and log.
    fn render_mount(
        &mut self,
        id: u64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let Some(profile) = self.profile(id).cloned() else {
            return div().into_any_element();
        };
        let status = self.status(id).clone();
        let sample = self.state.activity.get(&id);
        let (_, tone) = view::activity_line(&status, sample);
        let problem = view::cache_warning(sample)
            .or(view::status_hint(&status))
            .map(str::to_string);
        let queues = view::queues(&self.state, id);
        let cache = if self.clearing == Some(id) {
            "Clearing…".to_string()
        } else {
            view::cache_line(&profile, sample, self.usage.get(&id).copied())
        };

        let summary = h_flex()
            .gap_8()
            .items_start()
            .child(fact(
                "Folder",
                view::short_path(&profile.mountpoint, self.home.as_deref()),
                true,
                cx,
            ))
            .child(fact("Remote", view::remote_path(&profile), true, cx))
            .child(fact("Cache", cache, false, cx));

        let count = |pane: &view::QueuePane| (!pane.rows.is_empty()).then_some(pane.rows.len());
        let tab = |label: &'static str, count: Option<usize>, cx: &App| {
            Tab::new().label(label).when_some(count, |tab, count| {
                tab.suffix(
                    div()
                        .ml_1()
                        .text_xs()
                        .text_color(style::muted(cx))
                        .child(count.to_string()),
                )
            })
        };
        let selected = match self.pane {
            Pane::Uploads => 0,
            Pane::Downloads => 1,
            Pane::Log => 2,
        };
        let tabs = TabBar::new("panes")
            .underline()
            .px(style::INSET)
            .selected_index(selected)
            .child(tab("Uploads", count(&queues.uploads), cx))
            .child(tab("Downloads", count(&queues.downloads), cx))
            .child(tab("Log", None, cx))
            .when(self.pane == Pane::Log, |this| {
                this.suffix(
                    Button::new("reload-log")
                        .ghost()
                        .xsmall()
                        .icon(IconName::RefreshCw)
                        .tooltip("Reload log")
                        .on_click(cx.listener(|this, _, window, cx| this.load_log(window, cx))),
                )
            })
            .on_click(cx.listener(|this, index: &usize, window, cx| {
                let pane = match index {
                    0 => Pane::Uploads,
                    1 => Pane::Downloads,
                    _ => Pane::Log,
                };
                this.select_pane(pane, window, cx);
            }));

        let pane = match self.pane {
            Pane::Uploads => self
                .render_queue(id, true, queues.uploads, cx)
                .into_any_element(),
            Pane::Downloads => self
                .render_queue(id, false, queues.downloads, cx)
                .into_any_element(),
            Pane::Log => self.render_log(window, cx).into_any_element(),
        };

        v_flex()
            .flex_1()
            .min_h_0()
            .child(
                v_flex()
                    .px(style::INSET)
                    .pt_1()
                    .pb_4()
                    .gap_3()
                    .when_some(problem, |this, problem| {
                        this.child(
                            h_flex()
                                .gap_2()
                                .items_start()
                                .text_sm()
                                .text_color(style::tone(tone, cx))
                                .when(matches!(tone, Tone::Warning | Tone::Danger), |this| {
                                    this.child(
                                        Icon::new(IconName::TriangleAlert)
                                            .small()
                                            .flex_shrink_0()
                                            .mt(px(2.)),
                                    )
                                })
                                .child(div().flex_1().min_w_0().child(problem)),
                        )
                    })
                    .child(summary),
            )
            .child(tabs)
            .child(pane)
            .into_any_element()
    }

    fn render_queue(
        &mut self,
        id: u64,
        uploads: bool,
        pane: view::QueuePane,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        if pane.rows.is_empty() {
            return v_flex()
                .flex_1()
                .items_center()
                .justify_center()
                .gap_1()
                .px(style::INSET)
                .text_sm()
                .text_color(style::muted(cx))
                .children(pane.facts)
                .children(
                    pane.hint
                        .map(|hint| div().text_xs().max_w(px(420.)).text_center().child(hint)),
                )
                .into_any_element();
        }
        let count = pane.rows.len();
        let batch = uploads
            .then(|| self.batches.get(&id))
            .flatten()
            .filter(|batch| batch.totals().is_some());
        // The summary is the queue's first ledger line: the whole batch's rail on its edge,
        // each file's own rail on the rows below.
        let header = h_flex()
            .relative()
            .px(style::INSET)
            .py_2()
            .gap_x_3()
            .flex_wrap()
            .whitespace_nowrap()
            .border_b_1()
            .border_color(style::rule(cx))
            .text_xs()
            .text_color(style::muted(cx))
            .children(pane.facts)
            .children(batch.and_then(view::UploadBatch::summary))
            .children(batch.and_then(view::UploadBatch::time_left))
            .children(pane.hint)
            .children(
                batch
                    .and_then(view::UploadBatch::progress)
                    .map(|fraction| progress_rail(fraction, cx)),
            );
        let list = uniform_list(
            "queue",
            count,
            cx.processor(move |this, range: std::ops::Range<usize>, _, cx| {
                let queues = view::queues(&this.state, id);
                let rows = if uploads {
                    queues.uploads.rows
                } else {
                    queues.downloads.rows
                };
                range
                    .filter_map(|index| rows.get(index).cloned())
                    .map(|row| queue_row(row, cx).into_any_element())
                    .collect::<Vec<_>>()
            }),
        )
        .flex_1()
        .track_scroll(&self.queue_scroll);
        v_flex()
            .flex_1()
            .min_h_0()
            .child(header)
            .child(
                v_flex()
                    .relative()
                    .flex_1()
                    .min_h_0()
                    .child(list)
                    .vertical_scrollbar(&self.queue_scroll),
            )
            .into_any_element()
    }

    fn render_log(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let text = if self.log.is_empty() {
            SharedString::from("No log yet")
        } else {
            self.log.clone()
        };
        let log = div()
            .id("log")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .track_scroll(&self.log_scroll)
            .px(style::INSET)
            .py_3()
            .font_family(cx.theme().mono_font_family.clone())
            .text_xs()
            .when(self.log.is_empty(), |this| {
                this.text_color(style::muted(cx))
            })
            .child(text);
        // The scrollbar overlays a non-scrolling wrapper, so it stays put while the log moves.
        v_flex()
            .relative()
            .flex_1()
            .min_h_0()
            .child(log)
            .vertical_scrollbar(&self.log_scroll)
    }
}

fn queue_row(row: view::QueueRow, cx: &App) -> impl IntoElement {
    div()
        .relative()
        .w_full()
        .h(style::QUEUE_ROW)
        .px(style::INSET)
        .flex()
        .flex_col()
        .justify_center()
        .gap_1()
        .border_b_1()
        .border_color(style::rule(cx))
        .child(style::path(div()).min_w_0().text_sm().child(row.name))
        .child(
            h_flex()
                .gap_3()
                .text_xs()
                .text_color(style::muted(cx))
                .children(row.facts),
        )
        .when(row.progress > 0.0, |this| {
            this.child(progress_rail(row.progress, cx))
        })
}

/// A labelled fact about the selected mount. Paths give way first, keeping their end visible.
fn fact(label: &'static str, value: String, path: bool, cx: &App) -> impl IntoElement {
    v_flex()
        .gap_0p5()
        .when(path, |this| this.min_w_0())
        .when(!path, |this| this.flex_shrink_0())
        .child(div().text_xs().text_color(style::muted(cx)).child(label))
        .child(style::path(div()).text_sm().child(value))
}

/// The row's lower edge fills with the progress of its transfers.
fn progress_rail(fraction: f64, cx: &App) -> impl IntoElement {
    div()
        .absolute()
        .left_0()
        .bottom_0()
        .h(style::RAIL)
        .w(relative(fraction as f32))
        .bg(style::rail(cx))
}

fn banner(id: &'static str, text: &str, tone: Tone, cx: &App) -> Stateful<Div> {
    let color = style::tone(tone, cx);
    h_flex()
        .id(id)
        .gap_2()
        .px(style::INSET)
        .py_2()
        .bg(cx.theme().muted)
        .border_b_1()
        .border_color(style::rule(cx))
        .text_sm()
        .child(match tone {
            Tone::Busy => Spinner::new().small().color(color).into_any_element(),
            _ => Icon::new(IconName::TriangleAlert)
                .small()
                .text_color(color)
                .into_any_element(),
        })
        .child(div().flex_1().min_w_0().child(text.to_string()))
}

#[derive(Clone)]
enum Suffix {
    None,
    Busy,
    Note(SharedString),
}

type ClickHandler = dyn Fn(&ClickEvent, &mut Window, &mut App);
type MenuBuilder = dyn Fn(PopupMenu, &mut Window, &mut Context<PopupMenu>) -> PopupMenu;

/// A mount in the sidebar, drawn like a shadcn sidebar menu button. Its own item rather than
/// `SidebarMenuItem`, so a long name truncates with an ellipsis instead of wrapping out of view.
#[derive(Clone)]
struct MountItem {
    name: SharedString,
    icon: Icon,
    /// Unmounted: the name steps back, the icon's shape says why.
    muted: bool,
    selected: bool,
    suffix: Suffix,
    on_click: Rc<ClickHandler>,
    menu: Rc<MenuBuilder>,
}

impl Collapsible for MountItem {
    fn collapsed(self, _: bool) -> Self {
        self
    }

    fn is_collapsed(&self) -> bool {
        false
    }
}

impl SidebarItem for MountItem {
    fn render(self, id: impl Into<ElementId>, _: &mut Window, cx: &mut App) -> impl IntoElement {
        let theme = cx.theme();
        let (accent, accent_text, muted) = (
            theme.sidebar_accent,
            theme.sidebar_accent_foreground,
            theme.muted_foreground,
        );
        let (click, menu) = (self.on_click, self.menu);
        div().py_0p5().child(
            h_flex()
                .id(id.into())
                .h_8()
                .px_2()
                .gap_2()
                .rounded(theme.radius)
                .text_sm()
                .cursor_pointer()
                .map(|this| {
                    if self.selected {
                        this.font_medium().bg(accent).text_color(accent_text)
                    } else {
                        this.hover(move |this| this.bg(accent.opacity(0.8)))
                    }
                })
                .child(self.icon.flex_shrink_0())
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .when(self.muted, |this| this.text_color(muted))
                        .child(self.name),
                )
                .map(|this| match self.suffix {
                    Suffix::None => this,
                    Suffix::Busy => this.child(Spinner::new().xsmall().color(muted)),
                    Suffix::Note(note) => this.child(
                        div()
                            .flex_shrink_0()
                            .text_xs()
                            .font_normal()
                            .text_color(muted)
                            .child(note),
                    ),
                })
                .on_click(move |event, window, cx| click(event, window, cx))
                .context_menu(move |m, window, cx| menu(m, window, cx)),
        )
    }
}

struct Confirm {
    title: String,
    detail: String,
    verb: &'static str,
    back: &'static str,
    danger: bool,
}

fn confirm(
    window: &mut Window,
    cx: &mut App,
    request: Confirm,
    yes: impl Fn(&mut Window, &mut App) + 'static,
) {
    let yes = Rc::new(yes);
    window.open_dialog(cx, move |dialog, _, cx| {
        let (enter, click) = (yes.clone(), yes.clone());
        let accept = Button::new("confirm")
            .label(request.verb)
            .on_click(move |_, window, cx| {
                click(window, cx);
                window.close_dialog(cx);
            });
        dialog
            .title(request.title.clone())
            .w(px(400.))
            .child(
                div()
                    .text_sm()
                    .text_color(style::muted(cx))
                    .child(request.detail.clone()),
            )
            .button_props(DialogButtonProps::default().on_ok(move |_, window, cx| {
                enter(window, cx);
                true
            }))
            .footer(
                DialogFooter::new()
                    .child(
                        Button::new("back")
                            .outline()
                            .label(request.back)
                            .on_click(|_, window, cx| window.close_dialog(cx)),
                    )
                    .child(if request.danger {
                        accept.danger()
                    } else {
                        accept.primary()
                    }),
            )
    });
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
    .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::ThemeChoice;

    #[test]
    fn theme_choice_round_trips_and_defaults_to_the_system() {
        for choice in ThemeChoice::ALL {
            assert_eq!(ThemeChoice::parse(&format!("{}\n", choice.key())), choice);
        }
        assert_eq!(ThemeChoice::parse(""), ThemeChoice::System);
        assert_eq!(ThemeChoice::parse("solarized"), ThemeChoice::System);
    }
}
