//! `hive task` — a project's task board (Apple Reminders), for people and for agents.
//!
//! The project defaults to the one the working directory belongs to, a worktree
//! resolving up to its project — so a Claude conversation in any of a project's
//! checkouts reads and writes the same board without being told which.

use crate::cli::TaskCommand;
use crate::common::projects::{ProjectRegistry, TasksConfig};
use crate::common::task_schema;
use crate::common::tasks::{self, BodyEdit, NewTask, Task};
use anyhow::{anyhow, bail, Result};

/// The board to act on: `--project`, else the cwd's project.
fn board(project: Option<String>) -> Result<(String, TasksConfig)> {
    let reg = ProjectRegistry::load();
    let key = match project {
        Some(p) => tasks::project_of(&p).to_string(),
        None => {
            let cwd = std::env::current_dir()?.to_string_lossy().into_owned();
            tasks::project_for_cwd(&cwd, &reg).ok_or_else(|| {
                anyhow!("this directory is not in a registered project — pass --project <key>")
            })?
        }
    };
    let cfg = reg
        .projects
        .get(&key)
        .ok_or_else(|| anyhow!("no project '{key}'"))?
        .tasks
        .clone();
    match cfg {
        Some(c) => Ok((key, c)),
        None => {
            let have: Vec<String> = tasks::boards(&reg).into_iter().map(|(k, _)| k).collect();
            bail!(
                "project '{key}' has no task board — add a [projects.{key}.tasks] table with a \
                 `list` to ~/.hive/projects.toml{}",
                if have.is_empty() {
                    String::new()
                } else {
                    format!(" (boards exist for: {})", have.join(", "))
                }
            )
        }
    }
}

/// A task by full id or unambiguous prefix (the 8 chars `list` prints).
fn resolve<'a>(tasks: &'a [Task], id: &str) -> Result<&'a Task> {
    let id = id.to_lowercase();
    let mut hits = tasks
        .iter()
        .filter(|t| t.id.to_lowercase().starts_with(&id));
    match (hits.next(), hits.next()) {
        (Some(t), None) => Ok(t),
        (Some(_), Some(_)) => bail!("'{id}' matches several tasks — use more of the id"),
        _ => bail!(
            "no task '{id}' on this board (completed tasks beyond the latest few aren't loaded)"
        ),
    }
}

fn short(id: &str) -> &str {
    &id[..id.len().min(8)]
}

/// A text argument. `-` reads stdin to the end, so a long multi-line body needs no shell
/// quoting: `hive task set <id> --append - <<'EOF' … EOF`.
fn text_arg(v: String) -> Result<String> {
    if v != "-" {
        return Ok(v);
    }
    use std::io::{IsTerminal, Read};
    if std::io::stdin().is_terminal() {
        eprintln!("reading the text from stdin — end with Ctrl-D");
    }
    let mut text = String::new();
    std::io::stdin().read_to_string(&mut text)?;
    Ok(text)
}

fn line_count(s: &str) -> usize {
    s.lines().count()
}

fn lines(n: usize) -> String {
    format!("{n} line{}", if n == 1 { "" } else { "s" })
}

/// One line saying what a body edit did, from the body before and after.
fn body_change(before: &str, after: &str, edit: &BodyEdit) -> String {
    if before == after {
        return "body: unchanged".into();
    }
    match edit {
        BodyEdit::Replace(_) if after.is_empty() => "body: cleared".into(),
        BodyEdit::Replace(_) => format!("body: replaced ({})", lines(line_count(after))),
        BodyEdit::Append(a) => format!(
            "body: +{} ({} total)",
            lines(line_count(a.trim())),
            line_count(after)
        ),
    }
}

fn line(t: &Task, group_by: Option<&str>, stale: bool) -> String {
    let mut s = format!("  {}  {}", short(&t.id), t.title);
    let mut meta: Vec<String> = Vec::new();
    if t.priority != "none" {
        meta.push(t.priority.clone());
    }
    if let Some(d) = &t.due {
        meta.push(format!("due {}", d.split('T').next().unwrap_or(d)));
    }
    if let Some(g) = group_by {
        if let Some((_, v)) = t.fields.iter().find(|(k, _)| k.eq_ignore_ascii_case(g)) {
            meta.push(format!("{g}={v}"));
        }
    }
    if let Some(b) = t.fields.iter().find(|(k, _)| k == "blocked-by") {
        if b.1 != "none" {
            meta.push(format!("blocked-by: {}", b.1));
        }
    }
    if let Some(c) = &t.conversation {
        meta.push(format!("conv {}", short(c)));
    }
    if stale {
        if let Some(d) = t.days_since_modified() {
            meta.push(format!("untouched {d}d"));
        }
    }
    if !meta.is_empty() {
        s.push_str(&format!("  ({})", meta.join(", ")));
    }
    s
}

pub fn run_task(cmd: TaskCommand) -> Result<()> {
    match cmd {
        TaskCommand::Boards => {
            let reg = ProjectRegistry::load();
            let b = tasks::boards(&reg);
            if b.is_empty() {
                println!("No task boards. Add [projects.<key>.tasks] with a `list` to ~/.hive/projects.toml");
            }
            for (k, c) in b {
                println!(
                    "{k:<18} Reminders list \"{}\"  columns: {}",
                    c.list,
                    c.columns().join(" · ")
                );
            }
            Ok(())
        }
        TaskCommand::List {
            project,
            column,
            json,
        } => {
            let (key, cfg) = board(project)?;
            let mut all = tasks::fetch(&cfg)?;
            if let Some(c) = &column {
                let c = c.to_lowercase();
                all.retain(|t| t.column == c);
            }
            if json {
                println!("{}", serde_json::to_string_pretty(&all)?);
                return Ok(());
            }
            println!("📋 {key} · \"{}\" · {} tasks", cfg.list, all.len());
            for col in cfg.columns() {
                let rows: Vec<&Task> = all.iter().filter(|t| t.column == col).collect();
                if rows.is_empty() && column.is_none() {
                    continue;
                }
                println!("\n{col} ({})", rows.len());
                for t in rows {
                    println!("{}", line(t, cfg.group_by.as_deref(), t.is_stale(&cfg)));
                }
            }
            Ok(())
        }
        TaskCommand::Show { id, project } => {
            let (_, cfg) = board(project)?;
            let all = tasks::fetch(&cfg)?;
            let t = resolve(&all, &id)?;
            println!("{}\n  id: {}\n  column: {}", t.title, t.id, t.column);
            if let Some(d) = &t.due {
                println!("  due: {d}");
            }
            println!("  priority: {}", t.priority);
            for (k, v) in &t.fields {
                println!("  {k}: {v}");
            }
            if !t.body.is_empty() {
                println!("\n{}", t.body);
            }
            Ok(())
        }
        TaskCommand::Add {
            title,
            project,
            column,
            field,
            body,
            due,
            priority,
        } => {
            let (key, cfg) = board(project)?;
            let mut fields = Vec::new();
            for f in field {
                let (k, v) = f
                    .split_once('=')
                    .ok_or_else(|| anyhow!("--field wants key=value, got '{f}'"))?;
                fields.push((k.trim().to_string(), v.trim().to_string()));
            }
            let made = tasks::add_task(
                &cfg,
                NewTask {
                    title: title.join(" "),
                    column,
                    fields,
                    body,
                    due,
                    priority,
                },
            )?;
            println!("added to {key}'s board: {made}");
            Ok(())
        }
        TaskCommand::Move {
            id,
            column,
            project,
            by,
        } => {
            let (_, cfg) = board(project)?;
            let all = tasks::fetch(&cfg)?;
            let before = resolve(&all, &id)?;
            let full = before.id.clone();
            let cleared = before
                .fields
                .iter()
                .find(|(k, _)| k == "blocked-by")
                .map(|(_, v)| v.clone());
            let t = tasks::move_task(&cfg, &full, &column, by.as_deref())?;
            println!("{} → {}", t.title, t.column);
            if let (true, true, Some(c)) = (cfg.strict, t.column != "blocked", cleared) {
                println!("  cleared blocked-by: {c}");
            }
            Ok(())
        }
        TaskCommand::Set {
            id,
            fields,
            body,
            append,
            project,
        } => {
            let (_, cfg) = board(project)?;
            // Read stdin before touching Reminders: a failure here writes nothing.
            let body_edit = match (body, append) {
                (Some(b), _) => Some(BodyEdit::Replace(text_arg(b)?)),
                (None, Some(a)) => {
                    let a = text_arg(a)?;
                    if a.trim().is_empty() {
                        bail!("--append got no text");
                    }
                    Some(BodyEdit::Append(a))
                }
                (None, None) => None,
            };
            let all = tasks::fetch(&cfg)?;
            let before = resolve(&all, &id)?;
            let (full, old_body) = (before.id.clone(), before.body.clone());
            let (mut sets, mut removes) = (Vec::new(), Vec::new());
            let mut native = tasks::NativeEdit::default();
            for f in fields {
                let (k, v) = f
                    .split_once('=')
                    .ok_or_else(|| anyhow!("expected key=value (or key= to remove), got '{f}'"))?;
                let (k, v) = (k.trim(), v.trim());
                // `priority`, `due` and `title` belong to Reminders, never the header.
                match k.to_lowercase().as_str() {
                    "priority" if v.is_empty() => bail!("priority=none to clear it"),
                    "priority" => native.priority = Some(v.to_string()),
                    "due" => native.due = Some((!v.is_empty()).then(|| v.to_string())),
                    "title" => native.title = Some(v.to_string()),
                    _ if v.is_empty() => removes.push(k.to_string()),
                    _ => sets.push((k.to_string(), v.to_string())),
                }
            }
            let mut t = None;
            if !sets.is_empty() || !removes.is_empty() {
                t = Some(tasks::set_fields(&cfg, &full, &sets, &removes)?);
            }
            if !native.is_empty() {
                t = Some(tasks::edit_native(&cfg, &full, &native)?);
            }
            if let Some(e) = &body_edit {
                t = Some(tasks::edit_body(&cfg, &full, e)?);
            }
            let t = t.ok_or_else(|| anyhow!("nothing to set"))?;
            println!("{}", t.title);
            println!(
                "  priority: {}{}",
                t.priority,
                t.due
                    .as_deref()
                    .map(|d| format!("   due: {}", d.split('T').next().unwrap_or(d)))
                    .unwrap_or_default()
            );
            for (k, v) in &t.fields {
                println!("  {k}: {v}");
            }
            if let Some(e) = &body_edit {
                println!("  {}", body_change(&old_body, &t.body, e));
                // Reminders keeps no history: a rewrite's old text lives only in this output.
                if matches!(e, BodyEdit::Replace(_)) && !old_body.is_empty() && old_body != t.body {
                    eprintln!("\nprevious body (not kept anywhere else):\n{old_body}");
                }
            }
            Ok(())
        }
        TaskCommand::Schema { project, json } => {
            let (key, cfg) = board(project)?;
            let fields = task_schema::fields(&cfg);
            if json {
                println!("{}", serde_json::to_string_pretty(&fields)?);
                return Ok(());
            }
            println!(
                "📐 {key} · schema {}",
                if cfg.strict {
                    "ENFORCED on add / set / move"
                } else {
                    "not enforced yet (strict = false) — `hive task lint` shows drift"
                }
            );
            println!(
                "   header order: {}",
                fields
                    .iter()
                    .map(|f| f.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            );
            for f in &fields {
                let vals = if f.values.is_empty() {
                    String::new()
                } else {
                    format!("  one of: {}", f.values.join(" | "))
                };
                println!(
                    "\n  {}{}  [{:?}{}]{vals}\n      {}",
                    f.name,
                    if f.required { " *" } else { "" },
                    f.kind,
                    if f.core { ", core" } else { ", this board" },
                    f.doc
                );
            }
            println!("\n  * required.  blocked-by: present only while status is blocked.");
            if !cfg.renames.is_empty() {
                println!(
                    "  legacy names still in use: {}",
                    cfg.renames
                        .iter()
                        .map(|(a, b)| format!("{a}→{b}"))
                        .collect::<Vec<_>>()
                        .join(", ")
                );
            }
            if !cfg.id_prefix.is_empty() {
                println!(
                    "  title ids by {}: {}",
                    cfg.id_field.as_deref().unwrap_or("area"),
                    cfg.id_prefix
                        .iter()
                        .map(|(a, b)| format!("{a}→[{b}-nnn]"))
                        .collect::<Vec<_>>()
                        .join(", ")
                );
            }
            Ok(())
        }
        TaskCommand::Lint { project, all, json } => {
            let (key, cfg) = board(project)?;
            let tasks = tasks::fetch(&cfg)?;
            let rows: Vec<(&Task, Vec<&task_schema::Issue>)> = tasks
                .iter()
                .map(|t| {
                    (
                        t,
                        t.issues
                            .iter()
                            .filter(|i| all || i.level == task_schema::Level::Error)
                            .collect::<Vec<_>>(),
                    )
                })
                .filter(|(_, v)| !v.is_empty())
                .collect();
            if json {
                let out: Vec<_> = rows
                    .iter()
                    .map(|(t, v)| serde_json::json!({"id": t.id, "title": t.title, "issues": v}))
                    .collect();
                println!("{}", serde_json::to_string_pretty(&out)?);
                return Ok(());
            }
            println!(
                "🔎 {key} · {} of {} tasks break the schema",
                rows.len(),
                tasks.len()
            );
            for (t, v) in &rows {
                println!("\n  {}  {}", short(&t.id), t.title);
                for i in v {
                    println!("      - {}", i.msg);
                }
            }
            if rows.is_empty() {
                println!("  clean.");
            }
            Ok(())
        }
        TaskCommand::Done { id, project } => {
            let (_, cfg) = board(project)?;
            let all = tasks::fetch(&cfg)?;
            let full = resolve(&all, &id)?.id.clone();
            let t = tasks::move_task(&cfg, &full, "done", None)?;
            println!("{} → {}", t.title, t.column);
            Ok(())
        }
        TaskCommand::Link { id, project, conv } => {
            let (_, cfg) = board(project)?;
            let conv = conv
                .or_else(crate::cli::fork::running_conversation_id)
                .ok_or_else(|| anyhow!("not inside a Claude conversation — pass --conv <uuid>"))?;
            let all = tasks::fetch(&cfg)?;
            let full = resolve(&all, &id)?.id.clone();
            let t = tasks::set_field(&cfg, &full, "conversation", &conv)?;
            println!("linked {} to conversation {}", t.title, short(&conv));
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn body_change_says_what_happened() {
        let rep = BodyEdit::Replace(String::new());
        let app = |t: &str| BodyEdit::Append(t.into());
        assert_eq!(body_change("a", "a", &rep), "body: unchanged");
        assert_eq!(body_change("a\nb", "", &rep), "body: cleared");
        assert_eq!(body_change("a", "x", &rep), "body: replaced (1 line)");
        assert_eq!(
            body_change("a", "x\ny\nz", &rep),
            "body: replaced (3 lines)"
        );
        // The blank separator is not counted as added text.
        assert_eq!(
            body_change("a\nb", "a\nb\n\nc\nd\n", &app("c\nd\n")),
            "body: +2 lines (5 total)"
        );
        assert_eq!(body_change("", "x", &app("x")), "body: +1 line (1 total)");
    }

    #[test]
    fn text_that_is_not_a_dash_passes_through() {
        assert_eq!(text_arg("## Review".into()).unwrap(), "## Review");
        assert_eq!(text_arg("".into()).unwrap(), "");
    }
}
