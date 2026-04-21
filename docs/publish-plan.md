# Publish Plan

Working document for the "make hive public-ready" effort. A fresh session can pick any unchecked item by reading `CLAUDE.md` then jumping into the section.

## Launch scope

**v0.1.0 is macOS-only.** Linux is deferred.

- Signature features (Chrome tab matching, iTerm2 spread/collapse, port detection) are mac-only via JXA/AppleScript/libproc. Linux gets `#[cfg]` stubs.
- Linux CI job stays as a compile gate; no Linux tarballs in the release.
- README, Cargo.toml `categories`, announcement copy all assume macOS.

## v0.1.0 shipped — 2026-04-21

**`v0.1.0` tagged + released.** → [github.com/emiperez95/hive/releases/tag/v0.1.0](https://github.com/emiperez95/hive/releases/tag/v0.1.0)

- Tag points at `f31395f` on `origin/main`.
- Release workflow produced both mac tarballs:
  - `hive-aarch64-apple-darwin.tar.gz` (1.59 MB)
  - `hive-x86_64-apple-darwin.tar.gz` (1.66 MB)
- Auto-generated release notes include the PR history.
- End-to-end verified: `install.sh` downloads the v0.1.0 tarball via `releases/latest/download/…`, extracts, `hive --version` → `hive 0.1.0`.

### Commits that shipped in v0.1.0

- `f31395f` Refresh publish-plan to reflect shipped work
- `b2a26e4` Add CONTRIBUTING, CHANGELOG, SECURITY, troubleshooting, hooks docs
- `93db016` Warn on missing tmux / claude CLI during hive setup
- `f6d3ddc` Polish README for public launch (§E1)
- `d601ecf` Harden setup/uninstall safety (§A3)
- `5a7029b` Expand Cargo.toml metadata + add issue/PR templates (§C1, §G)
- `5fb1cef` Polish onboarding: setup footer, empty-state TUI, hive web docs (§A3, §A4, §E1)
- `61f7061` Ship prebuilt-binary install path; drop Rust requirement (§D1, §D2, §D3)
- `457e2c6` Trim release workflow to macOS-only targets (§D1)
- `78bd747` Add publish plan + expand gitignore

See [publish-plan-a5-findings.md](./publish-plan-a5-findings.md) for the 42-finding audit that drove most of the pre-tag work.

## What's left — post-tag polish + announcement

### Before the public announcement

- [ ] **E3 — demo GIF** of the TUI. Capture with `asciinema` → `agg`, store at `assets/demo.gif`, embed at the top of the README. Highest-leverage polish item.
- [ ] **G — social preview image** (1280×640) uploaded via GitHub settings.
- [ ] **H — announce**: Claude Code Discord / community forum, `r/ClaudeAI`, optional `r/rust` / HN Show once the GIF is ready.

### Ongoing polish (ship as patches / on main)

- [ ] **F — CI additions**: `cargo publish --dry-run` job, `cargo audit` / `cargo deny`, optionally a shell-based setup smoke test (A7).
- [ ] **A7 — automated setup test** (`tests/setup_smoke.sh` or Rust tempfile-based). The A5 harness at `/tmp/hive-a5/env.sh` is a fine starting point.
- [ ] **A6 — sanity passes**: unicode / spaces in `$HOME`; `CLAUDE_CONFIG_DIR` unset default paths.
- [ ] **A8 — state lifecycle**: verify TUI filters stale state.json entries when tmux session is killed; consider on-launch cleanup (today it's hook-fire-only with 10m TTL).
- [ ] **A9 — `hive connect --detached`** flag for scripted use. Small CLI addition.
- [ ] **A3 leftovers**: `--dry-run` flag (big refactor — print diff without writing), `--help` flag pollution cleanup (clap-derive restructure), preserve user's settings.json key order instead of alphabetizing.
- [ ] **C2 — verified MSRV**. Cargo.toml claims `1.82` but never built against that exact toolchain. `rustup install 1.82.0 && cargo +1.82.0 build` to confirm, bump if needed.
- [ ] **C3 — crates.io name reservation**. Not blocking since we ship via GitHub Releases; nice for `cargo install` discoverability.
- [ ] **D4 — Homebrew tap / AUR / Nix flake**. Distribution reach.
- [ ] **G — enable Discussions**, add `CODE_OF_CONDUCT.md`.

---

## Reference — section-by-section status

### A — First-time install & setup

- **A1. Dependency pre-flight**
  - [x] Prerequisites documented in README.
  - [x] `install.sh` runs `hive --version` after copy (§D2).
  - [x] Always print "Next steps" block even on PATH warning.
  - [x] `hive setup` warns when `tmux` / `claude` CLI missing from PATH.
  - [ ] Optional `hive doctor` subcommand — defer unless users ask.

- **A2. Setup edge-case reference** — behavior summary, not a todo. See §A3 for fixes.
  - ✅ Fresh `~/.claude/` creates parent dirs.
  - ✅ Missing settings.json creates clean one.
  - ✅ Existing unrelated hooks preserved.
  - ✅ Re-run is idempotent.
  - ✅ Malformed JSON → backed up + clear error.
  - ✅ No tmux server → one friendly message + tmux.conf snippets always printed.
  - ⚠️ Setup still reformats settings.json key order (cosmetic, Finding #20).

- **A3. Setup/uninstall safety**
  - [x] `--yes` flag on setup + uninstall.
  - [x] Atomic writes (`.tmp` + rename).
  - [x] Backup to `settings.json.bak` before first mutation.
  - [x] Malformed JSON handled (moved to `.bak.malformed.<ts>`).
  - [x] Tmux-missing message collapsed to one line + snippets always printed.
  - [x] Setup completion footer.
  - [x] Uninstall symmetry (removes create-project, rmdirs empty parents, prints footer).
  - [x] `HIVE_NO_NOTIFY=1` env var.
  - [ ] `--dry-run` flag. Deferred — big refactor.
  - [ ] `hive setup --help` flag pollution (clap-derive restructure).
  - [ ] Preserve settings.json key order (Finding #20).
  - [ ] Document absolute binary path in hooks somewhere (troubleshooting doc, Finding #21).
  - [ ] Add `hive uninstall` to CLAUDE.md command list.

- **A4. Empty-state UX**
  - [x] TUI list-view empty-state message ("No sessions yet. Press N…").
  - [x] `hive start` non-TTY → friendly anyhow error, exit 1, no panic.
  - [ ] Empty-state when projects exist but no sessions are running.
  - [ ] `hive cycle-next` / `cycle-prev` silent exit with no sessions — stderr message.
  - [ ] Verify `hive --debug` writes to `~/.cache/hive/debug.log` for non-TUI commands (or document that it's TUI-only).

- **A5. Cold-install walkthrough — ✅ COMPLETE.** Findings at [publish-plan-a5-findings.md](./publish-plan-a5-findings.md).

- **A6. Mac-only sanity checks**
  - [ ] Unicode / spaces in `$HOME`.
  - [ ] `CLAUDE_CONFIG_DIR` unset → default paths.

- **A7. Automated setup test** — open.

- **A8. State lifecycle** — open (Finding #35).

- **A9. CLI gaps**
  - [ ] `hive connect --detached`.
  - [ ] Document approval-via-send-keys caveat in troubleshooting.

### B — Repo hygiene — ✅ complete

- [x] Scrub PII from `.claude/agents/janus-wt-portal.md`.
- [x] `.gitignore` expanded (CLAUDE.local.md, .claude/settings.local.json, .claude/commands/hive/).
- [x] `CLAUDE.local.md` already untracked (gitignored).
- [x] `.obsidian-page.md` confirmed not in `git ls-files`.
- [x] `gitleaks detect` swept git history (87 commits, 1.04 MB) + working tree (1.22 GB) — zero leaks.

### C — Cargo.toml + crates.io

- **C1. Metadata** — ✅ done. `authors`, `keywords`, `categories`, `homepage`, `documentation`, `readme`, `rust-version`, `exclude`, `cargo publish --dry-run` validated.
- **C2. MSRV**
  - [x] Set to `1.82` (matches `Option::is_none_or` usage).
  - [ ] Verify builds with an actual `1.82.0` toolchain — currently a claim, not proven.
- **C3. Name reservation** — deferred. Not blocking since install flow is GitHub Releases. Revisit if we want `cargo install hive`.

### D — Distribution

- **D1. Release workflow** — ✅ trimmed to mac-only, verified end-to-end with `v0.0.1-test` throwaway tag (both tarballs produced, release created, tag + release deleted).
- **D2. Prebuilt install script** — ✅ `install.sh` rewritten; downloads + extracts + verifies with `hive --version`. Supports `./install.sh v0.1.0` for pinning and `HIVE_INSTALL_DIR` override.
- **D3. `hive update`** — ✅ rewritten to download latest release tarball, rename-replace the current executable, re-run `hive setup`.
- **D4. Homebrew / AUR / Nix** — deferred.

### E — Documentation

- **E1. README** — ✅ done. Badges, Prerequisites, Quick Start, Install (prebuilt + source), `hive web` section, `--yes` / `--debug` / `HIVE_NO_NOTIFY` in System, Advanced section with auth-profiles + hooks-doc links, tmux binding caveat, session glossary, mac-only platform note, Troubleshooting + contributing footer.
- **E2. New docs files** — ✅ shipped.
  - [x] `CONTRIBUTING.md`
  - [x] `CHANGELOG.md` (seeded with `v0.1.0`)
  - [x] `SECURITY.md`
  - [x] `docs/troubleshooting.md` (hooks, tmux persistence, update failures, non-TTY panics, approval gotchas, `~/.hive/` cleanup)
  - [x] `docs/hooks.md` (worktree lifecycle contract — events, env vars, metadata protocol, examples)
- **E3. Demo GIF** — open, highest-value polish before wider announcement.

### F — CI / release workflow

- [x] Release workflow trimmed + verified via dry-run tag.
- [ ] `cargo publish --dry-run` job.
- [ ] `cargo audit` or `cargo deny check`.
- [ ] Shell-based setup smoke test once A7 lands.

### G — GitHub repo polish

- [x] Description + 8 topics set via `gh repo edit`.
- [x] Issue templates (bug.yml, feature.yml, config.yml).
- [x] PR template.
- [ ] Social preview image (1280×640).
- [ ] Enable Discussions.
- [ ] `CODE_OF_CONDUCT.md` (optional).

### H — Launch

- [x] `CHANGELOG.md` seeded with v0.1.0 entry.
- [x] Tag + push `v0.1.0` — release workflow built both mac tarballs and published https://github.com/emiperez95/hive/releases/tag/v0.1.0
- [ ] `cargo publish` — only if §C3 done.
- [ ] Announce: Claude Code Discord/community, `r/ClaudeAI`, `r/rust`, optional HN Show (needs demo GIF).

---

## Suggested next steps

Ordered by leverage, pick whatever you feel like:

1. **Capture the demo GIF** (E3) — single highest-impact polish item; 10–15 min with `asciinema` + `agg`.
2. **Social preview image** (G) — purely GitHub UI, no code.
3. **Announce** (H) — Claude Code community + `r/ClaudeAI` first. Hold `r/rust` / HN until the GIF is ready.
4. **Ongoing**: A7 (test harness), F (CI hardening), A6 / A8 / A9 sanity passes, C2 MSRV verification. None are blockers for additional patch releases.

## Notes for future Claude sessions

- Owner is Emiliano Perez. Before user-visible changes (tone, naming) sanity-check with them.
- Don't push tags or run `cargo publish` without explicit confirmation — one-way doors.
- `CLAUDE.md` at repo root is the architecture bible — read it before touching code.
- `/tmp/hive-a5/env.sh` sets up an isolated `HOME` + tmux socket for smoke-testing install/setup flows.
- **Never run unscoped `tmux kill-server`** — it kills the user's real server. Always prefix with `TMUX_TMPDIR=<isolated>` AND `unset TMUX` on the same line, or use `tmux -L <socket>`.
- This file should be archived (or deleted) once publishing is done.
