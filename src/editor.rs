//! Add and edit a mount. One dialog, one field anatomy: label, control, then inline errors.
use crate::style;
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, IconName, IndexPath, Sizable as _, StyledExt as _,
    WindowExt as _,
    button::{Button, ButtonVariants as _},
    checkbox::Checkbox,
    dialog::{DialogButtonProps, DialogFooter},
    h_flex,
    input::{Input, InputEvent, InputState},
    select::{Select, SelectState},
    v_flex,
};
use gpui_kit::{prelude::*, *};
use rclone_tray::{
    backend::{self, Request},
    config::Profile,
};
use std::{
    path::PathBuf,
    sync::mpsc::{self, Sender, TryRecvError},
    time::Duration,
};

pub struct Editor {
    profile: Option<Profile>,
    remotes: Vec<String>,
    choices: Vec<String>,
    name: Entity<InputState>,
    remote: Entity<SelectState<Vec<String>>>,
    folder: Entity<InputState>,
    mountpoint: Entity<InputState>,
    cache: Entity<InputState>,
    at_start: bool,
    error: Option<String>,
    saving: bool,
    requests: Sender<Request>,
    _subscriptions: Vec<Subscription>,
}

impl Editor {
    pub fn open(
        remotes: &[String],
        profile: Option<Profile>,
        home: Option<PathBuf>,
        requests: Sender<Request>,
        window: &mut Window,
        cx: &mut App,
    ) -> Entity<Self> {
        let title = if profile.is_some() {
            "Edit mount"
        } else {
            "Add mount"
        };
        let editor = cx.new(|cx| Self::new(remotes, profile, home, requests, window, cx));
        let focus = std::cell::Cell::new(true);
        let view = editor.clone();
        window.open_dialog(cx, move |dialog, window, cx| {
            if focus.replace(false) {
                view.read(cx)
                    .name
                    .read(cx)
                    .focus_handle(cx)
                    .focus(window, cx);
            }
            let saving = view.read(cx).saving;
            let (submit, enter) = (view.clone(), view.clone());
            dialog
                .title(title)
                .w(px(480.))
                .child(view.clone())
                .button_props(DialogButtonProps::default().on_ok(move |_, window, cx| {
                    enter.update(cx, |editor, cx| editor.save(window, cx));
                    false
                }))
                .footer(
                    DialogFooter::new()
                        .child(
                            Button::new("cancel")
                                .outline()
                                .label("Cancel")
                                .on_click(|_, window, cx| window.close_dialog(cx)),
                        )
                        .child(
                            Button::new("save")
                                .primary()
                                .label("Save")
                                .loading(saving)
                                .disabled(saving)
                                .on_click(move |_, window, cx| {
                                    submit.update(cx, |editor, cx| editor.save(window, cx))
                                }),
                        ),
                )
        });
        editor
    }

    fn new(
        remotes: &[String],
        profile: Option<Profile>,
        home: Option<PathBuf>,
        requests: Sender<Request>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let value = |text: &str| text.to_string();
        let p = profile.as_ref();
        let name = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("Work Drive")
                .default_value(p.map_or(String::new(), |p| value(&p.name)))
        });
        let folder = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("Whole remote")
                .default_value(p.map_or(String::new(), |p| value(&p.folder)))
        });
        let example = home
            .map(|home| home.join("Cloud/Work"))
            .unwrap_or_else(|| PathBuf::from("/home/you/Cloud/Work"));
        let mountpoint = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(example.display().to_string())
                .default_value(p.map_or(String::new(), |p| p.mountpoint.display().to_string()))
        });
        let cache = cx.new(|cx| {
            InputState::new(window, cx)
                .validate(|text, _| text.chars().all(|c| c.is_ascii_digit()) && text.len() <= 6)
                .default_value(p.map_or(10, |p| p.cache_gib).to_string())
        });
        let choices = Self::choices(remotes, p);
        let selected = p
            .and_then(|p| choices.iter().position(|r| *r == p.remote))
            .or((choices.len() == 1).then_some(0))
            .map(IndexPath::new);
        let remote = cx.new(|cx| SelectState::new(choices.clone(), selected, window, cx));
        let mut subscriptions = vec![];
        for input in [&name, &folder, &mountpoint, &cache] {
            subscriptions.push(
                cx.subscribe_in(input, window, |this, _, event, window, cx| match event {
                    InputEvent::PressEnter { .. } => this.save(window, cx),
                    InputEvent::Change if this.error.take().is_some() => cx.notify(),
                    _ => {}
                }),
            );
        }
        Self {
            at_start: p.is_some_and(|p| p.mount_at_start),
            profile,
            remotes: remotes.to_vec(),
            choices,
            name,
            remote,
            folder,
            mountpoint,
            cache,
            error: None,
            saving: false,
            requests,
            _subscriptions: subscriptions,
        }
    }

    /// A remote that left rclone's config stays listed, so saving cannot silently switch to
    /// another account. Validation then explains the problem.
    fn choices(remotes: &[String], profile: Option<&Profile>) -> Vec<String> {
        let mut choices = remotes.to_vec();
        if let Some(p) = profile
            && !choices.contains(&p.remote)
        {
            choices.insert(0, p.remote.clone());
        }
        choices
    }

    pub fn set_remotes(
        &mut self,
        remotes: Vec<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let current = self.remote.read(cx).selected_value().cloned();
        self.choices = Self::choices(&remotes, self.profile.as_ref());
        self.remotes = remotes;
        let choices = self.choices.clone();
        let selected = current
            .and_then(|r| choices.iter().position(|c| *c == r))
            .or((choices.len() == 1).then_some(0));
        self.remote.update(cx, |state, cx| {
            state.set_items(choices, window, cx);
            if let Some(index) = selected {
                state.set_selected_index(Some(IndexPath::new(index)), window, cx);
            }
        });
        cx.notify();
    }

    fn browse(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let picked = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some("Choose".into()),
        });
        cx.spawn_in(window, async move |this, cx| {
            if let Ok(Ok(Some(paths))) = picked.await
                && let Some(path) = paths.into_iter().next()
            {
                let _ = this.update_in(cx, |this, window, cx| {
                    this.mountpoint.update(cx, |input, cx| {
                        input.set_value(path.display().to_string(), window, cx)
                    });
                });
            }
        })
        .detach();
    }

    fn save(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.saving {
            return;
        }
        let text = |input: &Entity<InputState>, cx: &App| input.read(cx).value().to_string();
        let profile = Profile {
            id: self.profile.as_ref().map_or(0, |p| p.id),
            name: text(&self.name, cx).trim().into(),
            remote: self
                .remote
                .read(cx)
                .selected_value()
                .cloned()
                .unwrap_or_default(),
            folder: text(&self.folder, cx).trim().into(),
            mountpoint: PathBuf::from(text(&self.mountpoint, cx).trim()),
            cache_gib: text(&self.cache, cx).parse().unwrap_or(0),
            mount_at_start: self.at_start,
        };
        let checked = if profile.mountpoint.as_os_str().is_empty() {
            Err("Choose a mount folder.".to_string())
        } else if profile.cache_gib == 0 {
            Err("Give the cache at least 1 GiB.".to_string())
        } else {
            backend::validate(&profile, &self.remotes)
        };
        if let Err(error) = checked {
            self.error = Some(error);
            cx.notify();
            return;
        }
        let (saved, reply) = mpsc::channel();
        if self.requests.send(Request::Save(profile, saved)).is_err() {
            self.error = Some("Rclone Tray stopped. Start it again, then save.".into());
            cx.notify();
            return;
        }
        self.saving = true;
        self.error = None;
        cx.notify();
        window.refresh();
        cx.spawn_in(window, async move |this, cx| {
            let result = loop {
                cx.background_executor()
                    .timer(Duration::from_millis(50))
                    .await;
                match reply.try_recv() {
                    Ok(result) => break result,
                    Err(TryRecvError::Empty) => continue,
                    Err(TryRecvError::Disconnected) => {
                        break Err("Rclone Tray stopped. Start it again, then save.".into());
                    }
                }
            };
            let _ = this.update_in(cx, |this, window, cx| {
                this.saving = false;
                match result {
                    Ok(()) => window.close_dialog(cx),
                    Err(message) => this.error = Some(message),
                }
                cx.notify();
                window.refresh();
            });
        })
        .detach();
    }
}

fn field(label: &'static str, control: impl IntoElement) -> Div {
    v_flex()
        .gap_1p5()
        .child(div().text_sm().font_medium().child(label))
        .child(control)
}

impl Render for Editor {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let no_remotes = self.choices.is_empty();
        v_flex()
            .gap_4()
            .pb_1()
            .when_some(self.error.clone(), |this, error| {
                this.child(
                    h_flex()
                        .gap_2()
                        .items_start()
                        .text_sm()
                        .text_color(cx.theme().danger)
                        .child(
                            gpui_kit::component::Icon::new(IconName::CircleAlert)
                                .small()
                                .flex_shrink_0()
                                .mt(px(2.)),
                        )
                        .child(div().flex_1().child(error)),
                )
            })
            .child(field("Name", Input::new(&self.name).disabled(self.saving)))
            .child(field(
                "Remote",
                if no_remotes {
                    div()
                        .text_sm()
                        .text_color(style::muted(cx))
                        .child(
                            "No remotes found. Add one with rclone config, then refresh remotes.",
                        )
                        .into_any_element()
                } else {
                    Select::new(&self.remote)
                        .placeholder("Choose a remote")
                        .disabled(self.saving)
                        .into_any_element()
                },
            ))
            .child(field(
                "Remote folder",
                Input::new(&self.folder).disabled(self.saving),
            ))
            .child(field(
                "Mount folder",
                h_flex()
                    .gap_2()
                    .child(
                        div()
                            .flex_1()
                            .child(Input::new(&self.mountpoint).disabled(self.saving)),
                    )
                    .child(
                        Button::new("browse")
                            .outline()
                            .label("Browse…")
                            .disabled(self.saving)
                            .on_click(cx.listener(|this, _, window, cx| this.browse(window, cx))),
                    ),
            ))
            .child(field(
                "Cache target",
                div().w(px(160.)).child(
                    Input::new(&self.cache)
                        .suffix(div().text_sm().text_color(style::muted(cx)).child("GiB"))
                        .disabled(self.saving),
                ),
            ))
            .child(
                Checkbox::new("at-start")
                    .label("Mount when Rclone Tray starts")
                    .checked(self.at_start)
                    .disabled(self.saving)
                    .on_click(cx.listener(|this, checked: &bool, _, cx| {
                        this.at_start = *checked;
                        cx.notify();
                    })),
            )
    }
}
