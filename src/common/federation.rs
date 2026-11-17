//! Federation — other machines' conversations, as rows in this machine's registry.
//!
//! Each configured `[[remotes]]` entry with an `api` gets one poller thread that fetches
//! its `/api/registry` once a second with a hard 300 ms connect and 800 ms read bound. The
//! gather never waits on the network: it reads whatever the pollers last stored. A remote
//! that is down, slow or unreachable simply contributes nothing and reads as `✗` in the
//! title bar — it can never stall the 1 s tick.
//!
//! The long-running loops (TUI, web data thread) federate through the pollers. Two one-shots
//! federate once, synchronously, through [`merge_now`]: `hive cycle-free` (`Ctrl+g` visits
//! remote windows) and the TUI's `--detail` / `--project-detail` start-up. Every other
//! one-shot, `hive box pack` among them, uses the plain gather, so it can never pick a
//! remote conversation by accident.
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
    stream.set_read_timeout(Some(if path.starts_with("/api/box/") {
        // pack / unpack run git, rsync and project hooks on the far side.
        Duration::from_secs(300)
    } else if method == "POST" {
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
        // The far side's own explanation (`{"error": "..."}`) is the useful part.
        let why = serde_json::from_str::<serde_json::Value>(body)
            .ok()
            .and_then(|v| v.get("error").and_then(|e| e.as_str()).map(String::from))
            .unwrap_or_else(|| body.trim().chars().take(200).collect());
        bail!("{host} answered {code}: {why}");
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

/// Generous bound for moving a box: a tree can be hundreds of MB. Applies per read/write,
/// not to the whole transfer.
const TRANSFER_TIMEOUT: Duration = Duration::from_secs(120);

fn connect(api: &str) -> Result<(TcpStream, String)> {
    let (host, port) = split_api(api).ok_or_else(|| anyhow!("bad api url '{api}'"))?;
    let addr = (host.as_str(), port)
        .to_socket_addrs()?
        .next()
        .ok_or_else(|| anyhow!("cannot resolve {host}"))?;
    let s = TcpStream::connect_timeout(&addr, CONNECT_TIMEOUT)?;
    s.set_read_timeout(Some(TRANSFER_TIMEOUT))?;
    s.set_write_timeout(Some(TRANSFER_TIMEOUT))?;
    Ok((s, host))
}

/// Read a response head; returns (status code, the stream positioned at the body).
fn read_head(stream: &mut TcpStream) -> Result<(String, Vec<u8>)> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        let n = stream.read(&mut chunk)?;
        if n == 0 {
            bail!("connection closed before a response");
        }
        buf.extend_from_slice(&chunk[..n]);
        if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            let head = String::from_utf8_lossy(&buf[..pos]).to_string();
            let code = head.split_whitespace().nth(1).unwrap_or("").to_string();
            return Ok((code, buf[pos + 4..].to_vec()));
        }
    }
}

/// Stream a local file as the body of `PUT path`. Returns the response body.
pub fn put_file(api: &str, path: &str, file: &std::path::Path) -> Result<String> {
    let (mut s, host) = connect(api)?;
    let len = std::fs::metadata(file)?.len();
    write!(
        s,
        "PUT {path} HTTP/1.0\r\nHost: {host}\r\nContent-Type: application/x-tar\r\n\
         Content-Length: {len}\r\n\r\n"
    )?;
    std::io::copy(&mut std::fs::File::open(file)?, &mut s)?;
    let (code, mut rest) = read_head(&mut s)?;
    s.read_to_end(&mut rest)?;
    let body = String::from_utf8_lossy(&rest).to_string();
    if code != "200" {
        let why = serde_json::from_str::<serde_json::Value>(&body)
            .ok()
            .and_then(|v| v.get("error").and_then(|e| e.as_str()).map(String::from))
            .unwrap_or_else(|| body.trim().to_string());
        bail!("{host} answered {code}: {why}");
    }
    Ok(body)
}

/// Stream the body of `GET path` into a local file.
pub fn get_to_file(api: &str, path: &str, dest: &std::path::Path) -> Result<()> {
    let (mut s, host) = connect(api)?;
    write!(s, "GET {path} HTTP/1.0\r\nHost: {host}\r\n\r\n")?;
    let (code, first) = read_head(&mut s)?;
    let mut out = std::fs::File::create(dest)?;
    out.write_all(&first)?;
    std::io::copy(&mut s, &mut out)?;
    if code != "200" {
        let msg = std::fs::read_to_string(dest).unwrap_or_default();
        bail!("{host} answered {code}: {}", msg.trim());
    }
    Ok(())
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

/// [`merge_into`] for a one-shot (a keypress handler): no poller has run in this process, so
/// ask each remote once, now. Bounded by the fetch timeouts (300ms connect, 800ms read) and
/// run in parallel, so a remote that is down costs one connect timeout, not one per remote.
pub fn merge_now(reg: &mut crate::common::registry::ConversationRegistry) {
    let handles: Vec<_> = HiveConfig::load()
        .remotes
        .into_iter()
        .filter_map(|r| r.api.map(|api| (r.name, api)))
        .map(|(name, api)| std::thread::spawn(move || (name, fetch_registry(&api))))
        .collect();
    let hosts = crate::common::projects::project_hosts();
    let mut up = std::collections::HashMap::new();
    for h in handles {
        if let Ok((name, fetched)) = h.join() {
            up.insert(name.clone(), fetched.is_ok());
            if let Ok(conversations) = fetched {
                reg.merge_remote(&name, conversations, &hosts);
            }
        }
    }
    reg.settle_homes(&up);
}

/// Fold every reachable remote into `reg`, then give each conversation that was shipped
/// away its home row (`settle_homes`). A down remote contributes no rows of its own, but
/// the conversations this machine knows it holds stay listed, marked unreachable.
pub fn merge_into(reg: &mut crate::common::registry::ConversationRegistry) {
    let hosts = crate::common::projects::project_hosts();
    let mut up = std::collections::HashMap::new();
    for s in snapshots() {
        up.insert(s.name.clone(), s.up == Some(true));
        if s.up == Some(true) {
            reg.merge_remote(&s.name, s.conversations, &hosts);
        }
    }
    reg.settle_homes(&up);
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
