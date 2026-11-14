//! File transfer on the `"files"` DataChannel, both ways.
//!
//! Uploads (viewer → host) land in the signed-in user's Downloads folder under a name that
//! is free there (never overwriting). Downloads (host → viewer) come from below that user's
//! home folder, which the viewer can browse. Every file operation runs on one worker thread
//! per viewer that acts as that user ([`FileAccess`]), never as the agent's own account.
//!
//! Wire format. Requests and replies are JSON text messages; file data are binary messages:
//! a 4-byte big-endian transfer id (chosen by the viewer), then up to [`CHUNK`] bytes.
//! ```text
//! viewer → host                                  host → viewer
//! {"op":"hello"}                                 {"op":"ready","user":..,"home":..,"downloads":..}
//!                                                {"op":"unavailable","reason":..}
//! {"op":"list","path":"Documents"}               {"op":"list","path":..,"entries":[{"name","dir","size","mtime","hidden"}]}
//!                                                {"op":"list","path":..,"error":..}
//! {"op":"put","id":1,"name":"a.txt","size":5}    {"op":"accepted","id":1,"name":"a (1).txt"}
//! <id><bytes>…                                   {"op":"progress","id":1,"bytes":n}   every ACK_EVERY written
//! {"op":"end","id":1}                            {"op":"done","id":1,"name":"a (1).txt"}
//! {"op":"get","id":2,"path":"Documents/b.pdf"}   {"op":"accepted","id":2,"name":"b.pdf","size":n}
//!                                                <id><bytes>…  {"op":"done","id":2}
//! {"op":"pause","id":2} / {"op":"resume","id":2} (downloads; an upload pauses by not sending)
//! {"op":"cancel","id":n}                         {"op":"error","id":n,"error":..}
//! ```
//! A cancelled, failed or unfinished upload (the viewer left) is deleted.

use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::PathBuf;
use std::sync::mpsc as std_mpsc;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
pub use dalker_core::files::FileAccessFactory;
use dalker_core::files::{create_unique, list, open_file};
use dalker_core::{FileAccess, PlatformError};
use serde::Deserialize;
use serde_json::json;
use tokio::sync::mpsc;
use webrtc::data_channel::data_channel_message::DataChannelMessage;
use webrtc::data_channel::RTCDataChannel;

use crate::SessionId;

/// Label of the DataChannel that carries file transfers.
pub const FILES_CHANNEL: &str = "files";

/// File bytes per binary message. With the 4-byte header it stays under the 64 KiB message
/// size webrtc-rs and browsers agree on.
pub const CHUNK: usize = 60 * 1024;

/// An upload is acknowledged every this many bytes written; the viewer keeps at most a few
/// MiB unacknowledged, so a slow disk slows the sender instead of filling memory here.
pub const ACK_EVERY: u64 = 256 * 1024;

/// Host → viewer data waiting in the channel's send buffer before downloads hold off.
const HIGH_WATER: usize = 1024 * 1024;

#[derive(Debug)]
pub(crate) enum In {
    Text(String),
    Data(Bytes),
}

#[derive(Debug)]
pub(crate) enum Out {
    Text(String),
    Data(Bytes),
}

#[derive(Debug, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
enum Request {
    Hello,
    List {
        #[serde(default)]
        path: String,
    },
    Put {
        id: u32,
        name: String,
        size: u64,
    },
    End {
        id: u32,
    },
    Get {
        id: u32,
        path: String,
    },
    Pause {
        id: u32,
    },
    Resume {
        id: u32,
    },
    Cancel {
        id: u32,
    },
}

/// Runs file transfers for one viewer's `"files"` channel until it closes.
pub(crate) fn attach(id: SessionId, dc: Arc<RTCDataChannel>, access: FileAccessFactory) {
    let (in_tx, in_rx) = std_mpsc::channel::<In>();
    let (out_tx, out_rx) = mpsc::channel::<Out>(16);
    dc.on_message(Box::new(move |msg: DataChannelMessage| {
        let m = if msg.is_string {
            In::Text(String::from_utf8_lossy(&msg.data).into_owned())
        } else {
            In::Data(msg.data)
        };
        let _ = in_tx.send(m);
        Box::pin(async {})
    }));
    let spawned = std::thread::Builder::new()
        .name(format!("dalker-files-{id}"))
        .spawn(move || Worker::new(access, out_tx).run(in_rx));
    if let Err(e) = spawned {
        tracing::warn!(session = id, "no file transfers: {e}");
        return;
    }
    tokio::spawn(send_to_viewer(id, dc, out_rx));
}

/// Sends what the worker produces, holding file data back while the channel's buffer is
/// full. Ends (and so stops the worker) when the channel closes.
async fn send_to_viewer(id: SessionId, dc: Arc<RTCDataChannel>, mut out: mpsc::Receiver<Out>) {
    while let Some(m) = out.recv().await {
        let sent = match m {
            Out::Text(t) => dc.send_text(t).await,
            Out::Data(b) => {
                while dc.buffered_amount().await > HIGH_WATER {
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
                dc.send(&b).await
            }
        };
        if let Err(e) = sent {
            tracing::debug!(session = id, "file channel closed: {e}");
            return;
        }
    }
}

struct Upload {
    file: File,
    path: PathBuf,
    name: String,
    size: u64,
    written: u64,
    acked: u64,
}

struct Download {
    file: File,
    size: u64,
    sent: u64,
    paused: bool,
}

pub(crate) struct Worker {
    factory: FileAccessFactory,
    access: Option<FileAccess>,
    uploads: HashMap<u32, Upload>,
    downloads: HashMap<u32, Download>,
    out: mpsc::Sender<Out>,
    /// A download chunk that did not fit in `out` yet.
    held: Option<(u32, Bytes)>,
}

impl Worker {
    pub(crate) fn new(factory: FileAccessFactory, out: mpsc::Sender<Out>) -> Self {
        Self {
            factory,
            access: None,
            uploads: HashMap::new(),
            downloads: HashMap::new(),
            out,
            held: None,
        }
    }

    /// Serves requests until the viewer leaves (the incoming side closes or the outgoing one
    /// does), then deletes unfinished uploads.
    pub(crate) fn run(mut self, incoming: std_mpsc::Receiver<In>) {
        // Opened here: the access acts for this thread only.
        if self.open_access() {
            self.ready_message();
        }
        loop {
            let busy = self.downloads.values().any(|d| !d.paused) || self.held.is_some();
            let msg = if busy {
                match incoming.try_recv() {
                    Ok(m) => Some(m),
                    Err(std_mpsc::TryRecvError::Empty) => None,
                    Err(std_mpsc::TryRecvError::Disconnected) => break,
                }
            } else {
                match incoming.recv_timeout(Duration::from_secs(1)) {
                    Ok(m) => Some(m),
                    Err(std_mpsc::RecvTimeoutError::Timeout) => None,
                    Err(std_mpsc::RecvTimeoutError::Disconnected) => break,
                }
            };
            if self.out.is_closed() {
                break;
            }
            let got = msg.is_some();
            match msg {
                Some(In::Text(t)) => self.request(&t),
                Some(In::Data(b)) => self.data(b),
                None => {}
            }
            let pumped = self.pump_downloads();
            if busy && !got && !pumped {
                std::thread::sleep(Duration::from_millis(2));
            }
        }
        for (_, up) in self.uploads.drain() {
            discard(up);
        }
        self.downloads.clear();
        // Stop acting as the user on this thread.
        self.access = None;
    }

    fn send(&self, v: serde_json::Value) {
        let _ = self.out.blocking_send(Out::Text(v.to_string()));
    }

    fn error(&self, id: u32, msg: impl Into<String>) {
        self.send(json!({"op": "error", "id": id, "error": msg.into()}));
    }

    /// Opens the user if that hasn't worked yet and tells the viewer how it went.
    fn open_access(&mut self) -> bool {
        if self.access.is_none() {
            match (self.factory)() {
                Ok(a) => {
                    tracing::info!(user = %a.user, downloads = %a.downloads.display(), "file transfers ready");
                    self.access = Some(a);
                }
                Err(e) => {
                    match &e {
                        PlatformError::Fatal(_) => {
                            tracing::warn!("file transfers unavailable: {e}")
                        }
                        _ => tracing::info!("file transfers unavailable: {e}"),
                    }
                    self.send(json!({"op": "unavailable", "reason": e.message()}));
                    return false;
                }
            }
        }
        true
    }

    fn ready_message(&self) {
        if let Some(a) = &self.access {
            self.send(json!({
                "op": "ready",
                "user": a.user,
                "home": a.home.display().to_string(),
                "downloads": a.downloads.display().to_string(),
            }));
        }
    }

    fn request(&mut self, text: &str) {
        let req = match serde_json::from_str::<Request>(text) {
            Ok(r) => r,
            Err(e) => {
                tracing::debug!("bad file request: {e}");
                return;
            }
        };
        match req {
            Request::Hello => {
                if self.open_access() {
                    self.ready_message();
                }
            }
            Request::List { path } => {
                if !self.open_access() {
                    return;
                }
                let home = &self.access.as_ref().unwrap().home;
                let reply = match list(home, &path) {
                    Ok(entries) => json!({"op": "list", "path": path, "entries": entries}),
                    Err(e) => json!({"op": "list", "path": path, "error": e}),
                };
                self.send(reply);
            }
            Request::Put { id, name, size } => {
                if !self.open_access() {
                    return self.error(id, "file transfer is not available");
                }
                if self.uploads.contains_key(&id) || self.downloads.contains_key(&id) {
                    return self.error(id, "transfer id in use");
                }
                let downloads = self.access.as_ref().unwrap().downloads.clone();
                match create_unique(&downloads, &name) {
                    Ok((file, path, name)) => {
                        tracing::info!(file = %path.display(), size, "receiving a file from a viewer");
                        self.send(json!({"op": "accepted", "id": id, "name": name}));
                        self.uploads.insert(
                            id,
                            Upload {
                                file,
                                path,
                                name,
                                size,
                                written: 0,
                                acked: 0,
                            },
                        );
                    }
                    Err(e) => self.error(id, e),
                }
            }
            Request::End { id } => {
                let Some(mut up) = self.uploads.remove(&id) else {
                    return;
                };
                if up.written != up.size {
                    let msg = format!("incomplete: {} of {} bytes", up.written, up.size);
                    discard(up);
                    return self.error(id, msg);
                }
                if let Err(e) = up.file.flush() {
                    let msg = format!("could not save the file: {e}");
                    discard(up);
                    return self.error(id, msg);
                }
                tracing::info!(file = %up.path.display(), bytes = up.written, "file received");
                self.send(json!({"op": "done", "id": id, "name": up.name}));
            }
            Request::Get { id, path } => {
                if !self.open_access() {
                    return self.error(id, "file transfer is not available");
                }
                if self.uploads.contains_key(&id) || self.downloads.contains_key(&id) {
                    return self.error(id, "transfer id in use");
                }
                let home = &self.access.as_ref().unwrap().home;
                match open_file(home, &path) {
                    Ok((file, name, size)) => {
                        tracing::info!(file = %path, size, "sending a file to a viewer");
                        self.send(json!({"op": "accepted", "id": id, "name": name, "size": size}));
                        self.downloads.insert(
                            id,
                            Download {
                                file,
                                size,
                                sent: 0,
                                paused: false,
                            },
                        );
                    }
                    Err(e) => self.error(id, e),
                }
            }
            Request::Pause { id } => {
                if let Some(d) = self.downloads.get_mut(&id) {
                    d.paused = true;
                }
            }
            Request::Resume { id } => {
                if let Some(d) = self.downloads.get_mut(&id) {
                    d.paused = false;
                }
            }
            Request::Cancel { id } => {
                if let Some(up) = self.uploads.remove(&id) {
                    tracing::info!(file = %up.path.display(), "upload cancelled by the viewer");
                    discard(up);
                }
                self.downloads.remove(&id);
                if self.held.as_ref().is_some_and(|(h, _)| *h == id) {
                    self.held = None;
                }
            }
        }
    }

    fn data(&mut self, b: Bytes) {
        if b.len() < 4 {
            return;
        }
        let id = u32::from_be_bytes([b[0], b[1], b[2], b[3]]);
        let Some(up) = self.uploads.get_mut(&id) else {
            return;
        };
        let data = &b[4..];
        if up.written + data.len() as u64 > up.size {
            let up = self.uploads.remove(&id).unwrap();
            discard(up);
            return self.error(id, "more data than announced");
        }
        if let Err(e) = up.file.write_all(data) {
            let up = self.uploads.remove(&id).unwrap();
            discard(up);
            return self.error(id, format!("could not write the file: {e}"));
        }
        up.written += data.len() as u64;
        if up.written - up.acked >= ACK_EVERY {
            up.acked = up.written;
            let bytes = up.written;
            self.send(json!({"op": "progress", "id": id, "bytes": bytes}));
        }
    }

    /// Queues the next chunk of every running download that has room. True when anything
    /// was queued.
    fn pump_downloads(&mut self) -> bool {
        let mut progressed = false;
        if let Some((id, chunk)) = self.held.take() {
            match self.out.try_send(Out::Data(chunk)) {
                Ok(()) => progressed = true,
                Err(mpsc::error::TrySendError::Full(Out::Data(c))) => {
                    self.held = Some((id, c));
                    return false;
                }
                Err(_) => return false,
            }
        }
        let ids: Vec<u32> = self
            .downloads
            .iter()
            .filter(|(_, d)| !d.paused)
            .map(|(id, _)| *id)
            .collect();
        for id in ids {
            if self.held.is_some() {
                break;
            }
            let d = self.downloads.get_mut(&id).unwrap();
            let mut buf = vec![0u8; 4 + CHUNK];
            buf[..4].copy_from_slice(&id.to_be_bytes());
            let n = match read_full(&mut d.file, &mut buf[4..]) {
                Ok(n) => n,
                Err(e) => {
                    self.downloads.remove(&id);
                    self.error(id, format!("could not read the file: {e}"));
                    continue;
                }
            };
            if n == 0 {
                let d = self.downloads.remove(&id).unwrap();
                if d.sent != d.size {
                    tracing::info!(
                        sent = d.sent,
                        size = d.size,
                        "file changed size while it was sent"
                    );
                }
                self.send(json!({"op": "done", "id": id, "bytes": d.sent}));
                progressed = true;
                continue;
            }
            d.sent += n as u64;
            buf.truncate(4 + n);
            match self.out.try_send(Out::Data(Bytes::from(buf))) {
                Ok(()) => {}
                Err(mpsc::error::TrySendError::Full(Out::Data(c))) => self.held = Some((id, c)),
                Err(_) => return progressed,
            }
            progressed = true;
        }
        progressed
    }
}

/// Fills `buf` as far as the file goes.
fn read_full(file: &mut File, buf: &mut [u8]) -> io::Result<usize> {
    let mut n = 0;
    while n < buf.len() {
        match file.read(&mut buf[n..]) {
            Ok(0) => break,
            Ok(k) => n += k,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(n)
}

fn discard(up: Upload) {
    drop(up.file);
    if let Err(e) = fs::remove_file(&up.path) {
        tracing::warn!(file = %up.path.display(), "could not delete an unfinished upload: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dalker_core::FileAccess;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::Arc;

    fn temp_dir(tag: &str) -> PathBuf {
        static N: AtomicU64 = AtomicU64::new(0);
        let d = std::env::temp_dir().join(format!(
            "dalker-files-{tag}-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    fn worker_for(
        home: PathBuf,
    ) -> (
        std_mpsc::Sender<In>,
        mpsc::Receiver<Out>,
        std::thread::JoinHandle<()>,
    ) {
        let factory: FileAccessFactory = Arc::new(move || {
            Ok(FileAccess {
                user: "tester".into(),
                home: home.clone(),
                downloads: home.join("Downloads"),
                guard: Box::new(()),
            })
        });
        let (in_tx, in_rx) = std_mpsc::channel();
        let (out_tx, out_rx) = mpsc::channel(16);
        let t = std::thread::spawn(move || Worker::new(factory, out_tx).run(in_rx));
        (in_tx, out_rx, t)
    }

    fn text(out: &mut mpsc::Receiver<Out>) -> serde_json::Value {
        loop {
            match out.blocking_recv().expect("worker reply") {
                Out::Text(t) => return serde_json::from_str(&t).unwrap(),
                Out::Data(_) => continue,
            }
        }
    }

    fn chunk(id: u32, data: &[u8]) -> In {
        let mut b = id.to_be_bytes().to_vec();
        b.extend_from_slice(data);
        In::Data(Bytes::from(b))
    }

    #[test]
    fn upload_lands_in_downloads_without_overwriting() {
        let home = temp_dir("upload");
        fs::create_dir_all(home.join("Downloads")).unwrap();
        fs::write(home.join("Downloads/photo.jpg"), b"old").unwrap();
        let (tx, mut rx, t) = worker_for(home.clone());
        let data: Vec<u8> = (0..(ACK_EVERY as usize * 2 + 10))
            .map(|i| i as u8)
            .collect();
        tx.send(In::Text(
            json!({"op":"put","id":7,"name":"photo.jpg","size":data.len()}).to_string(),
        ))
        .unwrap();
        let ready = text(&mut rx);
        assert_eq!(ready["op"], "ready");
        let acc = text(&mut rx);
        assert_eq!(
            (acc["op"].as_str(), acc["name"].as_str()),
            (Some("accepted"), Some("photo (1).jpg"))
        );
        for c in data.chunks(CHUNK) {
            tx.send(chunk(7, c)).unwrap();
        }
        tx.send(In::Text(r#"{"op":"end","id":7}"#.into())).unwrap();
        let mut last = text(&mut rx);
        while last["op"] == "progress" {
            last = text(&mut rx);
        }
        assert_eq!(last["op"], "done");
        assert_eq!(
            fs::read(home.join("Downloads/photo (1).jpg")).unwrap(),
            data
        );
        assert_eq!(fs::read(home.join("Downloads/photo.jpg")).unwrap(), b"old");

        // A cancelled upload leaves nothing behind.
        tx.send(In::Text(
            json!({"op":"put","id":8,"name":"big.bin","size":100}).to_string(),
        ))
        .unwrap();
        assert_eq!(text(&mut rx)["op"], "accepted");
        tx.send(chunk(8, &[1; 10])).unwrap();
        tx.send(In::Text(r#"{"op":"cancel","id":8}"#.into()))
            .unwrap();
        // So is one the viewer never finished.
        tx.send(In::Text(
            json!({"op":"put","id":9,"name":"left.bin","size":100}).to_string(),
        ))
        .unwrap();
        assert_eq!(text(&mut rx)["op"], "accepted");
        drop(tx);
        t.join().unwrap();
        assert!(!home.join("Downloads/big.bin").exists());
        assert!(!home.join("Downloads/left.bin").exists());
        fs::remove_dir_all(&home).unwrap();
    }

    #[test]
    fn download_and_listing() {
        let home = temp_dir("download");
        fs::create_dir_all(home.join("Documents")).unwrap();
        let data: Vec<u8> = (0..(CHUNK * 3 + 5)).map(|i| (i * 7) as u8).collect();
        fs::write(home.join("Documents/b.bin"), &data).unwrap();
        fs::write(home.join(".hidden"), b"").unwrap();
        let (tx, mut rx, t) = worker_for(home.clone());
        assert_eq!(text(&mut rx)["op"], "ready");

        tx.send(In::Text(r#"{"op":"list","path":""}"#.into()))
            .unwrap();
        let l = text(&mut rx);
        let entries = l["entries"].as_array().unwrap();
        assert_eq!(entries[0]["name"], "Documents");
        assert_eq!(entries[0]["dir"], true);
        #[cfg(unix)]
        assert!(entries
            .iter()
            .any(|e| e["name"] == ".hidden" && e["hidden"] == true));
        tx.send(In::Text(r#"{"op":"list","path":".."}"#.into()))
            .unwrap();
        assert!(text(&mut rx)["error"].is_string());

        tx.send(In::Text(
            r#"{"op":"get","id":3,"path":"Documents/b.bin"}"#.into(),
        ))
        .unwrap();
        let mut got = Vec::new();
        let acc = match rx.blocking_recv().unwrap() {
            Out::Text(t) => serde_json::from_str::<serde_json::Value>(&t).unwrap(),
            Out::Data(_) => panic!("data before accepted"),
        };
        assert_eq!(
            (acc["op"].as_str(), acc["size"].as_u64()),
            (Some("accepted"), Some(data.len() as u64))
        );
        let done = loop {
            match rx.blocking_recv().unwrap() {
                Out::Data(b) => {
                    assert_eq!(&b[..4], &3u32.to_be_bytes());
                    got.extend_from_slice(&b[4..]);
                }
                Out::Text(t) => break serde_json::from_str::<serde_json::Value>(&t).unwrap(),
            }
        };
        assert_eq!(done["op"], "done");
        assert_eq!(got, data);

        tx.send(In::Text(
            r#"{"op":"get","id":4,"path":"../../etc/passwd"}"#.into(),
        ))
        .unwrap();
        assert_eq!(text(&mut rx)["op"], "error");
        drop(tx);
        t.join().unwrap();
        fs::remove_dir_all(&home).unwrap();
    }

    #[test]
    fn unavailable_until_someone_signs_in() {
        let home = temp_dir("signin");
        let signed_in = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = Arc::clone(&signed_in);
        let h = home.clone();
        let factory: FileAccessFactory = Arc::new(move || {
            if !flag.load(Ordering::SeqCst) {
                return Err(PlatformError::Transient(
                    "Nobody is signed in on the host.".into(),
                ));
            }
            Ok(FileAccess {
                user: "u".into(),
                home: h.clone(),
                downloads: h.join("Downloads"),
                guard: Box::new(()),
            })
        });
        let (tx, in_rx) = std_mpsc::channel();
        let (out_tx, mut rx) = mpsc::channel(16);
        let t = std::thread::spawn(move || Worker::new(factory, out_tx).run(in_rx));
        assert_eq!(text(&mut rx)["op"], "unavailable");
        signed_in.store(true, Ordering::SeqCst);
        tx.send(In::Text(r#"{"op":"hello"}"#.into())).unwrap();
        assert_eq!(text(&mut rx)["op"], "ready");
        drop(tx);
        t.join().unwrap();
        fs::remove_dir_all(&home).unwrap();
    }
}
