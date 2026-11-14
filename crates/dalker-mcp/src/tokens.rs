//! Bearer tokens for the MCP endpoint.
//!
//! A token looks like `dkmcp_<id>_<secret>`: `id` is 8 hex digits that name the token (and
//! find its record without trying every hash), `secret` is 32 random bytes in hex. The host
//! stores only an argon2 hash of the whole token plus metadata, one config line per token:
//!
//! ```text
//! DALKER_MCP_TOKEN_<id>=<name>;<scopes>;<created unix>;<expires unix or ->;<argon2 PHC hash>
//! ```
//!
//! Tokens are created, listed and revoked on the host only (`dalkerd mcp-token ...`).

use std::collections::{BTreeSet, HashMap};
use std::fmt;
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use argon2::password_hash::rand_core::OsRng;
use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use argon2::Argon2;
use rand::TryRng;

/// Prefix of the config keys that hold token records.
pub const CONFIG_KEY_PREFIX: &str = "DALKER_MCP_TOKEN_";
/// Prefix of the config keys that keep a token itself, so the host's settings window can copy
/// it again later (`DALKER_MCP_SECRET_<id>=dkmcp_...`). The agent never reads them: it
/// checks requests against the hash. Like the rest of the config file, they are for the
/// administrators' eyes only.
pub const SECRET_KEY_PREFIX: &str = "DALKER_MCP_SECRET_";
const TOKEN_PREFIX: &str = "dkmcp_";

/// What a token may do. Checked per tool.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Scope {
    /// Host info, screenshots, screen-change waits, and `dalker://screen`.
    View,
    /// Mouse and keyboard.
    Input,
    /// Read and write the host's clipboard.
    Clipboard,
    /// List, read, and upload files.
    Files,
}

impl Scope {
    pub const ALL: [Scope; 4] = [Scope::View, Scope::Input, Scope::Clipboard, Scope::Files];

    pub fn as_str(self) -> &'static str {
        match self {
            Scope::View => "view",
            Scope::Input => "input",
            Scope::Clipboard => "clipboard",
            Scope::Files => "files",
        }
    }
}

impl fmt::Display for Scope {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for Scope {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, String> {
        Scope::ALL
            .into_iter()
            .find(|sc| sc.as_str() == s.trim())
            .ok_or_else(|| {
                format!(
                    "unknown scope `{}` (known: view, input, clipboard, files)",
                    s.trim()
                )
            })
    }
}

/// Parses `"view,input"` into a set of scopes. At least one is required.
pub fn parse_scopes(list: &str) -> Result<BTreeSet<Scope>, String> {
    let scopes = list
        .split(',')
        .filter(|s| !s.trim().is_empty())
        .map(str::parse)
        .collect::<Result<BTreeSet<_>, _>>()?;
    if scopes.is_empty() {
        return Err("at least one scope is needed (view, input, clipboard, files)".into());
    }
    Ok(scopes)
}

pub fn format_scopes(scopes: &BTreeSet<Scope>) -> String {
    scopes
        .iter()
        .map(|s| s.as_str())
        .collect::<Vec<_>>()
        .join(",")
}

/// Parses a lifetime like `30d`, `12h`, `45m` or `never`.
pub fn parse_lifetime(s: &str) -> Result<Option<Duration>, String> {
    let s = s.trim();
    if s.eq_ignore_ascii_case("never") {
        return Ok(None);
    }
    let (num, unit) = s.split_at(s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len()));
    let n: u64 = num
        .parse()
        .map_err(|_| format!("bad lifetime `{s}` (use e.g. 30d, 12h, 45m or never)"))?;
    let secs = match unit {
        "d" => n * 86_400,
        "h" => n * 3600,
        "m" => n * 60,
        _ => {
            return Err(format!(
                "bad lifetime `{s}` (use e.g. 30d, 12h, 45m or never)"
            ))
        }
    };
    if secs == 0 {
        return Err("lifetime must be positive".into());
    }
    Ok(Some(Duration::from_secs(secs)))
}

pub fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// What the host stores about one token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TokenRecord {
    pub id: String,
    pub name: String,
    pub scopes: BTreeSet<Scope>,
    /// Unix seconds.
    pub created: u64,
    /// Unix seconds; `None` = never.
    pub expires: Option<u64>,
    /// Argon2 PHC hash of the whole token string.
    pub hash: String,
}

impl TokenRecord {
    /// Config key of this record (`DALKER_MCP_TOKEN_<id>`).
    pub fn config_key(&self) -> String {
        format!("{CONFIG_KEY_PREFIX}{}", self.id)
    }

    /// Config key that keeps the token itself (`DALKER_MCP_SECRET_<id>`).
    pub fn secret_key(&self) -> String {
        format!("{SECRET_KEY_PREFIX}{}", self.id)
    }

    pub fn config_value(&self) -> String {
        let expires = self.expires.map_or("-".to_owned(), |e| e.to_string());
        format!(
            "{};{};{};{expires};{}",
            self.name,
            format_scopes(&self.scopes),
            self.created,
            self.hash
        )
    }

    /// Parses a config line; `None` if `key` is not a token key or the value is malformed.
    pub fn from_config(key: &str, value: &str) -> Option<Self> {
        let id = key.strip_prefix(CONFIG_KEY_PREFIX)?;
        if !valid_id(id) {
            return None;
        }
        let mut parts = value.splitn(5, ';');
        let name = parts.next()?.to_owned();
        let scopes = parse_scopes(parts.next()?).ok()?;
        let created = parts.next()?.parse().ok()?;
        let expires = match parts.next()? {
            "-" => None,
            e => Some(e.parse().ok()?),
        };
        let hash = parts.next()?.to_owned();
        PasswordHash::new(&hash).ok()?;
        Some(Self {
            id: id.to_owned(),
            name,
            scopes,
            created,
            expires,
            hash,
        })
    }

    pub fn expired_at(&self, now: u64) -> bool {
        self.expires.is_some_and(|e| now >= e)
    }
}

fn valid_id(id: &str) -> bool {
    id.len() == 8
        && id
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// Token names are shown in logs and the tray; keep them simple. Spaces are fine inside a
/// name ("Claude Desktop"), not at its ends.
pub fn validate_name(name: &str) -> Result<(), String> {
    let ok = !name.is_empty()
        && name.len() <= 64
        && name.trim() == name
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '@' | ' '));
    if ok {
        Ok(())
    } else {
        Err("token name: 1-64 characters, letters, digits, spaces and - _ . @ only".into())
    }
}

/// Makes a new token. Returns the secret token (shown once) and the record to store.
pub fn generate(
    name: &str,
    scopes: BTreeSet<Scope>,
    lifetime: Option<Duration>,
) -> anyhow::Result<(String, TokenRecord)> {
    validate_name(name).map_err(anyhow::Error::msg)?;
    anyhow::ensure!(!scopes.is_empty(), "at least one scope is needed");
    let mut id = [0u8; 4];
    let mut secret = [0u8; 32];
    rand::rngs::SysRng.try_fill_bytes(&mut id)?;
    rand::rngs::SysRng.try_fill_bytes(&mut secret)?;
    let id = hex(&id);
    let token = format!("{TOKEN_PREFIX}{id}_{}", hex(&secret));
    let salt = SaltString::generate(&mut OsRng);
    let hash = Argon2::default()
        .hash_password(token.as_bytes(), &salt)
        .map_err(|e| anyhow::anyhow!("argon2: {e}"))?
        .to_string();
    let created = unix_now();
    let expires = lifetime.map(|l| created + l.as_secs());
    Ok((
        token,
        TokenRecord {
            id,
            name: name.to_owned(),
            scopes,
            created,
            expires,
            hash,
        },
    ))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// The token id in a presented token, if it is shaped like one of ours.
pub fn token_id(token: &str) -> Option<&str> {
    let rest = token.strip_prefix(TOKEN_PREFIX)?;
    let (id, secret) = rest.split_once('_')?;
    (valid_id(id) && secret.len() == 64).then_some(id)
}

/// Who a request authenticated as.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Identity {
    pub token_id: String,
    pub name: String,
    pub scopes: BTreeSet<Scope>,
}

impl Identity {
    pub fn has(&self, scope: Scope) -> bool {
        self.scopes.contains(&scope)
    }
}

/// Why a token was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthError {
    /// Not a token we know (malformed, unknown id, wrong secret, revoked).
    Invalid,
    Expired,
}

/// The tokens the host currently accepts. Replaced as a whole when the config file changes.
#[derive(Default)]
pub struct TokenStore {
    records: Mutex<HashMap<String, TokenRecord>>,
    /// Tokens whose hash already checked out: argon2 is far too slow to run on every
    /// request. Keyed by id, holds the exact token that matched (memory only).
    verified: Mutex<HashMap<String, String>>,
}

impl TokenStore {
    pub fn new(records: impl IntoIterator<Item = TokenRecord>) -> Self {
        let store = Self::default();
        store.replace(records);
        store
    }

    /// Installs a new set of records; returns the ids that were removed or changed (their
    /// sessions must end).
    pub fn replace(&self, records: impl IntoIterator<Item = TokenRecord>) -> Vec<String> {
        let new: HashMap<String, TokenRecord> =
            records.into_iter().map(|r| (r.id.clone(), r)).collect();
        let mut cur = self.records.lock().unwrap();
        let gone: Vec<String> = cur
            .iter()
            .filter(|(id, r)| new.get(*id) != Some(r))
            .map(|(id, _)| id.clone())
            .collect();
        let mut verified = self.verified.lock().unwrap();
        for id in &gone {
            verified.remove(id);
        }
        *cur = new;
        gone
    }

    pub fn records(&self) -> Vec<TokenRecord> {
        let mut v: Vec<_> = self.records.lock().unwrap().values().cloned().collect();
        v.sort_by(|a, b| a.created.cmp(&b.created).then_with(|| a.id.cmp(&b.id)));
        v
    }

    pub fn is_empty(&self) -> bool {
        self.records.lock().unwrap().is_empty()
    }

    /// Whether `token_id` is still a live (known, unexpired) token.
    pub fn is_live(&self, token_id: &str, now: u64) -> bool {
        self.records
            .lock()
            .unwrap()
            .get(token_id)
            .is_some_and(|r| !r.expired_at(now))
    }

    /// Checks a presented bearer token. Blocking (argon2) the first time a token is seen.
    pub fn verify(&self, token: &str, now: u64) -> Result<Identity, AuthError> {
        let id = token_id(token).ok_or(AuthError::Invalid)?;
        let record = self
            .records
            .lock()
            .unwrap()
            .get(id)
            .cloned()
            .ok_or(AuthError::Invalid)?;
        let cached = self
            .verified
            .lock()
            .unwrap()
            .get(id)
            .is_some_and(|t| constant_time_eq(t.as_bytes(), token.as_bytes()));
        if !cached {
            let hash = PasswordHash::new(&record.hash).map_err(|_| AuthError::Invalid)?;
            Argon2::default()
                .verify_password(token.as_bytes(), &hash)
                .map_err(|_| AuthError::Invalid)?;
            // Only cache if the record wasn't replaced meanwhile.
            if self.records.lock().unwrap().get(id) == Some(&record) {
                self.verified
                    .lock()
                    .unwrap()
                    .insert(id.to_owned(), token.to_owned());
            }
        }
        if record.expired_at(now) {
            return Err(AuthError::Expired);
        }
        Ok(Identity {
            token_id: record.id,
            name: record.name,
            scopes: record.scopes,
        })
    }
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scopes(s: &str) -> BTreeSet<Scope> {
        parse_scopes(s).unwrap()
    }

    #[test]
    fn scopes_parse_and_print() {
        assert_eq!(format_scopes(&scopes("input, view,view")), "view,input");
        assert_eq!(
            format_scopes(&scopes("files,clipboard,input,view")),
            "view,input,clipboard,files"
        );
        assert!(parse_scopes("").is_err());
        assert!(parse_scopes("view,bogus").is_err());
        assert!(parse_scopes("VIEW").is_err(), "scopes are lowercase");
    }

    #[test]
    fn lifetimes() {
        assert_eq!(
            parse_lifetime("30d").unwrap(),
            Some(Duration::from_secs(30 * 86_400))
        );
        assert_eq!(
            parse_lifetime("12h").unwrap(),
            Some(Duration::from_secs(12 * 3600))
        );
        assert_eq!(
            parse_lifetime("45m").unwrap(),
            Some(Duration::from_secs(45 * 60))
        );
        assert_eq!(parse_lifetime("never").unwrap(), None);
        for bad in ["", "d", "0d", "10", "10s", "-1d", "1.5d"] {
            assert!(parse_lifetime(bad).is_err(), "{bad:?} should be rejected");
        }
    }

    #[test]
    fn names() {
        assert!(validate_name("claude-laptop").is_ok());
        assert!(validate_name("ci_bot.2@lab").is_ok());
        assert!(validate_name("").is_err());
        assert!(validate_name("Claude Desktop").is_ok());
        assert!(validate_name(" padded").is_err());
        assert!(validate_name("tab\there").is_err());
        assert!(
            validate_name("semi;colon").is_err(),
            "`;` separates record fields"
        );
        assert!(validate_name(&"x".repeat(65)).is_err());
    }

    #[test]
    fn record_roundtrips_through_config() {
        let (token, rec) = generate(
            "laptop",
            scopes("view,input"),
            Some(Duration::from_secs(3600)),
        )
        .unwrap();
        assert_eq!(token_id(&token), Some(rec.id.as_str()));
        assert!(!rec.hash.contains(&token), "only the hash is stored");
        let parsed = TokenRecord::from_config(&rec.config_key(), &rec.config_value()).unwrap();
        assert_eq!(parsed, rec);
        assert_eq!(parsed.expires, Some(rec.created + 3600));

        let (_, never) = generate("n", scopes("view"), None).unwrap();
        let parsed = TokenRecord::from_config(&never.config_key(), &never.config_value()).unwrap();
        assert_eq!(parsed.expires, None);

        assert!(TokenRecord::from_config("DALKER_BIND", "x").is_none());
        assert!(
            TokenRecord::from_config(&rec.config_key(), "laptop;view;1;-;not-a-hash").is_none()
        );
        assert!(TokenRecord::from_config("DALKER_MCP_TOKEN_XYZ", &rec.config_value()).is_none());
    }

    #[test]
    fn verify_accepts_the_token_only() {
        let (token, rec) = generate("laptop", scopes("view,clipboard"), None).unwrap();
        let store = TokenStore::new([rec.clone()]);
        let now = unix_now();
        let who = store.verify(&token, now).unwrap();
        assert_eq!(who.name, "laptop");
        assert!(who.has(Scope::View) && who.has(Scope::Clipboard) && !who.has(Scope::Input));
        // Second time comes from the cache.
        assert_eq!(store.verify(&token, now).unwrap(), who);

        let mut wrong = token.clone();
        let last = wrong.pop().unwrap();
        wrong.push(if last == '0' { '1' } else { '0' });
        assert_eq!(
            store.verify(&wrong, now),
            Err(AuthError::Invalid),
            "wrong secret, even after a cached success"
        );
        assert_eq!(
            store.verify("dkmcp_00000000_00", now),
            Err(AuthError::Invalid)
        );
        assert_eq!(store.verify("Bearer nope", now), Err(AuthError::Invalid));
        let (other, _) = generate("other", scopes("view"), None).unwrap();
        assert_eq!(
            store.verify(&other, now),
            Err(AuthError::Invalid),
            "unknown id"
        );
    }

    #[test]
    fn expired_and_revoked_tokens_are_refused() {
        let (token, mut rec) =
            generate("t", scopes("view"), Some(Duration::from_secs(60))).unwrap();
        let store = TokenStore::new([rec.clone()]);
        let now = unix_now();
        assert!(store.verify(&token, now).is_ok());
        assert!(store.is_live(&rec.id, now));
        assert_eq!(store.verify(&token, now + 61), Err(AuthError::Expired));
        assert!(!store.is_live(&rec.id, now + 61));

        // Changing a record (e.g. narrower scopes) counts as removed and drops the cache.
        rec.scopes = scopes("view,input");
        assert_eq!(store.replace([rec.clone()]), vec![rec.id.clone()]);
        assert!(store.verify(&token, now).unwrap().has(Scope::Input));
        assert!(
            store.replace([rec.clone()]).is_empty(),
            "unchanged records stay"
        );

        assert_eq!(store.replace([]), vec![rec.id.clone()]);
        assert_eq!(store.verify(&token, now), Err(AuthError::Invalid));
        assert!(!store.is_live(&rec.id, now));
    }
}
