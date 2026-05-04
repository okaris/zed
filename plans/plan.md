# Zed Fork: Agent Session Manager

## Vision

A Zed fork that turns it into an agent orchestration environment. Run multiple Claude Code / Codex / Gemini / any-agent sessions with persistent, named, grouped terminals — review diffs, edit code, and manage agents all in one window.

Like Conductor, but:
- Built into a real editor (not a standalone app)
- Folder-based grouping (not repo-scoped)
- Sessions persist independently via PTY holding (not tied to app lifecycle)
- Agent-agnostic plugin system with resume/recovery support

## Architecture

```
Zed (UI)
├── Session Panel (new, left/right dock)
│   └── Groups > Sessions (tree view)
├── Terminal Area (existing, modified)
│   └── Attaches to persistent sessions instead of raw PTYs
└── Agent Panel (existing, untouched)

pty_server (library crate, embedded in Zed)
├── Holds PTYs alive after Zed closes (fork + unix socket)
├── Agent preset system (launch, resume, env per agent type)
├── Metadata store (groups, sessions, agent state)
└── Also compiled into zed-remote-server for SSH/remote

Local:  Zed → pty_server library calls directly
Remote: Zed → RPC → zed-remote-server (includes pty_server) → PTY on remote host
```

## Status

### Done
- [x] **Phase 1: pty_server core** (`crates/pty_server/`)
  - PTY holding: double-fork + openpty + unix socket + poll loop
  - Daemons survive Zed crashes: stdin/stdout to /dev/null, stderr to per-session log file
  - Client attach/detach with protocol (versioned packets)
  - CLI binary `pty-server create/attach/list/kill/view`
- [x] **Phase 2: Metadata & Agent Presets**
  - JSON-based session/group persistence (`~/.config/zed-sessions/sessions.json`)
  - Session liveness detection via socket probe
  - Agent preset system with per-agent launch/resume definitions
  - Built-in presets: Claude Code, Codex, Gemini, Copilot, Amp, Terminal
  - Resume strategies: `none`, `continue_in_directory`, `resume_by_id`
- [x] **Phase 3: Session Panel UI** (`crates/session_panel/`)
  - Tree view with groups → sessions, status indicators (running/dead)
  - Right-click context menus (rename, delete, resume)
  - **+** opens an agent preset picker; auto-names sessions (`claude-1`, `zsh-2`)
  - Inline rename via `Editor::single_line`
  - Click on dead session → auto-resume via preset's resume strategy
- [x] **Phase 4: Terminal Integration** (combined panel)
  - Session panel **embeds the only `TerminalPanel` instance** — replaces the
    bare terminal panel registration in `zed.rs`
  - Layout: collapsible sidebar (sessions list) + embedded TerminalPanel,
    bottom dock by default; sidebar can be toggled to make the panel act
    exactly like the bare terminal panel
  - Click a session → spawns `pty-server attach` as a tab in the embedded
    terminal panel
- [x] **Reattach state replay** — tmux-style with one-up
  - vt100 virtual terminal in the daemon tracks live screen state
  - Hybrid history: `main_log` byte buffer for shell scrollback (recorded
    only when *not* in alt-screen, so TUI redraws don't pollute history) +
    vt100's `contents_formatted` overlay when reattaching to a TUI app
  - On reattach: replay shell history first, then layer alt-screen via
    `\x1b[?1049h` if a TUI is running. Exiting the TUI naturally drops
    back to shell history — strictly better than tmux which always wraps
    the whole session in alt-screen
  - Per-session `.state` (full snapshot) and `.scrollback` (alt-screen
    flattened to plain text) files persisted every 5s + on graceful exit
- [x] **Frozen-state resume**
  - Click a dead session → daemon resumes via preset, prior `.scrollback`
    is replayed as scrollback above the fresh shell prompt
  - `pty-server view <state>` subcommand for read-only inspection
- [x] **stdin filter in attach client**
  - Strips terminal-response CSI sequences (DA1, DECRPM, cursor-position
    reports) so they never reach the daemon's PTY where they'd get echoed
    back as visible junk in scrollback

### Next
- [ ] **Phase 5: Remote Integration** (SSH via `zed-remote-server`)
- [ ] **Phase 6: Polish** (keybindings, search, drag-and-drop, settings)

## Key Design Decisions

### PTY persistence: Rust reimplementation of abduco (not tmux)
- abduco's core is ~500 lines of C: double-fork, forkpty, unix socket, byte shuttle
- Our Rust version: same logic, uses libc directly for PTY/poll, nix for fork/signal
- No tmux — we don't want a second multiplexer fighting Zed's UI
- Sessions survive Zed closing but NOT reboots
- On reboot: detect dead sessions, offer restart using agent resume strategy

### Agent preset plugin system
Each agent type is defined declaratively:
```json
{
  "name": "Claude Code",
  "binary": "claude",
  "launch_args": ["--dangerously-skip-permissions", "--name", "{session_id}"],
  "resume": {
    "kind": "resume_by_id",
    "args": ["--dangerously-skip-permissions", "--resume", "{session_id}"]
  }
}
```
Resume strategies:
- **none** — agent can't resume, just relaunch fresh (Codex, Copilot, Terminal)
- **continue_in_directory** — agent resumes last conversation in cwd (Gemini)
- **resume_by_id** — agent resumes specific session by ID (Claude Code)

This means after a reboot or crash, we can automatically recover agent conversations
where the agent supports it, using our stored session metadata.

### pty_server is a library, not a separate binary
- Compiled into Zed directly for local use
- Compiled into `zed-remote-server` for remote/SSH use
- Same code path whether local or remote
- No second binary to upload, version-check, or lifecycle-manage
- CLI binary (`pty-server`) exists for standalone testing/development

### Remote/SSH integration
Zed already uploads `zed-remote-server` to SSH hosts. By including pty_server
as a dependency of remote_server:
- Sessions on remote hosts persist when Zed disconnects (the whole point)
- Session panel talks to pty_server via the existing Zed RPC channel
- No separate SSH exec needed — reuses the connection Zed already has
- `pty-server` binary is NOT shipped separately

### Upstream compatibility
95% of work is in new crates. Existing Zed code touched:
- `Cargo.toml` — 2 lines (workspace member + dependency)
- `crates/zed/src/main.rs` — 1 line (`session_panel::init(cx);`)
- `crates/zed/src/zed.rs` — ~5 lines (register session panel, drop the
  standalone `terminal_panel` registration since session panel embeds it)
- `crates/remote_server/Cargo.toml` — 1 line (add pty_server dep, future)
- Rebase strategy: weekly onto upstream/main, conflict surface is tiny

### Folder-based workspacing (not repo-scoped)
- Groups point to any directory, not necessarily a git repo
- Multiple groups can point to the same directory
- No git worktree management — that's Conductor's model, not ours

## Crates

### `crates/pty_server/` (done)
PTY persistence library + CLI. Modules:
- `protocol.rs` — Wire format: versioned packets (content/attach/detach/resize/exit)
- `server.rs` — PTY holder: fork, openpty, poll loop, multi-client unix socket,
  vt100 virtual terminal + main_log byte buffer (`History` type), atomic
  `.state`/`.scrollback` persistence
- `client.rs` — Attach to session socket, raw terminal mode, byte bridge,
  stdin filter that strips terminal-response CSI sequences
- `metadata.rs` — Groups, sessions, agent presets, JSON persistence, status refresh

### `crates/session_panel/` (done)
Combined session panel + terminal panel. Single `SessionPanel` struct that:
- Owns a `SessionStore` for groups/sessions/presets
- Owns the only `Entity<TerminalPanel>` in the workspace (replaces
  Zed's standalone terminal panel registration)
- Renders a horizontal split: collapsible session-list sidebar +
  embedded TerminalPanel
- Click a session entry → spawns `pty-server attach` as a tab in the
  embedded terminal panel
- Click a dead session → resumes via preset, replays prior `.scrollback`
  as plain-text scrollback above the fresh shell prompt
- **+** opens an agent preset picker; auto-names new sessions and
  spawns the daemon immediately

## Implementation Phases

### Phase 5: Remote Integration (next)
- Add pty_server as dependency of remote_server crate
- Expose session create/attach/list/kill as RPC commands
- Session panel works identically for local and remote projects
- Sessions on remote hosts persist across Zed disconnects

### Phase 6: Polish
- Keyboard shortcuts for session switching
- Session search/filter
- Drag-and-drop reordering within groups
- Settings integration (default preset, socket dir, etc.)
- Custom agent preset definitions in user config
- Orphaned-daemon adoption UI (enumerate sockets in socket dir, cross-
  reference with our JSON, offer to adopt or kill)
- Process renaming for daemon (so `pkill zed` doesn't sweep daemons)

## License

Zed is GPL-3.0 for most crates, Apache-2.0 for some foundational ones.
Fork must remain GPL-3.0. Can sell/distribute but must keep source open.

## Links

- Fork: https://github.com/okaris/zed
- Upstream: https://github.com/zed-industries/zed
- abduco (reference): https://github.com/martanne/abduco
- Conductor (inspiration): https://conductor.build
