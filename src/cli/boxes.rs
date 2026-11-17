//! `hive box pack|unpack|push|pull|send|mark|list` and `hive remote add|list` — move a
//! conversation as a directory of files. The mechanics live in `common/boxes.rs`; this
//! resolves the conversation, sequences gate → freeze → write, and picks the transport
//! (HTTP when the remote has an `api`, else ssh + rsync).

use anyhow::{anyhow, bail, Result};
use std::path::PathBuf;

use crate::common::boxes::{self, PackOptions, PackSource, UnpackOptions};
use crate::common::frozen::{self, FreezeTarget, FrozenState};
use crate::common::remote;

/// Exact id, or a unique prefix, among the registry's conversations.
fn resolve_conversation_id(
    reg: &crate::common::registry::ConversationRegistry,
    q: &str,
) -> Result<String> {
    if reg.conversations.contains_key(q) {
        return Ok(q.to_string());
    }
    let hits: Vec<&String> = reg
        .conversations
        .keys()
        .filter(|k| k.starts_with(q))
        .collect();
    match hits.as_slice() {
        [one] => Ok((*one).clone()),
        [] => bail!("no conversation matches '{q}'"),
        _ => bail!(
            "'{q}' matches {} conversations; give more of the id",
            hits.len()
        ),
    }
}

pub fn run_box_pack(conv: &str, force: bool, with_ignored: bool) -> Result<()> {
    let (id, dir, _) = pack_conversation(conv, force, with_ignored, false)?;
    println!("packed {} → {}", id, dir.display());
    Ok(())
}

/// Gate → freeze → write. Returns the conversation id, the box dir and its manifest.
///
/// `exact` refuses an id prefix: a request from another machine must name the whole
/// conversation, or `/api/box/a/pack` would freeze whichever one happens to start with `a`.
pub(crate) fn pack_conversation(
    conv: &str,
    force: bool,
    with_ignored: bool,
    exact: bool,
) -> Result<(String, PathBuf, boxes::Manifest)> {
    let reg = crate::common::conversations::gather_conversations();
    if exact && !reg.conversations.contains_key(conv) {
        bail!("no conversation with id '{conv}' (the full id is required)");
    }
    let id = resolve_conversation_id(&reg, conv)?;
    let c = &reg.conversations[&id];
    let placement = c.placement.as_ref().map(|p| {
        (
            p.session_name.as_str(),
            p.window_index.as_str(),
            p.window_name.as_str(),
        )
    });
    let note = if c.note.is_empty() {
        c.title.as_deref().unwrap_or("")
    } else {
        &c.note
    };
    let plan = boxes::plan_pack(
        &PackSource {
            id: &id,
            cwd: &c.cwd,
            note,
            placement,
            pids: &c.pids,
        },
        &PackOptions {
            force,
            with_ignored,
        },
    )?;
    if !plan.overridden.is_empty() {
        eprintln!("warning: packing anyway (--force); this will be lost:");
        for b in &plan.overridden {
            eprintln!("  - {b}");
        }
    }

    // Stop the process only once nothing can refuse the pack any more. The window's
    // files are then quiescent, which is what makes the snapshot consistent.
    let frozen_window = if let Some(p) = &c.placement {
        frozen::freeze_window(
            &FreezeTarget {
                session_name: p.session_name.clone(),
                window_index: p.window_index.clone(),
                window_name: p.window_name.clone(),
                cwd: c.cwd.clone(),
                claude_session_id: Some(id.clone()),
                host: None,
            },
            note,
        )?;
        true
    } else {
        false
    };

    // Say what the box is worth: complete by construction (it ran under `srt`), or only as
    // complete as what we could see.
    match &plan.manifest.sandbox {
        Some(set) => {
            eprintln!(
                "sandboxed: box complete by construction ({} writable paths)",
                set.len()
            );
            let extra = crate::common::config::HiveConfig::load()
                .sandbox
                .extra_write;
            for g in extra.iter().filter(|e| set.contains(e)) {
                eprintln!("  gap: {g} was writable and is not in the box");
            }
        }
        None => eprintln!(
            "not sandboxed: tree and transcript captured; writes elsewhere can't be ruled out"
        ),
    }

    match boxes::write_box(&plan) {
        Ok(dir) => Ok((id, dir, plan.manifest)),
        Err(e) if frozen_window => Err(anyhow!(
            "{e:#}\nthe window was frozen before the pack failed; thaw it from the 💤 bucket to resume"
        )),
        Err(e) => Err(e),
    }
}

/// How a box reaches a remote. A remote with an `api` takes boxes over HTTP (no ssh, rsync
/// or `hive` on its PATH needed — and it works where rsync does not exist); `--via-ssh`
/// forces the older ssh + rsync path, which is also what a remote without an `api` uses.
enum Via {
    Ssh,
    Http(String),
}

fn via(r: &crate::common::config::RemoteConfig, force_ssh: bool) -> Via {
    match (&r.api, force_ssh) {
        (Some(api), false) => Via::Http(api.clone()),
        _ => Via::Ssh,
    }
}

/// Copy the box directory `dir` to the remote's `~/.hive/boxes/<id>`.
fn send_box(
    r: &crate::common::config::RemoteConfig,
    v: &Via,
    id: &str,
    dir: &std::path::Path,
) -> Result<()> {
    match v {
        Via::Ssh => {
            let out = remote::ssh_output(r, "mkdir -p .hive/boxes")?;
            if !out.status.success() {
                bail!(
                    "cannot reach {}: {}",
                    r.name,
                    String::from_utf8_lossy(&out.stderr).trim()
                );
            }
            remote::rsync_dir_to(r, dir, &format!(".hive/boxes/{id}"))
        }
        Via::Http(api) => {
            let tmp =
                std::env::temp_dir().join(format!("hive-send-{id}-{}.tar", std::process::id()));
            boxes::tar_box(dir, &tmp)?;
            let res = crate::common::federation::put_file(api, &format!("/api/box/{id}"), &tmp);
            let _ = std::fs::remove_file(&tmp);
            res.map(|_| ())
        }
    }
}

/// Copy the remote's `~/.hive/boxes/<id>` over `dir`.
fn fetch_box(
    r: &crate::common::config::RemoteConfig,
    v: &Via,
    id: &str,
    dir: &std::path::Path,
) -> Result<()> {
    match v {
        Via::Ssh => remote::rsync_dir_from(r, &format!(".hive/boxes/{id}"), dir),
        Via::Http(api) => {
            let tmp =
                std::env::temp_dir().join(format!("hive-fetch-{id}-{}.tar", std::process::id()));
            // Through the same scratch → validate → install path as a box PUT to us: what
            // comes back from another machine is no more trusted than what it sends.
            let res = crate::common::federation::get_to_file(api, &format!("/api/box/{id}"), &tmp)
                .and_then(|_| boxes::receive_box(&mut std::fs::File::open(&tmp)?, id));
            let _ = std::fs::remove_file(&tmp);
            let installed = res?;
            if installed != dir {
                bail!("box {id} was installed at {}", installed.display());
            }
            Ok(())
        }
    }
}

/// Run a box operation on the remote. `args` are the `hive box <op>` arguments for ssh and
/// the JSON body for HTTP; both describe the same request.
fn remote_op(
    r: &crate::common::config::RemoteConfig,
    v: &Via,
    op: &str,
    id: &str,
    flags: &[(&str, bool)],
    location: Option<&str>,
) -> Result<String> {
    match v {
        Via::Ssh => {
            let mut args: Vec<String> = vec!["box".into(), op.into(), id.into()];
            for (k, _) in flags.iter().filter(|(_, on)| *on) {
                args.push(format!("--{}", k.replace('_', "-")));
            }
            if let Some(l) = location {
                args.push("--location".into());
                args.push(l.into());
            }
            let refs: Vec<&str> = args.iter().map(String::as_str).collect();
            remote::remote_hive_run(r, &refs)
        }
        Via::Http(api) => {
            let mut body = serde_json::Map::new();
            for (k, on) in flags {
                body.insert((*k).to_string(), (*on).into());
            }
            if let Some(l) = location {
                body.insert("location".into(), l.into());
            }
            let answer = crate::common::federation::http_request(
                api,
                "POST",
                &format!("/api/box/{id}/{op}"),
                Some(&serde_json::Value::Object(body).to_string()),
            )
            .map_err(|e| anyhow!("{}: {e}", r.name))?;
            // The one thing a successful answer can still have to say (ssh carries the
            // same line on stdout): the box is there but did not start.
            Ok(serde_json::from_str::<serde_json::Value>(&answer)
                .ok()
                .and_then(|v| v.get("warning").and_then(|w| w.as_str()).map(String::from))
                .map(|w| format!("warning: {w}\n"))
                .unwrap_or_default())
        }
    }
}

/// `hive box push`: pack, then `send`.
#[allow(clippy::too_many_arguments)]
pub fn run_box_push(
    conv: &str,
    remote: &str,
    force: bool,
    with_ignored: bool,
    thaw: bool,
    remove_local: bool,
    sandbox: bool,
    via_ssh: bool,
) -> Result<()> {
    let r = remote::find_remote(remote)?;
    let (id, dir, m) = pack_conversation(conv, force, with_ignored, false)?;
    ship(&r, &id, &dir, &m, thaw, remove_local, sandbox, via_ssh)
}

/// `hive box send`: transfer a box that is already packed — the second half of `push`, and
/// the retry when the transfer failed after the window was already frozen.
pub fn run_box_send(
    target: &str,
    remote: &str,
    thaw: bool,
    remove_local: bool,
    sandbox: bool,
    via_ssh: bool,
) -> Result<()> {
    let r = remote::find_remote(remote)?;
    let dir = resolve_box(target)?;
    let m = boxes::read_manifest(&dir)?;
    ship(&r, &m.id, &dir, &m, thaw, remove_local, sandbox, via_ssh)
}

/// Copy a packed box to the remote, unpack (and thaw) it there, then record that it now
/// lives there. The local working copy is left alone unless `remove_local` — nothing that
/// exists only here is removed by default.
#[allow(clippy::too_many_arguments)]
fn ship(
    r: &crate::common::config::RemoteConfig,
    id: &str,
    dir: &std::path::Path,
    m: &boxes::Manifest,
    thaw: bool,
    remove_local: bool,
    sandbox: bool,
    via_ssh: bool,
) -> Result<()> {
    let v = via(r, via_ssh);
    send_box(r, &v, id, dir)?;
    let out = remote_op(
        r,
        &v,
        "unpack",
        id,
        &[("thaw", thaw), ("sandbox", sandbox)],
        None,
    )?;
    print!("{out}");
    let not_running = out.lines().any(|l| l.starts_with("warning: "));
    ship_mark(id, Some(r.name.clone()), remove_local.then_some(m))?;
    // The project's own checkout went with it: the project lives there now.
    if m.main_checkout {
        crate::common::projects::set_lives_on(&m.project_key, Some(&r.name))?;
    }
    if not_running {
        println!(
            "{id} now lives on {} but is NOT running there (see the warning above)",
            r.name
        );
    } else {
        println!("{id} now lives on {}", r.name);
    }
    Ok(())
}

/// `hive box pull`: the reverse. The remote packs (freezing its window), the box is copied
/// back, and unpacked here.
pub fn run_box_pull(
    conv: &str,
    from: Option<&str>,
    force: bool,
    remove_remote: bool,
    via_ssh: bool,
) -> Result<()> {
    let sc = crate::common::registry::ConversationSidecar::load();
    let hits: Vec<(&String, &String)> = sc
        .conversations
        .iter()
        .filter(|(id, o)| id.starts_with(conv) && o.location.is_some())
        .map(|(id, o)| (id, o.location.as_ref().unwrap()))
        .collect();
    let (id, loc) = match hits.as_slice() {
        [one] => (one.0.clone(), one.1.clone()),
        [] => bail!("no shipped conversation matches '{conv}' (see `hive box list`)"),
        _ => bail!("'{conv}' matches {} shipped conversations", hits.len()),
    };
    let r = remote::find_remote(from.unwrap_or(&loc))?;
    let v = via(&r, via_ssh);
    let local_box = boxes::boxes_dir()?.join(&id);
    // Ask whether this machine can take it BEFORE the remote is touched. The box left here
    // by the push names the same path the one coming back will, so a leftover working copy
    // is caught while the conversation is still running over there.
    //
    // `HIVE_BOX_SHARED_PATHS` is for a rig where both "machines" are one filesystem (the
    // smoke tests' second `$HOME`): there the copy at that path IS the remote's, and it
    // only goes away once the remote releases it.
    let shared = std::env::var_os("HIVE_BOX_SHARED_PATHS").is_some();
    if let (false, Ok(m)) = (shared, boxes::read_manifest(&local_box)) {
        boxes::check_target(&m, std::path::Path::new(&m.root)).map_err(|e| {
            anyhow!(
                "{e}\nnothing was changed on {}; clear that and pull again",
                r.name
            )
        })?;
    }
    remote_op(&r, &v, "pack", &id, &[("force", force)], None)?;
    fetch_box(&r, &v, &id, &local_box)?;
    // And again on what actually arrived, still before the remote copy is released.
    let m = boxes::read_manifest(&local_box)?;
    let fits = if shared {
        Ok(())
    } else {
        boxes::check_target(&m, std::path::Path::new(&m.root))
    };
    fits.map_err(|e| {
        anyhow!(
            "{e}\nthe conversation is packed and still on {} (frozen there); \
             clear that and pull again",
            r.name
        )
    })?;

    // Order matters: once the files are restored here, this machine owns the conversation,
    // whatever happens next. So: restore, record that it is here, tell the remote, and only
    // then resume — a resume that fails must not leave the overlay saying "lives there"
    // (nothing could open it) while the files sit here (a second pull would be refused).
    //
    // The remote's copy is released only after the restore, so a failed restore leaves it
    // intact. The one exception is the shared-paths rig, where that copy occupies the very
    // path we restore onto and has to go first.
    let local = remote::local_name();
    let release_first = shared && remove_remote;
    if release_first {
        remote_op(
            &r,
            &v,
            "mark",
            &id,
            &[("remove_worktree", true)],
            Some(&local),
        )?;
    }
    let (m, settings) = restore_box(&id, false, false).map_err(|e| {
        anyhow!(
            "{e:#}\nthe conversation was not restored here; it is packed and frozen on {}",
            r.name
        )
    })?;
    boxes::set_location(&id, None)?;
    if !release_first {
        if let Err(e) = remote_op(
            &r,
            &v,
            "mark",
            &id,
            &[("remove_worktree", remove_remote)],
            Some(&local),
        ) {
            eprintln!(
                "warning: the conversation is restored here, but {} could not be told ({e:#}); \
                 it still lists a frozen copy — discard that one there",
                r.name
            );
        }
    }
    println!("{id} is back on this machine");
    thaw_box(&m, settings);
    Ok(())
}

/// `hive box mark`: record that a conversation was shipped (or came back). This is the
/// remote half of `pull` — run over ssh, or as `/api/box/<id>/mark` over HTTP — so the
/// machine that gave the conversation up stops offering it. `push` records its own side
/// through `ship_mark` directly.
pub fn run_box_mark(id: &str, location: Option<&str>, remove_worktree: bool) -> Result<()> {
    let dir = resolve_box(id)?;
    let m = boxes::read_manifest(&dir)?;
    ship_mark(
        &m.id,
        location.map(String::from),
        remove_worktree.then_some(&m),
    )?;
    // The project's own checkout went with the box: this machine must stop offering to
    // start work in it, the same as the machine that runs `push` records for itself.
    if m.main_checkout {
        crate::common::projects::set_lives_on(&m.project_key, location)?;
    }
    Ok(())
}

/// Whether `id` names a complete box here by its WHOLE id. Requests from another machine
/// go through this: the CLI's unique-prefix convenience must not be reachable over HTTP.
pub(crate) fn box_exists_exactly(id: &str) -> bool {
    boxes::valid_id(id)
        && boxes::boxes_dir()
            .map(|d| d.join(id).join("manifest.json").is_file())
            .unwrap_or(false)
}

fn ship_mark(id: &str, location: Option<String>, remove: Option<&boxes::Manifest>) -> Result<()> {
    // Before anything is recorded: removing a working copy the box no longer describes
    // would destroy work, and this is reachable from another machine (`/api/box/<id>/mark`).
    if let Some(m) = remove {
        if location.is_none() {
            bail!("refusing to remove the working copy of a conversation that lives here");
        }
        boxes::check_unchanged_since_pack(m, &boxes::boxes_dir()?.join(&m.id))?;
    }
    // A frozen entry here would offer a local thaw of something that now lives elsewhere.
    if location.is_some() {
        let _ = frozen::discard_frozen(id);
    }
    boxes::set_location(id, location)?;
    if let Some(m) = remove {
        boxes::remove_worktree_copy(m)?;
    }
    Ok(())
}

pub fn run_box_unpack(target: &str, thaw: bool, force: bool, sandbox: bool) -> Result<()> {
    unpack_box(target, thaw, force, sandbox).map(|_| ())
}

/// [`run_box_unpack`], returning why the conversation is not running when it was restored
/// but would not start — for a caller that has to pass that on (the HTTP endpoint).
pub(crate) fn unpack_box(
    target: &str,
    thaw: bool,
    force: bool,
    sandbox: bool,
) -> Result<Option<String>> {
    let (m, settings) = restore_box(target, force, sandbox)?;
    Ok(if thaw { thaw_box(&m, settings) } else { None })
}

/// Restore a box's files onto this machine without starting anything. Returns its manifest
/// and, when sandboxed, the `srt` settings file the resumed conversation must run under.
fn restore_box(
    target: &str,
    force: bool,
    sandbox: bool,
) -> Result<(boxes::Manifest, Option<String>)> {
    let dir = resolve_box(target)?;
    let m = boxes::read_manifest(&dir)?;
    let cfg = crate::common::config::HiveConfig::load();
    let sandboxed = sandbox || cfg.sandbox.enabled;
    if sandboxed {
        // Before anything is restored: a missing `srt` should cost nothing.
        crate::common::sandbox::require_srt()?;
    }
    let id = boxes::unpack(&dir, &UnpackOptions { force })?;
    println!("unpacked {} onto {}", id, m.root);
    // Its own checkout just landed here, so here is where the project lives.
    if m.main_checkout {
        crate::common::projects::set_lives_on(&m.project_key, None)?;
    }
    let settings = if sandboxed {
        let home = dirs::home_dir().ok_or_else(|| anyhow!("no home directory"))?;
        let plan = crate::common::sandbox::plan(&m, &home, &boxes::repo_for(&m)?, &cfg.sandbox);
        for g in plan.completeness_gaps() {
            eprintln!("gap: {g} is writable under the sandbox but is not carried by the box");
        }
        let path = crate::common::sandbox::write_settings(&id, &plan)?;
        println!(
            "sandbox: {} ({} writable paths)",
            path.display(),
            plan.write_set().len()
        );
        Some(path.to_string_lossy().to_string())
    } else {
        None
    };
    Ok((m, settings))
}

/// Resume a restored box. Never an error: by now the files are here and this machine owns
/// the conversation, so a window that would not start is something to report, not a reason
/// to tell the caller the move failed (it would retry onto a tree that is no longer empty).
/// The frozen entry is written first and survives a failed thaw, so the conversation — and
/// its sandbox settings — can be resumed from the 💤 bucket.
///
/// Returns the warning when it did not start. It is also printed on STDOUT, not stderr:
/// over ssh the pushing machine reads only this command's stdout, and a push that ends
/// "now lives on <remote>" with nothing running there must say so.
fn thaw_box(m: &boxes::Manifest, settings: Option<String>) -> Option<String> {
    let resumed = (|| -> Result<String> {
        let mut state = FrozenState::load();
        let mut entry = boxes::frozen_entry_for(m);
        entry.sandbox = settings;
        let key = entry.key();
        state.frozen.insert(key.clone(), entry);
        state.save()?;
        frozen::thaw_window(&key)
    })();
    match resumed {
        Ok(session) => {
            println!("resumed in session '{session}'");
            None
        }
        Err(e) => {
            let w = format!(
                "{} is restored on {} but could not be resumed ({e:#}); \
                 thaw it from the 💤 bucket there",
                m.id,
                remote::local_name()
            );
            println!("warning: {w}");
            Some(w)
        }
    }
}

pub fn run_box_list() -> Result<()> {
    let all = boxes::list_boxes();
    if all.is_empty() {
        println!("no boxes");
    }
    let sc = crate::common::registry::ConversationSidecar::load();
    for m in all {
        let at = sc
            .conversations
            .get(&m.id)
            .and_then(|o| o.location.as_deref())
            .map(|l| format!("  → {l}"))
            .unwrap_or_default();
        println!(
            "{}  {}  {}  packed {} on {}{}",
            &m.id[..m.id.len().min(8)],
            m.worktree_key.as_deref().unwrap_or(&m.project_key),
            m.branch,
            m.packed_at,
            m.packed_on,
            at
        );
    }
    Ok(())
}

/// A path to a box directory, or an id / unique id prefix under `~/.hive/boxes`.
fn resolve_box(target: &str) -> Result<PathBuf> {
    let as_path = PathBuf::from(target);
    if as_path.join("manifest.json").is_file() {
        return Ok(as_path);
    }
    let hits: Vec<_> = boxes::list_boxes()
        .into_iter()
        .filter(|m| m.id.starts_with(target))
        .collect();
    match hits.as_slice() {
        [one] => Ok(boxes::boxes_dir()?.join(&one.id)),
        [] => bail!("no box matches '{target}'"),
        _ => bail!(
            "'{target}' matches {} boxes; give more of the id",
            hits.len()
        ),
    }
}

pub fn run_remote_add(
    name: &str,
    ssh: &str,
    api: Option<String>,
    hive_path: Option<String>,
) -> Result<()> {
    crate::common::config::HiveConfig::add_remote(&crate::common::config::RemoteConfig {
        name: name.to_string(),
        ssh: ssh.to_string(),
        api,
        hive_path,
    })?;
    println!("added remote '{name}' ({ssh})");
    Ok(())
}

pub fn run_remote_list() -> Result<()> {
    let cfg = crate::common::config::HiveConfig::load();
    if cfg.remotes.is_empty() {
        println!("no remotes (hive remote add <name> --ssh user@host)");
    }
    for r in &cfg.remotes {
        println!("{}  {}  {}", r.name, r.ssh, r.api.as_deref().unwrap_or("-"));
    }
    Ok(())
}
