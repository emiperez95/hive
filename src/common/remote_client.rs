//! Background SSH client thread for connecting to remote hive servers.
//!
//! Each `RemoteHandle` spawns a background thread that runs `ssh <host>`
//! (with RemoteCommand set in SSH config to `hive serve --stdio`).
//! It reads JSON lines from the SSH process stdout and updates a shared snapshot.
//!
//! Snapshots are persisted to `~/.hive/cache/remote-{key}.json` on every update.
//! On startup, the cached snapshot is loaded so sessions appear immediately
//! while the SSH connection establishes in the background.

use crate::common::remotes::RemoteConfig;
use crate::ipc::remote_protocol::{ClientMessage, RemoteSessionData, ServerMessage};

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// Connection status for a remote
#[derive(Debug, Clone)]
pub enum ConnectionStatus {
    Connecting,
    Connected,
    Disconnected,
    Error(#[allow(dead_code)] String),
}

/// Snapshot of remote session data (updated by background thread)
#[derive(Debug)]
pub struct RemoteSnapshot {
    pub sessions: Vec<RemoteSessionData>,
    pub status: ConnectionStatus,
    pub last_update: Option<Instant>,
    pub error: Option<String>,
}

impl Default for RemoteSnapshot {
    fn default() -> Self {
        Self {
            sessions: Vec::new(),
            status: ConnectionStatus::Connecting,
            last_update: None,
            error: None,
        }
    }
}

/// Handle to a background SSH client thread
#[allow(dead_code)]
pub struct RemoteHandle {
    pub remote_key: String,
    pub config: RemoteConfig,
    pub snapshot: Arc<Mutex<RemoteSnapshot>>,
    stdin_writer: Arc<Mutex<Option<std::process::ChildStdin>>>,
    shutdown: Arc<AtomicBool>,
    join_handle: Option<JoinHandle<()>>,
}

/// Get the cache file path for a remote's snapshot
fn cache_path(remote_key: &str) -> Option<std::path::PathBuf> {
    crate::common::persistence::cache_dir().map(|d| d.join(format!("remote-{}.json", remote_key)))
}

/// Save sessions to disk cache (best-effort, errors ignored)
fn save_cache(remote_key: &str, sessions: &[RemoteSessionData]) {
    let Some(path) = cache_path(remote_key) else {
        return;
    };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let Ok(json) = serde_json::to_string(sessions) else {
        return;
    };
    let tmp = path.with_extension("json.tmp");
    if std::fs::write(&tmp, &json).is_ok() {
        let _ = std::fs::rename(&tmp, &path);
    }
}

/// Load sessions from disk cache (public, used by TUI to read sync data)
pub fn load_cache_pub(remote_key: &str) -> Vec<RemoteSessionData> {
    load_cache(remote_key)
}

/// Load sessions from disk cache (returns empty vec on any error)
fn load_cache(remote_key: &str) -> Vec<RemoteSessionData> {
    let Some(path) = cache_path(remote_key) else {
        return Vec::new();
    };
    let Ok(content) = std::fs::read_to_string(&path) else {
        return Vec::new();
    };
    serde_json::from_str(&content).unwrap_or_default()
}

impl RemoteHandle {
    /// Spawn a background thread that connects to the remote via SSH.
    /// Seeds the snapshot from disk cache so sessions appear immediately.
    pub fn spawn(remote_key: String, config: RemoteConfig) -> Self {
        // Seed from disk cache for instant display
        let cached = load_cache(&remote_key);
        let initial_status = if cached.is_empty() {
            ConnectionStatus::Connecting
        } else {
            // Show cached data as "connected" so TUI renders them
            ConnectionStatus::Connected
        };
        let snapshot = Arc::new(Mutex::new(RemoteSnapshot {
            sessions: cached,
            status: initial_status,
            last_update: None,
            error: None,
        }));

        let stdin_writer = Arc::new(Mutex::new(None::<std::process::ChildStdin>));
        let shutdown = Arc::new(AtomicBool::new(false));

        let snap = snapshot.clone();
        let stdin_w = stdin_writer.clone();
        let shut = shutdown.clone();
        let ssh_host = config.ssh_host.clone();
        let key = remote_key.clone();

        let join_handle = std::thread::Builder::new()
            .name(format!("remote-{}", remote_key))
            .spawn(move || {
                remote_thread_loop(&key, &ssh_host, &snap, &stdin_w, &shut);
            })
            .expect("Failed to spawn remote client thread");

        Self {
            remote_key,
            config,
            snapshot,
            stdin_writer,
            shutdown,
            join_handle: Some(join_handle),
        }
    }

    /// Send a command to the remote server via the SSH stdin pipe.
    #[allow(dead_code)]
    pub fn send_command(&self, msg: &ClientMessage) {
        if let Ok(mut guard) = self.stdin_writer.lock() {
            if let Some(ref mut stdin) = *guard {
                if let Ok(json) = serde_json::to_string(msg) {
                    let _ = writeln!(stdin, "{}", json);
                    let _ = stdin.flush();
                }
            }
        }
    }

    /// Shutdown the background thread and kill the SSH process.
    pub fn shutdown(&mut self) {
        self.shutdown.store(true, Ordering::SeqCst);
        // Clear stdin to unblock the SSH process
        if let Ok(mut guard) = self.stdin_writer.lock() {
            *guard = None;
        }
        if let Some(handle) = self.join_handle.take() {
            let _ = handle.join();
        }
    }
}

impl Drop for RemoteHandle {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// Main loop for the remote client thread. Handles reconnection with backoff.
fn remote_thread_loop(
    remote_key: &str,
    ssh_host: &str,
    snapshot: &Arc<Mutex<RemoteSnapshot>>,
    stdin_writer: &Arc<Mutex<Option<std::process::ChildStdin>>>,
    shutdown: &Arc<AtomicBool>,
) {
    let mut backoff = Duration::from_secs(5);
    let max_backoff = Duration::from_secs(60);

    while !shutdown.load(Ordering::SeqCst) {
        // Update status to Connecting (keep cached sessions visible)
        if let Ok(mut snap) = snapshot.lock() {
            snap.status = ConnectionStatus::Connecting;
            snap.error = None;
        }

        match spawn_ssh(ssh_host) {
            Ok(mut child) => {
                let stdout = child.stdout.take().expect("stdout piped");
                let stdin = child.stdin.take().expect("stdin piped");

                // Store stdin for command sending
                if let Ok(mut guard) = stdin_writer.lock() {
                    *guard = Some(stdin);
                }

                // Update status to Connected
                if let Ok(mut snap) = snapshot.lock() {
                    snap.status = ConnectionStatus::Connected;
                    snap.last_update = Some(Instant::now());
                }

                // Reset backoff on successful connection
                backoff = Duration::from_secs(5);

                // Read loop
                let reader = BufReader::new(stdout);
                for line in reader.lines() {
                    if shutdown.load(Ordering::SeqCst) {
                        break;
                    }
                    match line {
                        Ok(line) if !line.trim().is_empty() => {
                            match serde_json::from_str::<ServerMessage>(&line) {
                                Ok(ServerMessage::State { sessions }) => {
                                    save_cache(remote_key, &sessions);
                                    if let Ok(mut snap) = snapshot.lock() {
                                        snap.sessions = sessions;
                                        snap.last_update = Some(Instant::now());
                                    }
                                }
                                Ok(ServerMessage::Heartbeat) => {
                                    if let Ok(mut snap) = snapshot.lock() {
                                        snap.last_update = Some(Instant::now());
                                    }
                                }
                                Err(_) => {} // ignore malformed lines
                            }
                        }
                        Ok(_) => {} // empty line
                        Err(_) => break, // stdout closed
                    }
                }

                // Clean up SSH process
                if let Ok(mut guard) = stdin_writer.lock() {
                    *guard = None;
                }
                let _ = child.kill();
                let _ = child.wait();

                // Update status to Disconnected
                if let Ok(mut snap) = snapshot.lock() {
                    snap.status = ConnectionStatus::Disconnected;
                    snap.sessions.clear();
                }
            }
            Err(e) => {
                if let Ok(mut snap) = snapshot.lock() {
                    let msg = format!("{}", e);
                    snap.status = ConnectionStatus::Error(msg.clone());
                    snap.error = Some(msg);
                    snap.sessions.clear();
                }
            }
        }

        if shutdown.load(Ordering::SeqCst) {
            break;
        }

        // Wait with backoff before reconnecting
        let sleep_end = Instant::now() + backoff;
        while Instant::now() < sleep_end && !shutdown.load(Ordering::SeqCst) {
            std::thread::sleep(Duration::from_millis(500));
        }

        // Increase backoff (capped)
        backoff = (backoff * 2).min(max_backoff);
    }
}

/// Spawn an SSH process to the remote host.
/// The SSH config should have `RemoteCommand hive serve --stdio` set for the host.
fn spawn_ssh(ssh_host: &str) -> Result<Child, std::io::Error> {
    Command::new("ssh")
        .args(["-T", ssh_host])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
}
