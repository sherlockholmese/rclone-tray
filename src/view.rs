//! What the window says about mounts, kept free of GTK so every state can be tested.
use crate::{
    backend::{Snapshot, Status},
    config::Profile,
    control::{Activity, Transfer, byte_size},
};
use std::path::Path;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tone {
    Idle,
    Ok,
    Busy,
    Warning,
    Danger,
}

impl Tone {
    pub fn css_class(self) -> &'static str {
        match self {
            Self::Idle => "rclone-idle",
            Self::Ok => "rclone-ok",
            Self::Busy => "rclone-busy",
            Self::Warning => "rclone-warning",
            Self::Danger => "rclone-danger",
        }
    }

    /// Text mark for surfaces without custom drawing, such as the tray menu.
    pub fn mark(self) -> &'static str {
        match self {
            Self::Idle => "○",
            Self::Ok => "●",
            Self::Busy => "◌",
            Self::Warning | Self::Danger => "⚠",
        }
    }
}

/// The action a mount offers without opening its detail page.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Primary {
    Open,
    Mount,
    Cancel,
    Unavailable,
}

pub fn primary(status: &Status) -> Primary {
    match status {
        Status::Mounted | Status::External => Primary::Open,
        Status::Mounting | Status::Recovering | Status::Waiting => Primary::Cancel,
        Status::Unmounting => Primary::Unavailable,
        Status::Unmounted | Status::Error(_) => Primary::Mount,
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct Overview {
    pub headline: String,
    pub detail: String,
}

pub fn overview(state: &Snapshot) -> Overview {
    let total = state.config.profiles.len();
    let status = |id| state.statuses.get(&id).unwrap_or(&Status::Unmounted);
    let mounted: Vec<_> = state
        .config
        .profiles
        .iter()
        .filter(|p| matches!(status(p.id), Status::Mounted | Status::External))
        .collect();
    let headline = match (mounted.len(), total) {
        (_, 0) => "Rclone Tray".to_string(),
        (1, 1) => "Mounted".to_string(),
        (0, 1) => "Not mounted".to_string(),
        (count, _) if count == total => format!("All {total} mounted"),
        (count, _) => format!("{count} of {total} mounted"),
    };
    let attention = state
        .config
        .profiles
        .iter()
        .filter(|p| {
            let tone = activity_line(status(p.id), state.activity.get(&p.id)).1;
            matches!(status(p.id), Status::Error(_)) || tone == Tone::Danger
        })
        .count();
    let samples: Vec<_> = state
        .config
        .profiles
        .iter()
        .filter(|p| status(p.id).active())
        .filter_map(|p| state.activity.get(&p.id)?.as_ref().ok())
        .collect();
    let speed = rates_text(
        samples.iter().map(|a| a.rates().upload).sum(),
        samples.iter().map(|a| a.rates().download).sum(),
        samples.iter().map(|a| a.rates().other).sum(),
    );
    let queued = samples.iter().filter_map(|a| queued_uploads(a)).sum();
    let owned = mounted.iter().any(|p| *status(p.id) == Status::Mounted);
    let unavailable = state
        .config
        .profiles
        .iter()
        .filter(|p| status(p.id).active() && *status(p.id) != Status::External)
        .filter(|p| !matches!(state.activity.get(&p.id), Some(Ok(_))))
        .count();
    let detail = if state.quitting {
        "Quitting…".to_string()
    } else if attention > 0 {
        let verb = if attention == 1 { "needs" } else { "need" };
        format!("{attention} {verb} attention")
    } else if let Some(line) = join(speed, queued_text(queued)) {
        match unavailable {
            0 => line,
            count => format!("{line} · {count} unavailable"),
        }
    } else if unavailable > 0 {
        "Activity unavailable".to_string()
    } else if owned {
        "No uploads queued".to_string()
    } else {
        String::new()
    };
    Overview { headline, detail }
}

/// Short ledger text: the state when quiet, the live numbers when transfers run.
pub fn activity_line(status: &Status, sample: Option<&Result<Activity, String>>) -> (String, Tone) {
    let label = status.label().to_string();
    match status {
        Status::Unmounted => (label, Tone::Idle),
        Status::Mounting | Status::Recovering | Status::Unmounting => (label, Tone::Busy),
        Status::Waiting => (label, Tone::Warning),
        Status::External => (label, Tone::Ok),
        Status::Error(_) => (label, Tone::Danger),
        Status::Mounted => match sample {
            Some(Err(_)) => ("Activity unavailable".into(), Tone::Warning),
            None => (label, Tone::Ok),
            Some(Ok(activity)) => {
                if activity.cache.as_ref().is_ok_and(|c| c.out_of_space) {
                    return ("Cache full".into(), Tone::Danger);
                }
                if activity.cache.as_ref().is_ok_and(|c| c.errored_files > 0) {
                    return ("Upload needs retry".into(), Tone::Warning);
                }
                let rates = activity.rates();
                let speed = rates_text(rates.upload, rates.download, rates.other);
                let queued = queued_text(queued_uploads(activity).unwrap_or(0));
                (join(speed, queued).unwrap_or(label), Tone::Ok)
            }
        },
    }
}

/// The lamp and the row's button already say mounted or not. Text is for anything else.
pub fn ledger_note<'a>(status: &Status, line: &'a str) -> &'a str {
    if line == status.label() && matches!(status, Status::Mounted | Status::Unmounted) {
        ""
    } else {
        line
    }
}

pub fn status_hint(status: &Status) -> Option<&str> {
    match status {
        Status::Recovering => Some("Restoring cached uploads before the folder becomes available."),
        Status::Waiting => Some("Startup has exceeded 30 seconds. The log has details."),
        Status::External => Some("Managed by another session. Activity is unavailable."),
        Status::Error(error) => Some(error),
        _ => None,
    }
}

/// Cache trouble belongs beside the mount that has it, not only in a transient label.
pub fn cache_warning(sample: Option<&Result<Activity, String>>) -> Option<&'static str> {
    let cache = sample?.as_ref().ok()?.cache.as_ref().ok()?;
    if cache.out_of_space {
        Some("The cache is full. Free disk space so uploads can continue.")
    } else if cache.errored_files > 0 {
        Some("Some uploads failed and will retry. The log has details.")
    } else {
        None
    }
}

pub fn queued_uploads(activity: &Activity) -> Option<u64> {
    activity
        .uploads
        .as_ref()
        .ok()
        .map(|queue| queue.iter().filter(|item| !item.uploading).count() as u64)
        .or_else(|| activity.cache.as_ref().ok().map(|c| c.uploads_queued))
}

/// Share of the active transfers' known bytes. Unknown sizes are left out, never estimated.
pub fn progress(status: &Status, sample: Option<&Result<Activity, String>>) -> Option<f64> {
    if !status.active() {
        return None;
    }
    let activity = sample?.as_ref().ok()?;
    let known = activity.transfers.iter().filter(|t| t.size > 0);
    let (bytes, size) = known.fold((0.0, 0.0), |(bytes, size), t| {
        (bytes + t.bytes as f64, size + t.size as f64)
    });
    (size > 0.0).then(|| (bytes / size).clamp(0.0, 1.0))
}

pub fn short_path(path: &Path, home: Option<&Path>) -> String {
    match home.and_then(|home| path.strip_prefix(home).ok()) {
        Some(rest) if rest.as_os_str().is_empty() => "~".into(),
        Some(rest) => format!("~/{}", rest.display()),
        None => path.display().to_string(),
    }
}

pub fn remote_path(profile: &Profile) -> String {
    if profile.folder.is_empty() {
        profile.remote.clone()
    } else {
        format!("{}:{}", profile.remote, profile.folder)
    }
}

pub fn detail_line(profile: &Profile, sample: Option<&Result<Activity, String>>) -> String {
    let target = profile.cache_gib;
    let cache = match sample.and_then(|s| s.as_ref().ok()).map(|a| &a.cache) {
        Some(Ok(cache)) => format!(
            "{} cached of {target} GiB target",
            byte_size(cache.bytes_used as f64)
        ),
        _ => format!("{target} GiB cache target"),
    };
    format!("{} · {cache}", remote_path(profile))
}

fn rates_text(upload: f64, download: f64, other: f64) -> Option<String> {
    let parts = [("↑", upload), ("↓", download), ("Other", other)]
        .into_iter()
        .filter(|(_, rate)| *rate > 0.0)
        .map(|(mark, rate)| format!("{mark} {}/s", byte_size(rate)))
        .collect::<Vec<_>>();
    (!parts.is_empty()).then(|| parts.join("  "))
}

fn queued_text(count: u64) -> Option<String> {
    (count > 0).then(|| format!("{count} queued"))
}

fn join(first: Option<String>, second: Option<String>) -> Option<String> {
    match (first, second) {
        (Some(first), Some(second)) => Some(format!("{first} · {second}")),
        (first, second) => first.or(second),
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct QueueRow {
    pub name: String,
    pub detail: String,
    pub progress: f64,
}

#[derive(Debug, Default, PartialEq)]
pub struct QueuePane {
    pub notice: String,
    pub hint: Option<String>,
    pub rows: Vec<QueueRow>,
}

#[derive(Debug, Default, PartialEq)]
pub struct Queues {
    pub uploads: QueuePane,
    pub downloads: QueuePane,
}

/// Missing telemetry is reported as unavailable, never as an empty queue.
pub fn queues(snapshot: &Snapshot, id: u64) -> Queues {
    let profile = snapshot.config.profiles.iter().find(|p| p.id == id);
    let status = snapshot.statuses.get(&id).unwrap_or(&Status::Unmounted);
    let unavailable = if profile.is_none() {
        Some("Mount removed")
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
                "Queue unavailable, reconnecting…"
            } else {
                "Loading…"
            });
            let pane = || QueuePane {
                notice: message.into(),
                ..QueuePane::default()
            };
            return Queues {
                uploads: pane(),
                downloads: pane(),
            };
        }
    };
    Queues {
        uploads: upload_pane(activity),
        downloads: download_pane(activity),
    }
}

fn upload_pane(activity: &Activity) -> QueuePane {
    let mut pane = QueuePane::default();
    let mut waiting = Vec::new();
    let uploading = |t: &&Transfer| t.direction() == "Uploading";
    match &activity.uploads {
        Ok(queue) if queue.is_empty() => pane.notice = "No queued uploads".into(),
        Ok(queue) => {
            let active = queue.iter().filter(|item| item.uploading).count();
            pane.notice = format!("{active} uploading · {} queued", queue.len() - active);
            for item in queue {
                if let Some(transfer) = activity
                    .transfers
                    .iter()
                    .filter(uploading)
                    .find(|t| t.name == item.name)
                {
                    pane.rows.push(transfer_row(transfer));
                    continue;
                }
                let state = if item.uploading {
                    "Uploading".into()
                } else if item.expiry > 0.0 {
                    format!("Queued, eligible in {:.0}s", item.expiry.ceil())
                } else {
                    "Queued, waiting for a slot".into()
                };
                let attempts = if item.tries > 0 {
                    format!(" · {} attempts", item.tries)
                } else {
                    String::new()
                };
                let size = if item.size < 0 {
                    "Size unknown".into()
                } else {
                    byte_size(item.size as f64)
                };
                let row = QueueRow {
                    name: item.name.clone(),
                    detail: format!("{size} · {state}{attempts}"),
                    progress: 0.0,
                };
                if item.uploading {
                    pane.rows.push(row);
                } else {
                    waiting.push(row);
                }
            }
        }
        Err(error) => {
            pane.notice = "Upload queue unavailable".into();
            pane.hint = Some(error.clone());
        }
    }
    // Startup uploads can be visible before the VFS queue is registered. Never hide them.
    for transfer in activity.transfers.iter().filter(uploading) {
        let listed = activity
            .uploads
            .as_ref()
            .is_ok_and(|queue| queue.iter().any(|item| item.name == transfer.name));
        if !listed {
            pane.rows.push(transfer_row(transfer));
        }
    }
    // Moving files come first: a long waiting queue must not push them out of view.
    pane.rows.append(&mut waiting);
    pane
}

fn download_pane(activity: &Activity) -> QueuePane {
    let rows: Vec<_> = activity
        .transfers
        .iter()
        .filter(|t| t.direction() == "Downloading")
        .map(transfer_row)
        .collect();
    let unknown = activity
        .transfers
        .iter()
        .filter(|t| t.direction() == "Transferring")
        .count();
    let mut notice = if rows.is_empty() {
        "No active downloads".to_string()
    } else {
        format!("{} downloading", rows.len())
    };
    if unknown > 0 {
        notice.push_str(&format!(" · {unknown} unclassified"));
    }
    QueuePane {
        notice,
        hint: Some("Downloads start when files are read. rclone has no waiting queue.".into()),
        rows,
    }
}

fn transfer_row(transfer: &Transfer) -> QueueRow {
    let amount = if transfer.size > 0 {
        format!(
            "{} of {}",
            byte_size(transfer.bytes as f64),
            byte_size(transfer.size as f64)
        )
    } else {
        format!("{} transferred", byte_size(transfer.bytes as f64))
    };
    QueueRow {
        name: transfer.name.clone(),
        detail: format!("{amount} · {}/s", byte_size(transfer.speed_avg)),
        progress: if transfer.size > 0 {
            (transfer.bytes as f64 / transfer.size as f64).clamp(0.0, 1.0)
        } else {
            0.0
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        config::Config,
        control::{Cache, QueuedUpload},
    };
    use std::{collections::HashMap, path::PathBuf};

    fn profile(id: u64) -> Profile {
        Profile {
            id,
            name: format!("Mount {id}"),
            remote: "work".into(),
            folder: String::new(),
            mountpoint: PathBuf::from(format!("/home/you/{id}")),
            cache_gib: 10,
            mount_at_start: false,
        }
    }

    fn snapshot(statuses: &[Status]) -> Snapshot {
        let ids = 1..=statuses.len() as u64;
        Snapshot {
            config: Config {
                profiles: ids.clone().map(profile).collect(),
            },
            remotes: vec!["work".into()],
            statuses: ids.zip(statuses.iter().cloned()).collect(),
            activity: HashMap::new(),
            message: String::new(),
            quitting: false,
        }
    }

    fn cache() -> Cache {
        Cache {
            bytes_used: 0,
            uploads_queued: 0,
            uploads_in_progress: 0,
            errored_files: 0,
            out_of_space: false,
        }
    }

    fn idle() -> Activity {
        Activity {
            transfers: vec![],
            cache: Ok(cache()),
            uploads: Ok(vec![]),
        }
    }

    fn transfer(size: i64, bytes: u64) -> Transfer {
        Transfer {
            name: "report.pdf".into(),
            size,
            bytes,
            speed_avg: 2048.0,
            src_fs: None,
            dst_fs: Some("work:".into()),
        }
    }

    fn queued(name: &str) -> QueuedUpload {
        QueuedUpload {
            name: name.into(),
            size: 1024,
            expiry: 0.0,
            tries: 0,
            uploading: false,
        }
    }

    #[test]
    fn overview_counts_mounts_and_never_claims_an_empty_queue_without_telemetry() {
        let mut state = snapshot(&[Status::Mounted, Status::Mounted, Status::Unmounted]);
        assert_eq!(overview(&state).headline, "2 of 3 mounted");
        assert_eq!(overview(&state).detail, "Activity unavailable");
        state.activity.insert(1, Ok(idle()));
        state.activity.insert(2, Ok(idle()));
        assert_eq!(overview(&state).detail, "No uploads queued");
        state.statuses.insert(3, Status::Error("failed".into()));
        assert_eq!(overview(&state).detail, "1 needs attention");
        assert_eq!(overview(&snapshot(&[])).headline, "Rclone Tray");
        assert_eq!(overview(&snapshot(&[Status::Mounted])).headline, "Mounted");
        let all = snapshot(&[Status::Mounted, Status::External]);
        assert_eq!(overview(&all).headline, "All 2 mounted");
    }

    #[test]
    fn overview_reports_live_transfers_across_mounts() {
        let mut state = snapshot(&[Status::Mounted, Status::Recovering]);
        let mut first = idle();
        first.transfers = vec![transfer(4096, 1024)];
        let mut second = idle();
        second.uploads = Ok(vec![queued("a"), queued("b")]);
        state.activity.insert(1, Ok(first));
        state.activity.insert(2, Ok(second));
        assert_eq!(overview(&state).detail, "↑ 2.0 KiB/s · 2 queued");
        state.quitting = true;
        assert_eq!(overview(&state).detail, "Quitting…");
    }

    #[test]
    fn ledger_line_prefers_problems_then_numbers_then_state() {
        let quiet = Ok(idle());
        assert_eq!(
            activity_line(&Status::Mounted, Some(&quiet)),
            ("Mounted".into(), Tone::Ok)
        );
        let mut busy = idle();
        busy.transfers = vec![transfer(4096, 1024)];
        assert_eq!(
            activity_line(&Status::Mounted, Some(&Ok(busy))).0,
            "↑ 2.0 KiB/s"
        );
        let mut full = idle();
        full.cache = Ok(Cache {
            out_of_space: true,
            ..cache()
        });
        assert_eq!(
            activity_line(&Status::Mounted, Some(&Ok(full))),
            ("Cache full".into(), Tone::Danger)
        );
        assert_eq!(
            activity_line(&Status::Mounted, Some(&Err("down".into()))).1,
            Tone::Warning
        );
    }

    #[test]
    fn ledger_note_omits_only_the_plain_default_states() {
        assert_eq!(ledger_note(&Status::Mounted, "Mounted"), "");
        assert_eq!(ledger_note(&Status::Unmounted, "Unmounted"), "");
        assert_eq!(ledger_note(&Status::Mounted, "Cache full"), "Cache full");
        let label = Status::External.label().to_string();
        assert_eq!(ledger_note(&Status::External, &label), label);
    }

    #[test]
    fn every_status_offers_one_truthful_action() {
        assert_eq!(primary(&Status::Mounted), Primary::Open);
        assert_eq!(primary(&Status::External), Primary::Open);
        assert_eq!(primary(&Status::Unmounted), Primary::Mount);
        assert_eq!(primary(&Status::Error("x".into())), Primary::Mount);
        assert_eq!(primary(&Status::Recovering), Primary::Cancel);
        assert_eq!(primary(&Status::Unmounting), Primary::Unavailable);
    }

    #[test]
    fn progress_is_proportional_and_skips_unknown_sizes() {
        let mut activity = idle();
        activity.transfers = vec![transfer(1000, 250), transfer(3000, 750), transfer(-1, 99)];
        let sample = Ok(activity);
        assert_eq!(progress(&Status::Mounted, Some(&sample)), Some(0.25));
        assert_eq!(progress(&Status::Unmounted, Some(&sample)), None);
        assert_eq!(progress(&Status::Mounted, Some(&Ok(idle()))), None);
        let mut unknown = idle();
        unknown.transfers = vec![transfer(-1, 99)];
        assert_eq!(progress(&Status::Mounted, Some(&Ok(unknown))), None);
    }

    #[test]
    fn detail_line_names_the_remote_and_cache_without_inventing_usage() {
        let mut mount = profile(1);
        mount.folder = "Reports".into();
        assert_eq!(
            detail_line(&mount, None),
            "work:Reports · 10 GiB cache target"
        );
        assert_eq!(
            detail_line(&mount, Some(&Ok(idle()))),
            "work:Reports · 0 B cached of 10 GiB target"
        );
    }

    #[test]
    fn paths_shorten_only_inside_home() {
        let home = Path::new("/home/you");
        assert_eq!(
            short_path(Path::new("/home/you/Cloud"), Some(home)),
            "~/Cloud"
        );
        assert_eq!(short_path(home, Some(home)), "~");
        assert_eq!(
            short_path(Path::new("/home/youth"), Some(home)),
            "/home/youth"
        );
        assert_eq!(short_path(Path::new("/mnt/a"), None), "/mnt/a");
    }

    #[test]
    fn queues_distinguish_unavailable_from_empty_and_keep_startup_uploads() {
        let mut state = snapshot(&[Status::Mounted]);
        assert_eq!(queues(&state, 1).uploads.notice, "Loading…");
        state.activity.insert(1, Err("down".into()));
        assert_eq!(
            queues(&state, 1).uploads.notice,
            "Queue unavailable, reconnecting…"
        );
        state.activity.insert(1, Ok(idle()));
        assert_eq!(queues(&state, 1).uploads.notice, "No queued uploads");
        let mut starting = idle();
        starting.uploads = Err("VFS starting".into());
        starting.transfers = vec![transfer(1000, 500)];
        state.activity.insert(1, Ok(starting));
        let pane = queues(&state, 1).uploads;
        assert_eq!(pane.notice, "Upload queue unavailable");
        assert_eq!(pane.rows.len(), 1);
        assert_eq!(pane.rows[0].progress, 0.5);
        assert_eq!(queues(&state, 9).uploads.notice, "Mount removed");
        let mut mixed = idle();
        mixed.uploads = Ok(vec![queued("waiting"), queued("report.pdf")]);
        mixed.transfers = vec![transfer(1000, 500)];
        state.activity.insert(1, Ok(mixed));
        assert_eq!(queues(&state, 1).uploads.rows[0].name, "report.pdf");
    }
}
