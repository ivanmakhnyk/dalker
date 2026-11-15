//! Where the agent's TLS certificate comes from: `DALKER_TLS_CERT`/`DALKER_TLS_KEY`, or a
//! self-signed one in a `tls` folder beside the config file (made on first start). An agent that
//! can't write there (GNOME on Wayland runs it as the desktop user) keeps its own in its data
//! folder instead.

use std::path::{Path, PathBuf};

use anyhow::Result;
use dalker_web::tls::{self, CertSource, Identity};

/// Where the certificate is read from, best first.
#[derive(Debug, Clone)]
pub struct Certs {
    /// The host's own files.
    own: Option<(PathBuf, PathBuf)>,
    /// Folders for the self-signed certificate.
    dirs: Vec<PathBuf>,
    names: Vec<String>,
}

impl Certs {
    pub fn new(cert: Option<PathBuf>, key: Option<PathBuf>, config: &Path) -> Self {
        let mut dirs = Vec::new();
        if let Some(parent) = config.parent().filter(|p| !p.as_os_str().is_empty()) {
            dirs.push(parent.join("tls"));
        }
        dirs.extend(user_dir());
        Self {
            own: cert.zip(key),
            dirs,
            names: names(),
        }
    }

    /// The certificate to serve: the host's own, an existing self-signed one, or a new one in
    /// the first folder it can be written to.
    pub fn load(&self) -> Result<Identity> {
        if let Some((cert, key)) = &self.own {
            return Identity::from_files(cert, key);
        }
        let existing = self
            .dirs
            .iter()
            .find(|d| d.join(tls::CERT_FILE).exists() && d.join(tls::KEY_FILE).exists());
        if let Some(dir) = existing {
            match Identity::from_files(&dir.join(tls::CERT_FILE), &dir.join(tls::KEY_FILE)) {
                Ok(id) => return Ok(id),
                // Not readable by this user: try the next folder.
                Err(e) => tracing::debug!("TLS certificate in {}: {e:#}", dir.display()),
            }
        }
        self.renew()
    }

    /// Makes a new self-signed certificate in the first folder it can be written to.
    pub fn renew(&self) -> Result<Identity> {
        let mut last = None;
        for dir in &self.dirs {
            let source = CertSource::SelfSigned {
                dir: dir.clone(),
                names: self.names.clone(),
            };
            match tls::write_self_signed(dir, &self.names).and_then(|()| Identity::load(&source)) {
                Ok(id) => {
                    tracing::info!(
                        fingerprint = %id.fingerprint,
                        "made a self-signed TLS certificate in {}",
                        dir.display()
                    );
                    return Ok(id);
                }
                Err(e) => last = Some(e),
            }
        }
        Err(last.unwrap_or_else(|| anyhow::anyhow!("no folder for the TLS certificate")))
    }

    pub fn is_own(&self) -> bool {
        self.own.is_some()
    }
}

/// The names the self-signed certificate is made for: this computer's name and loopback.
fn names() -> Vec<String> {
    let host = crate::hostname();
    let mut names = Vec::new();
    let host = host.to_ascii_lowercase();
    let dns = |c: char| c.is_ascii_alphanumeric() || c == '-' || c == '.';
    if host != "unknown" && !host.is_empty() && host.chars().all(dns) {
        names.push(host);
    }
    names.extend(["localhost", "127.0.0.1", "::1"].map(str::to_owned));
    names
}

/// The agent's own data folder, for a certificate when the config folder isn't writable.
fn user_dir() -> Option<PathBuf> {
    if let Some(dir) = crate::local_data_dir() {
        return Some(dir.join("tls"));
    }
    let state = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/state")))?;
    Some(state.join("dalker").join("tls"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn makes_then_keeps_the_self_signed_certificate() {
        let dir = std::env::temp_dir().join(format!(
            "dalker-agent-tls-{}-{}",
            std::process::id(),
            rand::random::<u32>()
        ));
        let config = dir.join("dalker.conf");
        let certs = Certs::new(None, None, &config);
        let first = certs.load().unwrap();
        assert_eq!(first.cert_path, dir.join("tls").join(tls::CERT_FILE));
        assert_eq!(certs.load().unwrap().fingerprint, first.fingerprint);
        let renewed = certs.renew().unwrap();
        assert_ne!(renewed.fingerprint, first.fingerprint);
        // The host's own files win.
        let own = Certs::new(
            Some(dir.join("tls").join(tls::CERT_FILE)),
            Some(dir.join("tls").join(tls::KEY_FILE)),
            Path::new("/nonexistent/dalker.conf"),
        );
        assert!(own.is_own());
        assert_eq!(own.load().unwrap().fingerprint, renewed.fingerprint);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
