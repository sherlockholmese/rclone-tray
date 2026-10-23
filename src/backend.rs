use crate::config::{Config, Paths, Profile};
use crate::control::{Activity, Control, Update};
use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::mpsc::{self, Receiver, Sender},
    thread,
    time::{Duration, Instant},
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Status {
    Unmounted,
    Mounting,
    Recovering,
    Waiting,
    Mounted,
    Unmounting,
    External,
    Error(String),
}

impl Status {
    pub fn label(&self) -> &str {
        match self {
            Self::Unmounted => "Unmounted",
            Self::Mounting => "Mounting…",
            Self::Recovering => "Recovering cache…",
            Self::Waiting => "Taking longer…",
            Self::Mounted => "Mounted",
            Self::Unmounting => "Unmounting…",
            Self::External => "Mounted elsewhere",
            Self::Error(_) => "Needs attention",
        }
    }
    pub fn active(&self) -> bool {
        matches!(
            self,
            Self::Mounting
                | Self::Recovering
                | Self::Waiting
                | Self::Mounted
                | Self::Unmounting
                | Self::External
        )
    }
}

#[derive(Clone)]
pub struct Snapshot {
    pub config: Config,
    pub remotes: Vec<String>,
    pub statuses: HashMap<u64, Status>,
    pub activity: HashMap<u64, Result<Activity, String>>,
    pub message: String,
    pub quitting: bool,
}

pub enum Request {
    Refresh,
    Save(Profile, Sender<Result<(), String>>),
    Delete(u64),
    Mount(u64),
    Unmount(u64),
    Quit,
    CancelQuit,
}

pub enum Event {
    State(Snapshot),
    Quit,
}

struct Mount {
    child: Child,
    started: Instant,
    unmounting: bool,
    control: Control,
    _monitor: Sender<()>,
}

pub fn start(paths: Paths, config: Config) -> (Sender<Request>, Receiver<Event>) {
    let (tx, rx) = mpsc::channel();
    let (events, receiver) = mpsc::channel();
    thread::spawn(move || {
        let (updates, activity_rx) = mpsc::channel();
        let mut worker = Worker {
            paths,
            state: Snapshot {
                config,
                remotes: vec![],
                statuses: HashMap::new(),
                activity: HashMap::new(),
                message: String::new(),
                quitting: false,
            },
            mounts: HashMap::new(),
            updates,
            activity_rx,
        };
        worker.refresh();
        let startup: Vec<_> = worker
            .state
            .config
            .profiles
            .iter()
            .filter(|p| p.mount_at_start)
            .map(|p| p.id)
            .collect();
        for id in startup {
            worker.mount(id);
        }
        let _ = events.send(Event::State(worker.state.clone()));
        loop {
            let mut changed = false;
            match rx.recv_timeout(Duration::from_millis(300)) {
                Ok(request) => {
                    worker.request(request);
                    changed = true;
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
                Err(mpsc::RecvTimeoutError::Timeout) => {}
            }
            changed |= worker.poll();
            if worker.state.quitting && worker.mounts.is_empty() {
                let _ = events.send(Event::Quit);
                break;
            }
            if changed {
                let _ = events.send(Event::State(worker.state.clone()));
            }
        }
    });
    (tx, receiver)
}

struct Worker {
    paths: Paths,
    state: Snapshot,
    mounts: HashMap<u64, Mount>,
    updates: Sender<Update>,
    activity_rx: Receiver<Update>,
}

impl Worker {
    fn refresh(&mut self) {
        match Command::new("rclone")
            .arg("listremotes")
            .stdin(Stdio::null())
            .output()
        {
            Ok(output) if output.status.success() => {
                self.state.remotes = String::from_utf8_lossy(&output.stdout)
                    .lines()
                    .filter_map(|s| s.strip_suffix(':').map(str::to_owned))
                    .collect();
                self.state.remotes.sort();
                self.state.message = if self.state.remotes.is_empty() {
                    "No remotes found. Run rclone config in a terminal, then refresh.".into()
                } else {
                    String::new()
                };
            }
            Ok(output) => {
                self.state.message = format!(
                    "rclone listremotes failed: {}",
                    String::from_utf8_lossy(&output.stderr)
                )
            }
            Err(e) => {
                self.state.message = format!(
                    "Cannot run rclone: {e}. Install rclone and configure an account with rclone config."
                )
            }
        }
        for profile in &self.state.config.profiles {
            if !self.mounts.contains_key(&profile.id) {
                self.state.statuses.insert(
                    profile.id,
                    if is_mounted(&profile.mountpoint) {
                        Status::External
                    } else {
                        Status::Unmounted
                    },
                );
            }
        }
    }

    fn request(&mut self, request: Request) {
        if self.state.quitting && !matches!(request, Request::Quit | Request::CancelQuit) {
            if let Request::Save(_, reply) = request {
                let _ = reply.send(Err("Cancel quit before editing profiles.".into()));
            }
            return;
        }
        self.state.message.clear();
        match request {
            Request::Refresh => self.refresh(),
            Request::Mount(id) => self.mount(id),
            Request::Unmount(id) => self.unmount(id),
            Request::Save(mut profile, reply) => {
                let result = (|| {
                    if self
                        .state
                        .statuses
                        .get(&profile.id)
                        .is_some_and(Status::active)
                        || self.mounts.contains_key(&profile.id)
                    {
                        return Err("Unmount the profile before editing it.".into());
                    }
                    validate(&profile, &self.state.remotes)?;
                    if profile.id == 0 {
                        // Do not reuse a deleted profile's cache directory.
                        profile.id = std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .map_err(|e| e.to_string())?
                            .as_micros() as u64;
                    }
                    if self
                        .state
                        .config
                        .profiles
                        .iter()
                        .any(|p| p.id != profile.id && p.mountpoint == profile.mountpoint)
                    {
                        return Err("Another profile already uses this mountpoint.".into());
                    }
                    let mut config = self.state.config.clone();
                    config.profiles.retain(|p| p.id != profile.id);
                    config.profiles.push(profile.clone());
                    self.paths.save(&config)?;
                    self.state.config = config;
                    self.state.statuses.insert(
                        profile.id,
                        if is_mounted(&profile.mountpoint) {
                            Status::External
                        } else {
                            Status::Unmounted
                        },
                    );
                    Ok::<_, String>(())
                })();
                let _ = reply.send(result);
            }
            Request::Delete(id) => {
                if self.mounts.contains_key(&id)
                    || self.state.statuses.get(&id).is_some_and(Status::active)
                {
                    self.state.message = "Unmount the profile before deleting it.".into();
                    return;
                }
                let mut config = self.state.config.clone();
                config.profiles.retain(|p| p.id != id);
                match self.paths.save(&config) {
                    Ok(()) => {
                        self.state.config = config;
                        self.state.statuses.remove(&id);
                        self.state.activity.remove(&id);
                    }
                    Err(e) => self.state.message = e,
                }
            }
            Request::Quit => self.state.quitting = true,
            Request::CancelQuit => {
                self.state.quitting = false;
                self.state.message =
                    "Quit cancelled. Unmounts already in progress will finish.".into();
            }
        }
    }

    fn mount(&mut self, id: u64) {
        if self.mounts.contains_key(&id) {
            return;
        }
        let Some(profile) = self
            .state
            .config
            .profiles
            .iter()
            .find(|p| p.id == id)
            .cloned()
        else {
            return;
        };
        let result = (|| {
            validate(&profile, &self.state.remotes)?;
            if is_mounted(&profile.mountpoint) {
                return Err(
                    "Mountpoint is already mounted. Refresh to reconcile its state.".into(),
                );
            }
            prepare_mountpoint(&profile.mountpoint)?;
            let control = Control::new()?;
            fs::create_dir_all(&self.paths.logs).map_err(|e| e.to_string())?;
            let log = fs::File::create(self.paths.logs.join(format!("{id}.log")))
                .map_err(|e| e.to_string())?;
            let child = Command::new("rclone")
                .arg("mount")
                .arg(format!("{}:{}", profile.remote, profile.folder))
                .arg(&profile.mountpoint)
                .arg("--vfs-cache-mode")
                .arg("full")
                .arg("--cache-dir")
                .arg(self.paths.cache.join(id.to_string()))
                .arg("--vfs-cache-max-size")
                .arg(format!("{}G", profile.cache_gib))
                .arg("--log-level")
                .arg("INFO")
                .arg("--rc")
                .arg("--rc-addr")
                .arg(&control.address)
                .arg("--rc-no-auth=false")
                .env("RCLONE_RC_USER", "rclone-tray")
                .env("RCLONE_RC_PASS", &control.password)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::from(log))
                .spawn()
                .map_err(|e| format!("Cannot start rclone: {e}"))?;
            self.mounts.insert(
                id,
                Mount {
                    child,
                    started: Instant::now(),
                    unmounting: false,
                    _monitor: control.watch(id, self.updates.clone()),
                    control,
                },
            );
            self.state.activity.remove(&id);
            Ok::<_, String>(())
        })();
        self.state.statuses.insert(
            id,
            match result {
                Ok(()) => Status::Mounting,
                Err(e) => Status::Error(e),
            },
        );
    }

    fn unmount(&mut self, id: u64) {
        let Some(profile) = self.state.config.profiles.iter().find(|p| p.id == id) else {
            return;
        };
        if self.state.statuses.get(&id) == Some(&Status::External) {
            self.state.message = "This mount is not owned by this session. Unmount it with its original application, then refresh.".into();
            return;
        }
        if let Some(mount) = self.mounts.get_mut(&id) {
            if mount.unmounting {
                return;
            }
            // Recovery can upload cached writes before FUSE is mounted. Check on cancellation too.
            match mount.control.activity() {
                Ok(activity) if activity.pending_uploads() => {
                    self.state.message = "Uploads are still pending. Wait for the upload queue to finish before unmounting or cancelling startup.".into();
                    return;
                }
                Ok(activity) if activity.cache.is_ok() => {}
                _ => {
                    self.state.message = "Cannot check pending uploads. Startup may still be recovering cached files. Wait for cache status to become available before unmounting or cancelling; view the log for details.".into();
                    return;
                }
            }
            if !is_mounted(&profile.mountpoint) {
                // Graceful cancellation: never SIGKILL a process that may have cached writes.
                match Command::new("kill")
                    .args(["-TERM", "--", &mount.child.id().to_string()])
                    .status()
                {
                    Ok(status) if status.success() => {
                        mount.unmounting = true;
                        self.state.statuses.insert(id, Status::Unmounting);
                    }
                    _ => {
                        self.state.message =
                            "Could not request cancellation. Check the rclone log and retry.".into()
                    }
                }
                return;
            }
            let output = Command::new("fusermount3")
                .arg("-u")
                .arg("--")
                .arg(&profile.mountpoint)
                .output();
            match output {
                Ok(output) if output.status.success() => {
                    mount.unmounting = true;
                    self.state.statuses.insert(id, Status::Unmounting);
                }
                Ok(output) => {
                    self.state.message = format!(
                        "Unmount failed (close open files and retry): {}",
                        String::from_utf8_lossy(&output.stderr)
                    )
                }
                Err(e) => self.state.message = format!("Cannot run fusermount3: {e}"),
            }
        }
    }

    fn poll(&mut self) -> bool {
        let mut changed = false;
        for update in self.activity_rx.try_iter() {
            if self
                .mounts
                .get(&update.id)
                .is_some_and(|m| m.control.password == update.session && !m.unmounting)
                && self.state.activity.get(&update.id) != Some(&update.result)
            {
                self.state.activity.insert(update.id, update.result);
                changed = true;
            }
        }
        let mut finished = vec![];
        for (&id, mount) in &mut self.mounts {
            let profile = self
                .state
                .config
                .profiles
                .iter()
                .find(|p| p.id == id)
                .unwrap();
            let next = match mount.child.try_wait() {
                Ok(Some(exit)) => {
                    finished.push(id);
                    if self.state.quitting && (!mount.unmounting || is_mounted(&profile.mountpoint))
                    {
                        self.state.quitting = false;
                        self.state.message = "Quit paused: rclone stopped unexpectedly or left a mount behind. Check the profile log. Cached files are retained.".into();
                    }
                    if is_mounted(&profile.mountpoint) {
                        Status::External
                    } else if mount.unmounting {
                        Status::Unmounted
                    } else {
                        Status::Error(format!(
                            "rclone exited ({exit}). See the profile log for details."
                        ))
                    }
                }
                Ok(None) if mount.unmounting => Status::Unmounting,
                Ok(None) if is_mounted(&profile.mountpoint) => Status::Mounted,
                Ok(None)
                    if self.state.statuses.get(&id) == Some(&Status::Recovering)
                        || self
                            .state
                            .activity
                            .get(&id)
                            .is_some_and(|a| a.as_ref().is_ok_and(Activity::pending_uploads)) =>
                {
                    Status::Recovering
                }
                Ok(None) if mount.started.elapsed() > Duration::from_secs(30) => Status::Waiting,
                Ok(None) => Status::Mounting,
                Err(e) => Status::Error(format!("Cannot inspect rclone process: {e}")),
            };
            if self.state.statuses.get(&id) != Some(&next) {
                self.state.statuses.insert(id, next);
                changed = true;
            }
        }
        for id in finished {
            self.mounts.remove(&id);
            self.state.activity.remove(&id);
        }
        // Reconcile external unmounts without ever taking ownership of them.
        for profile in &self.state.config.profiles {
            if !self.mounts.contains_key(&profile.id) {
                let mounted = is_mounted(&profile.mountpoint);
                let previous = self.state.statuses.get(&profile.id);
                if mounted && previous != Some(&Status::External) {
                    self.state.statuses.insert(profile.id, Status::External);
                    changed = true;
                } else if !mounted && previous == Some(&Status::External) {
                    self.state.statuses.insert(profile.id, Status::Unmounted);
                    changed = true;
                }
            }
        }
        if self.state.quitting {
            let ready: Vec<_> = self
                .mounts
                .iter()
                .filter_map(|(&id, mount)| {
                    (!mount.unmounting
                        && self.state.activity.get(&id).is_some_and(|sample| {
                            sample.as_ref().is_ok_and(|activity| {
                                activity.cache.is_ok() && !activity.pending_uploads()
                            })
                        }))
                    .then_some(id)
                })
                .collect();
            for id in ready {
                // Recheck live RC data immediately before each normal unmount.
                self.unmount(id);
                if !self.mounts[&id].unmounting {
                    self.state.quitting = false;
                    self.state.message = format!("Quit paused: {}", self.state.message);
                    return true;
                }
                changed = true;
            }
            let message = if self.mounts.values().any(|mount| !mount.unmounting) {
                "Waiting for uploads and cache checks before quitting…"
            } else {
                "Unmounting before quitting…"
            };
            if self.state.message != message {
                self.state.message = message.into();
                changed = true;
            }
        }
        changed
    }
}

pub fn validate(profile: &Profile, remotes: &[String]) -> Result<(), String> {
    if profile.name.trim().is_empty() {
        return Err("Give this profile a name.".into());
    }
    if !remotes.contains(&profile.remote) {
        return Err("Choose an existing rclone remote, or refresh remotes.".into());
    }
    if !profile.mountpoint.is_absolute() || profile.mountpoint == Path::new("/") {
        return Err("Choose an absolute mountpoint other than /.".into());
    }
    if profile
        .mountpoint
        .components()
        .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        return Err("Mountpoint must not contain .. components.".into());
    }
    if profile.cache_gib == 0 {
        return Err("Cache size must be at least 1 GiB.".into());
    }
    Ok(())
}

fn prepare_mountpoint(path: &Path) -> Result<(), String> {
    fs::create_dir_all(path).map_err(|e| format!("Cannot create mountpoint: {e}"))?;
    if fs::canonicalize(path).map_err(|e| e.to_string())? != path {
        return Err("Use the canonical mountpoint path (no symlinks or . components).".into());
    }
    if fs::read_dir(path)
        .map_err(|e| e.to_string())?
        .next()
        .is_some()
    {
        return Err(
            "Mountpoint is not empty. Choose an empty directory so local files are not hidden."
                .into(),
        );
    }
    Ok(())
}

fn mount_paths(text: &str) -> Vec<PathBuf> {
    use std::os::unix::ffi::OsStringExt;
    text.lines()
        .filter_map(|line| line.split_whitespace().nth(4))
        .map(|field| {
            let bytes = field.as_bytes();
            let mut decoded = Vec::new();
            let mut i = 0;
            while i < bytes.len() {
                if bytes[i] == b'\\'
                    && i + 3 < bytes.len()
                    && bytes[i + 1..i + 4]
                        .iter()
                        .all(|b| (b'0'..=b'7').contains(b))
                {
                    decoded.push(
                        (bytes[i + 1] - b'0') * 64 + (bytes[i + 2] - b'0') * 8 + bytes[i + 3]
                            - b'0',
                    );
                    i += 4;
                } else {
                    decoded.push(bytes[i]);
                    i += 1;
                }
            }
            PathBuf::from(std::ffi::OsString::from_vec(decoded))
        })
        .collect()
}

pub fn is_mounted(path: &Path) -> bool {
    fs::read_to_string("/proc/self/mountinfo")
        .is_ok_and(|text| mount_paths(&text).contains(&path.to_path_buf()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mounting_never_hides_local_files() {
        let dir = tempfile::tempdir().unwrap();
        let mount = dir.path().join("mount");
        prepare_mountpoint(&mount).unwrap();
        fs::write(mount.join("important.txt"), "keep me").unwrap();
        assert!(
            prepare_mountpoint(&mount)
                .unwrap_err()
                .contains("not empty")
        );
        assert_eq!(
            fs::read_to_string(mount.join("important.txt")).unwrap(),
            "keep me"
        );
    }

    #[test]
    fn mountinfo_paths_with_spaces_are_recognized_exactly() {
        let text = "20 1 0:1 / /tmp/my\\040drive rw - fuse.rclone work: rw\n21 1 0:2 / /tmp/other rw - ext4 disk rw";
        let paths = mount_paths(text);
        assert!(paths.contains(&PathBuf::from("/tmp/my drive")));
        assert!(!paths.contains(&PathBuf::from("/tmp/my")));
    }

    #[test]
    fn delayed_startup_reports_recovery_and_never_cancels_unchecked_uploads() {
        use crate::control::Transfer;
        let dir = tempfile::tempdir().unwrap();
        let profile = Profile {
            id: 41,
            name: "Recovery test".into(),
            remote: "test".into(),
            folder: String::new(),
            mountpoint: dir.path().join("not-mounted"),
            cache_gib: 1,
            mount_at_start: false,
        };
        // A live, controllable process with no FUSE mount. EOF produces a startup failure.
        let child = Command::new("sh")
            .args(["-c", "read line; exit 7"])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let control = Control::new().unwrap();
        let session = control.password.clone();
        let (updates, activity_rx) = mpsc::channel();
        let (monitor, _) = mpsc::channel();
        let mut worker = Worker {
            paths: Paths {
                config: dir.path().join("profiles.toml"),
                cache: dir.path().join("cache"),
                logs: dir.path().join("logs"),
                autostart: dir.path().join("autostart.desktop"),
            },
            state: Snapshot {
                config: Config {
                    profiles: vec![profile.clone()],
                },
                remotes: vec!["test".into()],
                statuses: [(41, Status::Mounting)].into(),
                activity: HashMap::new(),
                message: String::new(),
                quitting: false,
            },
            mounts: [(
                41,
                Mount {
                    child,
                    started: Instant::now(),
                    unmounting: false,
                    control,
                    _monitor: monitor,
                },
            )]
            .into(),
            updates,
            activity_rx,
        };
        worker.poll();
        assert_eq!(worker.state.statuses[&41], Status::Mounting);
        worker.mounts.get_mut(&41).unwrap().started -= Duration::from_secs(31);
        worker.poll();
        assert_eq!(worker.state.statuses[&41], Status::Waiting);
        // An old mount's late sample must never change the new mount's state.
        let recovering = Ok(Activity {
            cache: Err("VFS not registered".into()),
            uploads: Err("VFS not registered".into()),
            transfers: vec![Transfer {
                name: "cached-write.bin".into(),
                size: 8000,
                bytes: 2000,
                speed_avg: 700.0,
                src_fs: Some("cache:".into()),
                dst_fs: Some("test:".into()),
            }],
        });
        worker
            .updates
            .send(Update {
                id: 41,
                session: "old-session".into(),
                result: recovering.clone(),
            })
            .unwrap();
        worker.poll();
        assert_eq!(worker.state.statuses[&41], Status::Waiting);
        worker
            .updates
            .send(Update {
                id: 41,
                session: session.clone(),
                result: recovering,
            })
            .unwrap();
        worker.poll();
        assert_eq!(worker.state.statuses[&41], Status::Recovering);
        assert_eq!(
            worker.state.activity[&41].as_ref().unwrap().rates().upload,
            700.0
        );
        worker.request(Request::Delete(41));
        assert_eq!(worker.state.config.profiles.len(), 1);
        worker.request(Request::Unmount(41));
        assert!(
            worker
                .state
                .message
                .contains("Cannot check pending uploads")
        );
        assert!(!worker.mounts[&41].unmounting);
        assert!(
            worker
                .mounts
                .get_mut(&41)
                .unwrap()
                .child
                .try_wait()
                .unwrap()
                .is_none()
        );
        worker
            .updates
            .send(Update {
                id: 41,
                session,
                result: Err("temporarily offline".into()),
            })
            .unwrap();
        worker.poll();
        assert_eq!(worker.state.statuses[&41], Status::Recovering);
        worker.request(Request::Quit);
        worker.poll();
        assert!(worker.state.quitting);
        assert!(worker.state.message.contains("Waiting for uploads"));
        assert!(!worker.mounts[&41].unmounting);
        worker.request(Request::CancelQuit);
        assert!(!worker.state.quitting);
        worker.request(Request::Quit);
        drop(worker.mounts.get_mut(&41).unwrap().child.stdin.take());
        let deadline = Instant::now() + Duration::from_secs(3);
        while worker.mounts.contains_key(&41) && Instant::now() < deadline {
            worker.poll();
            thread::sleep(Duration::from_millis(10));
        }
        assert!(matches!(worker.state.statuses[&41], Status::Error(_)));
        assert!(!worker.state.activity.contains_key(&41));
        assert!(!worker.mounts.contains_key(&41));
        assert!(!worker.state.quitting);
        assert!(worker.state.message.contains("Quit paused"));
    }

    #[test]
    #[ignore = "requires FUSE3, RCLONE_BWLIMIT=1M, and a disposable __tray_test local remote"]
    fn real_mount_lifecycle() {
        assert_eq!(
            std::env::var("RCLONE_BWLIMIT").as_deref(),
            Ok("1M"),
            "Use RCLONE_BWLIMIT=1M so active transfers can be observed"
        );
        fn wait(
            events: &Receiver<Event>,
            predicate: impl Fn(&Snapshot) -> bool,
        ) -> Result<Snapshot, String> {
            let deadline = Instant::now() + Duration::from_secs(20);
            loop {
                match events.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
                    Ok(Event::State(state)) => {
                        if predicate(&state) {
                            return Ok(state);
                        }
                        if let Some(Status::Error(error)) = state.statuses.get(&41) {
                            return Err(error.clone());
                        }
                    }
                    Ok(Event::Quit) => return Err("Worker quit unexpectedly".into()),
                    Err(e) => return Err(format!("Waiting for backend state: {e}")),
                }
            }
        }
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("source with spaces");
        fs::create_dir(&source).unwrap();
        fs::write(source.join("original.txt"), "remote content 123").unwrap();
        let download_data = vec![23u8; 5 * 1024 * 1024 + 123];
        fs::write(source.join("download.bin"), &download_data).unwrap();
        let paths = Paths {
            config: dir.path().join("profiles.toml"),
            cache: dir.path().join("cache"),
            logs: dir.path().join("logs"),
            autostart: dir.path().join("autostart.desktop"),
        };
        let profile = Profile {
            id: 41,
            name: "Disposable test".into(),
            remote: "__tray_test".into(),
            folder: source.to_string_lossy().into_owned(),
            mountpoint: dir.path().join("mount with spaces"),
            cache_gib: 1,
            mount_at_start: false,
        };
        let (tx, events) = start(paths.clone(), Config::default());
        let initial = wait(&events, |_| true).unwrap();
        assert!(
            initial.remotes.contains(&"__tray_test".to_string()),
            "Configure only a disposable local __tray_test remote for this test"
        );
        let (saved, reply) = mpsc::channel();
        tx.send(Request::Save(profile.clone(), saved)).unwrap();
        reply.recv_timeout(Duration::from_secs(5)).unwrap().unwrap();
        wait(&events, |s| s.config.profiles.len() == 1).unwrap();
        assert_eq!(
            paths.load().unwrap().profiles[0].mountpoint,
            profile.mountpoint
        );
        let mut duplicate = profile.clone();
        duplicate.id = 99;
        let (saved, reply) = mpsc::channel();
        tx.send(Request::Save(duplicate, saved)).unwrap();
        assert!(
            reply
                .recv_timeout(Duration::from_secs(5))
                .unwrap()
                .unwrap_err()
                .contains("mountpoint")
        );
        assert_eq!(paths.load().unwrap().profiles.len(), 1);
        tx.send(Request::Mount(41)).unwrap();
        let result = (|| {
            wait(&events, |s| s.statuses.get(&41) == Some(&Status::Mounted))?;
            wait(&events, |s| matches!(s.activity.get(&41), Some(Ok(_))))?;
            let log = fs::read_to_string(paths.logs.join("41.log")).map_err(|e| e.to_string())?;
            let endpoint = log
                .split_whitespace()
                .find(|s| s.starts_with("http://127.0.0.1:"))
                .ok_or("RC endpoint missing from rclone log")?;
            let response = reqwest::blocking::Client::builder()
                .no_proxy()
                .timeout(Duration::from_secs(2))
                .build()
                .unwrap()
                .post(format!("{}/core/stats", endpoint.trim_end_matches('/')))
                .body("{}")
                .send()
                .map_err(|e| e.to_string())?;
            if response.status() != reqwest::StatusCode::UNAUTHORIZED {
                return Err("RC endpoint accepted an unauthenticated request".into());
            }
            if fs::read_to_string(profile.mountpoint.join("original.txt"))
                .map_err(|e| e.to_string())?
                != "remote content 123"
            {
                return Err("Mounted read returned incorrect content".into());
            }
            let download_path = profile.mountpoint.join("download.bin");
            let reader = thread::spawn(move || fs::read(download_path));
            wait(&events, |s| {
                s.activity
                    .get(&41)
                    .and_then(|s| s.as_ref().ok())
                    .is_some_and(|a| {
                        a.transfers.iter().any(|t| {
                            t.direction() == "Downloading" && t.bytes > 0 && t.bytes < t.size as u64
                        })
                    })
            })?;
            if reader.join().unwrap().map_err(|e| e.to_string())? != download_data {
                return Err("Downloaded bytes differ from source".into());
            }
            // A process whose cwd is inside the mount makes a normal unmount busy.
            let mut busy = Command::new("sh")
                .args(["-c", "read line"])
                .current_dir(&profile.mountpoint)
                .stdin(Stdio::piped())
                .spawn()
                .unwrap();
            tx.send(Request::Quit).unwrap();
            let paused = wait(&events, |s| s.message.contains("Quit paused"));
            drop(busy.stdin.take());
            busy.wait().unwrap();
            let paused = paused?;
            if paused.quitting || paused.statuses.get(&41) != Some(&Status::Mounted) {
                return Err("Quit forced a busy mount or did not pause".into());
            }
            tx.send(Request::Delete(41)).unwrap();
            let rejected = wait(&events, |s| s.message.contains("before deleting"))?;
            if rejected.config.profiles.len() != 1 {
                return Err("Active profile was deleted".into());
            }
            let upload_data = vec![87u8; 5 * 1024 * 1024 + 321];
            fs::write(profile.mountpoint.join("upload.bin"), &upload_data)
                .map_err(|e| e.to_string())?;
            wait(&events, |s| {
                s.activity
                    .get(&41)
                    .and_then(|s| s.as_ref().ok())
                    .is_some_and(|a| {
                        a.uploads.as_ref().is_ok_and(|queue| {
                            queue.iter().any(|item| {
                                item.name == "upload.bin"
                                    && !item.uploading
                                    && item.size == (5 * 1024 * 1024 + 321)
                            })
                        })
                    })
            })?;
            tx.send(Request::Unmount(41)).unwrap();
            let refused = wait(&events, |s| s.message.contains("Uploads are still pending"))?;
            if refused.statuses.get(&41) != Some(&Status::Mounted) {
                return Err("Unmount proceeded with pending uploads".into());
            }
            tx.send(Request::Quit).unwrap();
            let waiting = wait(&events, |s| s.quitting)?;
            if waiting.statuses.get(&41) != Some(&Status::Mounted) {
                return Err("Quit unmounted before uploads finished".into());
            }
            tx.send(Request::CancelQuit).unwrap();
            wait(&events, |s| !s.quitting)?;
            let uploading = wait(&events, |s| {
                s.activity
                    .get(&41)
                    .and_then(|s| s.as_ref().ok())
                    .is_some_and(|a| {
                        a.transfers.iter().any(|t| {
                            t.direction() == "Uploading" && t.bytes > 0 && t.bytes < t.size as u64
                        }) && a.uploads.as_ref().is_ok_and(|queue| {
                            queue.iter().any(|item| {
                                item.name == "upload.bin" && item.uploading && item.tries > 0
                            })
                        })
                    })
            })?;
            let activity = uploading.activity[&41].as_ref().unwrap();
            if activity
                .uploads
                .as_ref()
                .unwrap()
                .iter()
                .filter(|item| item.name == "upload.bin")
                .count()
                != 1
            {
                return Err("Upload queue contains duplicate files".into());
            }
            tx.send(Request::Quit).unwrap();
            let deadline = Instant::now() + Duration::from_secs(20);
            let mut saw_unmounting = false;
            loop {
                match events.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
                    Ok(Event::Quit) => break,
                    Ok(Event::State(state)) => {
                        saw_unmounting |= state.statuses.get(&41) == Some(&Status::Unmounting);
                        if !state.quitting {
                            return Err(format!("Quit unexpectedly paused: {}", state.message));
                        }
                    }
                    Err(error) => return Err(format!("Quit did not finish: {error}")),
                }
            }
            if !saw_unmounting || is_mounted(&profile.mountpoint) {
                return Err("Quit did not clean up its mount".into());
            }
            if fs::read(source.join("upload.bin")).ok().as_deref() != Some(upload_data.as_slice()) {
                return Err("Quit lost pending upload data".into());
            }
            if !paths.cache.join("41").exists() || paths.load()?.profiles.len() != 1 {
                return Err("Quit deleted retained cache or profiles".into());
            }
            Ok::<_, String>(())
        })();
        if let Err(error) = &result {
            // Never force cleanup after failure; give queued writes time to finish.
            let _ = tx.send(Request::Quit);
            let deadline = Instant::now() + Duration::from_secs(30);
            while Instant::now() < deadline {
                if matches!(events.recv_timeout(Duration::from_secs(1)), Ok(Event::Quit)) {
                    break;
                }
            }
            eprintln!(
                "{}",
                fs::read_to_string(paths.logs.join("41.log")).unwrap_or_default()
            );
            eprintln!("{error}");
        }
        result.unwrap();
        assert!(!is_mounted(&profile.mountpoint));
        println!(
            "Verified persistence, authenticated RC, live download/upload progress, queued uploads, exact bytes, pending-upload protection, busy-mount quit refusal, cancel quit, automatic upload drain/unmount/exit, and retained cache/profiles."
        );
    }
}
