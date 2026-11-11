//! Sandbox-defined boxes (phase 5 of `docs/remote-boxes.md`).
//!
//! A box is only provably complete if the set of paths a conversation can have *changed* is
//! known. Running the session under `srt` (Anthropic's sandbox-runtime, one node process per
//! session) makes that set a fact rather than a hope: `allowWrite` is exactly the box's path
//! set plus the infrastructure Claude and hive's hooks need, and the kernel refuses the rest.
//!
//! The built-in Bash sandbox is not enough for this — it bounds shell commands only, while
//! file tools follow permission rules — so the whole `claude` process is wrapped.
//!
//! Nothing here is on by default: `[sandbox] enabled = true` (typically on the server, where
//! sessions run unattended) or `hive box unpack --sandbox`.

use anyhow::{anyhow, bail, Result};
use serde::Deserialize;
use std::path::{Path, PathBuf};

use crate::common::boxes::Manifest;
use crate::common::persistence::cache_dir;
use crate::common::projects::expand_tilde;
use crate::common::tmux::sh_quote;

/// The `[sandbox]` block of `config.toml`.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct SandboxConfig {
    /// Run every unpacked/thawed box under `srt`.
    #[serde(default)]
    pub enabled: bool,
    /// Domains reachable from inside, on top of Anthropic's own (without which Claude
    /// cannot run at all). Git remotes and package registries go here.
    #[serde(default)]
    pub allowed_domains: Vec<String>,
    /// Extra writable paths. Anything listed here is, by definition, state the box does
    /// NOT carry, so `pack` reports it as a completeness gap.
    #[serde(default)]
    pub extra_write: Vec<String>,
    /// Paths the conversation must not even READ (`~` is expanded): another program's
    /// database, other projects' credentials. `srt` enforces it in the kernel — `cat`, `ls`
    /// and a read-only `sqlite3` open are all refused — which a `Read(...)` deny rule in
    /// Claude's settings is not: that stops the Read tool, not `cat` through Bash.
    /// Empty by default: reads stay open everywhere, since a default set would be a guess
    /// about what is secret on a machine hive knows nothing about.
    #[serde(default)]
    pub deny_read: Vec<String>,
}

/// Reachable even with an empty `allowed_domains`: Claude itself needs these.
pub const BASE_DOMAINS: &[&str] = &[
    "api.anthropic.com",
    "*.anthropic.com",
    "claude.ai",
    "*.claude.ai",
];

#[derive(Debug, Clone, PartialEq)]
pub struct SandboxPlan {
    /// Paths that carry the conversation's own state — exactly what a box contains.
    pub carried: Vec<String>,
    /// Paths the runtime needs to write that are not conversation state (Claude's profile
    /// dir, hive's cache for hooks, temp).
    pub infrastructure: Vec<String>,
    /// User-granted writes (`extra_write`) the box does not carry.
    pub extra: Vec<String>,
    pub domains: Vec<String>,
    /// `deny_read`, `~` expanded.
    pub deny_read: Vec<String>,
}

impl SandboxPlan {
    /// The full `allowWrite` list.
    pub fn write_set(&self) -> Vec<String> {
        let mut v: Vec<String> = self
            .carried
            .iter()
            .chain(&self.infrastructure)
            .chain(&self.extra)
            .cloned()
            .collect();
        v.sort();
        v.dedup();
        v
    }

    /// Writable paths a box does not carry: the box is only complete if this is empty.
    pub fn completeness_gaps(&self) -> &[String] {
        &self.extra
    }
}

/// The plan for `m` on a machine whose home is `home`, with the project checkout at `repo`.
///
/// `carried` is `root` (the whole working tree incl. ignored files) and, for a linked
/// worktree, the repo's `.git` — commits, refs and worktree metadata are written THERE,
/// which is why a worktree-only allowlist would break `git commit`. The transcript's slug
/// dir lives inside the profile dir, which Claude needs whole (`sessions/`, history, …).
pub fn plan(m: &Manifest, home: &Path, repo: &Path, cfg: &SandboxConfig) -> SandboxPlan {
    let s = |p: &Path| p.to_string_lossy().to_string();
    let mut carried = vec![m.root.clone()];
    if !m.main_checkout {
        carried.push(s(&repo.join(".git")));
    }
    let mut infrastructure = vec![s(&home.join(&m.profile)), s(&home.join(".hive"))];
    if m.profile == ".claude" {
        infrastructure.push(s(&home.join(".claude.json")));
    }
    infrastructure.push("/tmp".to_string());
    infrastructure.push("/private/tmp".to_string());
    if let Ok(t) = std::fs::canonicalize(std::env::temp_dir()) {
        if let Some(mds) = keychain_cache_dir(&t) {
            infrastructure.push(s(&mds));
        }
        infrastructure.push(s(&t));
    }
    let mut domains: Vec<String> = BASE_DOMAINS.iter().map(|d| d.to_string()).collect();
    domains.extend(cfg.allowed_domains.iter().cloned());
    domains.dedup();
    // A deny over a parent of something the session needs (deny `~/Projects`, work in a
    // worktree under it) would lock the conversation out of its own tree; `allowRead` wins
    // over `denyRead` in srt, so what it must read is re-allowed explicitly.
    let deny_read = cfg
        .deny_read
        .iter()
        .map(|p| expand_tilde(p).to_string_lossy().to_string())
        .collect();
    SandboxPlan {
        carried,
        infrastructure,
        extra: cfg.extra_write.clone(),
        domains,
        deny_read,
    }
}

/// macOS only: the Security framework's per-user module database, `<user cache dir>/mds`,
/// beside the per-user temp dir (`…/T` → `…/C/mds`). Every keychain call opens it read-write;
/// without it they fail with "A Module Directory Service error has occurred", and since
/// Claude keeps its login in the keychain, a sandboxed Claude starts as "Not logged in".
/// Measured on the homeserver: unreadable under the default settings, readable with this
/// one path added.
fn keychain_cache_dir(temp_dir: &Path) -> Option<PathBuf> {
    if !cfg!(target_os = "macos") || temp_dir.file_name()? != "T" {
        return None;
    }
    Some(temp_dir.parent()?.join("C").join("mds"))
}

/// `srt`'s settings document. `deniedDomains` is required by srt even when empty.
pub fn settings_json(plan: &SandboxPlan) -> serde_json::Value {
    let mut fs = serde_json::json!({
        "denyRead": plan.deny_read,
        "allowWrite": plan.write_set(),
        "denyWrite": [],
    });
    // Only alongside a deny: the default document is the one verified against real `srt`
    // and `claude`, and an empty allow list means nothing.
    if !plan.deny_read.is_empty() {
        fs["allowRead"] = serde_json::json!(plan
            .carried
            .iter()
            .chain(&plan.infrastructure)
            .collect::<Vec<_>>());
    }
    serde_json::json!({
        "filesystem": fs,
        "network": { "allowedDomains": plan.domains, "deniedDomains": [] },
    })
}

pub fn settings_path(id: &str) -> Option<PathBuf> {
    cache_dir().map(|c| c.join("sandbox").join(format!("{id}.json")))
}

pub fn write_settings(id: &str, plan: &SandboxPlan) -> Result<PathBuf> {
    let path = settings_path(id).ok_or_else(|| anyhow!("cannot determine cache directory"))?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_string_pretty(&settings_json(plan))?)?;
    std::fs::rename(&tmp, &path)?;
    Ok(path)
}

/// The `allowWrite` list a conversation was last launched under, if it was sandboxed.
pub fn recorded_write_set(id: &str) -> Option<Vec<String>> {
    let raw = std::fs::read_to_string(settings_path(id)?).ok()?;
    let v: serde_json::Value = serde_json::from_str(&raw).ok()?;
    v["filesystem"]["allowWrite"].as_array().map(|a| {
        a.iter()
            .filter_map(|p| p.as_str().map(String::from))
            .collect()
    })
}

/// The command a tmux window types to run `inner` under `srt`. `-c` takes the whole
/// command string: `srt … claude --resume <id>` would have commander read `--resume` as
/// an srt option.
pub fn wrap(settings: &Path, inner: &str) -> String {
    format!(
        "srt --settings {} -c {}",
        sh_quote(&settings.to_string_lossy()),
        sh_quote(inner)
    )
}

pub fn srt_available() -> bool {
    std::process::Command::new("srt")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Fail early, before a window is created that would just print "command not found".
pub fn require_srt() -> Result<()> {
    if !srt_available() {
        bail!("`srt` is not installed (npm i -g @anthropic-ai/sandbox-runtime); cannot sandbox");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_keychain_module_cache_sits_beside_the_user_temp_dir() {
        let got = keychain_cache_dir(Path::new("/private/var/folders/81/abc/T"));
        if cfg!(target_os = "macos") {
            assert_eq!(
                got,
                Some(PathBuf::from("/private/var/folders/81/abc/C/mds"))
            );
        } else {
            assert_eq!(got, None);
        }
        // A temp dir that is not the per-user one (`TMPDIR=/tmp`) names no cache dir.
        assert_eq!(keychain_cache_dir(Path::new("/tmp")), None);
    }

    fn manifest(main: bool) -> Manifest {
        serde_json::from_value(serde_json::json!({
            "version": 1, "id": "id1", "project_key": "p", "worktree_key": null,
            "branch": "feat", "base": null, "head": "abc", "cwd": "/w/p-feat",
            "root": "/w/p-feat", "main_checkout": main, "profile": ".claude-work",
            "slug": "s", "session_name": "p", "window_name": "", "window_index": "",
            "ports": [], "note": "", "packed_at": "", "packed_on": "",
            "with_ignored": true, "has_bundle": false,
        }))
        .unwrap()
    }

    fn cfg() -> SandboxConfig {
        SandboxConfig::default()
    }

    #[test]
    fn a_worktree_box_may_write_the_repos_git_dir_a_main_checkout_does_not_need_it() {
        let wt = plan(&manifest(false), Path::new("/h"), Path::new("/w/p"), &cfg());
        assert!(wt.write_set().contains(&"/w/p/.git".to_string()));
        assert!(wt.write_set().contains(&"/w/p-feat".to_string()));
        let main = plan(&manifest(true), Path::new("/h"), Path::new("/w/p"), &cfg());
        assert!(!main.write_set().contains(&"/w/p/.git".to_string()));
    }

    #[test]
    fn the_profile_dir_and_hive_cache_are_writable_but_not_home() {
        let p = plan(&manifest(true), Path::new("/h"), Path::new("/w/p"), &cfg());
        let w = p.write_set();
        assert!(w.contains(&"/h/.claude-work".to_string()));
        assert!(w.contains(&"/h/.hive".to_string()));
        assert!(!w.contains(&"/h".to_string()), "never the whole home");
        // `~/.claude.json` is the default profile's; `.claude-work` keeps its own inside.
        assert!(!w.contains(&"/h/.claude.json".to_string()));
        let mut m = manifest(true);
        m.profile = ".claude".into();
        let w = plan(&m, Path::new("/h"), Path::new("/w/p"), &cfg()).write_set();
        assert!(w.contains(&"/h/.claude.json".to_string()));
    }

    #[test]
    fn extra_writes_are_reported_as_completeness_gaps() {
        let c = SandboxConfig {
            extra_write: vec!["/data/cache".into()],
            ..Default::default()
        };
        let p = plan(&manifest(true), Path::new("/h"), Path::new("/w/p"), &c);
        assert_eq!(p.completeness_gaps(), ["/data/cache"]);
        assert!(p.write_set().contains(&"/data/cache".to_string()));
        assert!(
            plan(&manifest(true), Path::new("/h"), Path::new("/w/p"), &cfg())
                .completeness_gaps()
                .is_empty()
        );
    }

    #[test]
    fn settings_carry_the_fields_srt_insists_on() {
        let p = plan(&manifest(true), Path::new("/h"), Path::new("/w/p"), &cfg());
        let v = settings_json(&p);
        assert_eq!(v["network"]["deniedDomains"], serde_json::json!([]));
        assert!(v["network"]["allowedDomains"]
            .as_array()
            .unwrap()
            .iter()
            .any(|d| d == "api.anthropic.com"));
        assert!(v["filesystem"]["allowWrite"].as_array().unwrap().len() >= 4);
    }

    #[test]
    fn wrap_keeps_resume_out_of_srts_option_parser() {
        let c = wrap(Path::new("/h/it's.json"), "claude --resume abc");
        assert_eq!(
            c,
            "srt --settings '/h/it'\\''s.json' -c 'claude --resume abc'"
        );
    }

    #[test]
    fn deny_read_is_tilde_expanded_and_re_allows_what_the_session_needs() {
        let c = SandboxConfig {
            deny_read: vec!["~/Library/Group Containers/x".into(), "/etc/secret".into()],
            ..Default::default()
        };
        let p = plan(&manifest(true), Path::new("/h"), Path::new("/w/p"), &c);
        let v = settings_json(&p);
        let deny: Vec<&str> = v["filesystem"]["denyRead"]
            .as_array()
            .unwrap()
            .iter()
            .map(|d| d.as_str().unwrap())
            .collect();
        assert!(deny.contains(&"/etc/secret"));
        assert!(
            deny.iter().all(|d| !d.starts_with('~')),
            "tilde left for srt to guess at: {deny:?}"
        );
        let allow = v["filesystem"]["allowRead"].as_array().unwrap();
        assert!(
            allow.iter().any(|a| a == "/w/p-feat"),
            "the box's own tree stays readable"
        );
        assert!(allow.iter().any(|a| a == "/h/.claude-work"));
    }

    #[test]
    fn without_deny_read_the_verified_default_document_is_unchanged() {
        let p = plan(&manifest(true), Path::new("/h"), Path::new("/w/p"), &cfg());
        let v = settings_json(&p);
        assert_eq!(v["filesystem"]["denyRead"], serde_json::json!([]));
        assert!(v["filesystem"].get("allowRead").is_none());
    }
}
