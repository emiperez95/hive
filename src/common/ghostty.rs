//! What hive wants from a Ghostty config — which it reads and never writes.
//!
//! The sidebar workflow lives half in Ghostty: the terminal has to open tmux with the sidebar
//! beside it, and keep that split readable. Neither is something hive can do from outside, and
//! a hand-curated terminal config is the wrong place for an installer to write unasked, so
//! `hive setup` only *inspects* it and prints what is missing.
//!
//! Two lines are hive-specific:
//!
//! - `initial-command = … hive start --sidebar` — the first window attaches tmux with the
//!   sidebar beside it (new windows, tabs and splits stay plain shells).
//! - `unfocused-split-opacity = 1` — the sidebar is usually the split without focus, and
//!   Ghostty's default of 0.7 dims it.
//!
//! Optionally, ⌘⇧ chords that send hive's tmux root keys. `Ctrl+n` / `Ctrl+p` / `Ctrl+g` /
//! `Ctrl+\` already reach tmux as typed, so these are a convenience (the author's iTerm2 map
//! carried them over), never a requirement. The control characters are derived from tmux's
//! live root table, not from a second list, so they cannot drift from what is bound.
//!
//! Everything here is a pure function over text, so it is tested without Ghostty or tmux.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// Ghostty is installed where the macOS app normally lives.
pub fn installed(home: &Path) -> bool {
    cfg!(target_os = "macos")
        && (Path::new("/Applications/Ghostty.app").exists()
            || home.join("Applications").join("Ghostty.app").exists())
}

/// The files Ghostty loads, lowest priority first (later ones override earlier ones).
fn config_paths(home: &Path) -> Vec<PathBuf> {
    let xdg = home.join(".config").join("ghostty");
    let app = home
        .join("Library")
        .join("Application Support")
        .join("com.mitchellh.ghostty");
    vec![
        xdg.join("config"),
        xdg.join("config.ghostty"),
        app.join("config"),
        app.join("config.ghostty"),
    ]
}

/// Every config file Ghostty would load, concatenated in load order, and the file a missing
/// line belongs in: the highest-priority one that exists, else the one the macOS app reads.
pub fn read_config(home: &Path) -> (String, PathBuf) {
    let paths = config_paths(home);
    let mut text = String::new();
    let mut target = None;
    for p in &paths {
        if let Ok(s) = std::fs::read_to_string(p) {
            text.push_str(&s);
            text.push('\n');
            target = Some(p.clone());
        }
    }
    let target = target.unwrap_or_else(|| paths.last().cloned().unwrap_or_default());
    (text, target)
}

/// `key = value` lines, comments and blanks dropped, one pair of quotes stripped. Ghostty
/// has only whole-line comments, so a `#` inside a value is part of the value.
fn settings<'a>(text: &'a str) -> impl Iterator<Item = (&'a str, &'a str)> + 'a {
    text.lines().filter_map(|l| {
        let l = l.trim();
        if l.is_empty() || l.starts_with('#') {
            return None;
        }
        let (k, v) = l.split_once('=')?;
        Some((k.trim(), v.trim().trim_matches('"')))
    })
}

/// A setting's effective value: Ghostty applies them in order, so the last one wins.
fn last<'a>(text: &'a str, key: &str) -> Option<&'a str> {
    settings(text)
        .filter(|(k, _)| *k == key)
        .map(|(_, v)| v)
        .last()
}

/// Does the first window run `hive start --sidebar`? Tolerates a path, a `direct:` / `shell:`
/// prefix and a wrapper (`env PATH=… hive start --sidebar`).
fn launches_sidebar(text: &str) -> bool {
    let Some(cmd) = last(text, "initial-command") else {
        return false;
    };
    let cmd = cmd
        .strip_prefix("direct:")
        .or_else(|| cmd.strip_prefix("shell:"))
        .unwrap_or(cmd);
    let words: Vec<&str> = cmd.split_whitespace().collect();
    let Some(at) = words
        .iter()
        .position(|w| *w == "hive" || w.ends_with("/hive"))
    else {
        return false;
    };
    words.get(at + 1) == Some(&"start") && words[at + 2..].contains(&"--sidebar")
}

/// Ghostty dims the split without focus to 0.7 unless told otherwise.
fn sidebar_undimmed(text: &str) -> bool {
    last(text, "unfocused-split-opacity")
        .and_then(|v| v.parse::<f32>().ok())
        .is_some_and(|v| v >= 1.0)
}

/// The control bytes some `keybind = <chord>=text:\xNN` already sends.
fn typed_ctrl_bytes(text: &str) -> HashSet<u8> {
    settings(text)
        .filter(|(k, _)| *k == "keybind")
        .filter_map(|(_, v)| {
            let action = v.split_once("=text:")?.1.trim().trim_matches('"');
            let hex = action.strip_prefix("\\x")?;
            if hex.len() == 2 {
                u8::from_str_radix(hex, 16).ok()
            } else {
                None
            }
        })
        .collect()
}

/// The byte a tmux `C-x` key arrives as: `C-n` is 0x0e, `C-\` is 0x1c.
fn ctrl_byte(tmux_key: &str) -> Option<u8> {
    let mut chars = tmux_key.strip_prefix("C-")?.chars();
    let (ch, rest) = (chars.next()?, chars.next());
    if rest.is_some() || !ch.is_ascii() {
        return None;
    }
    let b = ch as u8;
    (0x40..=0x7f).contains(&b).then_some(b & 0x1f)
}

/// The tmux root-table keys bound to hive, from `tmux list-keys` output. Those are the keys a
/// terminal chord would have to send; prefix-table bindings (`prefix s`) need no chord.
pub fn hive_root_keys(list_keys: &str) -> Vec<String> {
    let mut keys = Vec::new();
    for line in list_keys.lines().filter(|l| l.contains("hive")) {
        let mut words = line.split_whitespace();
        while let Some(w) = words.next() {
            if w == "-T" {
                if words.next() == Some("root") {
                    if let Some(k) = words.next() {
                        // tmux prints a backslash key as `C-\\`.
                        keys.push(k.replace("\\\\", "\\"));
                    }
                }
                break;
            }
        }
    }
    keys.sort();
    keys.dedup();
    keys
}

/// The ⌘⇧ chord suggested for a tmux key, and what it does. Only a suggestion: a key with no
/// entry here simply gets none, which is the safe way for a new binding to be missing.
fn chord_for(tmux_key: &str) -> Option<(&'static str, &'static str)> {
    match tmux_key {
        "C-n" => Some(("cmd+shift+bracket_right", "next session")),
        "C-p" => Some(("cmd+shift+bracket_left", "previous session")),
        "C-\\" => Some(("cmd+shift+backslash", "next window in the session")),
        "C-g" => Some(("cmd+shift+quote", "next free conversation")),
        _ => None,
    }
}

/// An optional chord that would send one of hive's tmux keys.
#[derive(Debug, PartialEq, Eq)]
pub struct Chord {
    pub trigger: &'static str,
    pub label: &'static str,
    pub byte: u8,
}

/// What a config does and does not say about hive's launch setup.
#[derive(Debug, PartialEq, Eq)]
pub struct Inspection {
    pub launches_sidebar: bool,
    pub sidebar_undimmed: bool,
    /// Chords for tmux root keys that no existing `keybind` already sends.
    pub chords: Vec<Chord>,
}

impl Inspection {
    /// Both hive-specific lines are in place (the chords are optional and never count).
    pub fn essentials_ok(&self) -> bool {
        self.launches_sidebar && self.sidebar_undimmed
    }
}

/// Inspect a Ghostty config against the tmux root keys hive has bound.
pub fn inspect(config: &str, tmux_root_keys: &[String]) -> Inspection {
    let typed = typed_ctrl_bytes(config);
    let chords = tmux_root_keys
        .iter()
        .filter_map(|k| {
            let byte = ctrl_byte(k)?;
            let (trigger, label) = chord_for(k)?;
            (!typed.contains(&byte)).then_some(Chord {
                trigger,
                label,
                byte,
            })
        })
        .collect();
    Inspection {
        launches_sidebar: launches_sidebar(config),
        sidebar_undimmed: sidebar_undimmed(config),
        chords,
    }
}

/// The lines to add, ready to paste. Only what is missing: nothing for a setting that is
/// already right, and the optional chords only alongside a missing essential (a config that
/// has the essentials is never nagged about shortcuts).
pub fn snippet(hive_bin: &str, i: &Inspection) -> String {
    let mut out = String::new();
    if !i.launches_sidebar {
        out.push_str(
            "# On launch, the first window attaches tmux with the hive sidebar beside it.\n\
             # Only the first window: new windows, tabs and splits stay plain shells.\n",
        );
        out.push_str(&format!("initial-command = {hive_bin} start --sidebar\n"));
    }
    if !i.sidebar_undimmed {
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(
            "# No dimming on the split without focus (the sidebar is usually that split).\n\
             unfocused-split-opacity = 1\n",
        );
    }
    if !i.essentials_ok() && !i.chords.is_empty() {
        out.push_str(
            "\n# Optional: chords that send hive's tmux keys. Ctrl+n, Ctrl+p, Ctrl+g and Ctrl+\\\n\
             # already work as typed; these are shortcuts, not requirements.\n",
        );
        for c in &i.chords {
            out.push_str(&format!(
                "# {}\nkeybind = {}=text:\\x{:02x}\n",
                c.label, c.trigger, c.byte
            ));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    /// A real, working config (the author's): launch command, undimmed sidebar, and ⌘⇧
    /// chords for all four tmux root keys.
    const WORKING: &str = r#"
# Ported from iTerm2's global key map (hive ghostty-support branch).

# Shift+Enter → newline (multi-line prompts in Claude Code)
keybind = shift+enter=text:\n

# Move between Ghostty split panes
keybind = cmd+shift+right=goto_split:next
keybind = cmd+shift+left=goto_split:previous

# tmux / hive navigation — each sends the control key bound in ~/.tmux.conf
keybind = cmd+shift+backslash=text:\x1c
keybind = cmd+shift+bracket_right=text:\x0e
keybind = cmd+shift+backquote=text:\x0e
keybind = cmd+shift+bracket_left=text:\x10
keybind = cmd+shift+quote=text:\x07

# On launch, the first window attaches tmux with the hive sidebar beside it.
# Only the first window: new windows, tabs and splits stay plain shells.
initial-command = /Users/someone/.local/bin/hive start --sidebar

theme = dark:Catppuccin Frappe,light:Catppuccin Latte

# No dimming on the split without focus (the sidebar is usually that split).
unfocused-split-opacity = 1
"#;

    /// `tmux list-keys` as tmux prints it: two backslashes for the `\` key.
    const LIST_KEYS: &str = r#"bind-key    -T prefix a       display-popup -E -h "70%" -w "80%" "hive --project-detail"
bind-key    -T prefix s       display-popup -E -h "70%" -w "80%" hive
bind-key  -T root C-\\                      run-shell "hive window-next --pane #{pane_id}"
bind-key  -T root C-g                       run-shell "hive cycle-free --pane #{pane_id}"
bind-key  -T root C-n                       run-shell "hive cycle-next --pane #{pane_id}"
bind-key  -T root C-p                       run-shell "hive cycle-prev --pane #{pane_id}"
bind-key  -T root MouseDown1Pane            select-pane -t = ; send-keys -M
bind-key  -T root C-x                       run-shell "something-else"
"#;

    fn keys() -> Vec<String> {
        hive_root_keys(LIST_KEYS)
    }

    #[test]
    fn a_working_config_needs_nothing() {
        let i = inspect(WORKING, &keys());
        assert!(i.launches_sidebar);
        assert!(i.sidebar_undimmed);
        assert!(i.chords.is_empty(), "every root key already has a chord");
        assert!(i.essentials_ok());
        assert_eq!(snippet("/bin/hive", &i), "");
    }

    #[test]
    fn an_empty_config_is_told_everything() {
        let i = inspect("", &keys());
        assert!(!i.launches_sidebar && !i.sidebar_undimmed && !i.essentials_ok());
        assert_eq!(i.chords.len(), 4);
        let s = snippet("/opt/bin/hive", &i);
        assert!(
            s.contains("initial-command = /opt/bin/hive start --sidebar\n"),
            "{s}"
        );
        assert!(s.contains("unfocused-split-opacity = 1\n"), "{s}");
        // The control byte comes from the tmux key, never from a second list.
        assert!(
            s.contains("keybind = cmd+shift+bracket_right=text:\\x0e\n"),
            "{s}"
        );
        assert!(
            s.contains("keybind = cmd+shift+bracket_left=text:\\x10\n"),
            "{s}"
        );
        assert!(
            s.contains("keybind = cmd+shift+backslash=text:\\x1c\n"),
            "{s}"
        );
        assert!(s.contains("keybind = cmd+shift+quote=text:\\x07\n"), "{s}");
    }

    #[test]
    fn only_what_is_missing_is_printed() {
        let launch_only = "initial-command = hive start --sidebar\n";
        let s = snippet("hive", &inspect(launch_only, &keys()));
        assert!(!s.contains("initial-command"), "{s}");
        assert!(s.contains("unfocused-split-opacity = 1"), "{s}");

        let dim_only = "unfocused-split-opacity = 1\n";
        let s = snippet("hive", &inspect(dim_only, &keys()));
        assert!(s.contains("initial-command"), "{s}");
        assert!(!s.contains("unfocused-split-opacity"), "{s}");
    }

    #[test]
    fn chords_are_never_pushed_on_a_config_that_has_the_essentials() {
        let essentials = "initial-command = hive start --sidebar\nunfocused-split-opacity = 1\n";
        let i = inspect(essentials, &keys());
        assert!(i.essentials_ok());
        assert_eq!(
            i.chords.len(),
            4,
            "they are known, just not worth mentioning"
        );
        assert_eq!(snippet("hive", &i), "");
    }

    #[test]
    fn a_chord_is_only_suggested_for_a_key_nothing_sends_yet() {
        let one = "keybind = cmd+shift+bracket_right=text:\\x0e\n";
        let i = inspect(one, &keys());
        assert_eq!(i.chords.len(), 3, "C-n is covered");
        assert!(i.chords.iter().all(|c| c.byte != 0x0e));
    }

    #[test]
    fn the_last_setting_wins() {
        let overridden = "initial-command = hive start --sidebar\ninitial-command = zsh\n";
        assert!(!launches_sidebar(overridden));
        let restored = "initial-command = zsh\ninitial-command = hive start --sidebar\n";
        assert!(launches_sidebar(restored));
        assert!(!sidebar_undimmed(
            "unfocused-split-opacity = 1\nunfocused-split-opacity = 0.7\n"
        ));
        assert!(sidebar_undimmed(
            "unfocused-split-opacity = 0.7\nunfocused-split-opacity = 1.0\n"
        ));
    }

    #[test]
    fn a_commented_out_setting_is_not_a_setting() {
        assert!(!launches_sidebar(
            "# initial-command = hive start --sidebar\n"
        ));
        assert!(!sidebar_undimmed("# unfocused-split-opacity = 1\n"));
        assert!(typed_ctrl_bytes("# keybind = a=text:\\x0e\n").is_empty());
    }

    #[test]
    fn the_launch_command_is_recognised_in_its_usual_forms() {
        for ok in [
            "initial-command = hive start --sidebar",
            "initial-command = /Users/x/.local/bin/hive start --sidebar",
            "initial-command = \"/Users/x/.local/bin/hive start --sidebar\"",
            "initial-command = direct:/usr/local/bin/hive start --sidebar",
            "initial-command = shell:hive start --sidebar",
            "initial-command = env PATH=/opt/bin:/usr/bin /opt/bin/hive start --sidebar",
            "initial-command = hive start --sidebar --debug",
        ] {
            assert!(launches_sidebar(ok), "{ok}");
        }
        for bad in [
            "",
            "initial-command = hive start",
            "initial-command = hive --sidebar start",
            "initial-command = hive sidebar",
            "initial-command = tmux new -A",
            "initial-command = nothive start --sidebar",
            "command = hive start --sidebar",
        ] {
            assert!(!launches_sidebar(bad), "{bad}");
        }
    }

    #[test]
    fn opacity_below_one_or_unset_still_dims() {
        assert!(!sidebar_undimmed(""));
        assert!(!sidebar_undimmed("unfocused-split-opacity = 0.7\n"));
        assert!(!sidebar_undimmed("unfocused-split-opacity = nonsense\n"));
        assert!(sidebar_undimmed("unfocused-split-opacity = 1\n"));
    }

    #[test]
    fn a_tmux_ctrl_key_is_the_byte_it_arrives_as() {
        assert_eq!(ctrl_byte("C-n"), Some(0x0e));
        assert_eq!(ctrl_byte("C-g"), Some(0x07));
        assert_eq!(ctrl_byte("C-p"), Some(0x10));
        assert_eq!(ctrl_byte("C-\\"), Some(0x1c));
        assert_eq!(ctrl_byte("C-["), Some(0x1b));
        assert_eq!(ctrl_byte("C-N"), Some(0x0e), "case does not matter");
        for none in ["n", "C-", "C-nn", "C-M-n", "M-n", "F5", "", "C-é"] {
            assert_eq!(ctrl_byte(none), None, "{none}");
        }
    }

    #[test]
    fn only_root_keys_bound_to_hive_are_listed() {
        assert_eq!(keys(), vec!["C-\\", "C-g", "C-n", "C-p"]);
        assert!(hive_root_keys("").is_empty());
        assert!(hive_root_keys("bind-key -T prefix s display-popup hive").is_empty());
    }

    #[test]
    fn a_text_keybind_that_is_not_one_control_byte_is_ignored() {
        let cfg = "keybind = a=text:\\n\nkeybind = b=text:\\x0e\nkeybind = c=text:\\x0e0\nkeybind = d=goto_split:next\n";
        assert_eq!(typed_ctrl_bytes(cfg), HashSet::from([0x0e]));
    }

    static COUNTER: AtomicU64 = AtomicU64::new(0);
    fn temp_home() -> PathBuf {
        let id = COUNTER.fetch_add(1, Ordering::SeqCst);
        let d = std::env::temp_dir().join(format!("hive-ghostty-{}-{}", std::process::id(), id));
        let _ = std::fs::create_dir_all(&d);
        d
    }

    #[test]
    fn config_files_are_read_in_load_order_and_the_highest_is_the_target() {
        let home = temp_home();
        let app = home
            .join("Library")
            .join("Application Support")
            .join("com.mitchellh.ghostty");
        let xdg = home.join(".config").join("ghostty");
        std::fs::create_dir_all(&app).unwrap();
        std::fs::create_dir_all(&xdg).unwrap();
        std::fs::write(xdg.join("config"), "unfocused-split-opacity = 0.5\n").unwrap();
        std::fs::write(app.join("config.ghostty"), "unfocused-split-opacity = 1\n").unwrap();

        let (text, target) = read_config(&home);
        assert_eq!(target, app.join("config.ghostty"));
        assert!(
            sidebar_undimmed(&text),
            "the app-support file overrides the XDG one"
        );
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn with_no_config_the_target_is_where_the_macos_app_reads() {
        let home = temp_home();
        let (text, target) = read_config(&home);
        assert!(text.is_empty());
        assert_eq!(
            target,
            home.join("Library")
                .join("Application Support")
                .join("com.mitchellh.ghostty")
                .join("config.ghostty")
        );
        let _ = std::fs::remove_dir_all(&home);
    }
}
