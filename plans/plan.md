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
- [x] **Phase 1: pty_server core** (`crates/pty_server/`, ~800 lines, 25 tests)
  - PTY holding: fork + openpty + unix socket + poll loop
  - Client attach/detach with protocol (versioned packets)
  - Session survives Zed close, reattach works
  - CLI binary for testing: `pty-server create/attach/list/kill`
- [x] **Phase 2: Metadata & Agent Presets**
  - JSON-based session/group persistence
  - Session liveness detection via socket probe
  - Agent preset system with per-agent launch/resume definitions
  - Built-in presets: Claude Code, Codex, Gemini, Copilot, Amp, Terminal
  - Resume strategies: `none`, `continue_in_directory`, `resume_by_id`

### Next
- [ ] **Phase 3: Session Panel UI**
- [ ] **Phase 4: Terminal Integration**
- [ ] **Phase 5: Remote Integration**

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
- `crates/zed/src/zed.rs` — 1 line (register session panel)
- `crates/remote_server/Cargo.toml` — 1 line (add pty_server dep)
- Rebase strategy: weekly onto upstream/main, conflict surface is tiny

### Folder-based workspacing (not repo-scoped)
- Groups point to any directory, not necessarily a git repo
- Multiple groups can point to the same directory
- No git worktree management — that's Conductor's model, not ours

## Crates

### `crates/pty_server/` (done)
PTY persistence library + CLI. Modules:
- `protocol.rs` — Wire format: versioned packets (content/attach/detach/resize/exit)
- `server.rs` — PTY holder: fork, openpty, poll loop, multi-client unix socket
- `client.rs` — Attach to session socket, raw terminal mode, byte bridge
- `metadata.rs` — Groups, sessions, agent presets, JSON persistence, status refresh

### `crates/session_panel/` (next)
Zed sidebar panel:
- Implements `Panel` trait (like ProjectPanel)
- Tree view: Groups > Sessions with status indicators
- Actions: create group, add session (pick agent preset), rename, archive, kill
- Click to attach session in terminal area
- Agent preset picker on new session
- Refresh timer to update session statuses

## Implementation Phases

### Phase 3: Session Panel UI (next)
- New crate implementing `Panel` trait
- Tree view with groups and sessions
- Create group (name + path), add session (name + preset)
- Click to focus/attach session in terminal area
- Status indicators (running/idle/dead)
- Right-click context menu (rename, kill, archive, resume dead session)

### Phase 4: Terminal Integration
- Modify Zed's terminal spawn to optionally attach to a pty_server session
- Zed terminal runs `pty-server attach <id>` or calls library directly
- Reattach on Zed startup for known live sessions
- Dead session detection → auto-resume using agent preset strategy

### Phase 5: Remote Integration
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

## License

Zed is GPL-3.0 for most crates, Apache-2.0 for some foundational ones.
Fork must remain GPL-3.0. Can sell/distribute but must keep source open.

## Links

- Fork: https://github.com/okaris/zed
- Upstream: https://github.com/zed-industries/zed
- abduco (reference): https://github.com/martanne/abduco
- Conductor (inspiration): https://conductor.build
