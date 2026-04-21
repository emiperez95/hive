# Publish Plan

Working document for the "make hive public-ready" effort. Each item is self-contained so a fresh Claude session can pick it up without prior conversation context — read `CLAUDE.md` in the repo root for architecture context, then jump to a section below.

Check items off as they're completed. Reorder freely.

## Launch scope

**v0.1.0 is macOS-only.** Linux is deferred.

- Signature features (Chrome tab matching, iTerm2 spread/collapse, port detection) are mac-only via JXA/AppleScript/libproc. Linux gets `#[cfg]` stubs.
- Keep Linux CI as a "does it still compile?" gate; don't ship Linux tarballs or advertise Linux support.
- README, Cargo.toml `categories`, announcement copy all assume macOS.

## Current baseline (snapshot)

- Repo is already public at `github.com/emiperez95/hive` (MIT license).
- `Cargo.toml` version `0.1.0`, no git tags yet.
- CI (`.github/workflows/ci.yml`) runs build + test on macOS/Linux + clippy + fmt.
- Release workflow (`.github/workflows/release.yml`) builds 4 targets on `v*` tag push — **never fired**. Needs to be trimmed to mac-only.
- 166 tests pass (`cargo test`): 146 unit + 20 CLI smoke.
- `README.md` has features, install, commands, keybindings. Missing: prereqs, quick-start walkthrough, badges, screenshot. Also **missing `hive web` entirely** (Finding #03).
- `install.sh` builds from source; requires cargo. No prebuilt-binary install path exists.
- `hive update` (`src/cli/update.rs`) uses `cargo install --git` — also requires cargo.
- `hive setup` modifies `~/.claude/settings.json`, installs agent + command. Tmux bindings are **ephemeral** (live server only) and require manual `~/.tmux.conf` edits.
- **A5 cold-install walkthrough complete** → see [publish-plan-a5-findings.md](./publish-plan-a5-findings.md) for 42 findings.

---

## Section A — First-time install & setup (HIGHEST PRIORITY)

The critical gap: no one has ever walked through a cold install on a clean machine. Every item in this section must pass before we announce.

### A1. Dependency pre-flight audit

Findings feeding here: #01, #15, #08.

- [ ] Document all runtime prerequisites in `README.md` under a new "Prerequisites" section:
  - Required: `tmux`, Claude Code CLI, any TTY terminal.
  - Required for source install: Rust 1.XX+ (determine MSRV — see C1), `cargo`.
  - Required for `hive update` today: `cargo` (remove this requirement — see D3).
  - Optional: iTerm2 (spread/collapse), Chrome (tab matching), `terminal-notifier` (notifications fall back to `osascript`).
- [ ] Add early dependency check in `hive setup` — fail with a clear message if `tmux` or the `claude` CLI isn't on `PATH`. Code lives in `src/cli/setup.rs`.
- [ ] Make `install.sh` run `hive --version` after copy to verify the binary works (Finding #13). Always print the "Installation complete" / "Next steps" block even when the PATH warning fires (Finding #14).
- [ ] Optional: `hive doctor` subcommand that reports every dep status without modifying anything. A5 didn't prove it necessary; fold into `hive setup`'s pre-flight instead unless there's pull.

### A2. `hive setup` edge-case audit

A5 confirmed the following behaviors — this is a reference, not a todo:

- ✅ `~/.claude/` missing → setup creates it (parent dirs via `fs::create_dir_all`).
- ✅ `~/.claude/settings.json` missing → setup creates a clean one.
- ✅ Pre-existing unrelated hooks → preserved correctly in separate array entries.
- ✅ Re-run is idempotent for hooks/agent/command.
- ❌ `~/.claude/settings.json` malformed → raw serde error, no recovery (Finding #19).
- ❌ Re-run with no tmux server → 4 duplicate error lines, no guidance (Finding #16).
- ❌ Tmux bindings ephemeral (live server only) → user must edit `~/.tmux.conf` by hand, and the snippets only print on success (Findings #05, #17).
- ❌ Setup reformats settings.json top-to-bottom (alphabetizes, moves `model`) (Finding #20).

Follow-up items (fixes live in A3):

- [ ] Handle malformed settings.json gracefully (backup + rewrite, or error with filepath+hint). Finding #19.
- [ ] Decide: skip / version-check the agent file — don't silently overwrite.
- [ ] Preserve user's existing settings.json key order where possible (Finding #20).

### A3. `hive setup` / `hive uninstall` safety hardening

Findings feeding here: #05, #16, #17, #18, #19, #20, #21, #22, #23, #32, #38, #39, #40, #41, #42.

**Setup:**
- [ ] Back up `~/.claude/settings.json` to `.bak` (or timestamped `.bak.YYYYMMDD`) before first modification (Finding #19).
- [ ] Add `--dry-run` flag to `hive setup` that prints the diff it would apply without writing anything (Finding #23).
- [ ] Add `--yes` flag for scripted installs (Finding #23). Current workaround is `yes | hive setup`.
- [ ] Atomic writes (write `.tmp`, rename) for settings.json — matches the pattern used elsewhere.
- [ ] On missing tmux server: collapse the 4 duplicate error lines into one friendly message, AND always print the `~/.tmux.conf` snippets so the user can still persist bindings (Findings #16, #17).
- [ ] Gracefully handle malformed settings.json: back up the bad file, print filepath and serde error line, exit with a clear "settings.json was malformed — backed up to X. Re-run setup to write a clean one" message (Finding #19).
- [ ] Print a completion footer at the end of `hive setup` (success path): "Setup complete. Try `hive project add <name> --path <dir>` to register your first project, then `hive connect <name>`." Blocker — Finding #18.
- [ ] Suppress / clean `hive setup --help` global-flag pollution (Finding #22). Either scope flags per subcommand in clap-derive or drop them from setup's help.
- [ ] Consider whether the absolute binary path in settings.json hooks is the right choice (Finding #21). Likely yes, but at least document it in hive.md troubleshooting.

**Uninstall:**
- [ ] `hive uninstall` should also offer to remove `~/.claude/commands/hive/create-project.md` (Finding #38 — setup/uninstall asymmetry).
- [ ] Rmdir any empty `~/.claude/commands/hive/` and `~/.claude/agents/` left over after file removal (Finding #39).
- [ ] Print a completion footer listing what was NOT removed: `~/.hive/` data, the binary at `~/.local/bin/hive` (Findings #40, #41, #42).
- [ ] Add `hive uninstall` to the command list in `CLAUDE.md` (only in README today).

**Cross-cutting:**
- [ ] Add `HIVE_NO_NOTIFY=1` env var to suppress desktop notifications during scripting / CI / testing (Finding #32).

### A4. Empty-state UX in the TUI & CLI

Findings feeding here: #25, #26, #27, #36.

- [ ] Empty-state message in `render_session_list` (`src/tui/ui.rs:291`): when `session_infos.is_empty()`, render a centered "No sessions yet. Press **N** to register a project, or use `hive project add <key> --path <dir>`." Finding #25.
- [ ] Empty-state message when projects exist but no sessions are running — "Press Enter on a project to start a session."
- [ ] `hive start` in non-TTY context: catch the ratatui terminal-init error and print a friendly message + exit 1 instead of a raw panic (Finding #26).
- [ ] `hive cycle-next` / `cycle-prev` with no sessions: at least stderr "no sessions to cycle" (Finding #27).
- [ ] Verify `hive --debug` actually writes `~/.cache/hive/debug.log` for non-TUI commands. If it's TUI-only, document that explicitly (Finding #36).
- [ ] Model new empty-state strings on the good ones from `hive project list` and `hive todo list` (Findings #28, #29).

### A5. End-to-end cold-install walkthrough — ✅ COMPLETE

Walked through install → setup → project add → hook firing → multi-session → uninstall on macOS with an isolated `HOME=/tmp/hive-a5/home` + `TMUX_TMPDIR=/tmp/hive-a5/tmux-tmpdir` + `unset TMUX` for proper test isolation.

**Output**: [docs/publish-plan-a5-findings.md](./publish-plan-a5-findings.md) — 42 findings triaged into A1–A4 above and E1/A-state below.

**Not done (deferred by scope decision)**: Linux container walkthrough — macOS-only launch doesn't need it.

**Also produced**: `/tmp/hive-a5/env.sh` — the test harness env vars, useful as a starting point for A7's automated test.

Follow-up:
- [ ] Convert the walkthrough commands into a README "Quick Start" section (see E1).

### A6. Mac-only sanity checks — mostly deferred

Scope shrunk by launch-scope decision. Remaining items:

- [ ] Unicode / spaces in `$HOME` — `~/dir with space/ümlaut` sanity pass on macOS.
- [ ] `CLAUDE_CONFIG_DIR` unset → default path assumptions hold on macOS.
- [x] `hive start` with no tmux sessions + no projects: panics in non-TTY context (Finding #26 — now tracked under A4).

### A8. State lifecycle

Findings feeding here: #35.

- [ ] Verify the TUI filters stale state.json entries (tmux session killed but entry lingers) via cwd↔tmux matching. If it doesn't, add a filter (Finding #35).
- [ ] Consider running state cleanup on TUI launch in addition to hook-fire (eliminates the "no hook has fired in 10m so stale entries stay forever" case).

### A9. Subtle CLI gaps

Findings feeding here: #33, #34.

- [ ] `hive connect` — add `--detached` flag to create the tmux session without attaching (mainly unblocks scripts / tests, also useful for A7's automated test harness). Finding #34.
- [ ] Document the approval-via-send-keys design in troubleshooting: if approvals don't take, check the Claude pane is the frontmost one. Finding #33.

### A7. Automated setup test

- [ ] Shell-based integration test at `tests/setup_smoke.sh` (or Rust `tests/setup_smoke.rs` using `tempfile`):
  - Sets `HOME` to a tempdir populated with known fixtures.
  - Runs `hive setup`, diffs output against golden files.
  - Runs `hive setup` again, asserts no changes.
  - Runs `hive uninstall`, asserts tempdir matches pre-setup fixtures.
- [ ] Wire into CI (`.github/workflows/ci.yml`).

---

## Section B — Repo hygiene

- [ ] `.claude/agents/janus-wt-portal.md:161` — replace `/Users/emilianoperez/Projects/01-wyeworks/02-features/CSD-2345-auth-flow` with a generic example like `~/Projects/<project>/worktrees/CSD-2345-auth-flow`.
- [ ] Decide fate of `CLAUDE.local.md`:
  - Option A: delete (contents describe two external commands that are personal tooling).
  - Option B: untrack via `git rm --cached CLAUDE.local.md` and add to `.gitignore`.
  - Option C: move the useful content into `docs/integrations.md`.
- [ ] Expand `.gitignore`:
  ```
  CLAUDE.local.md
  .claude/settings.local.json
  .claude/commands/hive/
  ```
- [ ] Confirm `.obsidian-page.md` symlink is not tracked (`git ls-files | grep obsidian` → empty).
- [ ] Run a secret scanner once: `gitleaks detect --source . --no-banner` or equivalent. Fix anything it finds.

---

## Section C — `Cargo.toml` + crates.io

### C1. Metadata

- [ ] Add to `[package]`:
  ```toml
  authors = ["Emiliano Perez <PUBLIC_EMAIL>"]   # pick a public-facing email
  keywords = ["claude", "tmux", "tui", "dashboard", "ai"]   # max 5, each <= 20 chars
  categories = ["command-line-utilities", "development-tools"]
  homepage = "https://github.com/emiperez95/hive"
  documentation = "https://github.com/emiperez95/hive#readme"
  readme = "README.md"
  rust-version = "1.XX"   # pin MSRV — see C2
  exclude = [
      "target",
      ".github",
      "docs/publish-plan.md",
      "CLAUDE.md",
      "assets/icon.svg",
  ]
  ```
- [ ] Decide the public email to put in `authors`.

### C2. MSRV

- [ ] Determine the minimum Rust version that compiles hive clean. Candidates: whatever `ratatui 0.30` / `sysinfo 0.32` / `clap 4.5` require. Test with `cargo +1.75 build`, bump until it compiles.
- [ ] Set `rust-version` in Cargo.toml and document in README prereqs.

### C3. Name reservation

- [ ] Check `hive` availability on crates.io. It's a common word — likely taken.
- [ ] If taken, pick a fallback: `claude-hive`, `hivectl`, `hive-cli`. Update `name =` and any user-facing references.
- [ ] `cargo publish --dry-run --allow-dirty` passes.

---

## Section D — Distribution

### D1. Release workflow — trim to mac, then dry-run

- [ ] Edit `.github/workflows/release.yml` to drop the two Linux targets from the matrix. Leaves `x86_64-apple-darwin` + `aarch64-apple-darwin`. (Linux CI build job in `ci.yml` stays — compile-gate only.)
- [ ] Push a throwaway tag like `v0.0.1-test` to a private fork or test branch. Verify:
  - Builds both mac targets.
  - Packages as `hive-<target>.tar.gz`.
  - Creates a GitHub release with the 2 tarballs attached.
- [ ] Fix any workflow bugs surfaced. Delete the test tag + release.

### D2. Prebuilt-binary install script

- [ ] Rewrite `install.sh` (or add `install-prebuilt.sh`) to:
  - Detect arch (`uname -m` → `aarch64-apple-darwin` or `x86_64-apple-darwin`). If not Darwin, error with "mac-only for v0.1.0".
  - Map to a release asset name.
  - Download latest release tarball via `curl` from `https://github.com/emiperez95/hive/releases/latest`.
  - Extract to `~/.local/bin/hive`, chmod +x, print PATH reminder.
  - Verify with `hive --version` before reporting success (Finding #13).
- [ ] Add README "Install prebuilt" section above the cargo build instructions.
- [ ] Optional: a `curl ... | sh` one-liner in the README.

### D3. `hive update` — prefer prebuilt

- [ ] Update `src/cli/update.rs` to:
  1. Query GitHub releases API for the latest tag.
  2. If a binary matching current target triple exists, download + replace.
  3. Fallback: `cargo install --git` if cargo is present.
  4. Otherwise, print a clear "install cargo or download manually from <url>" message.
- [ ] Add tests where possible (mock the GitHub API call or factor out the download logic).

### D4. Later (not blocking)

- [ ] Homebrew tap (`emiperez95/homebrew-hive`).
- [ ] AUR package.
- [ ] Nix flake.

---

## Section E — Documentation

### E1. README rewrite

Findings feeding here: #01, #02, #03, #04, #06, #07, #09, #10, #11, #12.

- [ ] Top-of-README: one-sentence pitch + screenshot/GIF (Finding #06 — see E3).
- [ ] Badges row: CI status, crates.io version (once published), license, macOS platform badge.
- [ ] New "Prerequisites" section (tmux, Claude Code, Rust for source build) — Finding #01.
- [ ] New "Quick Start" section using the verified flow: install → setup → `project add` → `connect` → run claude → see status — Finding #02 (blocker).
- [ ] New "Install" section with two paths: prebuilt (preferred, mac tarball from Releases) and cargo.
- [ ] **Add a `hive web` section** — Finding #03 (blocker). Currently absent from README entirely.
- [ ] Replace Platform Support table with a single-line "macOS (Apple Silicon and Intel)" — Finding #04. Move "Adding Terminal Support" dev docs to CONTRIBUTING.md.
- [ ] Link "Claude Code" to Anthropic's install docs on first mention — Finding #07.
- [ ] Either drop `hive project import` from the user-facing command list or explain what `sesh.toml` is in one line — Finding #09.
- [ ] Add `--debug` to the commands reference (currently hidden; users will want it for bug reports) — Finding #10.
- [ ] Short glossary or inline clarifier for "session" (tmux session name derived from project, state keyed by Claude session_id) — Finding #11.
- [ ] Link to `docs/claude-auth-profiles.md` from a new "Advanced" section — Finding #12.
- [ ] Add `hive uninstall` to the command list (already in System subsection per README:111 — verify).
- [ ] Document the tmux binding persistence caveat: setup prints snippets you must paste into `~/.tmux.conf` yourself (Finding #05).

### E2. New documentation files

- [ ] `CONTRIBUTING.md` — dev setup, build/test/clippy commands, PR conventions, where to file bugs.
- [ ] `CHANGELOG.md` — start with the `v0.1.0` entry. Populate from git log.
- [ ] `docs/troubleshooting.md` — common issues (hooks not firing, tmux binding conflicts, Linux feature gaps).
- [ ] `docs/hooks.md` — worktree lifecycle hook contract (currently only in `CLAUDE.md`).
- [ ] `SECURITY.md` — minimal: where to report vulnerabilities.

### E3. Asset

- [ ] Capture a terminal recording of the TUI in action. Either a GIF (`asciinema` → `agg`) or an MP4.
- [ ] Store under `assets/demo.gif`, embed in README top.

---

## Section F — CI / release workflow

- [ ] Add `cargo publish --dry-run` as a CI job. Prevents metadata regressions.
- [ ] Add `cargo audit` or `cargo deny check` for dependency vulnerabilities / license compliance.
- [ ] Consider a Linux-path smoke-test job that exercises the `#[cfg(target_os = "macos")]` stub branches.
- [ ] Run the shell setup test from A7 in CI (Linux only; the macOS-specific bits can be gated).

---

## Section G — GitHub repo polish

- [ ] Set the repo description on GitHub: "Interactive Claude Code session dashboard for tmux".
- [ ] Add topics: `rust`, `tmux`, `claude-code`, `tui`, `cli`, `dashboard`.
- [ ] Upload a social preview image (1280×640) — settings → Social preview.
- [ ] Issue templates:
  - `.github/ISSUE_TEMPLATE/bug.yml`
  - `.github/ISSUE_TEMPLATE/feature.yml`
  - `.github/ISSUE_TEMPLATE/config.yml` (disable blank issues)
- [ ] PR template: `.github/pull_request_template.md`.
- [ ] Optional: enable Discussions.
- [ ] Optional: `CODE_OF_CONDUCT.md` (Contributor Covenant).

---

## Section H — Launch

- [ ] Final CHANGELOG entry for `v0.1.0`.
- [ ] Tag and push `v0.1.0` → triggers release workflow → binaries attached automatically.
- [ ] `cargo publish` to crates.io (if the name is available and C is done).
- [ ] Announce:
  - Claude Code Discord / community forum.
  - `r/ClaudeAI` post.
  - `r/rust` "This Week in Rust" submission (if Rust-framing feels right).
  - Optional: HN Show post — requires a polished demo GIF.

---

## Suggested order of attack

1. **A5** (cold-install walkthrough) — blocking, generates the real punch list.
2. **A1–A4, A6, A7** — work items produced by A5.
3. **B** (hygiene) — trivial, do alongside A.
4. **C** (Cargo.toml) — small, unblocks crates.io dry-run.
5. **D1** (release dry-run) → **D2, D3** — makes the binary install path real.
6. **E** (docs) — now that the flow works, document the true flow.
7. **F** (CI) — lock in the new tests.
8. **G** (GitHub polish) — cosmetic.
9. **H** (launch).

## Notes for future Claude sessions

- The plan owner is Emiliano Perez (user). Before making user-visible changes (content, tone, naming), sanity-check with them.
- Don't push tags or publish to crates.io without explicit confirmation — those are one-way doors.
- `CLAUDE.md` at the repo root is the architecture bible; read it before editing code.
- This file itself should be deleted or archived once publishing is done.
