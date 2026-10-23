use crate::backend::{Snapshot, Status};
use crate::control::{Rates, byte_size};
use ksni::{
    MenuItem, Tray,
    menu::{StandardItem, SubMenu},
};
use std::sync::mpsc::Sender;

#[derive(Clone)]
pub enum Action {
    Show,
    Mount(u64),
    Unmount(u64),
    Open(u64),
    Queue(u64),
    Log(u64),
    Quit,
}

pub struct AppTray {
    pub state: Snapshot,
    pub actions: Sender<Action>,
}

impl AppTray {
    fn transfer_summary(&self) -> String {
        let mut rates = Rates::default();
        let mut known = 0;
        let mut unavailable = 0;
        for (id, status) in &self.state.statuses {
            if !status.active() {
                continue;
            }
            match self
                .state
                .activity
                .get(id)
                .filter(|_| *status != Status::External)
            {
                Some(Ok(activity)) => {
                    let mount = activity.rates();
                    rates.upload += mount.upload;
                    rates.download += mount.download;
                    rates.other += mount.other;
                    known += 1;
                }
                _ => unavailable += 1,
            }
        }
        match (known, unavailable) {
            (0, 0) => "No active mounts".into(),
            (0, _) => "Transfer status unavailable".into(),
            (_, 0) => rates.label(),
            _ => format!("{} - {unavailable} unavailable", rates.label()),
        }
    }
}

impl Tray for AppTray {
    fn id(&self) -> String {
        "rclone-tray".into()
    }
    fn title(&self) -> String {
        format!("Rclone Tray — {}", self.transfer_summary())
    }
    fn tool_tip(&self) -> ksni::ToolTip {
        ksni::ToolTip {
            title: "Rclone Tray".into(),
            description: self.transfer_summary(),
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
        let mut menu = vec![
            StandardItem {
                label: self.transfer_summary(),
                enabled: false,
                ..Default::default()
            }
            .into(),
            MenuItem::Separator,
            item("Mounts & settings…", Action::Show).into(),
            MenuItem::Separator,
        ];
        if self.state.config.profiles.is_empty() {
            menu.push(
                StandardItem {
                    label: "No mount profiles yet".into(),
                    enabled: false,
                    ..Default::default()
                }
                .into(),
            );
        }
        for profile in &self.state.config.profiles {
            let id = profile.id;
            let status = self.state.statuses.get(&id).unwrap_or(&Status::Unmounted);
            let mut children = vec![];
            match self.state.activity.get(&id) {
                Some(Ok(activity)) if status.active() && *status != Status::External => {
                    let rates = activity.rates();
                    for label in [
                        format!("Upload {}/s", byte_size(rates.upload)),
                        format!("Download {}/s", byte_size(rates.download)),
                    ] {
                        children.push(
                            StandardItem {
                                label,
                                enabled: false,
                                ..Default::default()
                            }
                            .into(),
                        );
                    }
                    if rates.other > 0.0 {
                        children.push(
                            StandardItem {
                                label: format!("Other {}/s", byte_size(rates.other)),
                                enabled: false,
                                ..Default::default()
                            }
                            .into(),
                        );
                    }
                    if let Ok(cache) = &activity.cache
                        && cache.uploads_queued > 0
                    {
                        children.push(
                            StandardItem {
                                label: format!("{} uploads queued", cache.uploads_queued),
                                enabled: false,
                                ..Default::default()
                            }
                            .into(),
                        );
                    }
                }
                _ if status.active() => {
                    children.push(
                        StandardItem {
                            label: "Transfer status unavailable".into(),
                            enabled: false,
                            ..Default::default()
                        }
                        .into(),
                    );
                }
                _ => {}
            }
            children.push(item("View status", Action::Show).into());
            children.push(item("Transfer queue…", Action::Queue(id)).into());
            match status {
                Status::Mounted => {
                    children.push(item("Open folder", Action::Open(id)).into());
                    children.push(item("Unmount…", Action::Unmount(id)).into());
                }
                Status::Unmounted | Status::Error(_) => {
                    children.push(item("Mount", Action::Mount(id)).into())
                }
                Status::Mounting | Status::Recovering | Status::Waiting => {
                    children.push(item("Cancel mount…", Action::Unmount(id)).into())
                }
                _ => {}
            }
            children.push(item("View log", Action::Log(id)).into());
            menu.push(
                SubMenu {
                    label: format!("{} — {}", profile.name, status.label()),
                    submenu: children,
                    ..Default::default()
                }
                .into(),
            );
        }
        menu.push(MenuItem::Separator);
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
            "↑ 3.0 KiB/s - ↓ 3.5 KiB/s - 1 unavailable"
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
            "A deliberately long account name — Mounted"
        );
        assert_eq!(profile_menus[1].label, "Recovery — Recovering cache…");
        let labels: Vec<_> = profile_menus[1]
            .submenu
            .iter()
            .filter_map(|item| match item {
                MenuItem::Standard(item) => Some(item.label.as_str()),
                _ => None,
            })
            .collect();
        assert!(labels.contains(&"Upload 2.0 KiB/s"));
        assert!(labels.contains(&"Download 512 B/s"));
        tray.state.activity.clear();
        assert_eq!(tray.tool_tip().description, "Transfer status unavailable");
        tray.state.statuses.clear();
        assert_eq!(tray.tool_tip().description, "No active mounts");
        tray.state.statuses.insert(1, Status::Mounted);
        tray.state.activity.insert(
            1,
            Ok(Activity {
                cache: Err("not ready".into()),
                uploads: Err("not ready".into()),
                transfers: vec![],
            }),
        );
        assert_eq!(tray.tool_tip().description, "↑ 0 B/s - ↓ 0 B/s");
        assert_eq!(tray.icon_name(), "drive-harddisk");
    }
}
