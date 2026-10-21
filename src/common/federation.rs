//! Federation — other machines' conversations, as rows in this machine's registry.
//!
//! Each configured `[[remotes]]` entry with an `api` gets one poller thread that fetches
//! its `/api/registry` once a second with a hard 300 ms connect and 800 ms read bound. The
//! gather never waits on the network: it reads whatever the pollers last stored. A remote
//! that is down, slow or unreachable simply contributes nothing and reads as `✗` in the
//! title bar — it can never stall the 1 s tick.
//!
//! Only long-running loops (TUI, web data thread) federate. A one-shot command such as
//! `hive box pack` uses the plain gather, which also means it can never pick a remote
//! conversation by accident.
//!
//! The wire is the registry's own [`Conversation`] (serde), not the web views: the views are
//! lossy projections (no parent, no placement) and the peer needs the same model to merge.

use anyhow::{anyhow, bail, Result};
use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::sync::{Mutex, Once, OnceLock};
use std::time::Duration;

use crate::common::config::HiveConfig;
use crate::common::registry::Conversation;

const CONNECT_TIMEOUT: Duration = Duration::from_millis(300);
const READ_TIMEOUT: Duration = Duration::from_millis(800);
const POLL_EVERY: Duration = Duration::from_secs(1);

/// What the poller last learned about one remote.
#[derive(Debug, Clone)]
pub struct RemoteState {
    pub name: String,
    /// `None` until the first poll completes.
    pub up: Option<bool>,
    pub conversations: Vec<Conversation>,
}

static STATE: OnceLock<Mutex<BTreeMap<String, RemoteState>>> = OnceLock::new();
static STARTED: Once = Once::new();

fn state() -> &'static Mutex<BTreeMap<String, RemoteState>> {
    STATE.get_or_init(Default::default)
}

/// `http://host[:port]` → (host, port).
pub fn split_api(api: &str) -> Option<(String, u16)> {
    let rest = api
        .strip_prefix("http://")
        .unwrap_or(api)
        .trim_end_matches('/');
    let (host, port) = match rest.rsplit_once(':') {
        Some((h, p)) => (h, p.parse().ok()?),
        None => (rest, 80),
    };
    (!host.is_empty()).then(|| (host.to_string(), port))
}

/// One bounded HTTP/1.0 request (1.0, so the body is never chunked and the server closes).
pub fn http_request(api: &str, method: &str, path: &str, body: Option<&str>) -> Result<String> {
    let (host, port) = split_api(api).ok_or_else(|| anyhow!("bad api url '{api}'"))?;
    let addr = (host.as_str(), port)
        .to_socket_addrs()?
        .next()
        .ok_or_else(|| anyhow!("cannot resolve {host}"))?;
    let mut stream = TcpStream::connect_timeout(&addr, CONNECT_TIMEOUT)?;
    // A write (freeze, thaw) legitimately takes longer than a poll; only the poll is on the
    // 1 s tick and needs the tight bound.
    stream.set_read_timeout(Some(if method == "POST" {
        Duration::from_secs(5)
    } else {
        READ_TIMEOUT
    }))?;
    stream.set_write_timeout(Some(READ_TIMEOUT))?;
    let body = body.unwrap_or("");
    let req = if method == "POST" {
        format!(
            "POST {path} HTTP/1.0\r\nHost: {host}\r\nContent-Type: application/json\r\n\
             Content-Length: {}\r\n\r\n{body}",
            body.len()
        )
    } else {
        format!("GET {path} HTTP/1.0\r\nHost: {host}\r\n\r\n")
    };
    stream.write_all(req.as_bytes())?;
    let mut buf = Vec::new();
    stream.read_to_end(&mut buf)?;
    let text = String::from_utf8_lossy(&buf);
    let (head, body) = text
        .split_once("\r\n\r\n")
        .ok_or_else(|| anyhow!("malformed response"))?;
    let code = head.split_whitespace().nth(1).unwrap_or("");
    if code != "200" {
        bail!("{host} answered {code}");
    }
    Ok(body.to_string())
}

pub fn fetch_registry(api: &str) -> Result<Vec<Conversation>> {
    Ok(serde_json::from_str(&http_request(
        api,
        "GET",
        "/api/registry",
        None,
    )?)?)
}

fn poll_loop(name: String, api: String) {
    loop {
        let next = match fetch_registry(&api) {
            Ok(conversations) => RemoteState {
                name: name.clone(),
                up: Some(true),
                conversations,
            },
            Err(_) => RemoteState {
                name: name.clone(),
                up: Some(false),
                conversations: Vec::new(),
            },
        };
        if let Ok(mut st) = state().lock() {
            st.insert(name.clone(), next);
        }
        std::thread::sleep(POLL_EVERY);
    }
}

fn start_pollers() {
    for r in HiveConfig::load().remotes {
        let Some(api) = r.api else { continue };
        if let Ok(mut st) = state().lock() {
            st.insert(
                r.name.clone(),
                RemoteState {
                    name: r.name.clone(),
                    up: None,
                    conversations: Vec::new(),
                },
            );
        }
        std::thread::spawn(move || poll_loop(r.name, api));
    }
}

/// Start the pollers (once per process) and return what they have so far. Cheap and
/// non-blocking after the first call.
pub fn snapshots() -> Vec<RemoteState> {
    STARTED.call_once(start_pollers);
    state()
        .lock()
        .map(|s| s.values().cloned().collect())
        .unwrap_or_default()
}

/// Percent-encode everything but the unreserved set, for a query value.
pub fn enc(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// `host`'s dashboard API, if it is a configured remote with one.
pub fn api_of(host: &str) -> Option<String> {
    HiveConfig::load().remote(host).and_then(|r| r.api.clone())
}

/// The session name on the remote for a view/flag name of the form `"<session> @<host>"`.
pub fn real_session(name: &str, host: &str) -> String {
    name.strip_suffix(&format!(" @{host}"))
        .unwrap_or(name)
        .to_string()
}

/// POST `body` to a remote's dashboard API and return its JSON answer. This is how an
/// action on a remote row reaches the machine that can perform it: the remote's API is
/// the same one its own phone dashboard uses, so there is nothing new to implement there.
pub fn proxy_post(host: &str, path: &str, body: &serde_json::Value) -> Result<serde_json::Value> {
    let api = api_of(host).ok_or_else(|| anyhow!("remote '{host}' has no api configured"))?;
    let text = http_request(&api, "POST", path, Some(&body.to_string()))?;
    let v: serde_json::Value = serde_json::from_str(&text)?;
    if let Some(e) = v.get("error").and_then(|e| e.as_str()) {
        bail!("{host}: {e}");
    }
    Ok(v)
}

/// GET from a remote's dashboard API, returning the raw body.
pub fn proxy_get(host: &str, path: &str) -> Result<String> {
    let api = api_of(host).ok_or_else(|| anyhow!("remote '{host}' has no api configured"))?;
    http_request(&api, "GET", path, None)
}

/// A remote conversation by id, from the last poll.
pub fn find_conversation(id: &str) -> Option<Conversation> {
    let st = state().lock().ok()?;
    st.values().find_map(|r| {
        r.conversations
            .iter()
            .find(|c| c.id.as_str() == id)
            .map(|c| {
                let mut c = c.clone();
                c.host = Some(r.name.clone());
                c
            })
    })
}

/// Names of remotes that are configured but not answering — the title bar's `@mini ✗`.
pub fn down_remotes() -> Vec<String> {
    // Not `snapshots()`: this runs every frame and that clones every remote's rows.
    STARTED.call_once(start_pollers);
    state()
        .lock()
        .map(|s| {
            s.values()
                .filter(|r| r.up == Some(false))
                .map(|r| r.name.clone())
                .collect()
        })
        .unwrap_or_default()
}

/// Fold every reachable remote into `reg`. A down remote contributes nothing.
pub fn merge_into(reg: &mut crate::common::registry::ConversationRegistry) {
    for s in snapshots() {
        if s.up == Some(true) {
            reg.merge_remote(&s.name, s.conversations);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    #[test]
    fn api_urls_split_into_host_and_port() {
        assert_eq!(
            split_api("http://mini.local:8375"),
            Some(("mini.local".into(), 8375))
        );
        assert_eq!(
            split_api("http://mini.local/"),
            Some(("mini.local".into(), 80))
        );
        assert_eq!(split_api("10.0.0.2:9"), Some(("10.0.0.2".into(), 9)));
        assert_eq!(split_api("http://:80"), None);
    }

    /// Serve one canned response on an ephemeral port.
    fn serve_once(response: &'static str) -> String {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        std::thread::spawn(move || {
            if let Ok((mut s, _)) = l.accept() {
                let mut buf = [0u8; 1024];
                let _ = s.read(&mut buf);
                let _ = s.write_all(response.as_bytes());
            }
        });
        format!("http://127.0.0.1:{port}")
    }

    #[test]
    fn fetch_parses_a_peers_registry() {
        let api = serve_once("HTTP/1.0 200 OK\r\n\r\n[]");
        assert!(fetch_registry(&api).unwrap().is_empty());
    }

    #[test]
    fn a_non_200_is_an_error_not_an_empty_registry() {
        let api = serve_once("HTTP/1.0 500 Oops\r\n\r\n[]");
        assert!(fetch_registry(&api).is_err());
    }

    #[test]
    fn an_unreachable_remote_fails_fast() {
        // Nothing listens on port 1; the bound is the connect timeout, not the OS default.
        let t = std::time::Instant::now();
        assert!(fetch_registry("http://127.0.0.1:1").is_err());
        assert!(t.elapsed() < Duration::from_secs(2));
    }
}
