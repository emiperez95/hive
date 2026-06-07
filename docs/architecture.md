# Architecture

C4 model diagrams for hive. Rendered by GitHub via Mermaid.

## Level 1 — System Context

Who and what hive interacts with.

A high-level view. Mermaid's C4 auto-layout is rough — read top to bottom: people on top, hive in the middle, externals grouped below.

```mermaid
flowchart TB
    classDef person fill:#08427B,color:#fff,stroke:#073B6F
    classDef system fill:#1168BD,color:#fff,stroke:#0B4884
    classDef ext fill:#999,color:#fff,stroke:#6B6B6B

    dev(["👤 Developer"]):::person
    phone(["📱 Phone browser"]):::person

    hive["<b>hive</b><br/>TUI + web + hook handler"]:::system

    subgraph terminal["Terminal stack (local)"]
        tmux["tmux"]:::ext
        iterm["iTerm2"]:::ext
        chrome["Chrome"]:::ext
    end

    subgraph claude_grp["Claude Code"]
        claude["Claude CLI<br/>(in tmux pane)"]:::ext
    end

    subgraph cloud["Network services"]
        github["GitHub releases"]:::ext
        tts["TTSQwen"]:::ext
    end

    dev -->|CLI / TUI| hive
    dev -->|prefix+s/d| tmux
    phone -->|HTTP LAN| hive

    hive <-->|list/switch/send-keys| tmux
    hive -->|AppleScript: split/close| iterm
    hive -->|JXA: list/focus tabs| chrome

    claude -->|hook events: stdin JSON| hive
    hive -->|read JSONL logs| claude

    hive -->|fetch release| github
    hive -->|POST → HLS proxy| tts
```

## Level 2 — Containers

The processes/binaries that make up hive and how they share state.

```mermaid
C4Container
    title Containers — hive

    Person(dev, "Developer")
    Person(phone_user, "Phone user")

    System_Boundary(hive, "hive") {
        Container(cli, "CLI dispatcher", "Rust / clap", "main.rs — parses args, routes to subcommands")
        Container(tui, "TUI", "Rust / ratatui + crossterm", "1s refresh loop; reads state + tmux + sysinfo + Chrome")
        Container(hook, "Hook handler", "Rust", "`hive hook <event>` — short-lived process per Claude event")
        Container(web, "Web server", "Rust / tiny_http", "Sync HTTP server; embeds mobile SPA, proxies TTS HLS")
        Container(spa, "Web SPA", "HTML / JS / Prism / hls.js", "Mobile-first dashboard served from `web.html`")

        ContainerDb(state, "State files", "JSON / TOML / TXT under ~/.hive/", "state.json, worktrees.json, projects.toml, todos, flags")
        ContainerDb(jsonl, "Claude conversation logs", "JSONL on disk", "~/.claude*/projects/**/*.jsonl (read-only)")
    }

    System_Ext(claude, "Claude Code CLI")
    System_Ext(tmux, "tmux")
    System_Ext(iterm, "iTerm2")
    System_Ext(chrome, "Chrome")
    System_Ext(tts, "TTSQwen")
    System_Ext(github, "GitHub releases")

    Rel(dev, cli, "hive / hive wt / hive project / …")
    Rel(cli, tui, "default subcommand")
    Rel(cli, web, "hive web")
    Rel(phone_user, spa, "HTTPS-less LAN")
    Rel(spa, web, "GET /api/*  POST /api/*")

    Rel(claude, hook, "stdin JSON via Claude hook config")
    Rel(hook, state, "Atomic write (tmp + rename)")
    Rel(tui, state, "Read each refresh")
    Rel(web, state, "Read + write (todos, flags)")

    Rel(tui, jsonl, "Parse for status / messages")
    Rel(web, jsonl, "Parse for conversation view")

    Rel(tui, tmux, "list / switch / kill")
    Rel(web, tmux, "send-keys, new-session, kill")
    Rel(cli, tmux, "wt new → new-session with hooks")

    Rel(tui, iterm, "spread / collapse panes")
    Rel(tui, chrome, "list / focus tabs by port")

    Rel(web, tts, "POST /api/tts-hls → proxy /hls/*")
    Rel(cli, github, "hive update — fetch latest")
```

## Level 3 — Components

Zoomed in on the two most interesting containers.

### TUI container

```mermaid
C4Component
    title Components — TUI container

    Person(dev, "Developer")

    Container_Boundary(tui, "TUI") {
        Component(event_loop, "event_loop", "tui/event_loop.rs", "run_tui(): key handling, input modes, post-action dispatch")
        Component(app, "App", "tui/app.rs", "App struct, refresh(), session list, search, favorites, todos")
        Component(ui, "ui", "tui/ui.rs", "ratatui rendering: list, detail, search, help, modals")
    }

    Container_Boundary(common, "common modules") {
        Component(c_tmux, "tmux", "common/tmux.rs", "list/switch/send-keys/kill")
        Component(c_proc, "process", "common/process.rs", "Claude detection, sysinfo")
        Component(c_jsonl, "jsonl", "common/jsonl.rs", "Status + conversation parsing")
        Component(c_chrome, "chrome", "common/chrome.rs", "Tab list/focus via JXA")
        Component(c_ports, "ports", "common/ports.rs", "Listening ports via libproc")
        Component(c_persist, "persistence", "common/persistence.rs", "favorites, todos, muted, skipped files")
    }

    ContainerDb(state, "state.json", "JSON")
    System_Ext(tmux_ext, "tmux")
    System_Ext(chrome_ext, "Chrome")

    Rel(dev, event_loop, "Key events (crossterm)")
    Rel(event_loop, app, "refresh() / mutate state")
    Rel(event_loop, ui, "draw(frame, &app)")

    Rel(app, state, "HookState::load()")
    Rel(app, c_tmux, "list sessions")
    Rel(app, c_proc, "process tree, CPU/mem")
    Rel(app, c_jsonl, "parse status")
    Rel(app, c_chrome, "on-demand: tab titles")
    Rel(app, c_ports, "listening ports per pid")
    Rel(app, c_persist, "favorites / todos / flags")

    Rel(c_tmux, tmux_ext, "shell out")
    Rel(c_chrome, chrome_ext, "JXA via osascript")
```

### Hook handler container

```mermaid
C4Component
    title Components — Hook handler container

    System_Ext(claude, "Claude Code")

    Container_Boundary(hook, "Hook handler") {
        Component(cli_hook, "cli::hook", "cli/hook.rs", "run_hook(): parse stdin JSON → HookEvent")
        Component(handler, "daemon::hooks", "daemon/hooks.rs", "handle_hook_event() — maps event → SessionState")
        Component(notifier, "daemon::notifier", "daemon/notifier.rs", "Platform-native notifications")
        Component(messages, "ipc::messages", "ipc/messages.rs", "HookEvent, SessionState, HookState (load/save)")
    }

    ContainerDb(state, "state.json", "atomic write")
    System_Ext(notify_os, "OS notification system", "terminal-notifier / osascript / notify-send")

    Rel(claude, cli_hook, "stdin JSON")
    Rel(cli_hook, handler, "dispatch HookEvent")
    Rel(handler, messages, "load → mutate → save")
    Rel(messages, state, "atomic write (.tmp + rename)")
    Rel(handler, notifier, "needs_attention → notify")
    Rel(notifier, notify_os, "shell out")
```

## Level 4 — Code

The shared IPC data model — the contract between the hook handler (writer) and TUI/web (readers). Defined in `ipc/messages.rs`.

```mermaid
classDiagram
    class HookState {
        +HashMap~String, SessionState~ sessions
        +load() HookState
        +save() Result
        +cleanup_stale(threshold)
    }

    class SessionState {
        +String session_id
        +String cwd
        +SessionStatus status
        +bool needs_attention
        +Option~String~ last_activity
    }

    class SessionStatus {
        <<enumeration>>
        Waiting
        NeedsPermission
        EditApproval
        PlanReview
        QuestionAsked
        Working
        Unknown
    }

    class HookEvent {
        <<enumeration>>
        Stop
        PreToolUse
        PostToolUse
        PermissionRequest
        UserPromptSubmit
        Notification
        +session_id() str
        +cwd() str
    }

    class NeedsPermission {
        +String tool_name
        +Option~String~ description
    }

    class EditApproval {
        +String filename
    }

    HookState "1" *-- "many" SessionState : sessions
    SessionState --> SessionStatus : status
    SessionStatus <|-- NeedsPermission
    SessionStatus <|-- EditApproval
    HookEvent ..> SessionState : updates via daemon::hooks
```

## Notes

- **No daemon**: every container is short-lived (CLI, hook) or user-launched (TUI, web). Coordination is via file system only.
- **State writes** are atomic (write `.tmp`, rename) so concurrent hook events never corrupt `state.json`.
- **Read-only externals**: Claude's JSONL logs are never modified — hive only parses them for status and conversation rendering.
- **Code-level (C4 L4)** is shown only for the IPC types — they're the cross-container contract. Other modules are well-covered by the source itself + the module map in `CLAUDE.md`.
