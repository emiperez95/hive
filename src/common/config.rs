//! `~/.hive/config.toml` — global hive settings.
//!
//! Currently just the `[web]` block that drives web-server autostart. Kept
//! separate from `projects.toml` (the project registry) so unrelated settings
//! don't churn that file. Loading is best-effort: a missing or malformed file
//! yields defaults (every feature off), never an error.

use serde::Deserialize;

use crate::common::persistence::hive_home;

/// The reserved tmux session name hive uses to host an autostarted web server.
/// Filtered out of every session listing (see `common::tmux`) so it never shows
/// up as a switch/cycle target in the TUI, the web dashboard, or `hive start`.
pub const WEB_SESSION: &str = "__hive_web";

fn default_web_port() -> u16 {
    8375
}

/// The `[web]` block of `config.toml`.
#[derive(Debug, Clone, Deserialize)]
pub struct WebConfig {
    /// Auto-start `hive web` (as the detached `WEB_SESSION` tmux session) the
    /// first time the TUI opens and nothing is already listening on `port`.
    /// Default **off**: it binds `0.0.0.0:<port>`, a LAN-exposed surface, so it
    /// must be opted into explicitly.
    #[serde(default)]
    pub autostart: bool,
    /// Port the autostarted server listens on (matches `hive web --port`).
    #[serde(default = "default_web_port")]
    pub port: u16,
    /// TTS read-aloud host, passed through as `--tts-host`. Off when unset.
    #[serde(default)]
    pub tts_host: Option<String>,
}

impl Default for WebConfig {
    fn default() -> Self {
        WebConfig {
            autostart: false,
            port: default_web_port(),
            tts_host: None,
        }
    }
}

/// Per-million-token rates for one model, as published by the vendor.
///
/// All four classes are separate because they are priced separately and differ by
/// more than an order of magnitude — a cache read is a fraction of a fresh input
/// token, and on a long conversation cache reads dominate the token count while
/// contributing a small share of the bill.
#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq)]
pub struct ModelPrice {
    /// USD per million fresh input tokens.
    #[serde(default)]
    pub input: f64,
    /// USD per million output tokens.
    #[serde(default)]
    pub output: f64,
    /// USD per million tokens written to the prompt cache.
    #[serde(default)]
    pub cache_write: f64,
    /// USD per million tokens read from the prompt cache.
    #[serde(default)]
    pub cache_read: f64,
}

/// The parsed `config.toml`. Unknown tables (e.g. a legacy `[defaults]`) are
/// ignored, so this can grow without disturbing what's already on disk.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct HiveConfig {
    #[serde(default)]
    pub web: WebConfig,
    /// `[pricing.<model>]` blocks. **Ships empty on purpose.**
    ///
    /// Transcripts carry token counts but no cost, so a dollar figure requires a
    /// rate table — and a built-in one would be wrong the moment a model ships or
    /// a price changes, while looking authoritative. Unpriced models report tokens
    /// and omit cost rather than guessing. Keys match a model id exactly, else by
    /// longest substring, so `[pricing.opus]` covers every opus variant.
    #[serde(default)]
    pub pricing: std::collections::BTreeMap<String, ModelPrice>,
}

impl HiveConfig {
    /// The rate for `model`: an exact key, else the longest key contained in the
    /// model id. Longest-match keeps `opus` and `opus-5` unambiguous when both are
    /// configured.
    pub fn price_for(&self, model: &str) -> Option<&ModelPrice> {
        if let Some(p) = self.pricing.get(model) {
            return Some(p);
        }
        self.pricing
            .iter()
            .filter(|(k, _)| !k.is_empty() && model.contains(k.as_str()))
            .max_by_key(|(k, _)| k.len())
            .map(|(_, p)| p)
    }

    /// Cost in USD of one model's spend, or `None` when that model has no rate.
    ///
    /// Each class is priced at its own rate — they differ by more than an order of
    /// magnitude, so applying one blended rate would be arbitrary.
    pub fn cost_of(&self, model: &str, u: &crate::common::usage::ModelUsage) -> Option<f64> {
        let p = self.price_for(model)?;
        let per = |tokens: u64, rate: f64| (tokens as f64) * rate / 1_000_000.0;
        Some(
            per(u.input, p.input)
                + per(u.output, p.output)
                + per(u.cache_write, p.cache_write)
                + per(u.cache_read, p.cache_read),
        )
    }

    /// Per-bucket, per-model cost: `(main, sidechain, unpriced models)`.
    ///
    /// Main and sidechain are priced separately rather than merged so the display
    /// can put a figure on each row — including the subagent rows, which is the
    /// number that says whether fanning out paid for itself.
    #[allow(clippy::type_complexity)]
    pub fn cost_breakdown(
        &self,
        usage: &crate::common::usage::ConversationUsage,
    ) -> (
        std::collections::BTreeMap<String, f64>,
        std::collections::BTreeMap<String, f64>,
        Vec<String>,
    ) {
        let mut main = std::collections::BTreeMap::new();
        let mut side = std::collections::BTreeMap::new();
        let mut unpriced: Vec<String> = Vec::new();

        for (map, out) in [(&usage.main, &mut main), (&usage.sidechain, &mut side)] {
            for (model, u) in map {
                match self.cost_of(model, u) {
                    Some(c) => {
                        out.insert(model.clone(), c);
                    }
                    None if !u.is_empty() && !unpriced.contains(model) => {
                        unpriced.push(model.clone())
                    }
                    None => {}
                }
            }
        }
        unpriced.sort();
        (main, side, unpriced)
    }
}

impl HiveConfig {
    /// Load `~/.hive/config.toml`. Missing file or parse error → defaults.
    pub fn load() -> Self {
        let Some(path) = hive_home().map(|p| p.join("config.toml")) else {
            return Self::default();
        };
        let Ok(content) = std::fs::read_to_string(&path) else {
            return Self::default();
        };
        toml::from_str(&content).unwrap_or_else(|e| {
            eprintln!("Warning: failed to parse {}: {}", path.display(), e);
            Self::default()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_feature_off() {
        let c = HiveConfig::default();
        assert!(!c.web.autostart, "autostart is opt-in");
        assert_eq!(c.web.port, 8375);
        assert!(c.web.tts_host.is_none());
    }

    #[test]
    fn empty_toml_parses_to_defaults() {
        let c: HiveConfig = toml::from_str("").unwrap();
        assert!(!c.web.autostart);
        assert_eq!(c.web.port, 8375);
    }

    #[test]
    fn ignores_unrelated_legacy_tables() {
        // The `[defaults]` block older installs carry must not break parsing.
        let c: HiveConfig = toml::from_str(
            "[defaults]\nprojects_dir = \"~/x\"\nemoji = \"📁\"\n\n[web]\nautostart = true\n",
        )
        .unwrap();
        assert!(c.web.autostart);
        assert_eq!(c.web.port, 8375, "port falls back to its default");
    }

    #[test]
    fn pricing_defaults_to_empty_so_nothing_is_guessed() {
        let c = HiveConfig::default();
        assert!(c.pricing.is_empty());
        assert!(c.price_for("claude-opus-5").is_none());
    }

    #[test]
    fn price_lookup_prefers_exact_then_longest_substring() {
        let c: HiveConfig = toml::from_str(
            "[pricing.opus]\ninput = 1.0\n\n[pricing.\"claude-opus-5\"]\ninput = 2.0\n\n[pricing.sonnet]\ninput = 3.0\n",
        )
        .unwrap();
        assert_eq!(
            c.price_for("claude-opus-5").unwrap().input,
            2.0,
            "exact wins"
        );
        assert_eq!(
            c.price_for("claude-opus-4-1").unwrap().input,
            1.0,
            "falls back to the family key"
        );
        assert_eq!(c.price_for("claude-sonnet-5").unwrap().input, 3.0);
        assert!(c.price_for("claude-fable-5-1").is_none());
    }

    #[test]
    fn cost_prices_each_class_separately() {
        let c: HiveConfig = toml::from_str(
            "[pricing.m]\ninput = 10.0\noutput = 100.0\ncache_write = 20.0\ncache_read = 1.0\n",
        )
        .unwrap();
        let mut u = crate::common::usage::ConversationUsage::default();
        crate::common::usage::accumulate_line(
            r#"{"type":"assistant","message":{"model":"m","usage":{"input_tokens":1000000,"output_tokens":1000000,"cache_creation_input_tokens":1000000,"cache_read_input_tokens":1000000}}}"#,
            &mut u,
        );
        let (main, side, unpriced) = c.cost_breakdown(&u);
        assert_eq!(main.get("m"), Some(&131.0));
        assert!(side.is_empty());
        assert!(unpriced.is_empty());
    }

    #[test]
    fn sidechain_is_priced_on_its_own_row() {
        let c: HiveConfig = toml::from_str("[pricing.m]\noutput = 10.0\n").unwrap();
        let mut u = crate::common::usage::ConversationUsage::default();
        crate::common::usage::accumulate_line(
            r#"{"type":"assistant","message":{"model":"m","usage":{"output_tokens":1000000}}}"#,
            &mut u,
        );
        crate::common::usage::accumulate_line(
            r#"{"type":"assistant","isSidechain":true,"message":{"model":"m","usage":{"output_tokens":2000000}}}"#,
            &mut u,
        );
        let (main, side, _) = c.cost_breakdown(&u);
        assert_eq!(main.get("m"), Some(&10.0));
        assert_eq!(side.get("m"), Some(&20.0), "subagent spend is priced apart");
    }

    #[test]
    fn unpriced_models_are_named_not_counted_as_zero() {
        let c: HiveConfig = toml::from_str("[pricing.known]\noutput = 10.0\n").unwrap();
        let mut u = crate::common::usage::ConversationUsage::default();
        for model in ["known", "mystery"] {
            crate::common::usage::accumulate_line(
                &format!(
                    r#"{{"type":"assistant","message":{{"model":"{model}","usage":{{"output_tokens":1000000}}}}}}"#
                ),
                &mut u,
            );
        }
        let (main, _, unpriced) = c.cost_breakdown(&u);
        assert_eq!(
            main.get("known"),
            Some(&10.0),
            "the priced model still reports"
        );
        assert!(
            !main.contains_key("mystery"),
            "no zero stands in for a missing rate"
        );
        assert_eq!(unpriced, vec!["mystery"], "the gap is named, not hidden");
    }

    #[test]
    fn cost_is_none_when_nothing_can_be_priced() {
        let c = HiveConfig::default();
        let mut u = crate::common::usage::ConversationUsage::default();
        crate::common::usage::accumulate_line(
            r#"{"type":"assistant","message":{"model":"x","usage":{"output_tokens":5}}}"#,
            &mut u,
        );
        let (main, side, unpriced) = c.cost_breakdown(&u);
        assert!(
            main.is_empty() && side.is_empty(),
            "no cost is reported at all"
        );
        assert_eq!(unpriced, vec!["x"]);
    }

    #[test]
    fn web_block_roundtrips() {
        let c: HiveConfig = toml::from_str(
            "[web]\nautostart = true\nport = 9000\ntts_host = \"http://10.0.0.2:9800\"\n",
        )
        .unwrap();
        assert!(c.web.autostart);
        assert_eq!(c.web.port, 9000);
        assert_eq!(c.web.tts_host.as_deref(), Some("http://10.0.0.2:9800"));
    }
}
