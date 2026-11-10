//! The settings window: one ledger of mounts, and a detail page per mount.
use crate::{editor::Editor, style};
use gio::prelude::*;
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Icon, IconName, Sizable as _, StyledExt as _, Theme,
    ThemeMode, TitleBar, WindowExt as _,
    button::{Button, ButtonVariants as _},
    dialog::{DialogButtonProps, DialogFooter},
    h_flex,
    menu::{DropdownMenu as _, PopupMenuItem},
    notification::Notification,
    spinner::Spinner,
    switch::Switch,
    tab::{Tab, TabBar},
    v_flex,
};
use gpui_kit::{prelude::*, *};
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
gpui_kit::actions!(rclone_tray, [AddMount, Refresh, Quit, Back]);

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
    detail: Option<u64>,
    pane: Pane,
    notice: Option<String>,
    log: SharedString,
    log_scroll: ScrollHandle,
    list_scroll: ScrollHandle,
    queue_scroll: UniformListScrollHandle,
    focus: FocusHandle,
    autostart: bool,
    theme: ThemeChoice,
    /// Measured cache size of unmounted profiles, by id. Mounted ones report it live.
    usage: HashMap<u64, u64>,
    clearing: Option<u64>,
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

pub fn open(
    cx: &mut App,
    paths: Paths,
    incoming: Option<(Receiver<String>, InstanceGuard)>,
    action: Action,
) {
    let options = WindowOptions {
        window_bounds: Some(WindowBounds::centered(size(px(640.), px(520.)), cx)),
        window_min_size: Some(size(px(420.), px(360.))),
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
            detail: None,
            pane: Pane::Uploads,
            notice: None,
            log: SharedString::default(),
            log_scroll: ScrollHandle::new(),
            list_scroll: ScrollHandle::new(),
            queue_scroll: UniformListScrollHandle::new(),
            focus,
            usage: HashMap::new(),
            clearing: None,
            editor: None,
            _appearance: appearance,
        };
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
        if let Some(id) = self.detail
            && !snapshot.config.profiles.iter().any(|p| p.id == id)
        {
            self.detail = None;
        }
        // A mount that just stopped has a cache worth measuring again.
        if let Some(id) = self.detail
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
        cx.notify();
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
                self.open_detail(id, Pane::Uploads, window, cx);
            }
            Action::Log(id) => {
                window.activate_window();
                self.open_detail(id, Pane::Log, window, cx);
            }
            Action::Quit => {
                window.activate_window();
                self.quit(window, cx);
            }
            Action::QuitNow => self.send(Request::QuitNow, cx),
        }
    }

    fn open_detail(&mut self, id: u64, pane: Pane, window: &mut Window, cx: &mut Context<Self>) {
        if self.profile(id).is_none() {
            return;
        }
        self.detail = Some(id);
        self.measure(id, cx);
        self.select_pane(pane, window, cx);
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
        let Some(id) = self.detail else { return };
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

    fn start_backend(&mut self, cx: &mut Context<Self>) {
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
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                match result {
                    Ok(state) => {
                        let (requests, events) = ipc::connect(ipc::socket_path(&this.paths));
                        this.link = Link::Connected(requests, events);
                        this.state = state;
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

    fn on_back(&mut self, _: &Back, window: &mut Window, cx: &mut Context<Self>) {
        if self.detail.take().is_some() {
            window.focus(&self.focus, cx);
            cx.notify();
        } else {
            cx.propagate();
        }
    }
}

// MARK: Rendering

impl Render for Shell {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let body = match (&self.link, self.detail) {
            (Link::Offline { error, starting }, _) => self
                .render_offline(error.clone(), *starting, cx)
                .into_any_element(),
            (_, Some(id)) => self.render_detail(id, window, cx).into_any_element(),
            (_, None) => self.render_list(cx).into_any_element(),
        };
        v_flex()
            .id("shell")
            .key_context(CONTEXT)
            .track_focus(&self.focus)
            .on_action(cx.listener(Self::on_add))
            .on_action(cx.listener(Self::on_refresh))
            .on_action(cx.listener(Self::on_quit))
            .on_action(cx.listener(Self::on_back))
            .size_full()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .child(self.render_title_bar(cx))
            .children(self.render_banners(cx))
            .child(body)
    }
}

impl Shell {
    fn render_title_bar(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let connected = matches!(self.link, Link::Connected(..));
        let lead = match self.detail.and_then(|id| self.profile(id)) {
            Some(profile) => h_flex()
                .gap_1()
                .min_w_0()
                .child(
                    Button::new("back")
                        .ghost()
                        .small()
                        .icon(IconName::ArrowLeft)
                        .tooltip("All mounts")
                        .on_click(
                            cx.listener(|this, _, window, cx| this.on_back(&Back, window, cx)),
                        ),
                )
                .child(
                    div()
                        .min_w_0()
                        .truncate()
                        .text_sm()
                        .font_semibold()
                        .child(profile.name.clone()),
                ),
            None => h_flex().text_sm().font_semibold().child("Rclone Tray"),
        };
        let this = cx.entity().downgrade();
        let autostart = self.autostart;
        let current = self.theme;
        let menu = Button::new("menu")
            .ghost()
            .small()
            .icon(IconName::Ellipsis)
            .tooltip("More")
            .dropdown_menu_with_anchor(Anchor::TopRight, move |menu, _, _| {
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
        let add = (self.detail.is_none() && connected && !self.state.config.profiles.is_empty())
            .then(|| {
                Button::new("add")
                    .outline()
                    .small()
                    .icon(IconName::Plus)
                    .label("Add mount")
                    .disabled(self.state.quitting)
                    .tooltip("Ctrl+N")
                    .on_click(cx.listener(|this, _, window, cx| this.edit(None, window, cx)))
            });
        TitleBar::new()
            .bg(cx.theme().background)
            .border_color(style::rule(cx))
            .child(
                h_flex()
                    .flex_1()
                    .min_w_0()
                    .justify_between()
                    .pr_2()
                    .child(lead)
                    .child(h_flex().gap_1().children(add).child(menu)),
            )
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
                    .on_click(cx.listener(|this, _, _, cx| this.start_backend(cx))),
            )
    }

    fn render_list(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        if self.state.config.profiles.is_empty() {
            return self.render_empty(cx).into_any_element();
        }
        let mut overview = view::overview(&self.state);
        if self.state.quitting {
            // The banner above owns quit progress.
            overview.facts.clear();
        }
        let attention = overview
            .facts
            .first()
            .is_some_and(|fact| fact.ends_with("attention"));
        let header = h_flex()
            .px(style::INSET)
            .pt_4()
            .pb_2()
            .gap_3()
            .items_baseline()
            .child(div().text_lg().font_semibold().child(overview.headline))
            .children(overview.facts.into_iter().map(|fact| {
                div()
                    .text_sm()
                    .text_color(if attention {
                        cx.theme().danger
                    } else {
                        style::muted(cx)
                    })
                    .child(fact)
            }));
        let rows: Vec<_> = self
            .state
            .config
            .profiles
            .clone()
            .into_iter()
            .map(|profile| self.render_row(profile, cx).into_any_element())
            .collect();
        v_flex()
            .flex_1()
            .min_h_0()
            .child(header)
            .child(
                div()
                    .id("mounts")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .track_scroll(&self.list_scroll)
                    .children(rows),
            )
            .into_any_element()
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

    fn render_row(&mut self, profile: Profile, cx: &mut Context<Self>) -> impl IntoElement {
        let id = profile.id;
        let status = self.status(id).clone();
        let sample = self.state.activity.get(&id);
        let (facts, tone) = view::activity_line(&status, sample);
        let note = view::ledger_note(&status, facts);
        let progress = view::progress(&status, sample);
        let primary = view::primary(&status);
        let quitting = self.state.quitting;
        let row_id = SharedString::from(format!("mount-{id}"));
        div()
            .id(row_id.clone())
            .relative()
            .h(style::MOUNT_ROW)
            .flex()
            .items_center()
            .gap_3()
            .px(style::INSET)
            .border_b_1()
            .border_color(style::rule(cx))
            .cursor_pointer()
            .hover(|this| this.bg(style::hover(cx)))
            .on_click(cx.listener(move |this, _, window, cx| {
                this.open_detail(id, Pane::Uploads, window, cx)
            }))
            .child(controls().child(self.mount_switch(id, &status, &profile.name, quitting, cx)))
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .child(
                        div()
                            .truncate()
                            .text_sm()
                            .font_medium()
                            .when(!status.active(), |this| this.text_color(style::muted(cx)))
                            .child(profile.name.clone()),
                    )
                    .child(
                        style::path(div())
                            .text_xs()
                            .text_color(style::muted(cx))
                            .child(view::short_path(&profile.mountpoint, self.home.as_deref())),
                    ),
            )
            .child(status_note(note, tone, cx))
            .child(
                controls().child(
                    Button::new(SharedString::from(format!("open-{id}")))
                        .ghost()
                        .small()
                        .icon(IconName::FolderOpen)
                        .tooltip("Open folder")
                        .disabled(primary != Primary::Open)
                        .on_click(cx.listener(move |this, _, _, cx| this.open_folder(id, cx))),
                ),
            )
            .child(
                Button::new(SharedString::from(format!("details-{id}")))
                    .ghost()
                    .small()
                    .icon(IconName::ChevronRight)
                    .tooltip("Details")
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.open_detail(id, Pane::Uploads, window, cx)
                    })),
            )
            .children(progress.map(|fraction| progress_rail(fraction, cx)))
    }

    fn mount_switch(
        &self,
        id: u64,
        status: &Status,
        name: &str,
        quitting: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let locked = matches!(status, Status::Unmounting | Status::External) || quitting;
        let tooltip = match status {
            Status::External => "Mounted by another session".to_string(),
            Status::Unmounting => "Unmounting…".to_string(),
            _ if quitting => "Quitting…".to_string(),
            _ if status.active() => format!("Unmount {name}"),
            _ => format!("Mount {name}"),
        };
        Switch::new(SharedString::from(format!("switch-{id}")))
            .checked(status.active())
            .disabled(locked)
            .tooltip(tooltip.clone())
            .accessibility_label(tooltip)
            .on_click(
                cx.listener(move |this, on: &bool, window, cx| this.toggle(id, *on, window, cx)),
            )
    }

    fn render_detail(
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
        let mounted = view::primary(&status) == Primary::Open;
        let active = status.active();
        let busy = matches!(
            status,
            Status::Mounting | Status::Recovering | Status::Unmounting
        );
        let quitting = self.state.quitting;
        let queues = view::queues(&self.state, id);

        let identity = h_flex()
            .gap_4()
            .items_center()
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .gap_1()
                    .child(
                        style::path(div())
                            .text_lg()
                            .font_semibold()
                            .child(view::short_path(&profile.mountpoint, self.home.as_deref())),
                    )
                    .child(
                        h_flex()
                            .gap_3()
                            .text_sm()
                            .text_color(style::muted(cx))
                            .child(
                                h_flex()
                                    .min_w_0()
                                    .child(
                                        div().flex_shrink_0().child(format!("{}:", profile.remote)),
                                    )
                                    .child(
                                        style::path(div()).min_w_0().child(profile.folder.clone()),
                                    ),
                            )
                            .child(div().flex_shrink_0().child(view::cache_line(
                                &profile,
                                sample,
                                self.usage.get(&id).copied(),
                            ))),
                    ),
            )
            .child(
                Button::new("open")
                    .primary()
                    .icon(IconName::FolderOpen)
                    .label("Open folder")
                    .disabled(!mounted)
                    .on_click(cx.listener(move |this, _, _, cx| this.open_folder(id, cx))),
            );

        let controls = h_flex()
            .gap_2()
            .items_center()
            .child(self.mount_switch(id, &status, &profile.name, quitting, cx))
            .child(div().text_sm().child(status.label().to_string()))
            .when(busy, |this| {
                this.child(Spinner::new().small().color(style::muted(cx)))
            })
            .child(div().flex_1())
            .child(
                Button::new("clear-cache")
                    .ghost()
                    .small()
                    .label("Clear cache")
                    .loading(self.clearing == Some(id))
                    .disabled(
                        active
                            || quitting
                            || self.clearing.is_some()
                            || self.usage.get(&id) == Some(&0),
                    )
                    .tooltip(if active {
                        "Unmount to clear cache"
                    } else if self.usage.get(&id) == Some(&0) {
                        "Cache is empty"
                    } else {
                        "Delete local copies"
                    })
                    .on_click(
                        cx.listener(move |this, _, window, cx| this.confirm_clear(id, window, cx)),
                    ),
            )
            .child(
                Button::new("edit")
                    .ghost()
                    .small()
                    .label("Edit")
                    .disabled(active || quitting)
                    .tooltip(if active {
                        "Unmount to edit"
                    } else {
                        "Edit mount"
                    })
                    .on_click(cx.listener(move |this, _, window, cx| {
                        let profile = this.profile(id).cloned();
                        if profile.is_some() {
                            this.edit(profile, window, cx);
                        }
                    })),
            )
            .child(
                Button::new("remove")
                    .ghost()
                    .small()
                    .label("Remove")
                    .disabled(active || quitting)
                    .tooltip(if active {
                        "Unmount to remove"
                    } else {
                        "Remove mount"
                    })
                    .on_click(
                        cx.listener(move |this, _, window, cx| this.confirm_remove(id, window, cx)),
                    ),
            );

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
                    .pt_4()
                    .pb_3()
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
                    .child(identity)
                    .child(controls),
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
        let header = h_flex()
            .px(style::INSET)
            .py_2()
            .gap_3()
            .text_xs()
            .text_color(style::muted(cx))
            .children(pane.facts)
            .children(pane.hint);
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
            .child(list)
            .into_any_element()
    }

    fn render_log(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let text = if self.log.is_empty() {
            SharedString::from("No log yet. It appears after the first mount attempt.")
        } else {
            self.log.clone()
        };
        div()
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
            .child(text)
    }
}

/// Holds controls inside a clickable row. A press here belongs to the control, so it never
/// also opens the row's details.
fn controls() -> Div {
    h_flex().on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
}

fn queue_row(row: view::QueueRow, cx: &App) -> impl IntoElement {
    div()
        .relative()
        .h(style::QUEUE_ROW)
        .px(style::INSET)
        .flex()
        .flex_col()
        .justify_center()
        .gap_1()
        .border_b_1()
        .border_color(style::rule(cx))
        .child(div().truncate().text_sm().child(row.name))
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

/// Live state beside a mount: a spinner while it works, an icon when it needs a decision.
fn status_note(facts: Vec<String>, tone: Tone, cx: &App) -> impl IntoElement {
    h_flex()
        .flex_shrink_0()
        .gap_3()
        .text_xs()
        .text_color(style::tone(tone, cx))
        .when(tone == Tone::Busy && !facts.is_empty(), |this| {
            this.child(Spinner::new().xsmall().color(style::muted(cx)))
        })
        .when(matches!(tone, Tone::Warning | Tone::Danger), |this| {
            this.child(Icon::new(IconName::TriangleAlert).xsmall())
        })
        .children(facts)
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
