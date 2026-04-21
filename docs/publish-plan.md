# Publish Plan

Working document for the "make hive public-ready" effort. A fresh session can pick any unchecked item by reading `CLAUDE.md` then jumping into the section.

## Launch scope

**v0.1.0 is macOS-only.** Linux is deferred.

- Signature features (Chrome tab matching, iTerm2 spread/collapse, port detection) are mac-only via JXA/AppleScript/libproc. Linux gets `#[cfg]` stubs.
- Linux CI job stays as a compile gate; no Linux tarballs in the release.
- README, Cargo.toml `categories`, announcement copy all assume macOS.

## Ready-to-tag status (2026-04-21)

**The repo is tag-ready for `v0.1.0`.** Everything in sections A1–A4 blockers, B, C1, D1–D3, E1, G1 is done. What remains is either non-blocking polish (E2/E3/F), nice-to-have follow-ups (A3 `--dry-run`, A7 test harness, A8/A9), or the tag itself (H).

Recent commits on `origin/main`:

- `f6d3ddc` Polish README for public launch (§E1)
- `d601ecf` Harden setup/uninstall safety (§A3)
- `5a7029b` Expand Cargo.toml metadata + add issue/PR templates (§C1, §G)
- `5fb1cef` Polish onboarding: setup footer, empty-state TUI, hive web docs (§A3, §A4, §E1)
- `61f7061` Ship prebuilt-binary install path; drop Rust requirement (§D1, §D2, §D3)
- `457e2c6` Trim release workflow to macOS-only targets (§D1)
- `78bd747` Add publish plan + expand gitignore

See [publish-plan-a5-findings.md](./publish-plan-a5-findings.md) for the 42-finding audit that drove most of this.

## What's left before the v0.1.0 tag (by severity)

**None of these are blockers.** They can ship under `v0.1.0` or land as patch releases.

### Nice-to-have before announcement

- [ ] **E3 — demo GIF** of the TUI. First impression lever. Capture with `asciinema` → `agg`, store at `assets/demo.gif`, embed at the top of the README.
- [ ] **E2 — `CHANGELOG.md`** seeded with `v0.1.0` entry. Let the GitHub auto-release-notes do the initial body; just add our own summary on top.
- [ ] **A1 — dependency pre-flight** in `hive setup`. Fail with a clear message when `tmux` or `claude` CLI isn't on PATH instead of silently half-working.
- [ ] **G — social preview image** (1280×640) uploaded via GitHub settings.

### Background polish (ship later)

- [ ] **E2 — CONTRIBUTING.md, SECURITY.md, docs/troubleshooting.md, docs/hooks.md**. Useful once there are external contributors / questions.
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
- [ ] **B — `gitleaks detect`** secret scan once before announcing widely.

### Tagging (one-way)

- [ ] **H — tag `v0.1.0`** + push → release workflow builds mac tarballs → public release lands.
- [ ] **H — announce**: Claude Code Discord/community, `r/ClaudeAI`, optional `r/rust` + HN Show once the GIF is ready.

---

## Reference — section-by-section status

### A — First-time install & setup

- **A1. Dependency pre-flight**
  - [x] Prerequisites documented in README.
  - [x] `install.sh` runs `hive --version` after copy (§D2).
  - [x] Always print "Next steps" block even on PATH warning.
  - [ ] `hive setup` early check for `tmux`/`claude` CLI missing.
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

### B — Repo hygiene

- [x] Scrub PII from `.claude/agents/janus-wt-portal.md`.
- [x] `.gitignore` expanded (CLAUDE.local.md, .claude/settings.local.json, .claude/commands/hive/).
- [x] `CLAUDE.local.md` already untracked (gitignored, file exists on disk but isn't shipped).
- [x] `.obsidian-page.md` confirmed not in `git ls-files`.
- [ ] `gitleaks detect --source .` sweep before announcing.

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

- **E1. README** — ✅ done. Badges, Prerequisites, Quick Start, Install (prebuilt + source), `hive web` section, `--yes` / `--debug` / `HIVE_NO_NOTIFY` in System, Advanced section with auth-profiles link, tmux binding caveat, session glossary, mac-only platform note, dropped contributor-facing "Adding Terminal Support" dev docs.
- **E2. New docs files** — none done yet.
  - [ ] `CONTRIBUTING.md`
  - [ ] `CHANGELOG.md` (seed with `v0.1.0`)
  - [ ] `SECURITY.md`
  - [ ] `docs/troubleshooting.md` (hooks not firing, tmux binding conflicts, absolute-path note from §A3)
  - [ ] `docs/hooks.md` (worktree lifecycle hook contract — currently only in CLAUDE.md)
- **E3. Demo GIF** — open, highest-value polish.

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

- [ ] `CHANGELOG.md` with v0.1.0 entry (see E2).
- [ ] Tag + push `v0.1.0` — fires release workflow, produces public release with mac tarballs.
- [ ] `cargo publish` — only if §C3 done.
- [ ] Announce: Claude Code Discord/community, `r/ClaudeAI`, `r/rust`, optional HN Show (needs demo GIF).

---

## Suggested next steps

Pick whichever gives you the biggest boost:

1. **Tag `v0.1.0` now** — everything that needs to be in the first public release is in. The polish items (GIF, CONTRIBUTING, CHANGELOG, social preview) can ship as `v0.1.1` / a patch or just as main updates visible on the repo.
2. **Capture the demo GIF** first (E3) — the single-highest-leverage polish item before announcing to a wider audience. Takes 10–15 minutes with asciinema + agg.
3. **Seed `CHANGELOG.md`** (E2) — small, produces a cleaner release page on tag.
4. **Drop a `hive doctor` / pre-flight check** (A1) — the last interactive-setup rough edge for first-time users.

## Notes for future Claude sessions

- Owner is Emiliano Perez. Before user-visible changes (tone, naming) sanity-check with them.
- Don't push tags or run `cargo publish` without explicit confirmation — one-way doors.
- `CLAUDE.md` at repo root is the architecture bible — read it before touching code.
- `/tmp/hive-a5/env.sh` sets up an isolated `HOME` + tmux socket for smoke-testing install/setup flows.
- **Never run unscoped `tmux kill-server`** — it kills the user's real server. Always prefix with `TMUX_TMPDIR=<isolated>` AND `unset TMUX` on the same line, or use `tmux -L <socket>`.
- This file should be archived (or deleted) once publishing is done.
