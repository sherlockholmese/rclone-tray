use crate::backend::{Snapshot, Status};
use crate::control::byte_size;
use crate::view::{self, Primary};
use ksni::{
    MenuItem, Tray,
    menu::{StandardItem, SubMenu},
};
use std::sync::mpsc::Sender;

#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub enum Action {
    Show,
    Mount(u64),
    Unmount(u64),
    Open(u64),
    Queue(u64),
    Log(u64),
    Quit,
    QuitNow,
}

pub struct AppTray {
    pub state: Snapshot,
    pub actions: Sender<Action>,
}

impl AppTray {
    fn summary(&self) -> String {
        view::overview(&self.state).summary()
    }

    fn mount_menu(&self, id: u64, status: &Status) -> Vec<MenuItem<Self>> {
        let sample = self.state.activity.get(&id);
        let mut children = vec![note(status.label()).into()];
        if status.active() && *status != Status::External {
            if let Some(Ok(activity)) = sample {
                let rates = activity.rates();
                for (name, rate) in [
                    ("Upload", rates.upload),
                    ("Download", rates.download),
                    ("Other", rates.other),
                ] {
                    if rate > 0.0 {
                        children.push(note(&format!("{name} {}/s", byte_size(rate))).into());
                    }
                }
                if let Some(count) = view::queued_uploads(activity).filter(|count| *count > 0) {
                    children.push(note(&format!("{count} queued")).into());
                }
            } else {
                children.push(note("Transfer status unavailable").into());
            }
        }
        if let Some(warning) = view::cache_warning(sample) {
            children.push(note(warning).into());
        }
        children.push(MenuItem::Separator);
        match view::primary(status) {
            Primary::Open => children.push(item("Open folder", Action::Open(id)).into()),
            Primary::Mount => children.push(item("Mount", Action::Mount(id)).into()),
            Primary::Cancel => children.push(item("Cancel mount…", Action::Unmount(id)).into()),
            Primary::Unavailable => {}
        }
        if *status == Status::Mounted {
            children.push(item("Unmount…", Action::Unmount(id)).into());
        }
        children.push(item("Details…", Action::Queue(id)).into());
        children.push(item("View log", Action::Log(id)).into());
        children
    }
}

impl Tray for AppTray {
    fn id(&self) -> String {
        "rclone-tray".into()
    }
    fn title(&self) -> String {
        format!("Rclone Tray — {}", self.summary())
    }
    fn status(&self) -> ksni::Status {
        if self
            .state
            .statuses
            .values()
            .any(|status| matches!(status, Status::Error(_)))
        {
            ksni::Status::NeedsAttention
        } else {
            ksni::Status::Active
        }
    }
    fn tool_tip(&self) -> ksni::ToolTip {
        ksni::ToolTip {
            title: "Rclone Tray".into(),
            description: self.summary(),
            icon_name: self.icon_name(),
            ..Default::default()
        }
    }
    fn icon_name(&self) -> String {
        "drive-harddisk".into()
    }
    fn activate(&mut self, _x: i32, _y: i32) {
        let _ = self.actions.send(Action::Show);
    }
    fn watcher_offline(&self, _reason: ksni::OfflineReason) -> bool {
        let _ = self.actions.send(Action::Show);
        true
    }
    fn menu(&self) -> Vec<MenuItem<Self>> {
        let mut menu = vec![note(&self.summary()).into(), MenuItem::Separator];
        if self.state.config.profiles.is_empty() {
            menu.push(note("No mounts yet").into());
        }
        // Lead with the mark: menu hosts clip long labels from the end.
        for profile in &self.state.config.profiles {
            let id = profile.id;
            let status = self.state.statuses.get(&id).unwrap_or(&Status::Unmounted);
            let (_, tone) = view::activity_line(status, self.state.activity.get(&id));
            menu.push(
                SubMenu {
                    label: format!("{}  {}", tone.mark(), profile.name),
                    submenu: self.mount_menu(id, status),
                    ..Default::default()
                }
                .into(),
            );
        }
        menu.push(MenuItem::Separator);
        menu.push(item("Open Rclone Tray", Action::Show).into());
        if self.state.quitting {
            menu.push(item("Quit now", Action::QuitNow).into());
        }
        menu.push(
            item(
                if self.state.quitting {
                    "Cancel quit"
                } else {
                    "Quit…"
                },
                Action::Quit,
            )
            .into(),
        );
        menu
    }
}

fn note(label: &str) -> StandardItem<AppTray> {
    StandardItem {
        label: label.into(),
        enabled: false,
        ..Default::default()
    }
}

fn item(label: &str, action: Action) -> StandardItem<AppTray> {
    StandardItem {
        label: label.into(),
        activate: Box::new(move |tray: &mut AppTray| {
            let _ = tray.actions.send(action.clone());
        }),
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        config::{Config, Profile},
        control::{Activity, Transfer},
    };

    #[test]
    fn tray_speeds_include_recovery_and_do_not_compete_with_profile_names() {
        let activity = |upload, download| {
            Ok(Activity {
                cache: Err("VFS still starting".into()),
                uploads: Err("VFS still starting".into()),
                transfers: vec![
                    Transfer {
                        name: "upload.bin".into(),
                        size: 9000,
                        bytes: 100,
                        speed_avg: upload,
                        src_fs: Some("cache:".into()),
                        dst_fs: Some("remote:".into()),
                    },
                    Transfer {
                        name: "download.bin".into(),
                        size: 7000,
                        bytes: 200,
                        speed_avg: download,
                        src_fs: Some("remote:".into()),
                        dst_fs: None,
                    },
                ],
            })
        };
        let profile = Profile {
            id: 1,
            name: "A deliberately long account name".into(),
            remote: "test".into(),
            folder: String::new(),
            mountpoint: "/tmp/test-tray".into(),
            cache_gib: 1,
            mount_at_start: false,
        };
        let mut second = profile.clone();
        second.id = 2;
        second.name = "Recovery".into();
        let (actions, _) = std::sync::mpsc::channel();
        let mut tray = AppTray {
            actions,
            state: Snapshot {
                config: Config {
                    profiles: vec![profile, second],
                },
                remotes: vec![],
                message: String::new(),
                quitting: false,
                statuses: [
                    (1, Status::Mounted),
                    (2, Status::Recovering),
                    (3, Status::Unmounted),
                    (4, Status::External),
                ]
                .into(),
                activity: [
                    (1, activity(1024.0, 3072.0)),
                    (2, activity(2048.0, 512.0)),
                    (3, activity(99999.0, 99999.0)),
                ]
                .into(),
            },
        };
        assert_eq!(
            tray.tool_tip().description,
            "1 of 2 mounted, ↑ 3.0 KiB/s, ↓ 3.5 KiB/s"
        );
        let menu = tray.menu();
        let profile_menus: Vec<_> = menu
            .iter()
            .filter_map(|item| match item {
                MenuItem::SubMenu(menu) => Some(menu),
                _ => None,
            })
            .collect();
        assert_eq!(
            profile_menus[0].label,
            "●  A deliberately long account name"
        );
        assert_eq!(profile_menus[1].label, "◌  Recovery");
        let labels: Vec<_> = profile_menus[1]
            .submenu
            .iter()
            .filter_map(|item| match item {
                MenuItem::Standard(item) => Some(item.label.as_str()),
                _ => None,
            })
            .collect();
        assert!(labels.contains(&"Recovering cache…"));
        assert!(labels.contains(&"Upload 2.0 KiB/s"));
        assert!(labels.contains(&"Download 512 B/s"));
        tray.state.activity.clear();
        assert_eq!(
            tray.tool_tip().description,
            "1 of 2 mounted, Activity unavailable"
        );
        assert_eq!(tray.status(), ksni::Status::Active);
        tray.state
            .statuses
            .insert(1, Status::Error("failed".into()));
        assert_eq!(tray.status(), ksni::Status::NeedsAttention);
        tray.state.statuses.clear();
        assert_eq!(tray.tool_tip().description, "0 of 2 mounted");
        tray.state.statuses.insert(1, Status::Mounted);
        tray.state.activity.insert(
            1,
            Ok(Activity {
                cache: Err("not ready".into()),
                uploads: Err("not ready".into()),
                transfers: vec![],
            }),
        );
        assert_eq!(
            tray.tool_tip().description,
            "1 of 2 mounted, No uploads queued"
        );
        assert_eq!(tray.icon_name(), "drive-harddisk");
    }
}
