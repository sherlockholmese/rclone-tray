//! Private, local request/response transport. No GTK objects or rclone credentials cross it.
use crate::{
    backend::{Event, Request, Snapshot},
    config::{Paths, Profile},
};
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, File, OpenOptions},
    io::{self, BufRead, BufReader, Write},
    os::unix::{
        fs::{DirBuilderExt, MetadataExt, PermissionsExt},
        net::{UnixListener, UnixStream},
    },
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, Sender},
    },
    thread,
    time::Duration,
};

#[derive(Serialize, Deserialize)]
enum Command {
    State,
    Refresh,
    Save(Profile),
    Delete(u64),
    Mount(u64),
    Unmount(u64),
    Quit,
    CancelQuit,
}

#[derive(Serialize, Deserialize)]
enum Reply {
    State(Snapshot),
    Result(Result<(), String>),
}

pub fn socket_path(paths: &Paths) -> PathBuf {
    std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .map(|p| p.join("rclone-tray"))
        .unwrap_or_else(|| paths.config.parent().unwrap().join("ipc"))
        .join("control.sock")
}

pub struct Server {
    path: PathBuf,
    _lock: File,
    stopped: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
}

impl Server {
    pub fn start(
        path: PathBuf,
        state: Arc<Mutex<Snapshot>>,
        requests: Sender<Request>,
    ) -> io::Result<Self> {
        let directory = path.parent().unwrap();
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(directory)?;
        let metadata = fs::symlink_metadata(directory)?;
        let uid = fs::metadata("/proc/self")?.uid();
        if !metadata.is_dir() || metadata.uid() != uid || metadata.mode() & 0o077 != 0 {
            return Err(io::Error::other(
                "IPC directory must be owned by you and have mode 0700",
            ));
        }
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(directory.join("lock"))?;
        lock.try_lock()
            .map_err(|e| io::Error::other(e.to_string()))?;
        match fs::remove_file(&path) {
            Ok(()) => (),
            Err(e) if e.kind() == io::ErrorKind::NotFound => (),
            Err(e) => return Err(e),
        }
        let listener = UnixListener::bind(&path)?;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
        let stopped = Arc::new(AtomicBool::new(false));
        let stop = stopped.clone();
        let thread = thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        if stop.load(Ordering::Relaxed) {
                            break;
                        }
                        let result = (|| -> io::Result<()> {
                            stream.set_read_timeout(Some(Duration::from_secs(2)))?;
                            stream.set_write_timeout(Some(Duration::from_secs(2)))?;
                            let command: Command = receive(&stream)?;
                            let reply = match command {
                                Command::State => Reply::State(state.lock().unwrap().clone()),
                                Command::Save(profile) => {
                                    let (tx, rx) = mpsc::channel();
                                    let result = requests
                                        .send(Request::Save(profile, tx))
                                        .map_err(|_| "Backend stopped".to_string())
                                        .and_then(|()| {
                                            rx.recv_timeout(Duration::from_secs(5))
                                                .map_err(|e| e.to_string())
                                        })
                                        .and_then(|result| result);
                                    Reply::Result(result)
                                }
                                command => {
                                    let request = match command {
                                        Command::Refresh => Request::Refresh,
                                        Command::Delete(id) => Request::Delete(id),
                                        Command::Mount(id) => Request::Mount(id),
                                        Command::Unmount(id) => Request::Unmount(id),
                                        Command::Quit => Request::Quit,
                                        Command::CancelQuit => Request::CancelQuit,
                                        _ => unreachable!(),
                                    };
                                    Reply::Result(
                                        requests
                                            .send(request)
                                            .map_err(|_| "Backend stopped".into()),
                                    )
                                }
                            };
                            send(&mut stream, &reply)
                        })();
                        if let Err(error) = result {
                            eprintln!("IPC request: {error}");
                        }
                    }
                    Err(e) => {
                        eprintln!("IPC listener: {e}");
                        break;
                    }
                }
            }
        });
        Ok(Self {
            path,
            _lock: lock,
            stopped,
            thread: Some(thread),
        })
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::Relaxed);
        let _ = UnixStream::connect(&self.path);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        let _ = fs::remove_file(&self.path);
    }
}

fn send(stream: &mut UnixStream, value: &impl Serialize) -> io::Result<()> {
    serde_json::to_writer(&mut *stream, value)?;
    stream.write_all(b"\n")
}

fn receive<T: serde::de::DeserializeOwned>(stream: &UnixStream) -> io::Result<T> {
    let mut line = String::new();
    // Bound a local peer's allocation; large queues are supported without unbounded input.
    std::io::Read::take(BufReader::new(stream), 32 * 1024 * 1024).read_line(&mut line)?;
    Ok(serde_json::from_str(&line)?)
}

fn call(path: &Path, command: Command) -> Result<Reply, String> {
    let mut stream =
        UnixStream::connect(path).map_err(|e| format!("Cannot connect to tray backend: {e}"))?;
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .map_err(|e| e.to_string())?;
    stream
        .set_write_timeout(Some(Duration::from_secs(2)))
        .map_err(|e| e.to_string())?;
    send(&mut stream, &command).map_err(|e| e.to_string())?;
    receive(&stream).map_err(|e| e.to_string())
}

pub fn snapshot(path: &Path) -> Result<Snapshot, String> {
    match call(path, Command::State)? {
        Reply::State(state) => Ok(state),
        _ => Err("Unexpected backend reply".into()),
    }
}

pub fn connect(path: PathBuf) -> (Sender<Request>, Receiver<Event>) {
    let (tx, rx) = mpsc::channel();
    let (events, receiver) = mpsc::channel();
    thread::spawn(move || {
        let mut previous = None;
        loop {
            match snapshot(&path) {
                Ok(state) => {
                    if previous.as_ref() != Some(&state) {
                        previous = Some(state.clone());
                        if events.send(Event::State(state)).is_err() {
                            break;
                        }
                    }
                }
                Err(error) => {
                    eprintln!("{error}");
                    let _ = events.send(Event::Quit);
                    break;
                }
            }
            let request = match rx.recv_timeout(Duration::from_secs(1)) {
                Ok(request) => request,
                Err(mpsc::RecvTimeoutError::Timeout) => continue,
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            };
            let (command, saved) = match request {
                Request::Refresh => (Command::Refresh, None),
                Request::Save(profile, tx) => (Command::Save(profile), Some(tx)),
                Request::Delete(id) => (Command::Delete(id), None),
                Request::Mount(id) => (Command::Mount(id), None),
                Request::Unmount(id) => (Command::Unmount(id), None),
                Request::Quit => (Command::Quit, None),
                Request::CancelQuit => (Command::CancelQuit, None),
            };
            let result = call(&path, command).and_then(|reply| match reply {
                Reply::Result(result) => result,
                _ => Err("Unexpected backend reply".into()),
            });
            if let Some(saved) = saved {
                let _ = saved.send(result);
            } else if let Err(error) = result {
                let _ = events.send(Event::Problem(error));
            }
        }
    });
    (tx, receiver)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        backend::Status,
        config::Config,
        control::{Activity, QueuedUpload, Transfer},
    };

    #[test]
    fn clients_reconnect_without_stopping_server_or_losing_transfer_state() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("private/control.sock");
        let state = Arc::new(Mutex::new(Snapshot {
            config: Config {
                profiles: vec![Profile {
                    id: 73,
                    name: "Work files".into(),
                    remote: "work".into(),
                    folder: "Reports/日本語".into(),
                    mountpoint: dir.path().join("work files"),
                    cache_gib: 3,
                    mount_at_start: false,
                }],
            },
            remotes: vec!["work".into()],
            statuses: [(73, Status::Recovering)].into(),
            activity: [(
                73,
                Ok(Activity {
                    transfers: vec![Transfer {
                        name: "Reports/日本語.bin".into(),
                        size: 8193,
                        bytes: 371,
                        speed_avg: 123.5,
                        src_fs: Some("cache:".into()),
                        dst_fs: Some("work:".into()),
                    }],
                    cache: Err("VFS starting".into()),
                    uploads: Ok(vec![QueuedUpload {
                        name: "waiting.bin".into(),
                        size: -1,
                        expiry: 1.25,
                        tries: 2,
                        uploading: false,
                    }]),
                }),
            )]
            .into(),
            message: String::new(),
            quitting: false,
        }));
        let (requests, incoming) = mpsc::channel();
        let server = Server::start(path.clone(), state.clone(), requests.clone()).unwrap();
        assert_eq!(fs::metadata(&path).unwrap().mode() & 0o777, 0o600);
        assert!(Server::start(path.clone(), state.clone(), requests.clone()).is_err());
        let (client, events) = connect(path.clone());
        let Event::State(first) = events.recv_timeout(Duration::from_secs(3)).unwrap() else {
            panic!("Expected state")
        };
        assert!(first == *state.lock().unwrap());
        drop(client);
        drop(events);
        // Reopening a UI neither stops the server nor loses an in-flight mount/queue.
        let (client, events) = connect(path.clone());
        assert!(
            matches!(events.recv_timeout(Duration::from_secs(3)), Ok(Event::State(s)) if s == first)
        );
        let (saved, reply) = mpsc::channel();
        client
            .send(Request::Save(first.config.profiles[0].clone(), saved))
            .unwrap();
        let Request::Save(_, saved) = incoming.recv_timeout(Duration::from_secs(3)).unwrap() else {
            panic!("Expected save")
        };
        saved.send(Err("Profile is active".into())).unwrap();
        assert_eq!(
            reply.recv_timeout(Duration::from_secs(3)).unwrap(),
            Err("Profile is active".into())
        );
        state.lock().unwrap().statuses.insert(73, Status::Mounted);
        assert!(
            matches!(events.recv_timeout(Duration::from_secs(3)), Ok(Event::State(s)) if s.statuses[&73] == Status::Mounted)
        );
        drop(server);
        assert!(matches!(
            events.recv_timeout(Duration::from_secs(3)),
            Ok(Event::Quit)
        ));
        // A leftover socket from an interrupted process can be replaced after its lock is released.
        drop(UnixListener::bind(&path).unwrap());
        let restarted = Server::start(path.clone(), state.clone(), requests).unwrap();
        assert!(snapshot(&path).unwrap() == *state.lock().unwrap());
        drop(restarted);
        assert!(!path.exists());
    }
}
