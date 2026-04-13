//! File persistence for favorites, todos, and session restore.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;

/// Escape newlines for single-line file storage
pub(crate) fn escape_newlines(s: &str) -> String {
    s.replace('\\', "\\\\").replace('\n', "\\n")
}

/// Unescape newlines from file storage
pub(crate) fn unescape_newlines(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some('n') => result.push('\n'),
                Some('\\') => result.push('\\'),
                Some(other) => {
                    result.push('\\');
                    result.push(other);
                }
                None => result.push('\\'),
            }
        } else {
            result.push(c);
        }
    }
    result
}

/// Get the hive home directory: ~/.hive/
pub(crate) fn hive_home() -> Option<PathBuf> {
    dirs::home_dir().map(|p| p.join(".hive"))
}

/// Get the cache directory for hive: ~/.hive/cache/
pub(crate) fn cache_dir() -> Option<PathBuf> {
    hive_home().map(|p| p.join("cache"))
}

/// Get the path to the favorites file
pub fn get_favorites_file_path() -> Option<PathBuf> {
    cache_dir().map(|p| p.join("favorites.txt"))
}

/// Load favorite session names from disk
pub fn load_favorite_sessions() -> HashSet<String> {
    let Some(path) = get_favorites_file_path() else {
        return HashSet::new();
    };
    let Ok(file) = fs::File::open(&path) else {
        return HashSet::new();
    };
    BufReader::new(file)
        .lines()
        .map_while(Result::ok)
        .filter(|l| !l.trim().is_empty())
        .collect()
}

/// Save favorite session names to disk
pub fn save_favorite_sessions(sessions: &HashSet<String>) {
    let Some(path) = get_favorites_file_path() else {
        return;
    };
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    if let Ok(mut file) = fs::File::create(&path) {
        for name in sessions {
            let _ = writeln!(file, "{}", name);
        }
    }
}

/// Get the path to the session todos file
pub fn get_todos_file_path() -> Option<PathBuf> {
    cache_dir().map(|p| p.join("todos.txt"))
}

/// Load session todos from disk (name -> list of todos)
pub fn load_session_todos() -> HashMap<String, Vec<String>> {
    let Some(path) = get_todos_file_path() else {
        return HashMap::new();
    };
    let Ok(file) = fs::File::open(&path) else {
        return HashMap::new();
    };
    let mut todos: HashMap<String, Vec<String>> = HashMap::new();
    for line in BufReader::new(file).lines().map_while(Result::ok) {
        if line.trim().is_empty() {
            continue;
        }
        // Format: "session-name\ttodo text"
        if let Some((name, todo)) = line.split_once('\t') {
            todos
                .entry(name.to_string())
                .or_default()
                .push(unescape_newlines(todo));
        }
    }
    todos
}

/// Save session todos to disk (tab-separated: name\ttodo, one per line)
pub fn save_session_todos(todos: &HashMap<String, Vec<String>>) {
    let Some(path) = get_todos_file_path() else {
        return;
    };
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    if let Ok(mut file) = fs::File::create(&path) {
        for (name, items) in todos {
            for item in items {
                let _ = writeln!(file, "{}\t{}", name, escape_newlines(item));
            }
        }
    }
}

/// Get the path to the restore file for session persistence across restarts
pub fn get_restore_file_path() -> Option<PathBuf> {
    cache_dir().map(|p| p.join("restore.txt"))
}

/// Save restorable session names to disk (only sessions with sesh config)
pub fn save_restorable_sessions(session_names: &[String]) {
    let Some(path) = get_restore_file_path() else {
        return;
    };
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    if let Ok(mut file) = fs::File::create(&path) {
        for name in session_names {
            let _ = writeln!(file, "{}", name);
        }
    }
}

/// Get the path to the auto-approve sessions file
pub fn get_auto_approve_file_path() -> Option<PathBuf> {
    cache_dir().map(|p| p.join("auto-approve.txt"))
}

/// Load auto-approve session names from disk
pub fn load_auto_approve_sessions() -> HashSet<String> {
    let Some(path) = get_auto_approve_file_path() else {
        return HashSet::new();
    };
    let Ok(file) = fs::File::open(&path) else {
        return HashSet::new();
    };
    BufReader::new(file)
        .lines()
        .map_while(Result::ok)
        .filter(|l| !l.trim().is_empty())
        .collect()
}

/// Save auto-approve session names to disk
pub fn save_auto_approve_sessions(sessions: &HashSet<String>) {
    let Some(path) = get_auto_approve_file_path() else {
        return;
    };
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    if let Ok(mut file) = fs::File::create(&path) {
        for name in sessions {
            let _ = writeln!(file, "{}", name);
        }
    }
}

/// Get the path to the muted sessions file
pub fn get_muted_file_path() -> Option<PathBuf> {
    cache_dir().map(|p| p.join("muted.txt"))
}

/// Load muted session names from disk
pub fn load_muted_sessions() -> HashSet<String> {
    let Some(path) = get_muted_file_path() else {
        return HashSet::new();
    };
    let Ok(file) = fs::File::open(&path) else {
        return HashSet::new();
    };
    BufReader::new(file)
        .lines()
        .map_while(Result::ok)
        .filter(|l| !l.trim().is_empty())
        .collect()
}

/// Save muted session names to disk
pub fn save_muted_sessions(sessions: &HashSet<String>) {
    let Some(path) = get_muted_file_path() else {
        return;
    };
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    if let Ok(mut file) = fs::File::create(&path) {
        for name in sessions {
            let _ = writeln!(file, "{}", name);
        }
    }
}

/// Get the path to the skipped sessions file
pub fn get_skipped_file_path() -> Option<PathBuf> {
    cache_dir().map(|p| p.join("skipped.txt"))
}

/// Load skipped session names from disk
pub fn load_skipped_sessions() -> HashSet<String> {
    let Some(path) = get_skipped_file_path() else {
        return HashSet::new();
    };
    let Ok(file) = fs::File::open(&path) else {
        return HashSet::new();
    };
    BufReader::new(file)
        .lines()
        .map_while(Result::ok)
        .filter(|l| !l.trim().is_empty())
        .collect()
}

/// Save skipped session names to disk
pub fn save_skipped_sessions(sessions: &HashSet<String>) {
    let Some(path) = get_skipped_file_path() else {
        return;
    };
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    if let Ok(mut file) = fs::File::create(&path) {
        for name in sessions {
            let _ = writeln!(file, "{}", name);
        }
    }
}

/// Get the path to the global mute flag file
pub fn get_global_mute_path() -> Option<PathBuf> {
    cache_dir().map(|p| p.join("muted-global"))
}

/// Check if global mute is enabled (file existence)
pub fn is_globally_muted() -> bool {
    get_global_mute_path().map(|p| p.exists()).unwrap_or(false)
}

/// Get the path to the completed todos file
pub fn get_completed_todos_file_path() -> Option<PathBuf> {
    cache_dir().map(|p| p.join("todos-done.txt"))
}

/// Load completed todos from disk (name -> list of completed todos)
pub fn load_completed_todos() -> HashMap<String, Vec<String>> {
    let Some(path) = get_completed_todos_file_path() else {
        return HashMap::new();
    };
    let Ok(file) = fs::File::open(&path) else {
        return HashMap::new();
    };
    let mut todos: HashMap<String, Vec<String>> = HashMap::new();
    for line in BufReader::new(file).lines().map_while(Result::ok) {
        if line.trim().is_empty() {
            continue;
        }
        if let Some((name, todo)) = line.split_once('\t') {
            todos
                .entry(name.to_string())
                .or_default()
                .push(unescape_newlines(todo));
        }
    }
    todos
}

/// Save completed todos to disk (tab-separated: name\ttodo, one per line)
pub fn save_completed_todos(todos: &HashMap<String, Vec<String>>) {
    let Some(path) = get_completed_todos_file_path() else {
        return;
    };
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    if let Ok(mut file) = fs::File::create(&path) {
        for (name, items) in todos {
            for item in items {
                let _ = writeln!(file, "{}\t{}", name, escape_newlines(item));
            }
        }
    }
}

/// Rename session names across all persistence files (favorites, skipped, muted, auto-approve,
/// restore, todos, completed todos). Takes a map of old_name → new_name.
pub fn migrate_session_names(renames: &HashMap<String, String>) {
    if renames.is_empty() {
        return;
    }

    // Helper: rename entries in a HashSet-based file
    let migrate_set = |load: fn() -> HashSet<String>, save: fn(&HashSet<String>)| {
        let old = load();
        let new: HashSet<String> = old
            .into_iter()
            .map(|name| renames.get(&name).cloned().unwrap_or(name))
            .collect();
        save(&new);
    };

    migrate_set(load_favorite_sessions, save_favorite_sessions);
    migrate_set(load_skipped_sessions, save_skipped_sessions);
    migrate_set(load_muted_sessions, save_muted_sessions);
    migrate_set(load_auto_approve_sessions, save_auto_approve_sessions);

    // Restore file (Vec-based, one name per line)
    {
        if let Some(path) = get_restore_file_path() {
            if let Ok(content) = fs::read_to_string(&path) {
                let lines: Vec<String> = content
                    .lines()
                    .filter(|l| !l.trim().is_empty())
                    .map(|l| renames.get(l).map(|s| s.as_str()).unwrap_or(l).to_string())
                    .collect();
                if let Ok(mut file) = fs::File::create(&path) {
                    for name in &lines {
                        let _ = writeln!(file, "{}", name);
                    }
                }
            }
        }
    }

    // Todos (tab-separated: name\ttodo)
    let migrate_todos =
        |load: fn() -> HashMap<String, Vec<String>>, save: fn(&HashMap<String, Vec<String>>)| {
            let old = load();
            let new: HashMap<String, Vec<String>> = old
                .into_iter()
                .map(|(name, items)| {
                    let new_name = renames.get(&name).cloned().unwrap_or(name);
                    (new_name, items)
                })
                .collect();
            save(&new);
        };

    migrate_todos(load_session_todos, save_session_todos);
    migrate_todos(load_completed_todos, save_completed_todos);
}

/// Set global mute state (creates or removes the flag file)
pub fn set_global_mute(enabled: bool) {
    let Some(path) = get_global_mute_path() else {
        return;
    };
    if enabled {
        if let Some(parent) = path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        let _ = fs::File::create(&path);
    } else {
        let _ = fs::remove_file(&path);
    }
}

// --- Path-parameterized helpers for testability ---

#[cfg(test)]
/// Load a set of names from a file (one per line)
pub(crate) fn load_set_from(path: &std::path::Path) -> HashSet<String> {
    let Ok(file) = fs::File::open(path) else {
        return HashSet::new();
    };
    BufReader::new(file)
        .lines()
        .map_while(Result::ok)
        .filter(|l| !l.trim().is_empty())
        .collect()
}

#[cfg(test)]
/// Save a set of names to a file (one per line)
pub(crate) fn save_set_to(path: &std::path::Path, sessions: &HashSet<String>) {
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    if let Ok(mut file) = fs::File::create(path) {
        for name in sessions {
            let _ = writeln!(file, "{}", name);
        }
    }
}

#[cfg(test)]
/// Load tab-separated todos from a file (session\ttodo per line)
pub(crate) fn load_todos_from(path: &std::path::Path) -> HashMap<String, Vec<String>> {
    let Ok(file) = fs::File::open(path) else {
        return HashMap::new();
    };
    let mut todos: HashMap<String, Vec<String>> = HashMap::new();
    for line in BufReader::new(file).lines().map_while(Result::ok) {
        if line.trim().is_empty() {
            continue;
        }
        if let Some((name, todo)) = line.split_once('\t') {
            todos
                .entry(name.to_string())
                .or_default()
                .push(unescape_newlines(todo));
        }
    }
    todos
}

#[cfg(test)]
/// Save tab-separated todos to a file (session\ttodo per line)
pub(crate) fn save_todos_to(path: &std::path::Path, todos: &HashMap<String, Vec<String>>) {
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    if let Ok(mut file) = fs::File::create(path) {
        for (name, items) in todos {
            for item in items {
                let _ = writeln!(file, "{}\t{}", name, escape_newlines(item));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    use std::sync::atomic::{AtomicU64, Ordering};
    static TEST_COUNTER: AtomicU64 = AtomicU64::new(0);

    fn temp_dir() -> PathBuf {
        let id = TEST_COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!(
            "hive-test-{}-{}",
            std::process::id(),
            id
        ));
        let _ = fs::create_dir_all(&dir);
        dir
    }

    fn cleanup(dir: &Path) {
        let _ = fs::remove_dir_all(dir);
    }

    // --- escape / unescape ---

    #[test]
    fn test_escape_no_special_chars() {
        assert_eq!(escape_newlines("hello world"), "hello world");
    }

    #[test]
    fn test_escape_newline() {
        assert_eq!(escape_newlines("line1\nline2"), "line1\\nline2");
    }

    #[test]
    fn test_escape_backslash() {
        assert_eq!(escape_newlines("path\\to"), "path\\\\to");
    }

    #[test]
    fn test_escape_both() {
        assert_eq!(escape_newlines("a\\b\nc"), "a\\\\b\\nc");
    }

    #[test]
    fn test_unescape_no_special_chars() {
        assert_eq!(unescape_newlines("hello world"), "hello world");
    }

    #[test]
    fn test_unescape_newline() {
        assert_eq!(unescape_newlines("line1\\nline2"), "line1\nline2");
    }

    #[test]
    fn test_unescape_backslash() {
        assert_eq!(unescape_newlines("path\\\\to"), "path\\to");
    }

    #[test]
    fn test_unescape_trailing_backslash() {
        assert_eq!(unescape_newlines("end\\"), "end\\");
    }

    #[test]
    fn test_unescape_unknown_escape() {
        assert_eq!(unescape_newlines("\\t"), "\\t");
    }

    #[test]
    fn test_escape_unescape_roundtrip() {
        let original = "line1\nline2\\path\nline3";
        assert_eq!(unescape_newlines(&escape_newlines(original)), original);
    }

    #[test]
    fn test_escape_unescape_empty() {
        assert_eq!(unescape_newlines(&escape_newlines("")), "");
    }

    // --- set load/save ---

    #[test]
    fn test_save_and_load_set() {
        let dir = temp_dir();
        let path = dir.join("test-set.txt");

        let mut set = HashSet::new();
        set.insert("session-a".to_string());
        set.insert("session-b".to_string());

        save_set_to(&path, &set);
        let loaded = load_set_from(&path);

        assert_eq!(loaded, set);
        cleanup(&dir);
    }

    #[test]
    fn test_load_set_missing_file() {
        let path = PathBuf::from("/nonexistent/path/file.txt");
        let loaded = load_set_from(&path);
        assert!(loaded.is_empty());
    }

    #[test]
    fn test_save_set_empty() {
        let dir = temp_dir();
        let path = dir.join("empty-set.txt");

        save_set_to(&path, &HashSet::new());
        let loaded = load_set_from(&path);

        assert!(loaded.is_empty());
        cleanup(&dir);
    }

    #[test]
    fn test_load_set_ignores_blank_lines() {
        let dir = temp_dir();
        let path = dir.join("blanks.txt");
        fs::write(&path, "alpha\n\n  \nbeta\n").unwrap();

        let loaded = load_set_from(&path);
        assert_eq!(loaded.len(), 2);
        assert!(loaded.contains("alpha"));
        assert!(loaded.contains("beta"));
        cleanup(&dir);
    }

    #[test]
    fn test_save_set_overwrites() {
        let dir = temp_dir();
        let path = dir.join("overwrite.txt");

        let mut set1 = HashSet::new();
        set1.insert("old".to_string());
        save_set_to(&path, &set1);

        let mut set2 = HashSet::new();
        set2.insert("new".to_string());
        save_set_to(&path, &set2);

        let loaded = load_set_from(&path);
        assert_eq!(loaded.len(), 1);
        assert!(loaded.contains("new"));
        cleanup(&dir);
    }

    // --- todos load/save ---

    #[test]
    fn test_save_and_load_todos() {
        let dir = temp_dir();
        let path = dir.join("todos.txt");

        let mut todos = HashMap::new();
        todos.insert(
            "s1".to_string(),
            vec!["fix bug".to_string(), "write tests".to_string()],
        );
        todos.insert("s2".to_string(), vec!["deploy".to_string()]);

        save_todos_to(&path, &todos);
        let loaded = load_todos_from(&path);

        assert_eq!(loaded["s1"].len(), 2);
        assert!(loaded["s1"].contains(&"fix bug".to_string()));
        assert!(loaded["s1"].contains(&"write tests".to_string()));
        assert_eq!(loaded["s2"], vec!["deploy"]);
        cleanup(&dir);
    }

    #[test]
    fn test_load_todos_missing_file() {
        let path = PathBuf::from("/nonexistent/todos.txt");
        let loaded = load_todos_from(&path);
        assert!(loaded.is_empty());
    }

    #[test]
    fn test_todos_with_newlines_roundtrip() {
        let dir = temp_dir();
        let path = dir.join("todos-newlines.txt");

        let mut todos = HashMap::new();
        todos.insert(
            "s1".to_string(),
            vec!["line1\nline2".to_string(), "simple".to_string()],
        );

        save_todos_to(&path, &todos);
        let loaded = load_todos_from(&path);

        assert_eq!(loaded["s1"].len(), 2);
        assert!(loaded["s1"].contains(&"line1\nline2".to_string()));
        assert!(loaded["s1"].contains(&"simple".to_string()));
        cleanup(&dir);
    }

    #[test]
    fn test_todos_with_backslashes_roundtrip() {
        let dir = temp_dir();
        let path = dir.join("todos-backslash.txt");

        let mut todos = HashMap::new();
        todos.insert(
            "s1".to_string(),
            vec!["path\\to\\file".to_string()],
        );

        save_todos_to(&path, &todos);
        let loaded = load_todos_from(&path);

        assert_eq!(loaded["s1"], vec!["path\\to\\file"]);
        cleanup(&dir);
    }

    #[test]
    fn test_todos_ignores_blank_lines() {
        let dir = temp_dir();
        let path = dir.join("todos-blanks.txt");
        fs::write(&path, "s1\ttodo1\n\n  \ns2\ttodo2\n").unwrap();

        let loaded = load_todos_from(&path);
        assert_eq!(loaded.len(), 2);
        cleanup(&dir);
    }

    #[test]
    fn test_todos_ignores_lines_without_tab() {
        let dir = temp_dir();
        let path = dir.join("todos-notab.txt");
        fs::write(&path, "s1\ttodo1\nbadline\ns2\ttodo2\n").unwrap();

        let loaded = load_todos_from(&path);
        assert_eq!(loaded.len(), 2);
        assert!(!loaded.contains_key("badline"));
        cleanup(&dir);
    }

    // --- set with emoji session names ---

    #[test]
    fn test_set_with_emoji_names() {
        let dir = temp_dir();
        let path = dir.join("emoji-set.txt");

        let mut set = HashSet::new();
        set.insert("🐝 hive".to_string());
        set.insert("🌳 [clear] CSD-2527".to_string());

        save_set_to(&path, &set);
        let loaded = load_set_from(&path);

        assert_eq!(loaded, set);
        cleanup(&dir);
    }

    #[test]
    fn test_todos_with_emoji_session_names() {
        let dir = temp_dir();
        let path = dir.join("emoji-todos.txt");

        let mut todos = HashMap::new();
        todos.insert("🐝 hive".to_string(), vec!["add tests".to_string()]);

        save_todos_to(&path, &todos);
        let loaded = load_todos_from(&path);

        assert_eq!(loaded["🐝 hive"], vec!["add tests"]);
        cleanup(&dir);
    }
}
