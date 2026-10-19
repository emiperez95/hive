//! The sidebar's only data source: the local web dashboard's JSON API.
//!
//! Every request runs on a short-lived thread and posts its answer back as a
//! [`Msg`], so a slow endpoint (the branch namer takes seconds) never stalls a
//! keystroke. Two loops run for the sidebar's lifetime: the `/api/active` poll
//! and the `/api/ambient-wait` long-poll that pushes focus changes.

use serde::Deserialize;
use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpStream};
use std::sync::mpsc::Sender;
use std::time::Duration;

use crate::serve::web_types::SessionView;

/// Where the tmux client is, plus the mute flags, from `/api/ambient`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Focus {
    pub session: Option<String>,
    pub window_index: Option<String>,
    pub global_mute: bool,
    pub muted_projects: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct FrozenItem {
    pub key: String,
    pub session_name: String,
    #[serde(default)]
    pub window_label: String,
    #[serde(default)]
    pub note: String,
    #[serde(default)]
    pub relative: String,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct ProjectItem {
    pub key: String,
    #[serde(default)]
    pub emoji: String,
    pub display_name: String,
    pub session_name: String,
    #[serde(default)]
    pub archived: bool,
}

#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
pub struct Tokens {
    #[serde(default)]
    pub input: u64,
    #[serde(default)]
    pub output: u64,
    #[serde(default)]
    pub cache_write: u64,
    #[serde(default)]
    pub cache_read: u64,
}

impl Tokens {
    pub fn sum(&self) -> u64 {
        self.input + self.output + self.cache_write + self.cache_read
    }
}

#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
pub struct Usage {
    #[serde(default)]
    pub main: BTreeMap<String, Tokens>,
    #[serde(default)]
    pub sidechain: BTreeMap<String, Tokens>,
}

#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
pub struct Health {
    #[serde(default)]
    pub branch: String,
    #[serde(default)]
    pub detached: bool,
    pub base: Option<String>,
    #[serde(default)]
    pub dirty: usize,
    #[serde(default)]
    pub untracked: usize,
    #[serde(default)]
    pub ahead: usize,
    #[serde(default)]
    pub behind: usize,
    #[serde(default)]
    pub merged: bool,
    pub last_commit_unix: Option<i64>,
}

/// `/api/conv-stats` — spend for one conversation, plus its tree's git state.
#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
pub struct Stats {
    #[serde(default)]
    pub id: String,
    pub error: Option<String>,
    #[serde(default)]
    pub usage: Usage,
    #[serde(default)]
    pub totals: Tokens,
    #[serde(default)]
    pub cost_main: BTreeMap<String, f64>,
    #[serde(default)]
    pub cost_sidechain: BTreeMap<String, f64>,
    pub cost_usd: Option<f64>,
    #[serde(default)]
    pub unpriced_models: Vec<String>,
    pub worktree: Option<Health>,
}

#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
pub struct Todos {
    #[serde(default)]
    pub todos: Vec<String>,
    #[serde(default)]
    pub done: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct Job {
    pub state: String,
    #[serde(default)]
    pub message: String,
    pub session: Option<String>,
}

/// Everything a background request can report back.
pub enum Msg {
    Active(Result<Vec<SessionView>, String>),
    Ambient(Focus),
    Frozen(Vec<FrozenItem>),
    Projects(Vec<ProjectItem>),
    Stats(String, Box<Stats>),
    Notes(String, String),
    NotesSaved(String, bool),
    Todos(String, Todos),
    GlobalMute(bool),
    ProjectMute(String, bool),
    /// Something changed server-side (thaw, discard): refetch the lists now.
    Refresh(Option<String>),
    Target(String, Option<String>),
    Branch(Result<(String, String), String>),
    Dispatched(Result<Option<u64>, String>),
    Job(Job),
}

#[derive(Clone)]
pub struct Api {
    port: u16,
    tx: Sender<Msg>,
}

impl Api {
    pub fn new(port: u16, tx: Sender<Msg>) -> Self {
        Self { port, tx }
    }

    /// Run `f` on its own thread and send whatever it returns.
    fn spawn(&self, f: impl FnOnce(u16) -> Option<Msg> + Send + 'static) {
        let (port, tx) = (self.port, self.tx.clone());
        std::thread::spawn(move || {
            if let Some(m) = f(port) {
                let _ = tx.send(m);
            }
        });
    }

    /// The two loops that run for the sidebar's lifetime.
    pub fn start_loops(&self) {
        let (port, tx) = (self.port, self.tx.clone());
        std::thread::spawn(move || loop {
            let r = get(port, "/api/active", 3)
                .ok_or_else(|| format!("no hive web on :{port}"))
                .and_then(|b| serde_json::from_str(&b).map_err(|e| e.to_string()));
            if tx.send(Msg::Active(r)).is_err() {
                return;
            }
            std::thread::sleep(Duration::from_millis(1500));
        });

        // Focus: held open until the client moves (≤25s), answered at once the
        // first time. A failure backs off rather than spinning.
        let (port, tx) = (self.port, self.tx.clone());
        std::thread::spawn(move || {
            let mut gen: Option<u64> = None;
            loop {
                let path = match gen {
                    Some(g) => format!("/api/ambient-wait?gen={g}"),
                    None => "/api/ambient".to_string(),
                };
                let Some(v) = get(port, &path, 30).and_then(|b| json(&b)) else {
                    std::thread::sleep(Duration::from_secs(2));
                    continue;
                };
                gen = v["gen"].as_u64();
                if tx.send(Msg::Ambient(parse_focus(&v))).is_err() {
                    return;
                }
            }
        });
    }

    pub fn fetch_frozen(&self) {
        self.spawn(|p| {
            Some(Msg::Frozen(
                serde_json::from_str(&get(p, "/api/frozen", 5)?).ok()?,
            ))
        });
    }

    pub fn fetch_projects(&self) {
        self.spawn(|p| {
            Some(Msg::Projects(
                serde_json::from_str(&get(p, "/api/projects", 5)?).ok()?,
            ))
        });
    }

    pub fn fetch_stats(&self, id: String) {
        self.spawn(move |p| {
            let body = get(p, &format!("/api/conv-stats?id={}", enc(&id)), 15)?;
            Some(Msg::Stats(id, Box::new(serde_json::from_str(&body).ok()?)))
        });
    }

    pub fn fetch_notes(&self, id: String) {
        self.spawn(move |p| {
            let v = json(&get(p, &format!("/api/conv-notes?id={}", enc(&id)), 5)?)?;
            let notes = v["notes"].as_str().unwrap_or_default().to_string();
            Some(Msg::Notes(id, notes))
        });
    }

    pub fn save_notes(&self, id: String, notes: String) {
        self.spawn(move |p| {
            let body = serde_json::json!({ "id": id, "notes": notes });
            let ok = post(p, "/api/conv-notes", &body)
                .and_then(|b| json(&b))
                .is_some_and(|v| v["ok"].as_bool() == Some(true));
            Some(Msg::NotesSaved(id, ok))
        });
    }

    pub fn fetch_todos(&self, session: String) {
        self.spawn(move |p| {
            let body = get(p, &format!("/api/todos?session={}", enc(&session)), 5)?;
            Some(Msg::Todos(session, serde_json::from_str(&body).ok()?))
        });
    }

    /// `action` is add | done | delete | reopen; `extra` carries text / index.
    pub fn todo_action(&self, session: String, action: &'static str, extra: serde_json::Value) {
        self.spawn(move |p| {
            let mut body = serde_json::json!({ "session": session, "action": action });
            if let (Some(b), Some(e)) = (body.as_object_mut(), extra.as_object()) {
                b.extend(e.clone());
            }
            let r = post(p, "/api/todos", &body)?;
            Some(Msg::Todos(session, serde_json::from_str(&r).ok()?))
        });
    }

    pub fn toggle_mute(&self) {
        self.spawn(|p| {
            let v = json(&post(p, "/api/toggle-mute", &serde_json::json!({}))?)?;
            Some(Msg::GlobalMute(v["global_mute"].as_bool()?))
        });
    }

    pub fn toggle_project_mute(&self, key: String) {
        self.spawn(move |p| {
            let body = serde_json::json!({ "key": key });
            let v = json(&post(p, "/api/toggle-project-mute", &body)?)?;
            Some(Msg::ProjectMute(key, v["muted"].as_bool()?))
        });
    }

    /// Reports the error text, if any, through `Refresh`.
    pub fn thaw(&self, key: String) {
        self.spawn(move |p| {
            let v = post(p, "/api/thaw", &serde_json::json!({ "key": key })).and_then(|b| json(&b));
            let err = match v {
                Some(v) => v["error"].as_str().map(str::to_string),
                None => Some("couldn't reach hive".into()),
            };
            Some(Msg::Refresh(err))
        });
    }

    pub fn fetch_target(&self, session: String) {
        self.spawn(move |p| {
            let v = json(&get(
                p,
                &format!("/api/todo-target?session={}", enc(&session)),
                5,
            )?);
            let project = v.and_then(|v| v["project"].as_str().map(str::to_string));
            Some(Msg::Target(session, project))
        });
    }

    pub fn suggest_branch(&self, session: String, text: String) {
        self.spawn(move |p| {
            let path = format!(
                "/api/todo-branch?session={}&text={}",
                enc(&session),
                enc(&text)
            );
            let r = match get(p, &path, 30).and_then(|b| json(&b)) {
                None => Err("couldn't reach hive".to_string()),
                Some(v) => match v["error"].as_str() {
                    Some(e) => Err(e.to_string()),
                    None => Ok((
                        v["branch"].as_str().unwrap_or_default().to_string(),
                        v["source"].as_str().unwrap_or_default().to_string(),
                    )),
                },
            };
            Some(Msg::Branch(r))
        });
    }

    /// `mode` is send | new | worktree. `Ok(Some(job))` for a worktree.
    pub fn dispatch(&self, body: serde_json::Value) {
        self.spawn(move |p| {
            let r = match post(p, "/api/todo-dispatch", &body).and_then(|b| json(&b)) {
                None => Err("couldn't reach hive".to_string()),
                Some(v) => match v["error"].as_str() {
                    Some(e) => Err(e.to_string()),
                    None => Ok(v["job"].as_u64()),
                },
            };
            Some(Msg::Dispatched(r))
        });
    }

    pub fn fetch_job(&self, id: u64) {
        self.spawn(move |p| {
            Some(Msg::Job(
                serde_json::from_str(&get(p, &format!("/api/job?id={id}"), 5)?).ok()?,
            ))
        });
    }
}

fn parse_focus(v: &serde_json::Value) -> Focus {
    Focus {
        session: v["session"].as_str().map(str::to_string),
        window_index: v["window_index"].as_str().map(str::to_string),
        global_mute: v["global_mute"].as_bool().unwrap_or(false),
        muted_projects: v["muted_projects"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|x| x.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default(),
    }
}

fn json(body: &str) -> Option<serde_json::Value> {
    serde_json::from_str(body).ok()
}

/// Percent-encode a query value. Session names carry spaces, emoji and
/// `[project]` tags.
pub fn enc(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn get(port: u16, path: &str, timeout_secs: u64) -> Option<String> {
    request(
        port,
        &format!("GET {path} HTTP/1.0\r\nHost: localhost\r\n\r\n"),
        timeout_secs,
    )
}

fn post(port: u16, path: &str, body: &serde_json::Value) -> Option<String> {
    let body = body.to_string();
    request(
        port,
        &format!(
            "POST {path} HTTP/1.0\r\nHost: localhost\r\nContent-Type: application/json\r\n\
             Content-Length: {}\r\n\r\n{body}",
            body.len()
        ),
        10,
    )
}

/// Minimal HTTP/1.0 against the local dashboard: 1.0 so the response is never
/// chunked and the server closes the connection when it's done.
fn request(port: u16, req: &str, timeout_secs: u64) -> Option<String> {
    let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
    let mut stream = TcpStream::connect_timeout(&addr, Duration::from_millis(300)).ok()?;
    stream
        .set_read_timeout(Some(Duration::from_secs(timeout_secs)))
        .ok()?;
    stream.write_all(req.as_bytes()).ok()?;
    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).ok()?;
    let text = String::from_utf8_lossy(&buf);
    let (head, body) = text.split_once("\r\n\r\n")?;
    (head.split_whitespace().nth(1) == Some("200")).then(|| body.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enc_escapes_everything_but_unreserved() {
        assert_eq!(enc("a-b_c.d~e"), "a-b_c.d~e");
        assert_eq!(enc("🐝 [hive] x&y"), "%F0%9F%90%9D%20%5Bhive%5D%20x%26y");
    }

    #[test]
    fn focus_parses_mute_flags() {
        let f = parse_focus(&serde_json::json!({
            "session": "s", "window_index": "2", "global_mute": true,
            "muted_projects": ["a", "b"], "gen": 3,
        }));
        assert_eq!(f.session.as_deref(), Some("s"));
        assert_eq!(f.window_index.as_deref(), Some("2"));
        assert!(f.global_mute);
        assert_eq!(f.muted_projects, ["a", "b"]);
    }

    // The shape `/api/conv-stats` actually sends, abbreviated.
    #[test]
    fn stats_parse_from_the_wire_shape() {
        let s: Stats = serde_json::from_value(serde_json::json!({
            "id": "x",
            "usage": { "main": { "claude-opus-5": { "input": 1, "output": 2,
                        "cache_write": 3, "cache_read": 4, "thinking": 1 } },
                       "sidechain": {} },
            "totals": { "input": 1, "output": 2, "cache_write": 3, "cache_read": 4 },
            "cost_main": { "claude-opus-5": 0.5 }, "cost_sidechain": {},
            "cost_usd": 0.5, "unpriced_models": [],
            "worktree": { "root": "/r", "branch": "main", "detached": false, "base": "origin/main",
                          "dirty": 1, "untracked": 0, "ahead": 2, "behind": 0,
                          "merged": false, "last_commit_unix": 1 },
        }))
        .unwrap();
        assert_eq!(s.usage.main["claude-opus-5"].sum(), 10);
        assert_eq!(s.worktree.unwrap().ahead, 2);
    }
}
