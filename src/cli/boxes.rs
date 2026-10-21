//! `hive box pack|unpack|list` — move a conversation as a directory of files.
//! The mechanics live in `common/boxes.rs`; this resolves the conversation and
//! sequences gate → freeze → write.

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
    let (id, dir, _) = pack_conversation(conv, force, with_ignored)?;
    println!("packed {} → {}", id, dir.display());
    Ok(())
}

/// Gate → freeze → write. Returns the conversation id, the box dir and its manifest.
fn pack_conversation(
    conv: &str,
    force: bool,
    with_ignored: bool,
) -> Result<(String, PathBuf, boxes::Manifest)> {
    let reg = crate::common::conversations::gather_conversations();
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

    match boxes::write_box(&plan) {
        Ok(dir) => Ok((id, dir, plan.manifest)),
        Err(e) if frozen_window => Err(anyhow!(
            "{e:#}\nthe window was frozen before the pack failed; thaw it from the 💤 bucket to resume"
        )),
        Err(e) => Err(e),
    }
}

/// `hive box push`: pack, copy to the remote, unpack (and thaw) there, then record that it
/// now lives there. The local working copy is left alone unless `remove_local` — nothing
/// that exists only here is removed by default.
pub fn run_box_push(
    conv: &str,
    remote: &str,
    force: bool,
    with_ignored: bool,
    thaw: bool,
    remove_local: bool,
) -> Result<()> {
    let r = remote::find_remote(remote)?;
    let (id, dir, m) = pack_conversation(conv, force, with_ignored)?;
    let rel = format!(".hive/boxes/{id}");
    let out = remote::ssh_output(&r, "mkdir -p .hive/boxes")?;
    if !out.status.success() {
        bail!(
            "cannot reach {}: {}",
            r.name,
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    remote::rsync_dir_to(&r, &dir, &rel)?;
    let mut args = vec!["box", "unpack", id.as_str()];
    if thaw {
        args.push("--thaw");
    }
    let out = remote::remote_hive_run(&r, &args)?;
    print!("{out}");
    ship_mark(&id, Some(r.name.clone()), remove_local.then_some(&m))?;
    println!("{id} now lives on {}", r.name);
    Ok(())
}

/// `hive box pull`: the reverse. The remote packs (freezing its window), the box is copied
/// back, and unpacked here.
pub fn run_box_pull(
    conv: &str,
    from: Option<&str>,
    force: bool,
    remove_remote: bool,
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
    let mut pack_args = vec!["box", "pack", id.as_str()];
    if force {
        pack_args.push("--force");
    }
    remote::remote_hive_run(&r, &pack_args)?;
    let local_box = boxes::boxes_dir()?.join(&id);
    remote::rsync_dir_from(&r, &format!(".hive/boxes/{id}"), &local_box)?;

    // The box is on this disk now, so the remote copy is safe to release before unpacking
    // (and must be, when both sides share paths).
    let local = remote::local_name();
    let mark = |remove: bool| {
        let mut a = vec!["box", "mark", id.as_str(), "--location", local.as_str()];
        if remove {
            a.push("--remove-worktree");
        }
        remote::remote_hive_run(&r, &a)
    };
    if remove_remote {
        mark(true)?;
    }
    run_box_unpack(&id, true, false)?;
    if !remove_remote {
        mark(false)?;
    }
    boxes::set_location(&id, None)?;
    println!("{id} is back on this machine");
    Ok(())
}

/// `hive box mark`: record that a conversation was shipped (or came back). Run locally by
/// `push`, and over ssh by `pull` so each side stops offering what the other now owns.
pub fn run_box_mark(id: &str, location: Option<&str>, remove_worktree: bool) -> Result<()> {
    let dir = resolve_box(id)?;
    let m = boxes::read_manifest(&dir)?;
    ship_mark(
        &m.id,
        location.map(String::from),
        remove_worktree.then_some(&m),
    )
}

fn ship_mark(id: &str, location: Option<String>, remove: Option<&boxes::Manifest>) -> Result<()> {
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

pub fn run_box_unpack(target: &str, thaw: bool, force: bool) -> Result<()> {
    let dir = resolve_box(target)?;
    let m = boxes::read_manifest(&dir)?;
    let id = boxes::unpack(&dir, &UnpackOptions { force })?;
    println!("unpacked {} onto {}", id, m.root);
    if thaw {
        let mut state = FrozenState::load();
        let entry = boxes::frozen_entry_for(&m);
        let key = entry.key();
        state.frozen.insert(key.clone(), entry);
        state.save()?;
        let session = frozen::thaw_window(&key)?;
        println!("resumed in session '{session}'");
    }
    Ok(())
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
