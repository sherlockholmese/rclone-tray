//! Native desktop notifications, driven by changes rather than polling frequency.
use crate::{
    backend::{Snapshot, Status},
    config::Profile,
};
use gio::{self, prelude::*};
use std::collections::HashMap;

pub struct Notice {
    id: String,
    title: &'static str,
    body: String,
    urgent: bool,
}

impl Notice {
    fn profile(
        profile: &Profile,
        category: &str,
        title: &'static str,
        detail: &str,
        urgent: bool,
    ) -> Self {
        Self {
            id: format!("{category}-{}", profile.id),
            title,
            body: format!("{}\n{detail}", profile.name),
            urgent,
        }
    }

    pub fn problem(message: &str) -> Self {
        Self {
            id: "operation".into(),
            title: "Action needs attention",
            body: message.into(),
            urgent: true,
        }
    }

    pub fn send(&self, app: &impl IsA<gio::Application>) {
        let notification = gio::Notification::new(self.title);
        notification.set_body(Some(&self.body));
        notification.set_icon(&gio::ThemedIcon::new("drive-harddisk"));
        notification.set_priority(if self.urgent {
            gio::NotificationPriority::High
        } else {
            gio::NotificationPriority::Normal
        });
        notification.set_default_action("app.show");
        app.send_notification(Some(&self.id), &notification);
    }
}

struct ProfileState {
    status: Status,
    uploads_pending: bool,
    cache_out_of_space: bool,
    upload_errors: bool,
}

#[derive(Default)]
pub struct Notifications {
    profiles: HashMap<u64, ProfileState>,
}

impl Notifications {
    pub fn update(&mut self, snapshot: &Snapshot) -> Vec<Notice> {
        self.profiles
            .retain(|id, _| snapshot.config.profiles.iter().any(|p| p.id == *id));
        let mut notices = Vec::new();
        for profile in &snapshot.config.profiles {
            let status = snapshot
                .statuses
                .get(&profile.id)
                .unwrap_or(&Status::Unmounted);
            let previous = self.profiles.entry(profile.id).or_insert(ProfileState {
                status: Status::Unmounted,
                uploads_pending: false,
                cache_out_of_space: false,
                upload_errors: false,
            });
            if previous.status != *status {
                let notice = match status {
                    Status::Mounted if !snapshot.quitting => Some(Notice::profile(
                        profile,
                        "mount",
                        "Mounted",
                        &profile.mountpoint.display().to_string(),
                        false,
                    )),
                    Status::Unmounted
                        if previous.status == Status::Unmounting && !snapshot.quitting =>
                    {
                        Some(Notice::profile(
                            profile,
                            "mount",
                            "Unmounted",
                            &profile.mountpoint.display().to_string(),
                            false,
                        ))
                    }
                    Status::Error(error) => Some(Notice::profile(
                        profile,
                        "mount",
                        "Mount needs attention",
                        error,
                        true,
                    )),
                    Status::Waiting if !snapshot.quitting => Some(Notice::profile(
                        profile,
                        "mount",
                        "Mount is taking longer",
                        "Startup is still running. Check the mount log.",
                        false,
                    )),
                    _ => None,
                };
                if let Some(notice) = notice {
                    notices.push(notice);
                }
            }
            if matches!(
                status,
                Status::Mounting | Status::Recovering | Status::Waiting | Status::Mounted
            ) {
                if let Some(Ok(activity)) = snapshot.activity.get(&profile.id) {
                    if activity.pending_uploads() {
                        previous.uploads_pending = true;
                    } else if activity.cache.is_ok() && activity.uploads.is_ok() {
                        if previous.uploads_pending && !snapshot.quitting {
                            notices.push(Notice::profile(
                                profile,
                                "uploads",
                                "Uploads finished",
                                "Upload queue empty.",
                                false,
                            ));
                        }
                        previous.uploads_pending = false;
                    }
                    if let Ok(cache) = &activity.cache {
                        if (cache.out_of_space && !previous.cache_out_of_space)
                            || (cache.errored_files > 0 && !previous.upload_errors)
                        {
                            let detail = if cache.out_of_space {
                                "The local cache is out of space.".into()
                            } else {
                                format!(
                                    "{} cached files need upload retry. Check the mount log.",
                                    cache.errored_files
                                )
                            };
                            notices.push(Notice::profile(
                                profile,
                                "cache",
                                "Upload needs attention",
                                &detail,
                                true,
                            ));
                        }
                        previous.cache_out_of_space = cache.out_of_space;
                        previous.upload_errors = cache.errored_files > 0;
                    }
                }
            } else {
                // Never carry a previous mount's queue into a new session or an external mount.
                previous.uploads_pending = false;
                previous.cache_out_of_space = false;
                previous.upload_errors = false;
            }
            previous.status = status.clone();
        }
        notices
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        config::Config,
        control::{Activity, Cache, QueuedUpload, Transfer},
    };

    fn snapshot(status: Status) -> Snapshot {
        Snapshot {
            config: Config {
                profiles: vec![Profile {
                    id: 17,
                    name: "Work files".into(),
                    remote: "work".into(),
                    folder: String::new(),
                    mountpoint: "/tmp/work files".into(),
                    cache_gib: 1,
                    mount_at_start: false,
                }],
            },
            remotes: vec!["work".into()],
            statuses: [(17, status)].into(),
            activity: HashMap::new(),
            message: String::new(),
            quitting: false,
        }
    }

    fn activity(pending: bool) -> Activity {
        Activity {
            transfers: vec![],
            cache: Ok(Cache {
                bytes_used: 2048,
                uploads_queued: u64::from(pending),
                uploads_in_progress: 0,
                errored_files: 0,
                out_of_space: false,
            }),
            uploads: Ok(if pending {
                vec![QueuedUpload {
                    name: "report.txt".into(),
                    size: 2048,
                    expiry: 2.0,
                    tries: 0,
                    uploading: false,
                }]
            } else {
                vec![]
            }),
        }
    }

    #[test]
    fn mount_notifications_report_transitions_without_polling_spam() {
        let mut notices = Notifications::default();
        let mut state = snapshot(Status::External);
        assert!(notices.update(&state).is_empty());
        state.statuses.insert(17, Status::Unmounted);
        assert!(notices.update(&state).is_empty());
        state.statuses.insert(17, Status::Mounting);
        assert!(notices.update(&state).is_empty());
        state.statuses.insert(17, Status::Waiting);
        assert_eq!(notices.update(&state)[0].title, "Mount is taking longer");
        assert!(notices.update(&state).is_empty());
        state.statuses.insert(17, Status::Mounted);
        let mounted = notices.update(&state);
        assert_eq!(mounted.len(), 1);
        assert_eq!(mounted[0].title, "Mounted");
        assert_eq!(mounted[0].body, "Work files\n/tmp/work files");
        assert!(notices.update(&state).is_empty());
        state
            .statuses
            .insert(17, Status::Error("Authentication expired".into()));
        let failed = notices.update(&state);
        assert_eq!(failed[0].title, "Mount needs attention");
        assert!(failed[0].urgent);
        assert!(failed[0].body.contains("Authentication expired"));
        assert!(notices.update(&state).is_empty());
        state.statuses.insert(17, Status::Mounting);
        notices.update(&state);
        state.statuses.insert(17, Status::Mounted);
        assert_eq!(notices.update(&state)[0].id, mounted[0].id);
        state.statuses.insert(17, Status::Unmounting);
        assert!(notices.update(&state).is_empty());
        state.statuses.insert(17, Status::Unmounted);
        assert_eq!(notices.update(&state)[0].title, "Unmounted");
        state.quitting = true;
        state.statuses.insert(17, Status::Unmounting);
        notices.update(&state);
        state.statuses.insert(17, Status::Unmounted);
        assert!(notices.update(&state).is_empty());
    }

    #[test]
    fn upload_completion_requires_observed_work_and_confirmed_empty_telemetry() {
        let mut notices = Notifications::default();
        let mut state = snapshot(Status::Mounted);
        state.activity.insert(17, Ok(activity(false)));
        notices.update(&state);
        assert!(notices.update(&state).is_empty());
        state.activity.insert(17, Ok(activity(true)));
        assert!(notices.update(&state).is_empty());
        state.activity.insert(17, Err("offline".into()));
        assert!(notices.update(&state).is_empty());
        let mut unknown = activity(false);
        unknown.uploads = Err("queue unavailable".into());
        state.activity.insert(17, Ok(unknown));
        assert!(notices.update(&state).is_empty());
        let mut unknown = activity(false);
        unknown.cache = Err("cache unavailable".into());
        state.activity.insert(17, Ok(unknown));
        assert!(notices.update(&state).is_empty());
        state.activity.insert(17, Ok(activity(false)));
        let complete = notices.update(&state);
        assert_eq!(complete.len(), 1);
        assert_eq!(complete[0].title, "Uploads finished");
        assert!(notices.update(&state).is_empty());
        let mut download = activity(false);
        download.transfers.push(Transfer {
            name: "read.bin".into(),
            size: 7000,
            bytes: 2000,
            speed_avg: 73.0,
            src_fs: Some("work:".into()),
            dst_fs: None,
        });
        state.activity.insert(17, Ok(download));
        assert!(notices.update(&state).is_empty());
        state.activity.insert(17, Ok(activity(false)));
        assert!(notices.update(&state).is_empty());
        state.activity.insert(17, Ok(activity(true)));
        notices.update(&state);
        state.quitting = true;
        state.activity.insert(17, Ok(activity(false)));
        assert!(notices.update(&state).is_empty());
        state.quitting = false;
        assert!(notices.update(&state).is_empty());
    }

    #[test]
    fn cache_problems_realert_only_after_recovery_and_do_not_leak_between_profiles() {
        let mut notices = Notifications::default();
        let mut state = snapshot(Status::Mounted);
        let mut second = state.config.profiles[0].clone();
        second.id = 23;
        second.name = "Personal files".into();
        state.config.profiles.push(second);
        state.statuses.insert(23, Status::Mounted);
        notices.update(&state);
        let mut failed = activity(true);
        failed.cache.as_mut().unwrap().errored_files = 3;
        state.activity.insert(17, Ok(failed.clone()));
        let warning = notices.update(&state);
        assert_eq!(warning.len(), 1);
        assert!(warning[0].body.contains("3 cached files"));
        assert!(warning[0].urgent);
        assert!(notices.update(&state).is_empty());
        // Disk exhaustion must still alert when upload retries were already reported.
        failed.cache.as_mut().unwrap().out_of_space = true;
        state.activity.insert(17, Ok(failed.clone()));
        assert!(notices.update(&state)[0].body.contains("out of space"));
        state.activity.insert(17, Err("offline".into()));
        assert!(notices.update(&state).is_empty());
        state.activity.insert(17, Ok(failed.clone()));
        assert!(notices.update(&state).is_empty());
        state.activity.insert(23, Ok(activity(false)));
        assert!(notices.update(&state).is_empty());
        state.activity.insert(17, Ok(activity(false)));
        assert_eq!(notices.update(&state)[0].title, "Uploads finished");
        failed.cache.as_mut().unwrap().out_of_space = true;
        state.activity.insert(17, Ok(failed));
        assert!(notices.update(&state)[0].body.contains("out of space"));
        state.config.profiles.retain(|p| p.id != 17);
        assert!(notices.update(&state).is_empty());
    }

    #[test]
    #[ignore = "requires a private dbus-run-session and RCLONE_TRAY_TEST_BUS=1; never run on the desktop bus"]
    fn native_notification_delivery_and_click() {
        use std::{
            cell::Cell,
            rc::Rc,
            sync::mpsc,
            thread,
            time::{Duration, Instant},
        };
        assert_eq!(std::env::var("RCLONE_TRAY_TEST_BUS").as_deref(), Ok("1"));
        assert!(std::env::var_os("DISPLAY").is_none());
        assert!(std::env::var_os("WAYLAND_DISPLAY").is_none());
        let context = glib::MainContext::new();
        context.with_thread_default(|| {
            let connection = gio::bus_get_sync(gio::BusType::Session, gio::Cancellable::NONE).unwrap();
            let ownership = connection.call_sync(Some("org.freedesktop.DBus"), "/org/freedesktop/DBus", "org.freedesktop.DBus", "RequestName", Some(&("org.freedesktop.Notifications", 4u32).to_variant()), None, gio::DBusCallFlags::NONE, 2000, gio::Cancellable::NONE).unwrap();
            assert_eq!(ownership.get::<(u32,)>(), Some((1,)), "Refusing to use an existing notification daemon");
            let info = gio::DBusNodeInfo::for_xml(r#"<node><interface name="org.freedesktop.Notifications">
              <method name="Notify"><arg type="s" direction="in"/><arg type="u" direction="in"/><arg type="s" direction="in"/><arg type="s" direction="in"/><arg type="s" direction="in"/><arg type="as" direction="in"/><arg type="a{sv}" direction="in"/><arg type="i" direction="in"/><arg type="u" direction="out"/></method>
              <method name="GetCapabilities"><arg type="as" direction="out"/></method>
              <signal name="ActionInvoked"><arg type="u"/><arg type="s"/></signal>
            </interface></node>"#).unwrap();
            let (tx, rx) = mpsc::channel();
            let registration = connection.register_object("/org/freedesktop/Notifications", &info.lookup_interface("org.freedesktop.Notifications").unwrap())
                .method_call(move |_, _, _, _, method, parameters, invocation| {
                    if method == "Notify" {
                        invocation.return_value(Some(&(42u32,).to_variant()));
                        let _ = tx.send(parameters);
                    } else {
                        invocation.return_value(Some(&(vec!["actions", "body", "icon-static"],).to_variant()));
                    }
                }).build().unwrap();
            let app = gio::Application::new(Some("io.github.rclone_tray.NotificationTest"), gio::ApplicationFlags::NON_UNIQUE);
            app.register(gio::Cancellable::NONE).unwrap();
            let clicked = Rc::new(Cell::new(false));
            let capture = clicked.clone();
            let show = gio::SimpleAction::new("show", None);
            show.connect_activate(move |_, _| capture.set(true));
            app.add_action(&show);
            Notice::problem("Close open files and retry.").send(&app);
            let deadline = Instant::now() + Duration::from_secs(5);
            let receive = || {
                loop {
                    while context.iteration(false) {}
                    if let Ok(parameters) = rx.try_recv() { break parameters; }
                    assert!(Instant::now() < deadline, "Native Notify call was not delivered");
                    thread::sleep(Duration::from_millis(5));
                }
            };
            let parameters = receive();
            let hints = parameters.child_value(6).get::<HashMap<String, glib::Variant>>().unwrap();
            let icon = parameters.child_value(2);
            assert!(icon.str() == Some("drive-harddisk") || hints.get("image-path").and_then(|value| value.str()) == Some("drive-harddisk"), "Unexpected notification icon: {parameters}");
            assert_eq!(parameters.child_value(3).str(), Some("Action needs attention"));
            assert_eq!(parameters.child_value(4).str(), Some("Close open files and retry."));
            let actions = parameters.child_value(5).get::<Vec<String>>().unwrap();
            assert!(actions.iter().any(|action| action == "default"));
            // GIO maps High to normal FDO urgency; only Urgent maps to critical.
            assert_eq!(hints.get("urgency").and_then(|value| value.get::<u8>()), Some(1));
            // Let GIO consume the first Notify reply before checking replacement IDs.
            connection.call_sync(Some("org.freedesktop.DBus"), "/org/freedesktop/DBus", "org.freedesktop.DBus", "ListNames", None, None, gio::DBusCallFlags::NONE, 2000, gio::Cancellable::NONE).unwrap();
            while context.iteration(false) {}
            Notice::problem("Upload status unavailable.").send(&app);
            let replacement = receive();
            assert_eq!(replacement.child_value(1).get::<u32>(), Some(42));
            assert_eq!(replacement.child_value(4).str(), Some("Upload status unavailable."));
            connection.emit_signal(None, "/org/freedesktop/Notifications", "org.freedesktop.Notifications", "ActionInvoked", Some(&(42u32, "default").to_variant())).unwrap();
            while !clicked.get() {
                while context.iteration(false) {}
                assert!(Instant::now() < deadline, "Notification click did not activate app.show");
                thread::sleep(Duration::from_millis(5));
            }
            connection.unregister_object(registration).unwrap();
            println!("Verified native freedesktop Notify payload, themed drive icon, and notification click action on a private bus without a display.");
        }).unwrap();
    }
}
