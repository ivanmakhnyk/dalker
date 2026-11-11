//! Password-only sign-in (one access password per host, argon2 hash) and in-memory session
//! tokens. There is no user name: the password set on the host is the whole credential.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use argon2::password_hash::rand_core::OsRng;
use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use argon2::Argon2;
use rand::TryRng;
use tokio::sync::watch;

pub const COOKIE_NAME: &str = "dalker_session";

/// Whether the session cookie carries the `Secure` attribute (`DALKER_COOKIE_SECURE`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CookieSecure {
    /// Only when the request came in over HTTPS through a reverse proxy
    /// (`X-Forwarded-Proto: https`). Browsers drop a `Secure` cookie on plain HTTP, so a
    /// header sent by a client can only affect that client's own login.
    #[default]
    Auto,
    Always,
    Never,
}

impl CookieSecure {
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "auto" => Some(Self::Auto),
            "on" | "true" | "yes" | "1" => Some(Self::Always),
            "off" | "false" | "no" | "0" => Some(Self::Never),
            _ => None,
        }
    }
}

/// Hashes `password` into a PHC string (`$argon2id$v=19$...`).
pub fn hash_password(password: &str) -> anyhow::Result<String> {
    let salt = SaltString::generate(&mut OsRng);
    Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map(|h| h.to_string())
        .map_err(|e| anyhow::anyhow!("argon2: {e}"))
}

pub struct Credentials {
    /// PHC-formatted argon2 hash.
    pub password_hash: String,
}

/// Why a sign-in ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignOut {
    /// `POST /api/logout` with this session (another tab, for example).
    LoggedOut,
    /// The session reached its lifetime (`--session-hours`).
    Expired,
    /// The access password was changed on the host.
    PasswordChanged,
}

struct Session {
    expires: Instant,
    ended: watch::Sender<Option<SignOut>>,
}

impl Session {
    fn end(self, why: SignOut) {
        self.ended.send_replace(Some(why));
    }
}

/// Follows one signed-in session, so whatever it started (a stream) can end with it.
pub struct SignIn {
    expires: Instant,
    ended: watch::Receiver<Option<SignOut>>,
}

impl SignIn {
    /// Resolves when the session is signed out, its password changes, or it expires.
    pub async fn ended(mut self) -> SignOut {
        let expiry = tokio::time::sleep_until(tokio::time::Instant::from_std(self.expires));
        tokio::select! {
            r = self.ended.wait_for(Option::is_some) => match r {
                Ok(why) => why.unwrap_or(SignOut::Expired),
                // Dropped without a word: only expired sessions are pruned that way.
                Err(_) => SignOut::Expired,
            },
            _ = expiry => SignOut::Expired,
        }
    }
}

pub struct Authenticator {
    creds: Mutex<Credentials>,
    sessions: Mutex<HashMap<String, Session>>,
    ttl: Duration,
    cookie_secure: CookieSecure,
}

impl Authenticator {
    /// An empty hash means no password has been chosen yet (fresh install): nobody can sign in
    /// until one is set on the host.
    pub fn new(creds: Credentials, ttl: Duration) -> anyhow::Result<Self> {
        if !creds.password_hash.is_empty() {
            PasswordHash::new(&creds.password_hash)
                .map_err(|e| anyhow::anyhow!("invalid password hash: {e}"))?;
        }
        Ok(Self {
            creds: Mutex::new(creds),
            sessions: Mutex::new(HashMap::new()),
            ttl,
            cookie_secure: CookieSecure::default(),
        })
    }

    pub fn with_cookie_secure(mut self, mode: CookieSecure) -> Self {
        self.cookie_secure = mode;
        self
    }

    pub fn cookie_secure(&self) -> CookieSecure {
        self.cookie_secure
    }

    /// Whether an access password has been set on the host.
    pub fn has_password(&self) -> bool {
        !self.creds.lock().unwrap().password_hash.is_empty()
    }

    /// Replaces the login password hash, e.g. after it was changed on the host. There is no
    /// way to change it over HTTP. Every session is signed out.
    pub fn set_password_hash(&self, password_hash: &str) -> anyhow::Result<()> {
        PasswordHash::new(password_hash)
            .map_err(|e| anyhow::anyhow!("invalid password hash: {e}"))?;
        self.creds.lock().unwrap().password_hash = password_hash.to_owned();
        for (_, s) in self.sessions.lock().unwrap().drain() {
            s.end(SignOut::PasswordChanged);
        }
        Ok(())
    }

    /// The stored hash (to notice when the host's copy changes).
    pub fn password_hash(&self) -> String {
        self.creds.lock().unwrap().password_hash.clone()
    }

    pub fn ttl(&self) -> Duration {
        self.ttl
    }

    /// Checks the access password; returns a new session token on success.
    pub fn login(&self, password: &str) -> Option<String> {
        let stored_hash = self.creds.lock().unwrap().password_hash.clone();
        let hash = PasswordHash::new(&stored_hash).ok()?;
        Argon2::default()
            .verify_password(password.as_bytes(), &hash)
            .ok()?;
        let mut raw = [0u8; 32];
        rand::rngs::SysRng
            .try_fill_bytes(&mut raw)
            .expect("the OS random number generator failed");
        let token: String = raw.iter().map(|b| format!("{b:02x}")).collect();
        let mut sessions = self.sessions.lock().unwrap();
        let now = Instant::now();
        sessions.retain(|_, s| s.expires > now);
        sessions.insert(
            token.clone(),
            Session {
                expires: now + self.ttl,
                ended: watch::channel(None).0,
            },
        );
        Some(token)
    }

    pub fn is_valid(&self, token: &str) -> bool {
        self.sign_in(token).is_some()
    }

    /// The session behind a valid token, to follow until it ends.
    pub fn sign_in(&self, token: &str) -> Option<SignIn> {
        let mut sessions = self.sessions.lock().unwrap();
        match sessions.get(token) {
            Some(s) if s.expires > Instant::now() => Some(SignIn {
                expires: s.expires,
                ended: s.ended.subscribe(),
            }),
            Some(_) => {
                if let Some(s) = sessions.remove(token) {
                    s.end(SignOut::Expired);
                }
                None
            }
            None => None,
        }
    }

    pub fn logout(&self, token: &str) {
        if let Some(s) = self.sessions.lock().unwrap().remove(token) {
            s.end(SignOut::LoggedOut);
        }
    }
}

/// Extracts our session token from a `Cookie:` header value.
pub fn token_from_cookie_header(header: &str) -> Option<&str> {
    header
        .split(';')
        .filter_map(|kv| kv.trim().split_once('='))
        .find(|(k, _)| *k == COOKIE_NAME)
        .map(|(_, v)| v)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn auth() -> Authenticator {
        Authenticator::new(
            Credentials {
                password_hash: hash_password("s3cret").unwrap(),
            },
            Duration::from_secs(60),
        )
        .unwrap()
    }

    #[test]
    fn login_flow() {
        let a = auth();
        assert!(a.login("wrong").is_none());
        assert!(a.login("").is_none());
        let t = a.login("s3cret").expect("valid login");
        assert_eq!(t.len(), 64);
        assert!(a.is_valid(&t));
        a.logout(&t);
        assert!(!a.is_valid(&t));
    }

    #[test]
    fn expired_sessions_are_rejected() {
        let a = Authenticator::new(
            Credentials {
                password_hash: hash_password("p").unwrap(),
            },
            Duration::ZERO,
        )
        .unwrap();
        let t = a.login("p").unwrap();
        assert!(!a.is_valid(&t));
    }

    #[test]
    fn rejects_garbage_hash() {
        assert!(Authenticator::new(
            Credentials {
                password_hash: "plaintext".into()
            },
            Duration::from_secs(1)
        )
        .is_err());
    }

    #[test]
    fn new_hash_from_the_host_signs_everyone_out() {
        let a = auth();
        let old_session = a.login("s3cret").unwrap();
        assert!(a.set_password_hash("plaintext").is_err());
        let none = Authenticator::new(
            Credentials {
                password_hash: String::new(),
            },
            Duration::from_secs(60),
        )
        .unwrap();
        assert!(
            !none.has_password() && none.login("").is_none() && none.login("x").is_none(),
            "no password: no sign-in"
        );
        none.set_password_hash(&hash_password("chosen-later").unwrap())
            .unwrap();
        assert!(none.has_password() && none.login("chosen-later").is_some());
        assert!(a.is_valid(&old_session), "a bad hash changes nothing");

        a.set_password_hash(&hash_password("brand-new-pw1").unwrap())
            .unwrap();
        assert!(!a.is_valid(&old_session), "sessions are signed out");
        assert!(a.login("s3cret").is_none());
        assert!(a.login("brand-new-pw1").is_some());
    }

    #[tokio::test]
    async fn a_sign_in_reports_how_it_ended() {
        let a = auth();
        let t = a.login("s3cret").unwrap();
        let other = a.login("s3cret").unwrap();
        let (mine, theirs) = (a.sign_in(&t).unwrap(), a.sign_in(&other).unwrap());
        a.logout(&t);
        assert_eq!(mine.ended().await, SignOut::LoggedOut);
        a.set_password_hash(&hash_password("brand-new-pw1").unwrap())
            .unwrap();
        assert_eq!(theirs.ended().await, SignOut::PasswordChanged);
        assert!(a.sign_in(&t).is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn a_sign_in_ends_when_it_expires() {
        let a = auth();
        let t = a.login("s3cret").unwrap();
        let s = a.sign_in(&t).unwrap();
        let ended = tokio::spawn(s.ended());
        tokio::time::advance(Duration::from_secs(59)).await;
        assert!(!ended.is_finished());
        tokio::time::advance(Duration::from_secs(2)).await;
        assert_eq!(ended.await.unwrap(), SignOut::Expired);
    }

    #[test]
    fn cookie_parsing() {
        assert_eq!(
            token_from_cookie_header("a=1; dalker_session=abc; b=2"),
            Some("abc")
        );
        assert_eq!(token_from_cookie_header("a=1"), None);
    }
}
