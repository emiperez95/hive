//! The task schema: one shared **core** of header fields every board speaks, which a
//! board extends in `projects.toml`. hive's CLI is the only writer that enforces it;
//! edits made in the Reminders app are caught by `hive task lint` instead.
//!
//! Core fields are defined here, in code, so every agent on every board sees the same
//! vocabulary. A board adds its own fields, narrows a core field's values (`area` is a
//! free slug in general but a closed set on Thesis), and says which fields it requires.
//! `renames` maps a board's *legacy* names onto core ones, so existing reminders are
//! reported as "rename X to Y" rather than as unknown.

use crate::common::projects::TasksConfig;
use serde::Serialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Text,
    Enum,
    Bool,
    Duration,
    Url,
    Urls,
    Paths,
    Uuid,
}

impl Kind {
    pub fn parse(s: &str) -> Option<Kind> {
        Some(match s.to_lowercase().as_str() {
            "text" => Kind::Text,
            "enum" => Kind::Enum,
            "bool" => Kind::Bool,
            "duration" => Kind::Duration,
            "url" => Kind::Url,
            "urls" => Kind::Urls,
            "paths" => Kind::Paths,
            "uuid" => Kind::Uuid,
            _ => return None,
        })
    }
}

struct Core {
    name: &'static str,
    kind: Kind,
    values: &'static [&'static str],
    doc: &'static str,
}

/// The core, in canonical header order.
const CORE: &[Core] = &[
    Core {
        name: "status",
        kind: Kind::Enum,
        values: &[],
        doc: "board column; `done` also completes the reminder",
    },
    Core {
        name: "area",
        kind: Kind::Text,
        values: &[],
        doc: "coarse category of the work (a closed set on boards that narrow it)",
    },
    Core {
        name: "epic",
        kind: Kind::Text,
        values: &[],
        doc: "named stream of related tasks (slug)",
    },
    Core {
        name: "type",
        kind: Kind::Enum,
        values: &["chore", "sync", "review", "research", "feature", "design"],
        doc: "kind of work",
    },
    Core {
        name: "size",
        kind: Kind::Enum,
        values: &["XS", "S", "M", "L", "XL"],
        doc: "relative effort",
    },
    Core {
        name: "est",
        kind: Kind::Duration,
        values: &[],
        doc: "time estimate: 30m, 2h, 1d",
    },
    Core {
        name: "blocked-by",
        kind: Kind::Text,
        values: &[],
        doc: "what it waits on — present only while status is blocked (absent, never `none`)",
    },
    Core {
        name: "files",
        kind: Kind::Paths,
        values: &[],
        doc: "repo-relative paths the task reads or produces, comma-separated",
    },
    Core {
        name: "links",
        kind: Kind::Urls,
        values: &[],
        doc: "URLs, comma-separated",
    },
    Core {
        name: "repo",
        kind: Kind::Url,
        values: &[],
        doc: "repository URL the work lands in",
    },
    Core {
        name: "worktree",
        kind: Kind::Text,
        values: &[],
        doc: "worktree path working on it",
    },
    Core {
        name: "conversation",
        kind: Kind::Uuid,
        values: &[],
        doc: "Claude conversation working on it (`hive task link`)",
    },
];

#[derive(Debug, Clone, Serialize)]
pub struct Field {
    pub name: String,
    pub kind: Kind,
    /// Allowed values for an enum (empty = any).
    pub values: Vec<String>,
    pub doc: String,
    pub core: bool,
    pub required: bool,
}

/// The board's full field list: core first in canonical order, then its extensions.
pub fn fields(cfg: &TasksConfig) -> Vec<Field> {
    let mut out: Vec<Field> = CORE
        .iter()
        .map(|c| Field {
            name: c.name.to_string(),
            kind: c.kind,
            values: if c.name == "status" {
                cfg.columns()
            } else {
                c.values.iter().map(|v| v.to_string()).collect()
            },
            doc: c.doc.to_string(),
            core: true,
            required: c.name == "status",
        })
        .collect();
    for (name, def) in &cfg.fields {
        let name = name.to_lowercase();
        let kind = def.kind.as_deref().and_then(Kind::parse);
        let values: Vec<String> = def.values.clone();
        match out.iter_mut().find(|f| f.name == name) {
            Some(f) => {
                if name != "status" {
                    if !values.is_empty() {
                        f.values = values;
                        f.kind = Kind::Enum;
                    }
                    if let Some(k) = kind {
                        f.kind = k;
                    }
                }
                if let Some(d) = &def.doc {
                    f.doc = d.clone();
                }
            }
            None => out.push(Field {
                kind: kind.unwrap_or(if values.is_empty() {
                    Kind::Text
                } else {
                    Kind::Enum
                }),
                name,
                values,
                doc: def.doc.clone().unwrap_or_default(),
                core: false,
                required: false,
            }),
        }
    }
    for r in &cfg.required {
        if let Some(f) = out.iter_mut().find(|f| f.name == r.to_lowercase()) {
            f.required = true;
        }
    }
    out
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Level {
    /// Breaks the schema.
    Error,
    /// Cosmetic (field order).
    Warn,
}

#[derive(Debug, Clone, Serialize)]
pub struct Issue {
    pub level: Level,
    pub msg: String,
}

fn err(msg: String) -> Issue {
    Issue {
        level: Level::Error,
        msg,
    }
}

/// A one-off field no schema declares: any key starting `x-` (`x-reviewer: niko`).
/// Free text on every board, optional, never required — the escape hatch for a value
/// that doesn't merit a schema change. A field worth keeping gets declared under
/// `[projects.<k>.tasks.fields]` instead, which can type and restrict it.
pub fn is_custom(key: &str) -> bool {
    let k = key.to_lowercase();
    k.len() > 2
        && k.starts_with("x-")
        && k.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

fn custom_field(key: &str) -> Field {
    Field {
        name: key.to_lowercase(),
        kind: Kind::Text,
        values: Vec::new(),
        doc: "custom (x-) field".into(),
        core: false,
        required: false,
    }
}

/// A value that says something (not empty, not a `none` / `-` placeholder).
fn check_value_is_real(v: &str) -> bool {
    let v = v.trim();
    !(v.is_empty() || v.eq_ignore_ascii_case("none") || v == "-")
}

fn is_url(s: &str) -> bool {
    (s.starts_with("https://") || s.starts_with("http://")) && !s.contains(' ')
}

fn is_duration(s: &str) -> bool {
    let (n, u) = s.split_at(s.len().saturating_sub(1));
    !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()) && matches!(u, "m" | "h" | "d")
}

fn is_uuid(s: &str) -> bool {
    let p: Vec<&str> = s.split('-').collect();
    p.len() == 5
        && [8, 4, 4, 4, 12]
            .iter()
            .zip(&p)
            .all(|(n, x)| x.len() == *n && x.chars().all(|c| c.is_ascii_hexdigit()))
}

/// The canonical spelling of an enum value (case-insensitive match), if allowed.
pub fn canonical_value(f: &Field, v: &str) -> Option<String> {
    f.values.iter().find(|a| a.eq_ignore_ascii_case(v)).cloned()
}

/// Why `v` is not a valid value for `f` (None = fine).
pub fn check_value(f: &Field, v: &str) -> Option<String> {
    let v = v.trim();
    if v.is_empty() {
        return Some(format!("`{}` is empty — remove the field instead", f.name));
    }
    if v.eq_ignore_ascii_case("none") || v == "-" {
        return Some(format!(
            "`{}: {v}` — leave the field out instead of writing a placeholder",
            f.name
        ));
    }
    match f.kind {
        Kind::Enum => (canonical_value(f, v).is_none())
            .then(|| format!("`{}: {v}` — allowed: {}", f.name, f.values.join(", "))),
        Kind::Bool => {
            (!matches!(v, "yes" | "no")).then(|| format!("`{}: {v}` — yes or no", f.name))
        }
        Kind::Duration => {
            (!is_duration(v)).then(|| format!("`{}: {v}` — like 30m, 2h or 1d", f.name))
        }
        Kind::Url => (!is_url(v)).then(|| format!("`{}: {v}` — an http(s) URL", f.name)),
        Kind::Urls => v.split(',').map(str::trim).find(|u| !is_url(u)).map(|u| {
            format!(
                "`{}`: '{u}' is not an http(s) URL (comma-separated list)",
                f.name
            )
        }),
        Kind::Paths => v
            .split(',')
            .map(str::trim)
            .find(|p| p.is_empty() || is_url(p))
            .map(|p| {
                if p.is_empty() {
                    format!("`{}` has an empty entry", f.name)
                } else {
                    format!("`{}`: '{p}' is a URL — put it in `links`", f.name)
                }
            }),
        Kind::Uuid => (!is_uuid(v)).then(|| format!("`{}: {v}` — a conversation UUID", f.name)),
        Kind::Text => None,
    }
}

/// Header position of a field name in canonical order (unknown → after everything).
fn rank(fs: &[Field], name: &str) -> usize {
    fs.iter()
        .position(|f| f.name == name.to_lowercase())
        .unwrap_or(usize::MAX)
}

/// Every way this header breaks the board's schema.
pub fn validate(cfg: &TasksConfig, header: &[(String, String)], completed: bool) -> Vec<Issue> {
    let fs = fields(cfg);
    let mut out = Vec::new();
    let mut seen: Vec<String> = Vec::new();
    for (k, v) in header {
        let key = k.to_lowercase();
        if seen.contains(&key) {
            out.push(err(format!("`{k}` appears twice")));
        }
        seen.push(key.clone());
        if let Some(to) = cfg.renames.get(&key) {
            out.push(err(format!("legacy field `{k}` — now `{to}`")));
            // The renamed field has its own rules: say now if this value wouldn't survive.
            if let Some(m) = fs
                .iter()
                .find(|f| &f.name == to)
                .and_then(|f| check_value(f, v))
            {
                out.push(err(format!("  …and after the rename: {m}")));
            }
            continue;
        }
        match fs.iter().find(|f| f.name == key) {
            None if is_custom(&key) => {
                if k != &key {
                    out.push(err(format!("`{k}` should be spelled `{key}`")));
                }
                if let Some(m) = check_value(&custom_field(&key), v) {
                    out.push(err(m));
                }
            }
            None => out.push(err(format!(
                "unknown field `{k}` for this board (a one-off value can use an `x-` prefix)"
            ))),
            Some(f) => {
                if k != &f.name {
                    out.push(err(format!("`{k}` should be spelled `{}`", f.name)));
                }
                if let Some(m) = check_value(f, v) {
                    out.push(err(m));
                }
            }
        }
    }
    let has = |n: &str| seen.iter().any(|s| s == n);
    // `blocked-by: none` is already reported as a placeholder; it must not also read as
    // "a reason is set".
    let has_reason = header
        .iter()
        .any(|(k, v)| k.eq_ignore_ascii_case("blocked-by") && check_value_is_real(v));
    for f in fs.iter().filter(|f| f.required) {
        // A field that is still under its legacy name is reported as a rename, not as missing.
        let legacy = cfg
            .renames
            .iter()
            .any(|(old, new)| new == &f.name && seen.contains(old));
        if !has(&f.name) && !legacy {
            out.push(err(format!("missing required field `{}`", f.name)));
        }
    }
    let status = header
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("status"))
        .map(|(_, v)| v.to_lowercase());
    if status.as_deref() == Some("blocked") && !has_reason {
        out.push(err("status is blocked but `blocked-by` says on what".into()));
    }
    if has_reason && status.as_deref() != Some("blocked") {
        out.push(err("`blocked-by` is set but status is not blocked".into()));
    }
    if completed && status.as_deref() != Some("done") && status.is_some() {
        out.push(err("reminder is completed but status is not done".into()));
    }
    if !completed && status.as_deref() == Some("done") {
        out.push(err(
            "status is done but the reminder is not completed".into()
        ));
    }
    let ranks: Vec<usize> = header.iter().map(|(k, _)| rank(&fs, k)).collect();
    if ranks.windows(2).any(|w| w[0] > w[1]) {
        out.push(Issue {
            level: Level::Warn,
            msg: format!(
                "field order — canonical: {}",
                fs.iter()
                    .map(|f| f.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        });
    }
    out
}

/// Check the fields being *written* (not the whole header): what `set`/`add` enforce, so
/// a legacy-dirty task can still be moved without first being cleaned up.
pub fn validate_write(
    cfg: &TasksConfig,
    writes: &[(String, String)],
) -> Result<Vec<(String, String)>, Vec<String>> {
    let fs = fields(cfg);
    let mut errs = Vec::new();
    let mut out = Vec::new();
    for (k, v) in writes {
        let key = k.to_lowercase();
        if let Some(to) = cfg.renames.get(&key) {
            errs.push(format!("`{k}` is a legacy name — use `{to}`"));
            continue;
        }
        if is_custom(&key) && !fs.iter().any(|f| f.name == key) {
            match check_value(&custom_field(&key), v) {
                Some(m) => errs.push(m),
                None => out.push((key, v.trim().to_string())),
            }
            continue;
        }
        let Some(f) = fs.iter().find(|f| f.name == key) else {
            errs.push(format!(
                "unknown field `{k}` — this board has: {} (a one-off value can be named `x-{k}`)",
                fs.iter()
                    .map(|f| f.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
            continue;
        };
        if let Some(m) = check_value(f, v) {
            errs.push(m);
            continue;
        }
        let value = if f.kind == Kind::Enum {
            canonical_value(f, v.trim()).unwrap_or_else(|| v.trim().to_string())
        } else {
            v.trim().to_string()
        };
        out.push((f.name.clone(), value));
    }
    if errs.is_empty() {
        Ok(out)
    } else {
        Err(errs)
    }
}

/// Sort a header into canonical order (stable for unknown fields).
pub fn canonical_order(cfg: &TasksConfig, header: &mut [(String, String)]) {
    let fs = fields(cfg);
    header.sort_by_key(|(k, _)| rank(&fs, k));
}

/// Required fields a new task is still missing (status is supplied by `add`).
pub fn missing_required(cfg: &TasksConfig, header: &[(String, String)]) -> Vec<String> {
    fields(cfg)
        .into_iter()
        .filter(|f| f.required && !header.iter().any(|(k, _)| k.eq_ignore_ascii_case(&f.name)))
        .map(|f| f.name)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::projects::FieldDef;

    fn cfg() -> TasksConfig {
        TasksConfig {
            list: "L".into(),
            ..Default::default()
        }
    }

    fn h(v: &[(&str, &str)]) -> Vec<(String, String)> {
        v.iter()
            .map(|(a, b)| (a.to_string(), b.to_string()))
            .collect()
    }

    fn msgs(c: &TasksConfig, hdr: &[(&str, &str)], done: bool) -> Vec<String> {
        validate(c, &h(hdr), done)
            .into_iter()
            .map(|i| i.msg)
            .collect()
    }

    #[test]
    fn a_clean_header_has_no_issues() {
        let m = msgs(
            &cfg(),
            &[("status", "todo"), ("area", "infra"), ("size", "S")],
            false,
        );
        assert!(m.is_empty(), "{m:?}");
    }

    #[test]
    fn unknown_field_and_bad_values_are_errors() {
        let m = msgs(
            &cfg(),
            &[
                ("status", "todo"),
                ("epc", "x"),
                ("size", "huge"),
                ("est", "soon"),
            ],
            false,
        );
        assert!(m.iter().any(|x| x.contains("unknown field `epc`")));
        assert!(m.iter().any(|x| x.contains("size: huge")));
        assert!(m.iter().any(|x| x.contains("est: soon")));
    }

    #[test]
    fn placeholder_none_is_rejected_for_blocked_by() {
        let m = msgs(&cfg(), &[("status", "todo"), ("blocked-by", "none")], false);
        assert!(m.iter().any(|x| x.contains("leave the field out")), "{m:?}");
    }

    #[test]
    fn blocked_needs_a_reason_and_a_reason_needs_blocked() {
        assert!(msgs(&cfg(), &[("status", "blocked")], false)
            .iter()
            .any(|x| x.contains("says on what")));
        assert!(
            msgs(&cfg(), &[("status", "todo"), ("blocked-by", "x")], false)
                .iter()
                .any(|x| x.contains("not blocked"))
        );
    }

    #[test]
    fn completed_and_done_must_agree() {
        assert!(!msgs(&cfg(), &[("status", "todo")], true).is_empty());
        assert!(!msgs(&cfg(), &[("status", "done")], false).is_empty());
        assert!(msgs(&cfg(), &[("status", "done")], true).is_empty());
    }

    #[test]
    fn legacy_names_are_a_rename_not_unknown_or_missing() {
        let mut c = cfg();
        c.renames.insert("artefact".into(), "files".into());
        c.renames.insert("project".into(), "area".into());
        c.required = vec!["area".into()];
        let m = msgs(
            &c,
            &[("status", "todo"), ("project", "ops"), ("artefact", "a.md")],
            false,
        );
        assert!(m
            .iter()
            .any(|x| x.contains("legacy field `artefact` — now `files`")));
        assert!(!m.iter().any(|x| x.contains("unknown")));
        assert!(!m.iter().any(|x| x.contains("missing required")), "{m:?}");
    }

    #[test]
    fn a_board_narrows_a_core_field_and_adds_its_own() {
        let mut c = cfg();
        c.fields.insert(
            "area".into(),
            FieldDef {
                values: vec!["experiments".into(), "tooling".into()],
                ..Default::default()
            },
        );
        c.fields.insert(
            "log".into(),
            FieldDef {
                kind: Some("bool".into()),
                ..Default::default()
            },
        );
        assert!(msgs(
            &c,
            &[("status", "todo"), ("area", "tooling"), ("log", "yes")],
            false
        )
        .is_empty());
        let m = msgs(
            &c,
            &[("status", "todo"), ("area", "misc"), ("log", "maybe")],
            false,
        );
        assert!(m
            .iter()
            .any(|x| x.contains("allowed: experiments, tooling")));
        assert!(m.iter().any(|x| x.contains("yes or no")));
    }

    #[test]
    fn status_values_are_the_boards_columns() {
        let mut c = cfg();
        c.columns = vec!["todo".into(), "tracking".into(), "done".into()];
        assert!(msgs(&c, &[("status", "tracking")], false).is_empty());
        assert!(!msgs(&c, &[("status", "doing")], false).is_empty());
    }

    #[test]
    fn paths_reject_urls_and_urls_reject_paths() {
        let m = msgs(
            &cfg(),
            &[
                ("status", "todo"),
                ("files", "https://x.dev/a"),
                ("links", "notes/a.md"),
            ],
            false,
        );
        assert!(m.iter().any(|x| x.contains("put it in `links`")));
        assert!(m.iter().any(|x| x.contains("not an http(s) URL")));
    }

    #[test]
    fn field_order_is_a_warning_not_an_error() {
        let v = validate(&cfg(), &h(&[("size", "S"), ("status", "todo")]), false);
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].level, Level::Warn);
    }

    #[test]
    fn writes_canonicalise_enum_case_and_order() {
        let c = cfg();
        let w = validate_write(&c, &h(&[("SIZE", "s"), ("Type", "Review")])).unwrap();
        assert_eq!(w, h(&[("size", "S"), ("type", "review")]));
        let mut hdr = h(&[("size", "S"), ("status", "todo"), ("area", "x")]);
        canonical_order(&c, &mut hdr);
        assert_eq!(
            hdr.iter().map(|x| x.0.as_str()).collect::<Vec<_>>(),
            ["status", "area", "size"]
        );
    }

    #[test]
    fn writes_name_the_allowed_fields_on_a_typo() {
        let e = validate_write(&cfg(), &h(&[("epc", "x")])).unwrap_err();
        assert!(e[0].contains("this board has: status, area, epic"));
    }

    #[test]
    fn x_fields_are_free_text_on_any_board_but_still_no_placeholders() {
        let c = cfg();
        assert!(msgs(&c, &[("status", "todo"), ("x-reviewer", "niko")], false).is_empty());
        let m = msgs(&c, &[("status", "todo"), ("x-reviewer", "none")], false);
        assert!(m.iter().any(|x| x.contains("leave the field out")));
        // "x-" alone and non-x names are not custom
        assert!(!is_custom("x-") && !is_custom("reviewer") && is_custom("X-Reviewer"));
        let w = validate_write(&c, &h(&[("X-Reviewer", " niko ")])).unwrap();
        assert_eq!(w, h(&[("x-reviewer", "niko")]));
        assert!(validate_write(&c, &h(&[("x-reviewer", "")])).is_err());
    }

    #[test]
    fn custom_fields_sort_after_the_schema_and_never_count_as_required() {
        let c = cfg();
        let mut hdr = h(&[("x-a", "1"), ("size", "S"), ("status", "todo")]);
        canonical_order(&c, &mut hdr);
        assert_eq!(
            hdr.iter().map(|x| x.0.as_str()).collect::<Vec<_>>(),
            ["status", "size", "x-a"]
        );
        assert!(missing_required(&c, &hdr).is_empty());
    }

    #[test]
    fn conversation_must_be_a_uuid() {
        let m = msgs(
            &cfg(),
            &[("status", "doing"), ("conversation", "abc")],
            false,
        );
        assert!(m.iter().any(|x| x.contains("conversation UUID")));
    }
}
