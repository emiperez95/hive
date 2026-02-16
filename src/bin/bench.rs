//! Benchmark tool for measuring hive refresh performance.
//!
//! Run with: cargo run --release --bin hive-bench
//!
//! Measures the main components of each refresh cycle:
//! - sysinfo: System process information gathering (CPU/RAM)
//! - tmux: Session/window/pane discovery via tmux commands
//! - jsonl: Reading Claude status from jsonl files
//! - ports: Listening port detection via libproc
//! - chrome: Chrome tab discovery and port matching

use clap::Parser;
use hive::common::chrome::{get_chrome_tabs, match_tabs_to_ports};
use hive::common::ports::get_listening_ports_for_pids;
use hive::common::process::get_all_descendants;
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::Command;
use std::time::Instant;
use sysinfo::System;

#[derive(Parser)]
#[command(name = "hive-bench")]
#[command(about = "Benchmark hive refresh performance")]
struct Args {
    /// Number of iterations
    #[arg(short, long, default_value = "50")]
    iterations: usize,
}

fn main() {
    let args = Args::parse();

    println!("hive refresh benchmark");
    println!("======================\n");
    println!("Running {} refresh cycles...\n", args.iterations);

    let mut all_metrics: Vec<Metrics> = Vec::with_capacity(args.iterations);

    for i in 1..=args.iterations {
        let metrics = run_refresh_cycle();

        if i % 10 == 0 || i == 1 {
            println!(
                "[{:>2}] total={:>6.2}ms | sysinfo={:>6.2}ms | tmux={:>6.2}ms | jsonl={:>5.2}ms | ports={:>5.2}ms | chrome={:>5.2}ms",
                i, metrics.total_ms, metrics.sysinfo_ms, metrics.tmux_ms, metrics.jsonl_ms, metrics.ports_ms, metrics.chrome_ms,
            );
        }

        all_metrics.push(metrics);
    }

    let stats = Statistics::from_metrics(&all_metrics);

    println!(
        "\n--- Results over {} cycles ({} sessions) ---",
        args.iterations, stats.session_count
    );
    println!(
        "total:   {:>6.2}ms ± {:>5.2}ms",
        stats.total_mean, stats.total_stddev
    );
    println!(
        "sysinfo: {:>6.2}ms ± {:>5.2}ms ({:>4.1}%)",
        stats.sysinfo_mean,
        stats.sysinfo_stddev,
        (stats.sysinfo_mean / stats.total_mean) * 100.0
    );
    println!(
        "tmux:    {:>6.2}ms ± {:>5.2}ms ({:>4.1}%)",
        stats.tmux_mean,
        stats.tmux_stddev,
        (stats.tmux_mean / stats.total_mean) * 100.0
    );
    println!(
        "jsonl:   {:>6.2}ms ± {:>5.2}ms ({:>4.1}%)",
        stats.jsonl_mean,
        stats.jsonl_stddev,
        (stats.jsonl_mean / stats.total_mean) * 100.0
    );
    println!(
        "ports:   {:>6.2}ms ± {:>5.2}ms ({:>4.1}%)",
        stats.ports_mean,
        stats.ports_stddev,
        (stats.ports_mean / stats.total_mean) * 100.0
    );
    println!(
        "chrome:  {:>6.2}ms ± {:>5.2}ms ({:>4.1}%)",
        stats.chrome_mean,
        stats.chrome_stddev,
        (stats.chrome_mean / stats.total_mean) * 100.0
    );
}

struct Statistics {
    total_mean: f64,
    total_stddev: f64,
    sysinfo_mean: f64,
    sysinfo_stddev: f64,
    tmux_mean: f64,
    tmux_stddev: f64,
    jsonl_mean: f64,
    jsonl_stddev: f64,
    ports_mean: f64,
    ports_stddev: f64,
    chrome_mean: f64,
    chrome_stddev: f64,
    session_count: usize,
}

impl Statistics {
    fn from_metrics(metrics: &[Metrics]) -> Self {
        let n = metrics.len() as f64;

        let total_mean = metrics.iter().map(|m| m.total_ms).sum::<f64>() / n;
        let sysinfo_mean = metrics.iter().map(|m| m.sysinfo_ms).sum::<f64>() / n;
        let tmux_mean = metrics.iter().map(|m| m.tmux_ms).sum::<f64>() / n;
        let jsonl_mean = metrics.iter().map(|m| m.jsonl_ms).sum::<f64>() / n;
        let ports_mean = metrics.iter().map(|m| m.ports_ms).sum::<f64>() / n;
        let chrome_mean = metrics.iter().map(|m| m.chrome_ms).sum::<f64>() / n;

        let total_stddev = stddev(metrics.iter().map(|m| m.total_ms), total_mean, n);
        let sysinfo_stddev = stddev(metrics.iter().map(|m| m.sysinfo_ms), sysinfo_mean, n);
        let tmux_stddev = stddev(metrics.iter().map(|m| m.tmux_ms), tmux_mean, n);
        let jsonl_stddev = stddev(metrics.iter().map(|m| m.jsonl_ms), jsonl_mean, n);
        let ports_stddev = stddev(metrics.iter().map(|m| m.ports_ms), ports_mean, n);
        let chrome_stddev = stddev(metrics.iter().map(|m| m.chrome_ms), chrome_mean, n);

        let session_count = metrics.first().map(|m| m.session_count).unwrap_or(0);

        Self {
            total_mean,
            total_stddev,
            sysinfo_mean,
            sysinfo_stddev,
            tmux_mean,
            tmux_stddev,
            jsonl_mean,
            jsonl_stddev,
            ports_mean,
            ports_stddev,
            chrome_mean,
            chrome_stddev,
            session_count,
        }
    }
}

fn stddev(values: impl Iterator<Item = f64>, mean: f64, n: f64) -> f64 {
    (values.map(|v| (v - mean).powi(2)).sum::<f64>() / n).sqrt()
}

struct Metrics {
    total_ms: f64,
    sysinfo_ms: f64,
    tmux_ms: f64,
    jsonl_ms: f64,
    ports_ms: f64,
    chrome_ms: f64,
    session_count: usize,
    #[allow(dead_code)]
    window_count: usize,
    #[allow(dead_code)]
    pane_count: usize,
}

fn cwd_to_claude_projects_path(cwd: &str) -> PathBuf {
    let home = dirs::home_dir().unwrap_or_default();
    let encoded = cwd.replace('/', "-");
    home.join(".claude").join("projects").join(encoded)
}

fn find_latest_jsonl(projects_path: &PathBuf) -> Option<PathBuf> {
    let entries = fs::read_dir(projects_path).ok()?;
    entries
        .filter_map(|e| e.ok())
        .filter(|e| {
            e.path()
                .extension()
                .map(|ext| ext == "jsonl")
                .unwrap_or(false)
        })
        .max_by_key(|e| e.metadata().and_then(|m| m.modified()).ok())
        .map(|e| e.path())
}

fn read_last_lines(path: &PathBuf, n: usize) -> Vec<String> {
    let file = match fs::File::open(path) {
        Ok(f) => f,
        Err(_) => return Vec::new(),
    };
    let reader = BufReader::new(file);
    let lines: Vec<String> = reader.lines().map_while(Result::ok).collect();
    lines.into_iter().rev().take(n).collect()
}

fn run_refresh_cycle() -> Metrics {
    let total_start = Instant::now();

    // 1. sysinfo
    let sysinfo_start = Instant::now();
    let mut sys = System::new_all();
    sys.refresh_all();
    let sysinfo_ms = sysinfo_start.elapsed().as_secs_f64() * 1000.0;

    // 2. tmux discovery
    let tmux_start = Instant::now();
    let sessions_output = Command::new("tmux")
        .args(["list-sessions", "-F", "#{session_name}"])
        .output()
        .expect("tmux list-sessions failed");

    let sessions_str = String::from_utf8_lossy(&sessions_output.stdout);
    let session_count = sessions_str.lines().count();

    let mut window_count = 0;
    let mut pane_count = 0;
    let mut pane_cwds: Vec<String> = Vec::new();
    let mut all_pids: Vec<u32> = Vec::new();

    for session in sessions_str.lines() {
        let windows_output = Command::new("tmux")
            .args(["list-windows", "-t", session, "-F", "#{window_index}"])
            .output()
            .expect("tmux list-windows failed");

        for window in String::from_utf8_lossy(&windows_output.stdout).lines() {
            window_count += 1;
            let target = format!("{}:{}", session, window);
            let panes_output = Command::new("tmux")
                .args([
                    "list-panes",
                    "-t",
                    &target,
                    "-F",
                    "#{pane_pid}\t#{pane_current_path}",
                ])
                .output()
                .expect("tmux list-panes failed");

            for line in String::from_utf8_lossy(&panes_output.stdout).lines() {
                pane_count += 1;
                let mut parts = line.split('\t');
                if let Some(pid_str) = parts.next() {
                    if let Ok(pid) = pid_str.parse::<u32>() {
                        all_pids.push(pid);
                        get_all_descendants(&sys, pid, &mut all_pids);
                    }
                }
                if let Some(cwd) = parts.next() {
                    pane_cwds.push(cwd.to_string());
                }
            }
        }
    }
    let tmux_ms = tmux_start.elapsed().as_secs_f64() * 1000.0;

    // 3. jsonl reading
    let jsonl_start = Instant::now();
    for cwd in &pane_cwds {
        let projects_path = cwd_to_claude_projects_path(cwd);
        if let Some(jsonl_path) = find_latest_jsonl(&projects_path) {
            let _lines = read_last_lines(&jsonl_path, 10);
        }
    }
    let jsonl_ms = jsonl_start.elapsed().as_secs_f64() * 1000.0;

    // 4. ports
    let ports_start = Instant::now();
    let listening_ports = get_listening_ports_for_pids(&all_pids, &sys);
    let ports_ms = ports_start.elapsed().as_secs_f64() * 1000.0;

    // 5. chrome
    let chrome_start = Instant::now();
    let tabs = get_chrome_tabs();
    let _matched = match_tabs_to_ports(&tabs, &listening_ports);
    let chrome_ms = chrome_start.elapsed().as_secs_f64() * 1000.0;

    let total_ms = total_start.elapsed().as_secs_f64() * 1000.0;

    Metrics {
        total_ms,
        sysinfo_ms,
        tmux_ms,
        jsonl_ms,
        ports_ms,
        chrome_ms,
        session_count,
        window_count,
        pane_count,
    }
}
