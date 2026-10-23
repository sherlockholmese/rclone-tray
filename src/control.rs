//! Per-mount authenticated loopback RC connection. Credentials never enter logs or argv.
use reqwest::blocking::Client;
use serde::Deserialize;
use std::{
    io::Read,
    net::TcpListener,
    sync::mpsc::{self, Sender},
    thread,
    time::Duration,
};

#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Transfer {
    pub name: String,
    pub size: i64,
    #[serde(default)]
    pub bytes: u64,
    #[serde(default)]
    pub speed_avg: f64,
    pub src_fs: Option<String>,
    pub dst_fs: Option<String>,
}

impl Transfer {
    pub fn direction(&self) -> &'static str {
        // Cached VFS downloads have a source and no destination; writeback has a destination.
        // Older rclone versions omit both: never guess a direction in that case.
        if self.dst_fs.is_some() {
            "Uploading"
        } else if self.src_fs.is_some() {
            "Downloading"
        } else {
            "Transferring"
        }
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Cache {
    pub bytes_used: u64,
    pub uploads_queued: u64,
    pub uploads_in_progress: u64,
    pub errored_files: u64,
    pub out_of_space: bool,
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
pub struct QueuedUpload {
    pub name: String,
    pub size: i64,
    pub expiry: f64,
    pub tries: u32,
    pub uploading: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Activity {
    pub transfers: Vec<Transfer>,
    // The VFS cache may not be registered yet while rclone recovers cached writes.
    pub cache: Result<Cache, String>,
    pub uploads: Result<Vec<QueuedUpload>, String>,
}

#[derive(Default)]
pub struct Rates {
    pub upload: f64,
    pub download: f64,
    pub other: f64,
}

impl Rates {
    pub fn label(&self) -> String {
        let mut label = format!(
            "↑ {}/s - ↓ {}/s",
            byte_size(self.upload),
            byte_size(self.download)
        );
        if self.other > 0.0 {
            label.push_str(&format!(" - Other {}/s", byte_size(self.other)));
        }
        label
    }
}

impl Activity {
    pub fn rates(&self) -> Rates {
        let mut rates = Rates::default();
        for transfer in &self.transfers {
            match transfer.direction() {
                "Uploading" => rates.upload += transfer.speed_avg,
                "Downloading" => rates.download += transfer.speed_avg,
                _ => rates.other += transfer.speed_avg,
            }
        }
        rates
    }

    pub fn pending_uploads(&self) -> bool {
        self.transfers.iter().any(|t| t.direction() == "Uploading")
            || self.uploads.as_ref().is_ok_and(|queue| !queue.is_empty())
            || self.cache.as_ref().is_ok_and(|cache| {
                cache.uploads_queued > 0 || cache.uploads_in_progress > 0 || cache.errored_files > 0
            })
    }
}

pub fn byte_size(bytes: f64) -> String {
    let mut value = bytes.max(0.0);
    let mut unit = "B";
    for next in ["KiB", "MiB", "GiB", "TiB"] {
        if value < 1024.0 {
            break;
        }
        value /= 1024.0;
        unit = next;
    }
    if unit == "B" {
        format!("{value:.0} {unit}")
    } else {
        format!("{value:.1} {unit}")
    }
}

pub struct Update {
    pub id: u64,
    pub session: String,
    pub result: Result<Activity, String>,
}

#[derive(Clone)]
pub struct Control {
    pub address: String,
    pub password: String,
    client: Client,
}

impl Control {
    pub fn new() -> Result<Self, String> {
        let listener = TcpListener::bind("127.0.0.1:0").map_err(|e| e.to_string())?;
        let address = listener
            .local_addr()
            .map_err(|e| e.to_string())?
            .to_string();
        let mut random = [0u8; 32];
        std::fs::File::open("/dev/urandom")
            .and_then(|mut f| f.read_exact(&mut random))
            .map_err(|e| e.to_string())?;
        let password = random.iter().map(|b| format!("{b:02x}")).collect();
        let client = Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_millis(700))
            .build()
            .map_err(|e| e.to_string())?;
        Ok(Self {
            address,
            password,
            client,
        })
    }

    fn call<T: serde::de::DeserializeOwned>(&self, method: &str) -> Result<T, String> {
        self.client
            .post(format!("http://{}/{method}", self.address))
            .basic_auth("rclone-tray", Some(&self.password))
            .header("Content-Type", "application/json")
            .body("{}")
            .send()
            .and_then(|r| r.error_for_status())
            .and_then(|r| r.json())
            .map_err(|_| {
                "Transfer status unavailable. Check the rclone log or try again shortly.".into()
            })
    }

    pub fn activity(&self) -> Result<Activity, String> {
        #[derive(Deserialize)]
        struct Vfs {
            #[serde(rename = "diskCache")]
            disk_cache: Cache,
        }
        #[derive(Deserialize)]
        struct Stats {
            transferring: Option<Vec<Transfer>>,
        }
        #[derive(Deserialize)]
        struct Queue {
            queue: Vec<QueuedUpload>,
        }
        let transfers = self
            .call::<Stats>("core/stats")?
            .transferring
            .unwrap_or_default();
        let cache = self.call::<Vfs>("vfs/stats").map(|vfs| vfs.disk_cache);
        let uploads = self.call::<Queue>("vfs/queue").map(|queue| queue.queue).map_err(|_| {
            "Upload queue unavailable. The VFS cache may still be starting, or this rclone version may not support queue inspection.".into()
        });
        Ok(Activity {
            cache,
            transfers,
            uploads,
        })
    }

    /// Dropping the returned sender stops this monitor after its current bounded request.
    pub fn watch(&self, id: u64, updates: Sender<Update>) -> Sender<()> {
        let control = self.clone();
        let (stop, receiver) = mpsc::channel();
        thread::spawn(move || {
            loop {
                let update = Update {
                    id,
                    session: control.password.clone(),
                    result: control.activity(),
                };
                if updates.send(update).is_err() {
                    break;
                }
                match receiver.recv_timeout(Duration::from_secs(1)) {
                    Err(mpsc::RecvTimeoutError::Timeout) => {}
                    _ => break,
                }
            }
        });
        stop
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn rc_activity_distinguishes_directions_queue_and_unavailable_data() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let mut control = Control::new().unwrap();
        control.address = listener.local_addr().unwrap().to_string();
        let server = thread::spawn(move || {
            for (index, stream) in listener.incoming().take(7).enumerate() {
                let mut stream = stream.unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                let mut request = Vec::new();
                while !request.windows(4).any(|w| w == b"\r\n\r\n") {
                    let mut byte = [0];
                    stream.read_exact(&mut byte).unwrap();
                    request.push(byte[0]);
                }
                let request = String::from_utf8(request).unwrap();
                // Drain the request before closing: unread bytes can reset the connection.
                let length = request
                    .lines()
                    .find_map(|line| {
                        let (name, value) = line.split_once(':')?;
                        name.eq_ignore_ascii_case("content-length")
                            .then(|| value.trim().parse::<usize>().unwrap())
                    })
                    .unwrap_or(0);
                stream.read_exact(&mut vec![0; length]).unwrap();
                let body = if index >= 4 {
                    r#"{"error":"VFS not ready"}"#
                } else if request.contains("/vfs/stats ") {
                    r#"{"diskCache":{"bytesUsed":4096,"uploadsQueued":3,"uploadsInProgress":1,"erroredFiles":0,"outOfSpace":false}}"#
                } else if request.contains("/vfs/queue ") {
                    r#"{"queue":[{"name":"upload.txt","id":4,"size":900,"expiry":-2.5,"tries":1,"delay":5,"uploading":true},{"name":"waiting <&>.txt","id":9,"size":1700,"expiry":4.25,"tries":0,"delay":5,"uploading":false},{"name":"retry.txt","id":11,"size":-1,"expiry":-0.5,"tries":2,"delay":20,"uploading":false}]}"#
                } else {
                    r#"{"transferring":[{"name":"upload.txt","size":900,"bytes":300,"speedAvg":13.25,"srcFs":"cache:","dstFs":"remote:"},{"name":"download.txt","size":1200,"bytes":400,"speedAvg":27.5,"srcFs":"remote:"},{"name":"legacy.txt","size":-1,"speedAvg":5}]}"#
                };
                let status = if index >= 4 {
                    "503 Unavailable"
                } else {
                    "200 OK"
                };
                write!(stream, "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
            }
        });
        let activity = control.activity().unwrap();
        assert_eq!(activity.cache.as_ref().unwrap().uploads_queued, 3);
        assert!(activity.pending_uploads());
        assert_eq!(activity.transfers[0].direction(), "Uploading");
        assert_eq!(activity.transfers[1].direction(), "Downloading");
        assert_eq!(activity.transfers[2].direction(), "Transferring");
        assert_eq!(activity.transfers[1].bytes, 400);
        assert_eq!(activity.transfers[1].speed_avg, 27.5);
        assert_eq!(activity.rates().upload, 13.25);
        assert_eq!(activity.rates().download, 27.5);
        assert_eq!(activity.rates().other, 5.0);
        let queue = activity.uploads.as_ref().unwrap();
        assert_eq!(queue.len(), 3);
        assert!(queue[0].uploading);
        assert!(!queue[1].uploading);
        assert_eq!(queue[1].name, "waiting <&>.txt");
        assert_eq!(queue[1].size, 1700);
        assert_eq!(queue[1].expiry, 4.25);
        assert_eq!(queue[2].tries, 2);
        assert_eq!(queue[2].size, -1);
        // Startup recovery must remain visible before vfs/stats becomes available.
        let recovering = control.activity().unwrap();
        assert!(recovering.cache.is_err());
        assert!(recovering.uploads.is_err());
        assert!(recovering.pending_uploads());
        assert_eq!(recovering.rates().upload, 13.25);
        // An unavailable core/stats response must not become idle.
        assert!(control.activity().is_err());
        server.join().unwrap();
    }
}
