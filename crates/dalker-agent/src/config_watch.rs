//! One reader for the config file while the agent runs. The password, the MCP tokens and the
//! listener settings all follow the same snapshot, so one edit is seen by all of them in the
//! same read.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::watch;

use crate::cli;

/// How often the file is looked at.
const POLL: Duration = Duration::from_secs(2);

/// The file's `DALKER_*` settings, as [`cli::parse_config`] returns them.
pub type Snapshot = Arc<Vec<(String, String)>>;

/// The last value of `key` in `snapshot` (later lines win, as when loading the file).
pub fn value<'a>(snapshot: &'a [(String, String)], key: &str) -> Option<&'a str> {
    snapshot
        .iter()
        .rev()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.as_str())
}

/// Reads `path` now and then every [`POLL`], and publishes a new snapshot whenever its text
/// changed. A file that can't be read (missing, or not ours to read) changes nothing; until
/// the first successful read the snapshot is empty.
pub fn spawn(path: PathBuf) -> watch::Receiver<Snapshot> {
    let mut last = std::fs::read_to_string(&path).ok();
    let first = last.as_deref().map(cli::parse_config).unwrap_or_default();
    let (tx, rx) = watch::channel(Arc::new(first));
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(POLL).await;
            if tx.is_closed() {
                return;
            }
            let Ok(text) = std::fs::read_to_string(&path) else {
                continue;
            };
            if last.as_deref() == Some(text.as_str()) {
                continue;
            }
            tx.send_replace(Arc::new(cli::parse_config(&text)));
            last = Some(text);
        }
    });
    rx
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn one_read_per_change() {
        let dir = std::env::temp_dir().join(format!("dalker-config-watch-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("dalker.conf");
        std::fs::write(&path, "DALKER_WEB=on\n").unwrap();
        let mut rx = spawn(path.clone());
        assert_eq!(value(&rx.borrow_and_update(), "DALKER_WEB"), Some("on"));

        std::fs::write(&path, "DALKER_WEB=on\nDALKER_MCP=on\nDALKER_WEB=off\n").unwrap();
        tokio::time::timeout(Duration::from_secs(5), rx.changed())
            .await
            .expect("change seen")
            .unwrap();
        let snap = rx.borrow_and_update().clone();
        assert_eq!(
            value(&snap, "DALKER_WEB"),
            Some("off"),
            "the last line wins"
        );
        assert_eq!(value(&snap, "DALKER_MCP"), Some("on"));

        // Gone: nothing changes.
        std::fs::remove_file(&path).unwrap();
        assert!(
            tokio::time::timeout(POLL * 2, rx.changed()).await.is_err(),
            "a missing file is not an empty config"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
