//! Per-conversation token usage, read incrementally from the transcript.
//!
//! Every assistant entry in a Claude transcript carries `message.usage` with four
//! independently-priced token classes and the model that produced them. Summing
//! them into one "tokens" number would be actively misleading: on a measured
//! conversation here, cache reads outran output tokens by ~340x (160M vs 464k),
//! so a single total is dominated by the cheapest class and says nothing about
//! spend. The four classes are therefore kept apart all the way to the display.
//!
//! **Why this is not part of the scan cache.** `jsonl::scan_all_disk_conversations_cached`
//! reads a bounded head + tail of every transcript; usage needs *every* assistant
//! line. The corpus here is 446MB across 185 transcripts (largest single file
//! 67MB), so folding a full read into the 1s gather is out of the question — and
//! the conversation you're actively using is precisely the one whose 67MB file
//! would be re-read every tick. Instead this is computed **on demand for one
//! conversation** (the focused one) and accumulated **incrementally**: the cache
//! records how many bytes have been folded in, and a later call parses only what
//! was appended since.
//!
//! Transcripts are append-only in practice, but "in practice" is not a guarantee,
//! so the cache also fingerprints the file's head and falls back to a full re-read
//! if it ever changes (see [`usage_for_transcript`]).

use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::hash::{Hash, Hasher};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// The four independently-priced token classes, plus thinking tokens.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelUsage {
    #[serde(default)]
    pub input: u64,
    #[serde(default)]
    pub output: u64,
    #[serde(default)]
    pub cache_write: u64,
    #[serde(default)]
    pub cache_read: u64,
    /// Thinking tokens. **A subset of `output`, not a fifth class** — reported
    /// under `output_tokens_details`, already counted in `output_tokens`. Shown
    /// for insight; never added into a total or priced separately.
    #[serde(default)]
    pub thinking: u64,
}

impl ModelUsage {
    pub fn add(&mut self, other: &ModelUsage) {
        self.input += other.input;
        self.output += other.output;
        self.cache_write += other.cache_write;
        self.cache_read += other.cache_read;
        self.thinking += other.thinking;
    }

    /// Every billed token. Excludes `thinking`, which is part of `output`.
    pub fn total(&self) -> u64 {
        self.input + self.output + self.cache_write + self.cache_read
    }

    pub fn is_empty(&self) -> bool {
        self.total() == 0
    }
}

/// A conversation's usage, split by model and by whether it was spent on the main
/// thread or in a subagent.
///
/// The sidechain split is the point of the whole feature: it is the only number
/// that answers "is fanning out to subagents actually paying for itself", which a
/// merged total silently hides.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConversationUsage {
    /// Main-thread spend, keyed by model id.
    #[serde(default)]
    pub main: BTreeMap<String, ModelUsage>,
    /// Subagent spend (`isSidechain`), keyed by model id.
    #[serde(default)]
    pub sidechain: BTreeMap<String, ModelUsage>,
}

impl ConversationUsage {
    pub fn is_empty(&self) -> bool {
        self.main.is_empty() && self.sidechain.is_empty()
    }

    /// Every model that contributed, main or sidechain.
    pub fn models(&self) -> Vec<&str> {
        let mut v: Vec<&str> = self
            .main
            .keys()
            .chain(self.sidechain.keys())
            .map(String::as_str)
            .collect();
        v.sort_unstable();
        v.dedup();
        v
    }

    /// Everything, collapsed — for a headline figure only.
    pub fn totals(&self) -> ModelUsage {
        let mut t = ModelUsage::default();
        for u in self.main.values().chain(self.sidechain.values()) {
            t.add(u);
        }
        t
    }

    fn entry(&mut self, model: &str, sidechain: bool) -> &mut ModelUsage {
        let map = if sidechain {
            &mut self.sidechain
        } else {
            &mut self.main
        };
        map.entry(model.to_string()).or_default()
    }
}

/// Fold one transcript line into `usage`, if it carries assistant token usage.
///
/// Pure and line-at-a-time so the accumulation is unit-testable without a fixture
/// transcript, and so the incremental path and the full path share one parser.
pub fn accumulate_line(line: &str, usage: &mut ConversationUsage) {
    // Cheap reject first: most lines (user turns, tool results, snapshots) carry no
    // usage at all, and parsing 446MB of JSON to discover that is the slow way.
    if !line.contains("\"usage\"") {
        return;
    }
    let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
        return;
    };
    let Some(message) = v.get("message") else {
        return;
    };
    let Some(u) = message.get("usage") else {
        return;
    };

    // An unlabelled model would silently merge distinct rates under one key, so it
    // gets its own bucket rather than being folded into a neighbour's.
    let model = message
        .get("model")
        .and_then(|m| m.as_str())
        .unwrap_or("unknown");
    let sidechain = v
        .get("isSidechain")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);

    let n = |key: &str| u.get(key).and_then(serde_json::Value::as_u64).unwrap_or(0);
    let thinking = u
        .get("output_tokens_details")
        .and_then(|d| d.get("thinking_tokens"))
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0);

    let spent = ModelUsage {
        input: n("input_tokens"),
        output: n("output_tokens"),
        cache_write: n("cache_creation_input_tokens"),
        cache_read: n("cache_read_input_tokens"),
        thinking,
    };

    // Drop entries that spent nothing. Transcripts carry a `<synthetic>` model for
    // injected placeholder messages (API-error notices and the like) whose usage is
    // all zeros; bucketing it would put a model in the breakdown that cost nothing
    // and — worse — list it as *unpriced*, implying a missing rate where there is
    // no spend to price. A real turn always bills at least one class.
    if spent.total() == 0 {
        return;
    }

    usage.entry(model, sidechain).add(&spent);
}

/// Bump when the parse changes what an unchanged transcript yields — the cache is
/// keyed on a byte offset, which answers "did the file grow?", never "did our
/// reading of it change?". Same trap as `jsonl::SCAN_PARSER_VERSION`.
/// (v2: zero-usage entries, e.g. the `<synthetic>` placeholder model, no longer
/// create a bucket.)
const USAGE_PARSER_VERSION: u32 = 2;

/// How long an unused entry survives. Unlike the scan cache, this file is written
/// one conversation at a time and so never learns that a transcript is gone;
/// without a TTL it would grow forever.
const ENTRY_TTL_DAYS: i64 = 30;

/// Bytes of the transcript head fingerprinted to detect a rewrite rather than an
/// append. Cheap to re-read, and any edit to the opening entries changes it.
const HEAD_FINGERPRINT_BYTES: usize = 8 * 1024;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct UsageEntry {
    /// Bytes already folded into `usage`. Always lands on a line boundary.
    bytes_scanned: u64,
    /// Fingerprint of the first [`HEAD_FINGERPRINT_BYTES`], to catch a rewritten file.
    head_hash: u64,
    usage: ConversationUsage,
    /// RFC3339; drives [`ENTRY_TTL_DAYS`] pruning.
    #[serde(default)]
    last_used: Option<String>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct UsageCache {
    #[serde(default)]
    version: u32,
    #[serde(default)]
    entries: HashMap<String, UsageEntry>,
}

fn usage_cache_path() -> Option<PathBuf> {
    crate::common::persistence::cache_dir().map(|p| p.join("conversation-usage.json"))
}

fn load_usage_cache() -> UsageCache {
    let Some(path) = usage_cache_path() else {
        return UsageCache::default();
    };
    let Ok(content) = fs::read_to_string(&path) else {
        return UsageCache::default();
    };
    let cache: UsageCache = serde_json::from_str(&content).unwrap_or_default();
    if cache.version != USAGE_PARSER_VERSION {
        return UsageCache::default();
    }
    cache
}

fn save_usage_cache(mut cache: UsageCache) {
    let Some(path) = usage_cache_path() else {
        return;
    };
    let cutoff = chrono::Utc::now() - chrono::Duration::days(ENTRY_TTL_DAYS);
    cache.entries.retain(|_, e| match &e.last_used {
        Some(ts) => chrono::DateTime::parse_from_rfc3339(ts)
            .map(|t| t.with_timezone(&chrono::Utc) >= cutoff)
            .unwrap_or(false),
        None => false,
    });
    cache.version = USAGE_PARSER_VERSION;
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    if let Ok(content) = serde_json::to_string(&cache) {
        let tmp = path.with_extension("json.tmp");
        if fs::write(&tmp, &content).is_ok() {
            let _ = fs::rename(&tmp, &path);
        }
    }
}

fn head_fingerprint(path: &Path) -> u64 {
    let Ok(mut f) = fs::File::open(path) else {
        return 0;
    };
    let mut buf = vec![0u8; HEAD_FINGERPRINT_BYTES];
    let Ok(n) = f.read(&mut buf) else {
        return 0;
    };
    buf.truncate(n);
    let mut h = std::collections::hash_map::DefaultHasher::new();
    buf.hash(&mut h);
    h.finish()
}

/// Parse from `offset` to EOF, folding into `usage`.
///
/// Returns the offset of the end of the last **complete** line. A transcript being
/// written to can end mid-line; stopping at the last newline means the partial line
/// is re-read (whole) next time instead of being skipped forever.
fn scan_from(path: &Path, offset: u64, usage: &mut ConversationUsage) -> u64 {
    let Ok(mut file) = fs::File::open(path) else {
        return offset;
    };
    if file.seek(SeekFrom::Start(offset)).is_err() {
        return offset;
    }
    let mut buf = Vec::new();
    if file.read_to_end(&mut buf).is_err() {
        return offset;
    }
    let Some(last_nl) = buf.iter().rposition(|&b| b == b'\n') else {
        return offset; // nothing complete was appended
    };
    let complete = &buf[..=last_nl];
    for line in complete.split(|&b| b == b'\n') {
        if line.is_empty() {
            continue;
        }
        if let Ok(s) = std::str::from_utf8(line) {
            accumulate_line(s, usage);
        }
    }
    offset + complete.len() as u64
}

/// Token usage for one conversation's transcript, cached incrementally.
///
/// Cheap on the warm path (a stat plus an 8KB read when nothing was appended), and
/// proportional to the *appended* bytes otherwise. Falls back to a full re-read
/// when the file shrank or its head changed — i.e. when it was rewritten rather
/// than appended to, which would otherwise leave a permanently wrong total.
pub fn usage_for_transcript(id: &str, path: &Path) -> ConversationUsage {
    let size = fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    let mut cache = load_usage_cache();
    let head_hash = head_fingerprint(path);

    let prior = cache.entries.get(id).filter(|e| {
        // Append-only means: never shrank, and the head is byte-identical.
        e.bytes_scanned <= size && e.head_hash == head_hash
    });

    let (mut usage, from) = match prior {
        Some(e) => (e.usage.clone(), e.bytes_scanned),
        None => (ConversationUsage::default(), 0),
    };

    let bytes_scanned = if from < size {
        scan_from(path, from, &mut usage)
    } else {
        from
    };

    cache.entries.insert(
        id.to_string(),
        UsageEntry {
            bytes_scanned,
            head_hash,
            usage: usage.clone(),
            last_used: Some(chrono::Utc::now().to_rfc3339()),
        },
    );
    save_usage_cache(cache);
    usage
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn assistant_line(model: &str, out: u64, cache_read: u64, sidechain: bool) -> String {
        format!(
            r#"{{"type":"assistant","isSidechain":{sidechain},"message":{{"model":"{model}","usage":{{"input_tokens":2,"output_tokens":{out},"cache_creation_input_tokens":10,"cache_read_input_tokens":{cache_read},"output_tokens_details":{{"thinking_tokens":5}}}}}}}}"#
        )
    }

    #[test]
    fn accumulates_by_model() {
        let mut u = ConversationUsage::default();
        accumulate_line(&assistant_line("claude-opus-5", 100, 1000, false), &mut u);
        accumulate_line(&assistant_line("claude-opus-5", 50, 500, false), &mut u);
        accumulate_line(&assistant_line("sonnet", 7, 3, false), &mut u);

        let opus = u.main.get("claude-opus-5").unwrap();
        assert_eq!(opus.output, 150);
        assert_eq!(opus.cache_read, 1500);
        assert_eq!(opus.input, 4, "input accumulates across both entries");
        assert_eq!(u.main.get("sonnet").unwrap().output, 7);
        assert_eq!(u.models(), vec!["claude-opus-5", "sonnet"]);
    }

    #[test]
    fn sidechain_is_kept_separate() {
        let mut u = ConversationUsage::default();
        accumulate_line(&assistant_line("claude-opus-5", 100, 0, false), &mut u);
        accumulate_line(&assistant_line("claude-opus-5", 900, 0, true), &mut u);

        assert_eq!(u.main.get("claude-opus-5").unwrap().output, 100);
        assert_eq!(u.sidechain.get("claude-opus-5").unwrap().output, 900);
        assert_eq!(u.totals().output, 1000, "totals span both");
    }

    #[test]
    fn thinking_is_not_double_counted() {
        let mut u = ConversationUsage::default();
        accumulate_line(&assistant_line("m", 100, 0, false), &mut u);
        let m = u.main.get("m").unwrap();
        assert_eq!(m.thinking, 5);
        // 2 input + 100 output + 10 cache_write + 0 cache_read — thinking excluded.
        assert_eq!(m.total(), 112);
    }

    #[test]
    fn non_usage_lines_are_ignored() {
        let mut u = ConversationUsage::default();
        accumulate_line(r#"{"type":"user","message":{"content":"hi"}}"#, &mut u);
        accumulate_line("not json at all", &mut u);
        accumulate_line("", &mut u);
        assert!(u.is_empty());
    }

    #[test]
    fn zero_usage_entries_create_no_bucket() {
        let mut u = ConversationUsage::default();
        accumulate_line(
            r#"{"type":"assistant","message":{"model":"<synthetic>","usage":{"input_tokens":0,"output_tokens":0,"cache_creation_input_tokens":0,"cache_read_input_tokens":0}}}"#,
            &mut u,
        );
        assert!(
            u.is_empty(),
            "a placeholder that billed nothing must not appear as an unpriced model"
        );
    }

    #[test]
    fn missing_model_gets_its_own_bucket() {
        let mut u = ConversationUsage::default();
        accumulate_line(
            r#"{"type":"assistant","message":{"usage":{"output_tokens":9}}}"#,
            &mut u,
        );
        assert_eq!(u.main.get("unknown").unwrap().output, 9);
    }

    #[test]
    fn scan_from_stops_at_the_last_complete_line() {
        let dir = std::env::temp_dir().join(format!("hive-usage-{}", std::process::id()));
        let _ = fs::create_dir_all(&dir);
        let path = dir.join("t.jsonl");

        let full = assistant_line("m", 10, 0, false);
        let mut f = fs::File::create(&path).unwrap();
        // One complete line, then a partial one with no trailing newline.
        writeln!(f, "{full}").unwrap();
        write!(f, "{}", &full[..20]).unwrap();
        drop(f);

        let mut u = ConversationUsage::default();
        let end = scan_from(&path, 0, &mut u);
        assert_eq!(u.main.get("m").unwrap().output, 10);
        assert_eq!(
            end,
            full.len() as u64 + 1,
            "offset stops after the newline, so the partial line is re-read next time"
        );

        // Completing that line and resuming from `end` must count it exactly once.
        let mut f = fs::OpenOptions::new().append(true).open(&path).unwrap();
        writeln!(f, "{}", &full[20..]).unwrap();
        drop(f);
        let end2 = scan_from(&path, end, &mut u);
        assert_eq!(
            u.main.get("m").unwrap().output,
            20,
            "counted once, not twice"
        );
        assert_eq!(end2, fs::metadata(&path).unwrap().len());

        let _ = fs::remove_dir_all(&dir);
    }
}
