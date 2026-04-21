//! HTTP web server for mobile dashboard.
//!
//! Serves an embedded single-page app and JSON API for monitoring
//! and interacting with Claude sessions from a mobile browser.

use crate::common::jsonl::get_conversation_messages;
use crate::common::persistence::{
    load_auto_approve_sessions, load_completed_todos, load_favorite_sessions,
    load_session_todos, load_skipped_sessions, save_auto_approve_sessions,
    save_completed_todos, save_favorite_sessions, save_session_todos, save_skipped_sessions,
};
use crate::common::projects::{connect_project, ProjectRegistry};
use crate::common::tmux::{get_current_tmux_session_names, kill_tmux_session, send_text_to_pane};
use crate::ipc::messages::HookState;
use crate::serve::web_types::{ConversationMessage, ToolSummary};
use crate::serve::server::gather_session_data;

use anyhow::Result;
use serde::Deserialize;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use sysinfo::System;
use tiny_http::{Header, Method, Response, Server};

/// Embedded HTML frontend, included at compile time.
const WEB_HTML: &str = include_str!("web.html");

/// In dev mode, re-read web.html from disk on every request.
/// Falls back to the embedded version if the file isn't found.
fn get_html(dev_path: &Option<PathBuf>) -> String {
    if let Some(path) = dev_path {
        match std::fs::read_to_string(path) {
            Ok(contents) => return contents,
            Err(e) => {
                eprintln!("  warn: failed to read {}: {}, using embedded", path.display(), e);
            }
        }
    }
    WEB_HTML.to_string()
}

#[derive(Debug, Deserialize)]
struct SendRequest {
    session: String,
    text: String,
}

/// Run the HTTP web server on the given port.
/// When `dev` is true, serves web.html from disk (re-read per request) for live editing.
/// When `tts_host` is set, enables TTS proxy endpoints.
pub fn run_web_server(port: u16, dev: bool, tts_host: Option<String>) -> Result<()> {
    let bind_addr = format!("0.0.0.0:{}", port);
    let server =
        Server::http(&bind_addr).map_err(|e| anyhow::anyhow!("Failed to bind {}: {}", bind_addr, e))?;

    let dev_path = if dev {
        let path = std::env::current_dir()
            .unwrap_or_default()
            .join("src/serve/web.html");
        if path.exists() {
            eprintln!("Hive web dashboard running at http://0.0.0.0:{} (dev mode)", port);
            eprintln!("  Serving HTML from: {}", path.display());
            Some(path)
        } else {
            eprintln!("Hive web dashboard running at http://0.0.0.0:{}", port);
            eprintln!("  warn: --dev but {} not found, using embedded HTML", path.display());
            None
        }
    } else {
        eprintln!("Hive web dashboard running at http://0.0.0.0:{}", port);
        None
    };

    // Print access URLs
    if let Some(hostname) = get_local_hostname() {
        eprintln!("  Local: http://{}:{}", hostname, port);
    }
    for (label, ip) in get_all_ips(port) {
        eprintln!("  {}: http://{}:{}", label, ip, port);
    }

    // Shared session data between the data thread and request handlers
    let shared_data = Arc::new(Mutex::new(Vec::new()));
    let data_for_thread = Arc::clone(&shared_data);

    // Background data refresh thread (1s interval, same as TUI)
    std::thread::spawn(move || {
        let mut sys = System::new_all();
        sys.refresh_all();

        loop {
            sys.refresh_all();
            let hook_state = HookState::load();
            let sessions = gather_session_data(&sys, &hook_state);

            if let Ok(mut data) = data_for_thread.lock() {
                *data = sessions;
            }

            std::thread::sleep(Duration::from_secs(1));
        }
    });

    // Handle HTTP requests (blocking)
    for mut request in server.incoming_requests() {
        let url = request.url().to_string();
        let method = request.method().clone();

        match (method, url.as_str()) {
            (Method::Get, "/") => {
                let html = get_html(&dev_path);
                let header =
                    Header::from_bytes("Content-Type", "text/html; charset=utf-8").unwrap();
                let response = Response::from_string(html).with_header(header);
                let _ = request.respond(response);
            }

            (Method::Get, "/api/sessions") => {
                let json = if let Ok(data) = shared_data.lock() {
                    serde_json::to_string(&*data).unwrap_or_else(|_| "[]".to_string())
                } else {
                    "[]".to_string()
                };

                let header =
                    Header::from_bytes("Content-Type", "application/json").unwrap();
                let response = Response::from_string(json).with_header(header);
                let _ = request.respond(response);
            }

            (Method::Get, url) if url.starts_with("/api/messages?") => {
                // Parse ?session=... from query string
                let session_name = url
                    .split('?')
                    .nth(1)
                    .and_then(|qs| {
                        qs.split('&').find_map(|param| {
                            let (k, v) = param.split_once('=')?;
                            if k == "session" {
                                Some(urldecode(v))
                            } else {
                                None
                            }
                        })
                    });

                let json = if let Some(name) = session_name {
                    // Find the session's CWD from cached data
                    let cwd = shared_data
                        .lock()
                        .ok()
                        .and_then(|data| {
                            data.iter()
                                .find(|s| s.name == name)
                                .and_then(|s| s.cwd.clone())
                        });

                    if let Some(cwd) = cwd {
                        let messages: Vec<ConversationMessage> =
                            get_conversation_messages(&cwd)
                                .into_iter()
                                .map(|m| ConversationMessage {
                                    role: m.role,
                                    text: m.text,
                                    tools: m
                                        .tools
                                        .into_iter()
                                        .map(|t| ToolSummary {
                                            name: t.name,
                                            summary: t.summary,
                                            detail: t.detail,
                                        })
                                        .collect(),
                                })
                                .collect();
                        serde_json::to_string(&messages).unwrap_or_else(|_| "[]".to_string())
                    } else {
                        "[]".to_string()
                    }
                } else {
                    "[]".to_string()
                };

                let header =
                    Header::from_bytes("Content-Type", "application/json").unwrap();
                let response = Response::from_string(json).with_header(header);
                let _ = request.respond(response);
            }

            (Method::Get, "/api/config") => {
                let json = format!(r#"{{"tts":{}}}"#, tts_host.is_some());
                let header =
                    Header::from_bytes("Content-Type", "application/json").unwrap();
                let response = Response::from_string(json).with_header(header);
                let _ = request.respond(response);
            }

            (Method::Post, "/api/tts-hls") => {
                if let Some(ref host) = tts_host {
                    let mut body = String::new();
                    let _ = request.as_reader().read_to_string(&mut body);

                    let text_len = serde_json::from_str::<serde_json::Value>(&body)
                        .ok()
                        .and_then(|v| v.get("text")?.as_str().map(|s| s.len()))
                        .unwrap_or(0);

                    let speak_url = format!("{}/speak/hls", host);
                    eprintln!("  TTS HLS: requesting | {} chars input", text_len);
                    let start = std::time::Instant::now();

                    let output = std::process::Command::new("curl")
                        .args([
                            "-s",
                            "--max-time", "30",
                            "-X", "POST",
                            &speak_url,
                            "-H", "Content-Type: application/json",
                            "-d", &body,
                        ])
                        .output();

                    match output {
                        Ok(out) if out.status.success() => {
                            let result: serde_json::Value =
                                serde_json::from_slice(&out.stdout).unwrap_or_default();
                            eprintln!(
                                "  TTS HLS: session created in {}ms",
                                start.elapsed().as_millis()
                            );

                            // Wait for first segment before returning to browser
                            // Safari fails if playlist has no segments when first loaded
                            if let Some(playlist_path) = result.get("playlist_url").and_then(|v| v.as_str()) {
                                let playlist_url = format!("{}{}", host, playlist_path);
                                for _ in 0..30 {
                                    std::thread::sleep(Duration::from_millis(500));
                                    if let Ok(check) = std::process::Command::new("curl")
                                        .args(["-s", "--max-time", "2", &playlist_url])
                                        .output()
                                    {
                                        let body = String::from_utf8_lossy(&check.stdout);
                                        if body.contains("EXTINF") {
                                            eprintln!(
                                                "  TTS HLS: first segment ready at {}ms",
                                                start.elapsed().as_millis()
                                            );
                                            break;
                                        }
                                    }
                                }
                            }
                            let header =
                                Header::from_bytes("Content-Type", "application/json").unwrap();
                            let _ = request.respond(
                                Response::from_string(result.to_string()).with_header(header),
                            );
                        }
                        Ok(out) => {
                            let err = String::from_utf8_lossy(&out.stderr);
                            let header =
                                Header::from_bytes("Content-Type", "application/json").unwrap();
                            let _ = request.respond(
                                Response::from_string(format!(r#"{{"error":"{}"}}"#, err.replace('"', "'")))
                                    .with_header(header)
                                    .with_status_code(502),
                            );
                        }
                        Err(e) => {
                            let header =
                                Header::from_bytes("Content-Type", "application/json").unwrap();
                            let _ = request.respond(
                                Response::from_string(format!(r#"{{"error":"{}"}}"#, e))
                                    .with_header(header)
                                    .with_status_code(500),
                            );
                        }
                    }
                } else {
                    let header =
                        Header::from_bytes("Content-Type", "application/json").unwrap();
                    let _ = request.respond(
                        Response::from_string(r#"{"error":"TTS not configured"}"#)
                            .with_header(header)
                            .with_status_code(404),
                    );
                }
            }

            (Method::Post, "/api/send") => {
                let mut body = String::new();
                let _ = request.as_reader().read_to_string(&mut body);

                let result: Result<(), String> = (|| {
                    let req: SendRequest = serde_json::from_str(&body)
                        .map_err(|e| format!("Invalid JSON: {}", e))?;

                    // Find the session's pane info from cached data
                    let pane = shared_data
                        .lock()
                        .ok()
                        .and_then(|data| {
                            data.iter()
                                .find(|s| s.name == req.session)
                                .and_then(|s| s.pane.clone())
                        })
                        .ok_or_else(|| "Session not found or has no Claude pane".to_string())?;

                    send_text_to_pane(&pane.0, &pane.1, &pane.2, &req.text);
                    Ok(())
                })();

                let (status, body) = match result {
                    Ok(()) => (200, r#"{"ok":true}"#.to_string()),
                    Err(msg) => (400, format!(r#"{{"error":"{}"}}"#, msg)),
                };

                let header =
                    Header::from_bytes("Content-Type", "application/json").unwrap();
                let response = Response::from_string(body)
                    .with_header(header)
                    .with_status_code(status);
                let _ = request.respond(response);
            }

            (Method::Post, "/api/toggle-flag") => {
                let mut body = String::new();
                let _ = request.as_reader().read_to_string(&mut body);
                let json = (|| -> Option<String> {
                    let req: serde_json::Value = serde_json::from_str(&body).ok()?;
                    let session = req.get("session")?.as_str()?;
                    let flag = req.get("flag")?.as_str()?;
                    let value = match flag {
                        "favorite" => {
                            let mut set = load_favorite_sessions();
                            let v = if set.contains(session) { set.remove(session); false } else { set.insert(session.to_string()); true };
                            save_favorite_sessions(&set);
                            v
                        }
                        "auto_approve" => {
                            let mut set = load_auto_approve_sessions();
                            let v = if set.contains(session) { set.remove(session); false } else { set.insert(session.to_string()); true };
                            save_auto_approve_sessions(&set);
                            v
                        }
                        "skip" => {
                            let mut set = load_skipped_sessions();
                            let v = if set.contains(session) { set.remove(session); false } else { set.insert(session.to_string()); true };
                            save_skipped_sessions(&set);
                            v
                        }
                        _ => return Some(r#"{"error":"unknown flag"}"#.to_string()),
                    };
                    Some(format!(r#"{{"ok":true,"value":{}}}"#, value))
                })()
                .unwrap_or_else(|| r#"{"error":"invalid request"}"#.to_string());
                let header = Header::from_bytes("Content-Type", "application/json").unwrap();
                let _ = request.respond(Response::from_string(json).with_header(header));
            }

            (Method::Get, "/api/projects") => {
                let registry = ProjectRegistry::load();
                let existing = get_current_tmux_session_names();
                let mut projects: Vec<serde_json::Value> = registry
                    .projects
                    .iter()
                    .map(|(key, config)| {
                        let session_name = ProjectRegistry::session_name(key, config);
                        serde_json::json!({
                            "key": key,
                            "emoji": config.emoji,
                            "display_name": config.display_name.as_deref().unwrap_or(key),
                            "session_name": session_name,
                            "exists": existing.contains(&session_name),
                        })
                    })
                    .collect();
                projects.sort_by(|a, b| {
                    let a_exists = a["exists"].as_bool().unwrap_or(false);
                    let b_exists = b["exists"].as_bool().unwrap_or(false);
                    b_exists.cmp(&a_exists)
                        .then_with(|| a["key"].as_str().cmp(&b["key"].as_str()))
                });
                let json = serde_json::to_string(&projects).unwrap_or_else(|_| "[]".to_string());
                let header = Header::from_bytes("Content-Type", "application/json").unwrap();
                let _ = request.respond(Response::from_string(json).with_header(header));
            }

            (Method::Post, "/api/connect") => {
                let mut body = String::new();
                let _ = request.as_reader().read_to_string(&mut body);
                let json = (|| -> Option<String> {
                    let req: serde_json::Value = serde_json::from_str(&body).ok()?;
                    let session_name = req.get("session_name")?.as_str()?;
                    let ok = connect_project(session_name);
                    Some(format!(r#"{{"ok":{}}}"#, ok))
                })()
                .unwrap_or_else(|| r#"{"error":"invalid request"}"#.to_string());
                let header = Header::from_bytes("Content-Type", "application/json").unwrap();
                let _ = request.respond(Response::from_string(json).with_header(header));
            }

            (Method::Get, url) if url.starts_with("/api/session-info?") => {
                let session_name = url
                    .split('?')
                    .nth(1)
                    .and_then(|qs| {
                        qs.split('&').find_map(|param| {
                            let (k, v) = param.split_once('=')?;
                            if k == "session" { Some(urldecode(v)) } else { None }
                        })
                    });

                let json = if let Some(name) = session_name {
                    let session_data = shared_data.lock().ok().and_then(|data| {
                        data.iter().find(|s| s.name == name).cloned()
                    });

                    if let Some(s) = session_data {
                        let favorites = load_favorite_sessions();
                        let auto_approve = load_auto_approve_sessions();
                        let skipped = load_skipped_sessions();
                        let todos = load_session_todos();
                        let todos_done = load_completed_todos();

                        let active_todos: Vec<&str> = todos
                            .get(&name)
                            .map(|v| v.iter().map(|s| s.as_str()).collect())
                            .unwrap_or_default();
                        let done_todos: Vec<&str> = todos_done
                            .get(&name)
                            .map(|v| v.iter().map(|s| s.as_str()).collect())
                            .unwrap_or_default();

                        let processes: Vec<serde_json::Value> = s
                            .processes
                            .iter()
                            .map(|p| {
                                serde_json::json!({
                                    "name": p.name,
                                    "cpu_percent": p.cpu_percent,
                                    "memory_kb": p.memory_kb,
                                    "command": p.command,
                                })
                            })
                            .collect();

                        serde_json::json!({
                            "name": s.name,
                            "cwd": s.cwd,
                            "ports": s.ports,
                            "processes": processes,
                            "cpu": s.cpu,
                            "mem_kb": s.mem_kb,
                            "favorite": favorites.contains(&name),
                            "auto_approve": auto_approve.contains(&name),
                            "skipped": skipped.contains(&name),
                            "todos": active_todos,
                            "todos_done": done_todos,
                        })
                        .to_string()
                    } else {
                        r#"{"error":"session not found"}"#.to_string()
                    }
                } else {
                    r#"{"error":"missing session param"}"#.to_string()
                };

                let header = Header::from_bytes("Content-Type", "application/json").unwrap();
                let _ = request.respond(Response::from_string(json).with_header(header));
            }

            (Method::Post, "/api/todos") => {
                let mut body = String::new();
                let _ = request.as_reader().read_to_string(&mut body);
                let json = (|| -> Option<String> {
                    let req: serde_json::Value = serde_json::from_str(&body).ok()?;
                    let session = req.get("session")?.as_str()?.to_string();
                    let action = req.get("action")?.as_str()?;
                    let mut todos = load_session_todos();
                    let mut todos_done = load_completed_todos();

                    match action {
                        "add" => {
                            let text = req.get("text")?.as_str()?.to_string();
                            todos.entry(session.clone()).or_default().push(text);
                            save_session_todos(&todos);
                        }
                        "done" => {
                            let index = req.get("index")?.as_u64()? as usize;
                            let list = todos.get_mut(&session)?;
                            if index < list.len() {
                                let item = list.remove(index);
                                todos_done.entry(session.clone()).or_default().push(item);
                                save_session_todos(&todos);
                                save_completed_todos(&todos_done);
                            }
                        }
                        "delete" => {
                            let index = req.get("index")?.as_u64()? as usize;
                            let list = todos.get_mut(&session)?;
                            if index < list.len() {
                                list.remove(index);
                                save_session_todos(&todos);
                            }
                        }
                        _ => return Some(r#"{"error":"unknown action"}"#.to_string()),
                    }

                    let current: Vec<&str> = todos
                        .get(&session)
                        .map(|v| v.iter().map(|s| s.as_str()).collect())
                        .unwrap_or_default();
                    Some(serde_json::json!({"ok": true, "todos": current}).to_string())
                })()
                .unwrap_or_else(|| r#"{"error":"invalid request"}"#.to_string());
                let header = Header::from_bytes("Content-Type", "application/json").unwrap();
                let _ = request.respond(Response::from_string(json).with_header(header));
            }

            (Method::Post, "/api/tts-cancel") => {
                if let Some(ref host) = tts_host {
                    let mut body = String::new();
                    let _ = request.as_reader().read_to_string(&mut body);
                    let json = (|| -> Option<String> {
                        let req: serde_json::Value = serde_json::from_str(&body).ok()?;
                        let sid = req.get("session_id")?.as_str()?;
                        let url = format!("{}/hls/{}", host, sid);
                        eprintln!("  TTS cancel: {}", sid);
                        let _ = std::process::Command::new("curl")
                            .args(["-s", "-X", "DELETE", &url])
                            .output();
                        Some(r#"{"ok":true}"#.to_string())
                    })()
                    .unwrap_or_else(|| r#"{"ok":true}"#.to_string());
                    let header = Header::from_bytes("Content-Type", "application/json").unwrap();
                    let _ = request.respond(Response::from_string(json).with_header(header));
                } else {
                    let header = Header::from_bytes("Content-Type", "application/json").unwrap();
                    let _ = request.respond(Response::from_string(r#"{"ok":true}"#).with_header(header));
                }
            }

            (Method::Post, "/api/kill-session") => {
                let mut body = String::new();
                let _ = request.as_reader().read_to_string(&mut body);
                let json = (|| -> Option<String> {
                    let req: serde_json::Value = serde_json::from_str(&body).ok()?;
                    let session = req.get("session")?.as_str()?;
                    let ok = kill_tmux_session(session);
                    Some(format!(r#"{{"ok":{}}}"#, ok))
                })()
                .unwrap_or_else(|| r#"{"error":"invalid request"}"#.to_string());
                let header = Header::from_bytes("Content-Type", "application/json").unwrap();
                let _ = request.respond(Response::from_string(json).with_header(header));
            }

            (Method::Get, url) if url.starts_with("/hls/") => {
                // Proxy HLS playlist and segments from TTS server (same-origin for iOS Safari)
                if let Some(ref host) = tts_host {
                    let tts_url = format!("{}{}", host, url);
                    eprintln!("  HLS proxy: {} -> {}", url, tts_url);
                    let content_type = if url.ends_with(".m3u8") {
                        "application/vnd.apple.mpegurl"
                    } else if url.ends_with(".m4s") || url.ends_with(".mp4") {
                        "audio/mp4"
                    } else {
                        "video/MP2T"
                    };

                    match std::process::Command::new("curl")
                        .args(["-s", "--max-time", "10", &tts_url])
                        .output()
                    {
                        Ok(out) if out.status.success() => {
                            let body = &out.stdout;
                            let is_error = body.first() == Some(&b'{');
                            if is_error || body.is_empty() {
                                let preview = String::from_utf8_lossy(&body[..body.len().min(100)]);
                                eprintln!("  HLS proxy: {} -> 404 ({}B: {})", url, body.len(), preview);
                                let response =
                                    Response::from_string("Not Found").with_status_code(404);
                                let _ = request.respond(response);
                            } else {
                                eprintln!("  HLS proxy: {} -> 200 ({} bytes, {})", url, body.len(), content_type);
                                let header =
                                    Header::from_bytes("Content-Type", content_type).unwrap();
                                let response =
                                    Response::from_data(body.to_vec()).with_header(header);
                                let _ = request.respond(response);
                            }
                        }
                        _ => {
                            let response =
                                Response::from_string("Not Found").with_status_code(404);
                            let _ = request.respond(response);
                        }
                    }
                } else {
                    let response = Response::from_string("Not Found").with_status_code(404);
                    let _ = request.respond(response);
                }
            }

            _ => {
                let response = Response::from_string("Not Found").with_status_code(404);
                let _ = request.respond(response);
            }
        }
    }

    Ok(())
}

/// Percent-decoding for URL query parameters (handles multi-byte UTF-8).
fn urldecode(s: &str) -> String {
    let mut bytes = Vec::with_capacity(s.len());
    let mut chars = s.bytes();
    while let Some(b) = chars.next() {
        if b == b'%' {
            let hi = chars.next().unwrap_or(b'0');
            let lo = chars.next().unwrap_or(b'0');
            let hex = [hi, lo];
            if let Ok(s) = std::str::from_utf8(&hex) {
                if let Ok(val) = u8::from_str_radix(s, 16) {
                    bytes.push(val);
                    continue;
                }
            }
            bytes.push(b'%');
            bytes.push(hi);
            bytes.push(lo);
        } else if b == b'+' {
            bytes.push(b' ');
        } else {
            bytes.push(b);
        }
    }
    String::from_utf8(bytes).unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).to_string())
}

/// Get the machine's .local hostname (Bonjour/mDNS).
fn get_local_hostname() -> Option<String> {
    let output = std::process::Command::new("hostname")
        .output()
        .ok()?;
    let name = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if name.is_empty() {
        None
    } else if name.ends_with(".local") {
        Some(name)
    } else {
        Some(format!("{}.local", name))
    }
}

/// Try to get the machine's local network IP address.
/// Get all non-loopback IPv4 addresses with interface labels.
fn get_all_ips(_port: u16) -> Vec<(String, String)> {
    let output = match std::process::Command::new("ifconfig").output() {
        Ok(o) if o.status.success() => o,
        _ => return Vec::new(),
    };

    let text = String::from_utf8_lossy(&output.stdout);
    let mut results = Vec::new();
    let mut current_iface = String::new();

    for line in text.lines() {
        if !line.starts_with('\t') && !line.starts_with(' ') && line.contains(':') {
            current_iface = line.split(':').next().unwrap_or("").to_string();
        }
        if let Some(rest) = line.trim().strip_prefix("inet ") {
            let ip = rest.split_whitespace().next().unwrap_or("").to_string();
            if ip == "127.0.0.1" || ip.is_empty() {
                continue;
            }
            let label = if current_iface.starts_with("en") {
                "LAN".to_string()
            } else if current_iface.starts_with("utun") || current_iface.starts_with("tun") {
                "VPN".to_string()
            } else {
                current_iface.clone()
            };
            results.push((label, ip));
        }
    }

    results
}
