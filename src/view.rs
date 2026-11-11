//! What the window says about mounts, kept free of UI toolkits so every state can be tested.
use crate::{
    backend::{Snapshot, Status},
    config::Profile,
    control::{Activity, Transfer, byte_size},
};
use std::{collections::HashMap, path::Path};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tone {
    Idle,
    Ok,
    Busy,
    Warning,
    Danger,
}

impl Tone {
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
    /// Separate facts. Each surface lays them out itself, so none is joined with punctuation here.
    pub facts: Vec<String>,
}

impl Overview {
    /// One line for text-only surfaces such as the tray tooltip.
    pub fn summary(&self) -> String {
        std::iter::once(self.headline.as_str())
            .chain(self.facts.iter().map(String::as_str))
            .collect::<Vec<_>>()
            .join(", ")
    }
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
        (_, 0) => "No mounts".to_string(),
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
    let mut live = rates(
        samples.iter().map(|a| a.rates().upload).sum(),
        samples.iter().map(|a| a.rates().download).sum(),
        samples.iter().map(|a| a.rates().other).sum(),
    );
    live.extend(queued_text(
        samples.iter().filter_map(|a| queued_uploads(a)).sum(),
    ));
    let owned = mounted.iter().any(|p| *status(p.id) == Status::Mounted);
    let unavailable = state
        .config
        .profiles
        .iter()
        .filter(|p| status(p.id).active() && *status(p.id) != Status::External)
        .filter(|p| !matches!(state.activity.get(&p.id), Some(Ok(_))))
        .count();
    let facts = if state.quitting {
        vec!["Quitting…".to_string()]
    } else if attention > 0 {
        let verb = if attention == 1 { "needs" } else { "need" };
        vec![format!("{attention} {verb} attention")]
    } else if !live.is_empty() {
        if unavailable > 0 {
            live.push(format!("{unavailable} unavailable"));
        }
        live
    } else if unavailable > 0 {
        vec!["Activity unavailable".to_string()]
    } else if owned {
        vec!["No uploads queued".to_string()]
    } else {
        vec![]
    };
    Overview { headline, facts }
}

/// The state when quiet, the live numbers when transfers run. Facts stay separate.
pub fn activity_line(
    status: &Status,
    sample: Option<&Result<Activity, String>>,
) -> (Vec<String>, Tone) {
    let label = || vec![status.label().to_string()];
    match status {
        Status::Unmounted => (label(), Tone::Idle),
        Status::Mounting | Status::Recovering | Status::Unmounting => (label(), Tone::Busy),
        Status::Waiting => (label(), Tone::Warning),
        Status::External => (label(), Tone::Ok),
        Status::Error(_) => (label(), Tone::Danger),
        Status::Mounted => match sample {
            Some(Err(_)) => (vec!["Activity unavailable".into()], Tone::Warning),
            None => (label(), Tone::Ok),
            Some(Ok(activity)) => {
                if activity.cache.as_ref().is_ok_and(|c| c.out_of_space) {
                    return (vec!["Cache full".into()], Tone::Danger);
                }
                if activity.cache.as_ref().is_ok_and(|c| c.errored_files > 0) {
                    return (vec!["Upload needs retry".into()], Tone::Warning);
                }
                let r = activity.rates();
                let mut facts = rates(r.upload, r.download, r.other);
                facts.extend(queued_text(queued_uploads(activity).unwrap_or(0)));
                if facts.is_empty() {
                    (label(), Tone::Ok)
                } else {
                    (facts, Tone::Ok)
                }
            }
        },
    }
}

/// The switch already says mounted or not. Row text is for anything else.
/// A mount row's live facts in two lines that mirror its name and path: what is moving now
/// beside the name, what remains beside the path. Two fixed lines at every width, so the row
/// never grows. Problems and plain states keep the single line `activity_line` gives them.
pub fn ledger_lines(
    status: &Status,
    sample: Option<&Result<Activity, String>>,
    batch: Option<&UploadBatch>,
) -> (Vec<String>, Vec<String>, Tone) {
    let (facts, tone) = activity_line(status, sample);
    if let (Status::Mounted, Some(Ok(activity)), Tone::Ok) = (status, sample, tone) {
        let r = activity.rates();
        let now = rates(r.upload, r.download, r.other);
        let mut rest: Vec<String> = queued_text(queued_uploads(activity).unwrap_or(0))
            .into_iter()
            .collect();
        rest.extend(batch.and_then(UploadBatch::time_left));
        if !now.is_empty() || !rest.is_empty() {
            return (now, rest, tone);
        }
    }
    (ledger_note(status, facts), vec![], tone)
}

pub fn ledger_note(status: &Status, facts: Vec<String>) -> Vec<String> {
    let plain = facts.len() == 1 && facts[0] == status.label();
    if plain && matches!(status, Status::Mounted | Status::Unmounted) {
        vec![]
    } else {
        facts
    }
}

pub fn status_hint(status: &Status) -> Option<&str> {
    match status {
        Status::Recovering => Some("Uploading cached files before the folder opens."),
        Status::Waiting => Some("Startup is slow. The log shows why."),
        Status::External => Some("Mounted by another session."),
        Status::Error(error) => Some(error),
        _ => None,
    }
}

/// Cache trouble belongs beside the mount that has it, not only in a transient label.
pub fn cache_warning(sample: Option<&Result<Activity, String>>) -> Option<&'static str> {
    let cache = sample?.as_ref().ok()?.cache.as_ref().ok()?;
    if cache.out_of_space {
        Some("Cache is full. Free disk space so uploads can continue.")
    } else if cache.errored_files > 0 {
        Some("Some uploads failed and will retry.")
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

/// Overall progress of the uploads pending together on one mount. A batch begins when the first
/// upload appears and ends when none remain, so the bar does not restart with every file. Files
/// that leave the queue count as sent. Unknown sizes are left out, never estimated.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct UploadBatch {
    /// Size and bytes sent of each upload pending in the last sample.
    pending: HashMap<String, (u64, u64)>,
    /// Bytes of the uploads that left the queue during this batch.
    finished: u64,
    /// Combined speed of the uploads moving in the last sample, in bytes per second.
    rate: f64,
}

/// Time to move `bytes` at `rate`. Coarse past a minute, so it does not flicker between
/// samples. None when nothing is moving: an estimate is never invented.
pub fn time_left(bytes: u64, rate: f64) -> Option<String> {
    if bytes == 0 || rate < 1.0 {
        return None;
    }
    let seconds = (bytes as f64 / rate).ceil() as u64;
    let minutes = seconds.div_ceil(60);
    Some(match seconds {
        0..60 => format!("{seconds}s left"),
        60..3600 => format!("{minutes} min left"),
        _ if minutes.is_multiple_of(60) => format!("{} h left", minutes / 60),
        _ => format!("{} h {} min left", minutes / 60, minutes % 60),
    })
}

impl UploadBatch {
    pub fn observe(&mut self, activity: &Activity) {
        let mut now: HashMap<String, (u64, u64)> = HashMap::new();
        if let Ok(queue) = &activity.uploads {
            for item in queue.iter().filter(|item| item.size > 0) {
                let size = item.size as u64;
                // Between samples a finished transfer can vanish before its queue entry does.
                // Keep what it already sent rather than dropping the bar back.
                let sent = match self.pending.get(&item.name) {
                    Some(&(before, sent)) if item.uploading && before == size => sent,
                    _ => 0,
                };
                now.insert(item.name.clone(), (size, sent));
            }
        }
        let mut rate = 0.0;
        for transfer in &activity.transfers {
            if transfer.direction() == "Uploading" && transfer.size > 0 {
                let size = transfer.size as u64;
                now.insert(transfer.name.clone(), (size, transfer.bytes.min(size)));
                rate += transfer.speed_avg;
            }
        }
        if now.is_empty() {
            *self = Self::default();
            return;
        }
        self.rate = rate;
        for (name, (size, _)) in &self.pending {
            if !now.contains_key(name) {
                self.finished += size;
            }
        }
        self.pending = now;
    }

    /// Bytes sent and bytes in the whole batch, while uploads are pending.
    pub fn totals(&self) -> Option<(u64, u64)> {
        let (size, sent) = self
            .pending
            .values()
            .fold((0, 0), |(size, sent), &(s, b)| (size + s, sent + b));
        let total = self.finished + size;
        (!self.pending.is_empty() && total > 0).then_some((self.finished + sent, total))
    }

    pub fn progress(&self) -> Option<f64> {
        self.totals()
            .map(|(sent, total)| (sent as f64 / total as f64).clamp(0.0, 1.0))
    }

    /// The batch in words, beside the queue it summarizes.
    pub fn summary(&self) -> Option<String> {
        self.totals().map(|(sent, total)| {
            format!(
                "{} of {} uploaded",
                byte_size(sent as f64),
                byte_size(total as f64)
            )
        })
    }

    /// The rest of the batch at the current upload speed.
    pub fn time_left(&self) -> Option<String> {
        self.totals()
            .and_then(|(sent, total)| time_left(total - sent, self.rate))
    }
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
        format!("{}:", profile.remote)
    } else {
        format!("{}:{}", profile.remote, profile.folder)
    }
}

/// Live cache use while mounted, the measured size on disk otherwise.
pub fn cache_line(
    profile: &Profile,
    sample: Option<&Result<Activity, String>>,
    on_disk: Option<u64>,
) -> String {
    let target = profile.cache_gib;
    let used = match sample.and_then(|s| s.as_ref().ok()).map(|a| &a.cache) {
        Some(Ok(cache)) => Some(cache.bytes_used),
        _ => on_disk,
    };
    match used {
        Some(bytes) => format!("{} of {target} GiB cache", byte_size(bytes as f64)),
        None => format!("{target} GiB cache"),
    }
}

fn rates(upload: f64, download: f64, other: f64) -> Vec<String> {
    [("↑", upload), ("↓", download), ("Other", other)]
        .into_iter()
        .filter(|(_, rate)| *rate > 0.0)
        .map(|(mark, rate)| format!("{mark} {}/s", byte_size(rate)))
        .collect()
}

fn queued_text(count: u64) -> Option<String> {
    (count > 0).then(|| format!("{count} queued"))
}

#[derive(Clone, Debug, PartialEq)]
pub struct QueueRow {
    pub name: String,
    pub facts: Vec<String>,
    pub progress: f64,
}

#[derive(Debug, Default, PartialEq)]
pub struct QueuePane {
    pub facts: Vec<String>,
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
        Some("Mounted by another session")
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
                facts: vec![message.into()],
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
        Ok(queue) if queue.is_empty() => pane.facts = vec!["No queued uploads".into()],
        Ok(queue) => {
            let active = queue.iter().filter(|item| item.uploading).count();
            pane.facts = vec![
                format!("{active} uploading"),
                format!("{} queued", queue.len() - active),
            ];
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
                let mut facts = vec![if item.size < 0 {
                    "Size unknown".into()
                } else {
                    byte_size(item.size as f64)
                }];
                facts.push(if item.uploading {
                    "Uploading".into()
                } else if item.expiry > 0.0 {
                    format!("Starts in {:.0}s", item.expiry.ceil())
                } else {
                    "Waiting for a slot".into()
                });
                if item.tries > 0 {
                    facts.push(format!("{} attempts", item.tries));
                }
                let row = QueueRow {
                    name: item.name.clone(),
                    facts,
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
            pane.facts = vec!["Upload queue unavailable".into()];
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
    let mut facts = vec![if rows.is_empty() {
        "No active downloads".to_string()
    } else {
        format!("{} downloading", rows.len())
    }];
    // With one download its row already says this. Several share the line, so add them up,
    // counting only downloads whose size is known.
    if rows.len() > 1 {
        let (left, rate) = activity
            .transfers
            .iter()
            .filter(|t| t.direction() == "Downloading")
            .filter_map(|t| remaining(t).map(|left| (left, t.speed_avg)))
            .fold((0, 0.0), |(left, rate), (l, r)| (left + l, rate + r));
        facts.extend(time_left(left, rate));
    }
    if unknown > 0 {
        facts.push(format!("{unknown} unclassified"));
    }
    QueuePane {
        facts,
        hint: None,
        rows,
    }
}

/// Bytes still to move, when the size is known.
fn remaining(transfer: &Transfer) -> Option<u64> {
    (transfer.size > 0).then(|| (transfer.size as u64).saturating_sub(transfer.bytes))
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
    let mut facts = vec![amount, format!("{}/s", byte_size(transfer.speed_avg))];
    facts.extend(remaining(transfer).and_then(|left| time_left(left, transfer.speed_avg)));
    QueueRow {
        name: transfer.name.clone(),
        facts,
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
            upload_threads: 4,
            download_threads: 4,
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
        assert_eq!(overview(&state).facts, ["Activity unavailable"]);
        state.activity.insert(1, Ok(idle()));
        state.activity.insert(2, Ok(idle()));
        assert_eq!(overview(&state).facts, ["No uploads queued"]);
        state.statuses.insert(3, Status::Error("failed".into()));
        assert_eq!(overview(&state).facts, ["1 needs attention"]);
        assert_eq!(
            overview(&state).summary(),
            "2 of 3 mounted, 1 needs attention"
        );
        assert_eq!(overview(&snapshot(&[])).headline, "No mounts");
        assert!(overview(&snapshot(&[])).facts.is_empty());
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
        assert_eq!(overview(&state).facts, ["↑ 2.0 KiB/s", "2 queued"]);
        state.quitting = true;
        assert_eq!(overview(&state).facts, ["Quitting…"]);
    }

    #[test]
    fn ledger_line_prefers_problems_then_numbers_then_state() {
        let quiet = Ok(idle());
        assert_eq!(
            activity_line(&Status::Mounted, Some(&quiet)),
            (vec!["Mounted".to_string()], Tone::Ok)
        );
        let mut busy = idle();
        busy.transfers = vec![transfer(4096, 1024)];
        assert_eq!(
            activity_line(&Status::Mounted, Some(&Ok(busy))).0,
            ["↑ 2.0 KiB/s"]
        );
        let mut full = idle();
        full.cache = Ok(Cache {
            out_of_space: true,
            ..cache()
        });
        assert_eq!(
            activity_line(&Status::Mounted, Some(&Ok(full))),
            (vec!["Cache full".to_string()], Tone::Danger)
        );
        assert_eq!(
            activity_line(&Status::Mounted, Some(&Err("down".into()))).1,
            Tone::Warning
        );
    }

    #[test]
    fn ledger_note_omits_only_the_plain_default_states() {
        let note = |status: &Status, text: &str| ledger_note(status, vec![text.to_string()]);
        assert!(note(&Status::Mounted, "Mounted").is_empty());
        assert!(note(&Status::Unmounted, "Unmounted").is_empty());
        assert_eq!(note(&Status::Mounted, "Cache full"), ["Cache full"]);
        let label = Status::External.label();
        assert_eq!(note(&Status::External, label), [label]);
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
    fn ledger_lines_put_motion_beside_the_name_and_what_remains_beside_the_path() {
        let mut sample = idle();
        sample.uploads = Ok(vec![queued("a"), queued("b")]);
        sample.transfers = vec![transfer(4096, 0)];
        let mut batch = UploadBatch::default();
        batch.observe(&sample);
        let sample = Ok(sample);
        let (now, rest, tone) = ledger_lines(&Status::Mounted, Some(&sample), Some(&batch));
        assert_eq!(now, ["↑ 2.0 KiB/s"]);
        assert_eq!(rest, ["2 queued", "3s left"]);
        assert_eq!(tone, Tone::Ok);
        // Problems and plain states stay on one line, with no estimate beside them.
        let (now, rest, _) = ledger_lines(&Status::Unmounted, None, Some(&batch));
        assert_eq!((now, rest), (vec![], vec![]));
        let down = Err("down".to_string());
        let (now, rest, tone) = ledger_lines(&Status::Mounted, Some(&down), Some(&batch));
        assert_eq!(
            (now, rest, tone),
            (vec!["Activity unavailable".into()], vec![], Tone::Warning)
        );
    }

    #[test]
    fn time_left_is_coarse_and_never_invented() {
        assert_eq!(time_left(0, 100.0), None);
        assert_eq!(time_left(1000, 0.0), None);
        assert_eq!(time_left(1000, 100.0).unwrap(), "10s left");
        assert_eq!(time_left(5_900, 100.0).unwrap(), "59s left");
        assert_eq!(time_left(6_000, 100.0).unwrap(), "1 min left");
        assert_eq!(time_left(6_100, 100.0).unwrap(), "2 min left");
        assert_eq!(time_left(360_000, 100.0).unwrap(), "1 h left");
        assert_eq!(time_left(450_000, 100.0).unwrap(), "1 h 15 min left");

        // Several downloads share one estimate; a single one leaves it to its row.
        let download = |name: &str, size: i64, bytes: u64| Transfer {
            name: name.into(),
            src_fs: Some("work:".into()),
            dst_fs: None,
            ..transfer(size, bytes)
        };
        let mut sample = idle();
        sample.transfers = vec![download("a", 10_000_000, 0)];
        let pane = download_pane(&sample);
        assert_eq!(pane.facts, ["1 downloading"]);
        assert_eq!(pane.rows[0].facts[2], "1 h 22 min left");
        sample.transfers.push(download("b", 4096, 0));
        assert_eq!(
            download_pane(&sample).facts,
            ["2 downloading", "41 min left"]
        );
    }

    #[test]
    fn upload_progress_covers_the_whole_batch_and_skips_unknown_sizes() {
        let sized = |name: &str, size: i64, uploading: bool| QueuedUpload {
            size,
            uploading,
            ..queued(name)
        };
        let sending = |name: &str, size: i64, bytes: u64| Transfer {
            name: name.into(),
            ..transfer(size, bytes)
        };
        let mut batch = UploadBatch::default();
        assert_eq!(batch.progress(), None);

        // Two files of 1000 bytes, the first half sent; a third of unknown size is ignored.
        let mut sample = idle();
        sample.uploads = Ok(vec![
            sized("a", 1000, true),
            sized("b", 1000, false),
            sized("c", -1, false),
        ]);
        sample.transfers = vec![sending("a", 1000, 500)];
        batch.observe(&sample);
        assert_eq!(batch.totals(), Some((500, 2000)));

        // The transfer ends before its queue entry: what it sent is kept.
        sample.transfers.clear();
        batch.observe(&sample);
        assert_eq!(batch.totals(), Some((500, 2000)));

        // "a" finished and stays counted, so the bar continues instead of restarting.
        sample.uploads = Ok(vec![sized("b", 1000, true)]);
        sample.transfers = vec![sending("b", 1000, 250)];
        batch.observe(&sample);
        assert_eq!(batch.totals(), Some((1250, 2000)));
        assert_eq!(batch.progress(), Some(0.625));
        assert_eq!(batch.summary().unwrap(), "1.2 KiB of 2.0 KiB uploaded");
        // 750 bytes left at the 2 KiB/s of the one moving upload.
        assert_eq!(batch.time_left().unwrap(), "1s left");

        // A new file joins the running batch.
        sample.uploads = Ok(vec![sized("b", 1000, true), sized("d", 2000, false)]);
        batch.observe(&sample);
        assert_eq!(batch.totals(), Some((1250, 4000)));

        // Nothing left: the batch ends, and the next one starts from zero.
        batch.observe(&idle());
        assert_eq!(batch.progress(), None);
        assert_eq!(batch.summary(), None);
        sample.uploads = Ok(vec![sized("e", 100, false)]);
        sample.transfers.clear();
        batch.observe(&sample);
        assert_eq!(batch.totals(), Some((0, 100)));
    }

    #[test]
    fn detail_names_the_remote_and_cache_without_inventing_usage() {
        let mut mount = profile(1);
        assert_eq!(remote_path(&mount), "work:");
        mount.folder = "Reports".into();
        assert_eq!(remote_path(&mount), "work:Reports");
        assert_eq!(cache_line(&mount, None, None), "10 GiB cache");
        assert_eq!(
            cache_line(&mount, None, Some(3 << 20)),
            "3.0 MiB of 10 GiB cache"
        );
        assert_eq!(
            cache_line(&mount, Some(&Ok(idle())), Some(3 << 20)),
            "0 B of 10 GiB cache"
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
        assert_eq!(queues(&state, 1).uploads.facts, ["Loading…"]);
        state.activity.insert(1, Err("down".into()));
        assert_eq!(
            queues(&state, 1).uploads.facts,
            ["Queue unavailable, reconnecting…"]
        );
        state.activity.insert(1, Ok(idle()));
        assert_eq!(queues(&state, 1).uploads.facts, ["No queued uploads"]);
        assert_eq!(queues(&state, 1).downloads.facts, ["No active downloads"]);
        let mut starting = idle();
        starting.uploads = Err("VFS starting".into());
        starting.transfers = vec![transfer(1000, 500)];
        state.activity.insert(1, Ok(starting));
        let pane = queues(&state, 1).uploads;
        assert_eq!(pane.facts, ["Upload queue unavailable"]);
        assert_eq!(
            pane.rows[0].facts,
            ["500 B of 1000 B", "2.0 KiB/s", "1s left"]
        );
        assert_eq!(pane.rows.len(), 1);
        assert_eq!(pane.rows[0].progress, 0.5);
        assert_eq!(queues(&state, 9).uploads.facts, ["Mount removed"]);
        let mut mixed = idle();
        mixed.uploads = Ok(vec![queued("waiting"), queued("report.pdf")]);
        mixed.transfers = vec![transfer(1000, 500)];
        state.activity.insert(1, Ok(mixed));
        let pane = queues(&state, 1).uploads;
        assert_eq!(pane.facts, ["0 uploading", "2 queued"]);
        assert_eq!(pane.rows[0].name, "report.pdf");
        assert_eq!(pane.rows[1].facts, ["1.0 KiB", "Waiting for a slot"]);
    }
}
