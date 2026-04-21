# A5 — Cold-Install Findings

Observations from the `publish-plan.md` Section A5 walkthrough. Each finding is triaged against A1–A4 (or a new item) so the fixes can be picked up separately.

See the parent plan: [docs/publish-plan.md](./publish-plan.md).

## Scope decision

Launch is **macOS-only** (`v0.1.0`). Linux is deferred — no Docker walkthrough; only the macOS fresh-`HOME` pass is run.

## Environment

- **Host**: macOS (your actual machine)
- **Test `HOME`**: `/tmp/hive-a5/home` (isolated; hive's `~/.claude/`, `~/.hive/`, etc. all land here)
- **Test tmux socket**: `-L hive-a5` (isolated from your real tmux server)
- **Hive binary**: freshly built from current repo HEAD into `$TEST_HOME/.local/bin/hive`
- **Claude CLI**: not wired in; hook events simulated via `hive hook <event>` with piped JSON (the "Claude-less fallback" from the plan)

## Findings log

Findings recorded as we walk through. Format:

```
### Finding #NN — <one-line title>
- Step: <step number + name>
- Expected: ...
- Observed: ...
- Severity: blocker / friction / cosmetic
- Feeds into: publish-plan.md §A1 / A2 / A3 / A4 / new
```

---

## Step 1 — README fresh-read

### Finding #01 — No "Prerequisites" section
- Step: 1 (README fresh-read)
- Expected: Top of README lists what I need installed before `cargo install` works (tmux, Claude Code CLI, Rust toolchain).
- Observed: README jumps straight from Features → Install. If I don't have Rust, `cargo install` just errors. If I don't have tmux, nothing tells me until something silently breaks. If I don't know what "Claude Code" is, the opening line is my only hint.
- Severity: friction
- Feeds into: publish-plan.md §A1 (already has "Add Prerequisites section" task) and §E1

### Finding #02 — No Quick Start / end-to-end walkthrough
- Step: 1 (README fresh-read)
- Expected: A 5-minute "run these commands in order and see the dashboard light up" section right after Install.
- Observed: After `hive setup` the README dives straight into the Commands reference. New user has no idea what to do next to actually see a working dashboard. No example of registering a first project + connecting + seeing status update.
- Severity: blocker
- Feeds into: publish-plan.md §A5 (produce this), §E1 (add to README)

### Finding #03 — `hive web` entirely missing from README
- Step: 1 (README fresh-read)
- Expected: `hive web` (mobile dashboard with TTS, session management, conversation viewer) is prominently documented as a major feature.
- Observed: `hive --help` lists `web` as a subcommand; `CLAUDE.md` documents it extensively; README **does not mention it at all** — not in Features, not in Commands, not in System. A user reading only the README would never know the web dashboard exists.
- Severity: blocker
- Feeds into: publish-plan.md §E1 (README rewrite), new §E4 item

### Finding #04 — Platform Support table contradicts mac-first launch scope
- Step: 1 (README fresh-read)
- Expected: With the decision to launch macOS-only, README shouldn't advertise Linux support via a feature table.
- Observed: README has a Platform Support table listing macOS/Linux per feature (README:174-181). A Linux user would reasonably read this as "Linux is supported, just with stubs." Also contains a whole "Adding Terminal Support" section that's forward-looking dev docs rather than user docs.
- Severity: friction
- Feeds into: publish-plan.md §E1 — replace with "macOS only at launch" note; move dev-facing sections to CONTRIBUTING.md

### Finding #05 — Tmux keybinding persistence is misleading
- Step: 1 (README fresh-read) — cross-ref with Step 3 where we'll verify
- Expected: README says "Optionally bind `prefix+s` ... in tmux" — implies permanent binding.
- Observed: Per recon of `src/cli/setup.rs`, setup only runs `tmux bind-key` against the live server (ephemeral). README does NOT tell the user they need to edit `~/.tmux.conf` themselves. New user will bind once in the test run, then lose the binding the next day.
- Severity: friction (hypothesis — will verify in Step 3)
- Feeds into: publish-plan.md §A3 (setup safety) — either persist to tmux.conf or print clear follow-up instructions

### Finding #06 — No screenshot / demo GIF
- Step: 1 (README fresh-read)
- Expected: Visual of the TUI at the top of the README — the product is a visual tool, and decisions to try it depend on seeing what it looks like.
- Observed: No image at all. Just text.
- Severity: friction
- Feeds into: publish-plan.md §E3 (already has "demo GIF" task)

### Finding #07 — No link to Claude Code
- Step: 1 (README fresh-read)
- Expected: First-line mention of Claude Code is a link to anthropic's Claude Code CLI install docs.
- Observed: Bare text "Claude Code" with no link. A user who's never heard of it has nothing to click.
- Severity: cosmetic
- Feeds into: publish-plan.md §E1

### Finding #08 — `hive update` requires cargo but README doesn't say
- Step: 1 (README fresh-read)
- Expected: If `hive update` needs cargo, README either says so or update falls back to downloading a prebuilt binary.
- Observed: README documents `hive update` in System section with no caveats. Per recon, `src/cli/update.rs` shells out to `cargo install --git` — this will fail silently for a user who installed via prebuilt binary and doesn't have Rust.
- Severity: friction
- Feeds into: publish-plan.md §D3 (already has "prefer prebuilt in hive update" task)

### Finding #09 — `hive project import` references `sesh.toml` with no context
- Step: 1 (README fresh-read)
- Expected: Either a one-line explanation of what sesh is, a link, or drop this from user-facing docs.
- Observed: README:85 just says `hive project import     # import from sesh.toml` — a new user has no idea what sesh is or whether this applies to them.
- Severity: cosmetic
- Feeds into: publish-plan.md §E1

### Finding #10 — `--debug` flag missing from README
- Step: 1 (README fresh-read)
- Expected: Users reporting bugs can find the flag that produces a debug log.
- Observed: `hive --help` lists `--debug` ("Enable debug logging to ~/.cache/hive/debug.log"); README never mentions it.
- Severity: cosmetic
- Feeds into: publish-plan.md §E1 / new troubleshooting doc §E2

### Finding #11 — No indication of what a "session" means in the hive context
- Step: 1 (README fresh-read)
- Expected: Brief explainer: hive session = tmux session + (optional) Claude instance inside it, identified by tmux name.
- Observed: The word "session" appears repeatedly without grounding. Overloaded: tmux session, Claude conversation, hive project. A new user will guess.
- Severity: cosmetic
- Feeds into: publish-plan.md §E1 (glossary or inline explainer near top)

### Finding #12 — Auth profiles feature has no README pointer
- Step: 1 (README fresh-read)
- Expected: If `docs/claude-auth-profiles.md` exists, README links to it at least from "Commands" or a dedicated section.
- Observed: Feature is documented in `docs/` but README never mentions auth profiles exist. Most users would benefit from knowing about this for multi-identity setups.
- Severity: cosmetic
- Feeds into: publish-plan.md §E1

## Step 2 — Install

Ran `./install.sh` against the isolated test HOME (with `RUSTUP_HOME`/`CARGO_HOME` preserved to the real user so rustup could resolve the toolchain). Clean build (cached), clean copy into `$HOME/.local/bin/hive`. `hive --version` → `hive 0.1.0`. Install experience is good on the happy path.

### Finding #13 — install.sh doesn't verify binary works
- Step: 2 (install)
- Expected: Last step of install.sh runs `hive --version` (or at least `$INSTALL_DIR/hive --version`) so a silent copy failure surfaces.
- Observed: Script prints "Installation complete!" based only on `cp` not erroring. No runtime smoke check.
- Severity: cosmetic
- Feeds into: publish-plan.md §A1 (expand installer safety)

### Finding #14 — install.sh "PATH not set" branch ends cold
- Step: 2 (install)
- Expected: If `~/.local/bin` isn't in `$PATH`, the script explains how to add it AND confirms the install did succeed.
- Observed: When PATH check fails, install.sh prints the warning then exits without the "Installation complete!" / "Next steps" block (install.sh:33-49). User sees a warning and nothing else — unclear whether the install worked.
- Severity: friction
- Feeds into: publish-plan.md §A1

### Finding #15 — No pre-flight for tmux / claude CLI
- Step: 2 (install)
- Expected: install.sh (or first-run of hive) warns if tmux or `claude` aren't on PATH — the product is useless without them.
- Observed: install.sh only checks for cargo (install.sh:12). No mention of tmux or Claude Code CLI anywhere in the install path.
- Severity: friction
- Feeds into: publish-plan.md §A1 (dependency pre-flight — already a task)

## Step 3 — `hive setup`

Exercised 4 variants of setup:
1. Fresh `$HOME`, no tmux server → hooks/agent/command install, tmux binding prompt accepted but errors.
2. Fresh re-run (idempotency).
3. Fresh `$HOME`, tmux server running → bindings register, setup prints tmux.conf snippets.
4. Pre-existing settings.json with unrelated hooks → preserved correctly. Malformed JSON → setup crashes without damage.

### Finding #16 — Setup spews 4 raw "no server running" errors before friendly message
- Step: 3 (setup w/o tmux running)
- Expected: If no tmux server, one clear message like "tmux is not running — skipping keybindings. Start tmux and re-run `hive setup`, or add these lines to ~/.tmux.conf: ...".
- Observed: 4 duplicate `no server running on /private/tmp/.../default` lines, then a single `Could not register some keybindings (tmux not running?).` line. No tmux.conf snippets offered in the failure path (which is exactly when the user needs them).
- Severity: friction
- Feeds into: publish-plan.md §A3 (setup safety)

### Finding #17 — Tmux bindings only shown for tmux.conf when server IS running
- Step: 3 (tmux running path)
- Expected: The tmux.conf snippets should always be shown regardless of whether a live server accepted the binding. They are a config-file recommendation, not dependent on the server being up.
- Observed: Only the success path (server running) prints the "Add to ~/.tmux.conf to persist" snippets. Failure path has no persistence instructions.
- Severity: friction
- Feeds into: publish-plan.md §A3

### Finding #18 — No "next steps" completion message
- Step: 3 (both runs)
- Expected: After a full first-time setup, hive prints something like: "You're all set! Try `hive project add <name> --path <dir>` to register your first project, then `hive connect <name>` to launch a Claude session."
- Observed: Setup ends silently after the last installed component (or with the tmux failure message). User has no idea what to do next. Directly worsens Finding #02 (no quick-start).
- Severity: blocker
- Feeds into: publish-plan.md §A4 (empty-state), new §A3 item

### Finding #19 — Malformed settings.json: raw serde error, no recovery path
- Step: 3 (edge case: malformed settings.json)
- Expected: Either back up the malformed file, warn the user, or reject with a clear message pointing at the file and line.
- Observed: `Error: control character ( -) found while parsing a string at line 2 column 0`. Raw serde error bubbled to stderr. User has to know what serde is, what a control character means. The original file is left intact (no damage), but hive quits without doing anything useful.
- Severity: friction
- Feeds into: publish-plan.md §A3

### Finding #20 — settings.json reformatted (keys sorted alphabetically; top-level fields reordered)
- Step: 3 (edge case: existing unrelated hooks)
- Expected: Diff minimal — add hive hooks, don't rewrite the whole file.
- Observed: After setup, `model` field moved from top to bottom, hook entries alphabetized. Existing hook CONTENT is preserved, but the on-disk formatting is fully rewritten. Annoying for users who hand-maintain settings.json in a specific order.
- Severity: cosmetic
- Feeds into: publish-plan.md §A3

### Finding #21 — Hooks written with absolute path, fragile on binary move
- Step: 3 (inspecting settings.json after setup)
- Expected: Either the hook command uses `hive hook ...` (PATH-resolved) or hive proactively updates when run from a new location.
- Observed: Setup writes `/tmp/hive-a5/home/.local/bin/hive hook PreToolUse` (absolute path). If user later installs to `/opt/hive/` or similar and the old path is deleted, hooks silently no-op. Mitigation exists (`hive update` re-runs setup) but requires the user to realize the issue.
- Severity: friction
- Feeds into: publish-plan.md §A3 (or new §A-hooks)

### Finding #22 — `hive setup --help` shows global TUI flags that don't apply
- Step: 3 (UX audit)
- Expected: `hive setup --help` lists setup's own options only (or says "this command takes no arguments").
- Observed: Help shows `--filter`, `--watch`, `--detail`, `--debug`, `--picker` — all are global `hive` flags that don't apply to `setup`. Clap-derive artifact from shared options. Confusing.
- Severity: cosmetic
- Feeds into: publish-plan.md §A3 (or general CLI cleanup)

### Finding #23 — No `--dry-run`, `--yes`, or `--no-*` flags
- Step: 3 (UX audit)
- Expected: For scripted installs (CI, dotfile repos) and for cautious users, setup should support `--dry-run` (print diff), `--yes` (auto-accept), `--no-tmux` (skip binding prompt), etc.
- Observed: Setup is fully interactive. No flags. Have to `yes | hive setup` to run non-interactively, which blindly accepts everything.
- Severity: friction
- Feeds into: publish-plan.md §A3 (already has `--dry-run` task)

### Finding #24 — Idempotency holds for hooks/agent/command; fails for tmux (expected)
- Step: 3 (second run)
- Expected/Observed match: On re-run, hooks/agent/command show `[ok]` and no prompts. tmux bindings show `[missing]` every time (server state isn't persisted). Consistent with design — not a bug, but reinforces the "tmux bindings are ephemeral" UX issue (Findings #05, #17).
- Severity: not a finding, confirmation of design tension

## Step 4 — Empty-state TUI

Inspected `src/tui/ui.rs::render_session_list()` (line 291+) and surrounding layout. With zero `session_infos`, the list area draws only a single blank line. Footer at line 211+ shows keys including `[N]ew` for project creation.

Also exercised non-TUI empty-state commands: `hive cycle-next`, `hive todo list`, `hive start`, `hive project list`.

### Finding #25 — TUI main area is blank when no sessions
- Step: 4 (empty-state TUI)
- Expected: A centered message like "No sessions yet.  Press N to register your first project, or /path to search projects." Stays until the first session appears.
- Observed: `render_session_list` just pushes `Line::raw("")` and returns when `session_infos` is empty. No text. The only affordance is the footer's `[N]ew` among 8+ other keys — easy to miss, not self-explanatory.
- Severity: friction (close to blocker given how often a first-time user hits this)
- Feeds into: publish-plan.md §A4 (empty-state UX — already a task)

### Finding #26 — `hive start` panics in non-TTY contexts
- Step: 4 (empty-state + non-TTY)
- Expected: Graceful error like "hive start requires a terminal" with exit code 1.
- Observed: `thread 'main' panicked at ratatui-0.30.0/src/init.rs:299:16: failed to initialize terminal: Os { code: 6, kind: Uncategorized, message: "Device not configured" }`. Exit 101. Raw Rust panic text, no cleanup.
- Severity: friction
- Feeds into: new §A4 item — catch TTY init error, print friendly message

### Finding #27 — `hive cycle-next` with no sessions: silent success
- Step: 4 (empty-state)
- Expected: Either an informational stderr message ("no sessions to cycle") or a non-zero exit.
- Observed: Exit 0, no output. Scripts/hotkeys bound to cycle-next can't tell whether anything happened.
- Severity: cosmetic
- Feeds into: publish-plan.md §A4

### Finding #28 — `hive project list` empty-state is clean
- Step: 4 (empty-state)
- Observed: `No projects configured. Use 'hive project add' or 'hive project import'.` ← well-written, actionable. Good model for how other empty states should read.
- Severity: positive observation (not a finding)

### Finding #29 — `hive todo list` without a tmux session is clean
- Step: 4 (empty-state)
- Observed: `Error: Could not detect tmux session. Use --session <name>.` ← clear instruction, pointer to the fix. Good.
- Severity: positive observation (not a finding)

## Step 5-6 — Project registration + hook firing

Registered `demo` project, created tmux session `🐝 demo`, simulated hook events via stdin JSON (PreToolUse → PermissionRequest).

### Finding #30 — Project add / list UX is clean
- Step: 5 (project registration)
- Observed: `hive project add demo --emoji 🐝 --path $HOME/demo-proj` → `Added project '🐝 demo'`. `hive project list` prints a readable table. `projects.toml` is valid TOML with emoji preserved.
- Severity: positive observation (not a finding)

### Finding #31 — Hook pipeline works end-to-end
- Step: 6 (hook firing)
- Observed: Piping a PreToolUse JSON event to `hive hook PreToolUse` creates `~/.hive/cache/state.json` correctly. Status "Working", needs_attention false, cwd preserved. A second PermissionRequest event upgrades to NeedsPermission with `tool_name: "Bash: ls"`. State file written atomically. No background daemon needed.
- Severity: positive observation (not a finding)

### Finding #32 — PermissionRequest triggers real macOS notification unconditionally
- Step: 6 (hook firing)
- Expected: Some way to suppress notifications during testing / scripting.
- Observed: PermissionRequest hooks invoke `terminal-notifier` / `osascript` directly (src/daemon/notifier.rs). Global mute exists (`~/.hive/cache/muted-global`) but user would have to know to create it. No `hive hook --no-notify` or env var. Minor concern: a user simulating events or running hive in CI for tests will see popup notifications.
- Severity: cosmetic
- Feeds into: publish-plan.md §A3 (add `HIVE_NO_NOTIFY` env var or flag for testing)

### Finding #33 — Approval uses tmux send-keys to the Claude pane (indirect, fragile)
- Step: 6 (inspecting approval code)
- Observed: Per `src/tui/event_loop.rs:634-665`, approval sends `1`/`2` + `Enter` as tmux keystrokes to the Claude pane rather than calling any API. Relies on the Claude dialog being focused AT THAT EXACT MOMENT. Works for the common case but may misbehave if:
  - The user's Claude pane has a modal/dialog pending other input.
  - The pane coords (sess/win/pane) changed between TUI refresh and keypress.
  - User ran `claude` in a non-Claude mode.
- Severity: cosmetic (design note, not an observed bug in A5)
- Feeds into: general doc — add this caveat to README or troubleshooting

### Finding #34 — `hive connect` requires a TTY (no headless option)
- Step: 5 (project connect)
- Expected: A `--no-attach` flag or equivalent so scripts/tests can create a session without attaching.
- Observed: `hive connect` always ends with `switch_to_session` which requires TTY. No `-d`, no `--detached`. Tested workaround: `tmux new-session -d -s "$(hive …)"` but that requires parsing the session name.
- Severity: friction (mostly matters for testing / scripting)
- Feeds into: new §A-cli item

## Step 7 — Multi-session navigation

Added second project (`demo2` with 🦋), fired hooks for both, exercised cycle + todo commands.

### Finding #35 — Stale state entries persist up to 10 minutes after tmux kill
- Step: 7 (kill session)
- Expected: If the tmux session backing a state entry no longer exists, it could be hidden from the TUI on the next refresh.
- Observed: Killed `🦋 demo2` tmux session; state.json still contains its entry. Per CLAUDE.md, 10-minute TTL cleanup runs in the hook handler — only when *another* hook fires. If no hook fires, stale entries linger indefinitely. The TUI presumably filters via cwd↔tmux matching but I didn't verify visually.
- Severity: cosmetic
- Feeds into: new §A-state item (verify TUI filter; consider shorter TTL or cleanup on TUI launch)

### Finding #36 — `hive --debug` doesn't produce log for simple commands
- Step: 7 (diagnostic)
- Expected: `hive --debug cycle-next` produces `~/.cache/hive/debug.log` with at least a "started" entry.
- Observed: No log file created. Would disappoint someone trying to debug "why isn't my hotkey working?" Perhaps --debug only activates inside TUI loop; worth documenting.
- Severity: cosmetic
- Feeds into: publish-plan.md §E2 (troubleshooting doc)

### Finding #37 — `hive todo add/list` works cleanly
- Step: 7 (todos)
- Observed: Add → list → file write all work. `$HOME/.hive/cache/todos.txt` uses tab-separated format. Readable. Multi-session todos keyed by tmux session name.
- Severity: positive observation (not a finding)

## Step 8 — Uninstall

Ran `hive uninstall` (accepted all prompts). Diffed `.claude/` and `.hive/` against pre-setup snapshot.

### Finding #38 — `create-project.md` command never removed by uninstall (asymmetry)
- Step: 8 (uninstall)
- Expected: If setup installs `~/.claude/commands/hive/create-project.md`, uninstall should offer to remove it. Setup asks before installing (per recon); uninstall is silent.
- Observed: `run_uninstall()` in `src/cli/setup.rs:606` ends after the agent prompt. No prompt for create-project command. After uninstall, `~/.claude/commands/hive/create-project.md` and its parent dir remain on disk indefinitely.
- Severity: friction (setup/uninstall are asymmetric — violates user expectation)
- Feeds into: publish-plan.md §A3 (setup safety) / new §A-uninstall

### Finding #39 — `.claude/commands/hive/` empty parent dir may remain even after file removal
- Step: 8 (uninstall)
- Expected: If uninstall deletes the last file under `.claude/commands/hive/`, also rmdir the empty parent (and `.claude/agents/` if empty).
- Observed: Per code, uninstall only `fs::remove_file(&agent_path)` — parent dir left behind. Minor but users who `ls ~/.claude/` will still see `hive/` subdirs.
- Severity: cosmetic
- Feeds into: publish-plan.md §A3

### Finding #40 — `~/.hive/` directory entirely untouched by uninstall, not documented
- Step: 8 (uninstall)
- Expected: Uninstall tells the user "your hive data (projects.toml, todos, state) is still at `~/.hive/`. Run `rm -rf ~/.hive/` if you want a full wipe."
- Observed: All `~/.hive/` files remain (projects.toml, cache/state.json, cache/todos.txt, cache/debug.log). No message. User doesn't know they have residual data until they look for it.
- Severity: friction
- Feeds into: publish-plan.md §A3 (add final footer to uninstall listing what's left)

### Finding #41 — Uninstall doesn't print a "done" footer or next steps
- Step: 8 (uninstall)
- Expected: Single-line "Hive uninstalled. Remove `~/.hive/` to wipe data. Optionally `cargo uninstall hive` / `rm ~/.local/bin/hive` to remove the binary."
- Observed: Uninstall ends silently after the agent prompt response. User isn't sure if it finished.
- Severity: cosmetic
- Feeds into: publish-plan.md §A3

### Finding #42 — Uninstall doesn't remove the binary itself
- Step: 8 (uninstall)
- Expected: Binary is user's call to remove; uninstall should at least TELL the user where it is so they can clean it up.
- Observed: Nothing mentioned. Setup wrote the absolute binary path into every hook command (Finding #21) so technically `hive uninstall` could parse that and print the path. Doesn't.
- Severity: cosmetic
- Feeds into: publish-plan.md §A3

