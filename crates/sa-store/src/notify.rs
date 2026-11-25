//! Local settings for optional phone notifications.

use anyhow::{Result, anyhow, ensure};
use rusqlite::{Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use url::{Host, Position, Url};

pub const SERVER_CREDENTIALS_ERROR: &str = "服务器地址里不能包含用户名或密码";

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub enabled: bool,
    pub server: String,
    pub topic: String,
    pub needs_you: bool,
    pub done: bool,
    pub unfinished: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            enabled: false,
            server: "https://ntfy.sh".into(),
            topic: String::new(),
            needs_you: true,
            done: true,
            unfinished: true,
        }
    }
}

pub fn init(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS studio_notify(
           singleton INTEGER PRIMARY KEY CHECK(singleton=1), settings TEXT NOT NULL);",
    )?;
    Ok(())
}

pub fn load(conn: &Connection) -> Result<Settings> {
    let exists = conn
        .prepare_cached("SELECT 1 FROM sqlite_master WHERE type='table' AND name='studio_notify'")?
        .exists([])?;
    if !exists {
        return Ok(Settings::default());
    }
    let value: Option<String> = conn
        .query_row(
            "SELECT settings FROM studio_notify WHERE singleton=1",
            [],
            |row| row.get(0),
        )
        .optional()?;
    value
        .map(|text| {
            // A deserialization error can quote malformed stored values.
            serde_json::from_str(&text).map_err(|_| anyhow!("读取手机推送设置失败"))
        })
        .transpose()
        .map(Option::unwrap_or_default)
}

pub fn save(conn: &Connection, s: &Settings) -> Result<()> {
    let address = s.server.trim();
    let has_credentials = Url::parse(address).map_or_else(
        |_| malformed_url_has_userinfo(address),
        |url| url_has_credentials(&url),
    );
    ensure!(!has_credentials, "{SERVER_CREDENTIALS_ERROR}");
    init(conn)?;
    conn.execute(
        "INSERT INTO studio_notify(singleton,settings) VALUES(1,?1)
         ON CONFLICT(singleton) DO UPDATE SET settings=excluded.settings",
        [serde_json::to_string(s)?],
    )?;
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ServerProblem {
    Empty,
    NeedsHttps,
    Invalid,
    HasPath,
    HasCredentials,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TopicProblem {
    Empty,
    Invalid,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Check {
    pub server: Option<ServerProblem>,
    pub topic: Option<TopicProblem>,
    pub short_topic: bool,
    pub nothing_selected: bool,
}

pub fn check(s: &Settings) -> Check {
    if !s.enabled {
        return Check::default();
    }

    let address = s.server.trim();
    let server = if address.is_empty() {
        Some(ServerProblem::Empty)
    } else {
        match Url::parse(address) {
            Err(_) if malformed_url_has_userinfo(address) => Some(ServerProblem::HasCredentials),
            Err(_) => Some(ServerProblem::Invalid),
            Ok(url) if url_has_credentials(&url) => Some(ServerProblem::HasCredentials),
            Ok(url)
                if matches!(url.scheme(), "http" | "https")
                    && (url.path() != "/"
                        || has_non_root_path(address)
                        || url.query().is_some()
                        || url.fragment().is_some()) =>
            {
                Some(ServerProblem::HasPath)
            }
            Ok(url) if url.scheme() == "https" => None,
            Ok(url) if url.scheme() == "http" && url.host().is_some_and(local_host) => None,
            Ok(_) => Some(ServerProblem::NeedsHttps),
        }
    };

    let topic = if s.topic.is_empty() {
        Some(TopicProblem::Empty)
    } else if s.topic.len() > 64
        || !s
            .topic
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
    {
        Some(TopicProblem::Invalid)
    } else {
        None
    };

    Check {
        server,
        topic,
        short_topic: s.topic.chars().count() < 12,
        nothing_selected: !(s.needs_you || s.done || s.unfinished),
    }
}

/// Return a safe replacement only when userinfo must be removed.
pub fn redact_server_credentials(address: &str) -> Option<String> {
    let address = address.trim();
    match Url::parse(address) {
        Ok(url) if url_has_credentials(&url) => Some(format!(
            "{}{}",
            &url[..Position::BeforeUsername],
            &url[Position::BeforeHost..]
        )),
        Ok(_) => None,
        // A malformed authority can still contain credentials. Do not display it.
        Err(_) => malformed_url_has_userinfo(address).then(String::new),
    }
}

fn url_has_credentials(url: &Url) -> bool {
    !url.username().is_empty() || url.password().is_some()
}

fn malformed_url_has_userinfo(address: &str) -> bool {
    let locator = match address.split_once(':') {
        Some((scheme, rest))
            if scheme.eq_ignore_ascii_case("http")
                || scheme.eq_ignore_ascii_case("https")
                || rest.starts_with("//") =>
        {
            rest
        }
        _ => address,
    };
    locator
        .trim_start_matches(['/', '\\'])
        .split(['/', '\\', '?', '#'])
        .next()
        .is_some_and(|authority| authority.contains('@'))
}

fn has_non_root_path(address: &str) -> bool {
    // URL parsing normalizes dot segments. Inspect the supplied path too so
    // a path such as /team/.. cannot silently turn into a valid server setting.
    let Some((_, authority_and_path)) = address.split_once(':') else {
        return false;
    };
    let authority_and_path = authority_and_path.trim_start_matches(['/', '\\']);
    let before_suffix = authority_and_path
        .split(['?', '#'])
        .next()
        .unwrap_or_default();
    before_suffix
        .find(['/', '\\'])
        .is_some_and(|at| &before_suffix[at..] != "/")
}

fn local_host(host: Host<&str>) -> bool {
    match host {
        Host::Domain(name) => {
            name == "localhost" || name.ends_with(".local") || name.ends_with(".lan")
        }
        Host::Ipv4(ip) => {
            let [first, second, ..] = ip.octets();
            ip.is_loopback() || ip.is_private() || (first == 100 && (64..=127).contains(&second))
        }
        Host::Ipv6(ip) => ip.is_loopback(),
    }
}

pub fn random_topic() -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
    let mut topic = String::from("sa-");
    while topic.len() < 23 {
        // 248 is divisible by 62. Discard larger bytes to avoid modulo bias.
        for byte in random_bytes().into_iter().filter(|&byte| byte < 248) {
            topic.push(char::from(ALPHABET[usize::from(byte) % ALPHABET.len()]));
            if topic.len() == 23 {
                break;
            }
        }
    }
    topic
}

#[cfg(windows)]
fn random_bytes() -> [u8; 32] {
    use windows_sys::Win32::Security::Cryptography::{
        BCRYPT_USE_SYSTEM_PREFERRED_RNG, BCryptGenRandom,
    };
    let mut bytes = [0; 32];
    // The system-preferred RNG takes a null handle and writes to this buffer.
    let status = unsafe {
        BCryptGenRandom(
            std::ptr::null_mut(),
            bytes.as_mut_ptr(),
            bytes.len() as u32,
            BCRYPT_USE_SYSTEM_PREFERRED_RNG,
        )
    };
    assert!(status >= 0, "system RNG failed ({status:#x})");
    bytes
}

#[cfg(not(windows))]
fn random_bytes() -> [u8; 32] {
    use std::io::Read;
    let mut bytes = [0; 32];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut file| file.read_exact(&mut bytes))
        .expect("system RNG failed");
    bytes
}

#[cfg(test)]
mod tests;
