use collections::HashMap;
use std::path::PathBuf;

use editor::{Editor, EditorEvent};
use gpui::{
    actions, deferred, div, px, uniform_list, Action, App, AsyncWindowContext, Context,
    DismissEvent, Entity, EventEmitter, FocusHandle, Focusable, InteractiveElement, IntoElement,
    MouseButton, MouseDownEvent, ParentElement, Pixels, Render, StatefulInteractiveElement, Styled,
    Subscription, UniformListScrollHandle, WeakEntity, Window,
};
use pty_server::{SessionServer, SessionStatus, SessionStore};
use task::{RevealStrategy, RevealTarget, Shell, SpawnInTerminal, TaskId};
use terminal_view::terminal_panel::TerminalPanel;
use ui::prelude::*;
use ui::{ContextMenu, IconButton, IconName, Label};
use workspace::dock::{DockPosition, Panel, PanelEvent};
use workspace::Workspace;

actions!(
    session_panel,
    [
        /// Create a new session group.
        NewGroup,
        /// Create a new session in the selected group.
        NewSession,
        /// Rename the selected entry.
        Rename,
        /// Delete the selected entry.
        Delete,
        /// Resume the selected dead session via its preset's resume strategy.
        Resume,
        /// Confirm inline edit (rename/create).
        Confirm,
        /// Cancel inline edit.
        Cancel,
    ]
);

#[derive(Debug, Clone)]
enum EntryId {
    Group(String),
    Session { group_id: String, session_id: String },
}

#[derive(Debug, Clone)]
struct VisibleEntry {
    id: EntryId,
    depth: usize,
    is_group: bool,
    is_expanded: bool,
    label: String,
    status: Option<SessionStatus>,
    preset: Option<String>,
}

#[derive(Debug, Clone)]
enum InlineEdit {
    NewGroup,
    RenameGroup { group_id: String },
    NewSession { group_id: String },
    RenameSession { group_id: String, session_id: String },
}

pub struct SessionPanel {
    focus_handle: FocusHandle,
    workspace: WeakEntity<Workspace>,
    store: SessionStore,
    store_path: PathBuf,
    socket_dir: PathBuf,

    visible_entries: Vec<VisibleEntry>,
    expanded_groups: std::collections::HashSet<String>,
    selected_index: Option<usize>,
    scroll_handle: UniformListScrollHandle,

    inline_edit: Option<InlineEdit>,
    filename_editor: Entity<Editor>,

    context_menu: Option<(Entity<ContextMenu>, gpui::Point<Pixels>, Subscription)>,

    _subscriptions: Vec<Subscription>,
}

pub fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _window, _cx| {
        workspace.register_action(
            |workspace, _: &zed_actions::session_panel::ToggleFocus, window, cx| {
                workspace.toggle_panel_focus::<SessionPanel>(window, cx);
            },
        );
    })
    .detach();
}

impl SessionPanel {
    pub fn new(
        workspace: &mut Workspace,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) -> Entity<Self> {
        log::info!("[session_panel] new() called");
        let workspace_weak = workspace.weak_handle();

        cx.new(|cx| {
            log::info!("[session_panel] creating panel entity");
            let focus_handle = cx.focus_handle();

            let store_path = Self::default_store_path();
            let socket_dir = Self::default_socket_dir();
            let store = SessionStore::load(&store_path).unwrap_or_default();
            log::info!(
                "[session_panel] loaded store: {} groups, store_path={:?}",
                store.groups.len(),
                store_path
            );

            let filename_editor = cx.new(|cx| Editor::single_line(window, cx));
            let editor_subscription = cx.subscribe_in(
                &filename_editor,
                window,
                |session_panel: &mut Self, _, editor_event, window, cx| {
                    if let EditorEvent::Blurred = editor_event {
                        if session_panel.inline_edit.is_some() {
                            session_panel.confirm(&Confirm, window, cx);
                        }
                    }
                },
            );

            let mut panel = SessionPanel {
                focus_handle,
                workspace: workspace_weak,
                store,
                store_path,
                socket_dir,
                visible_entries: Vec::new(),
                expanded_groups: std::collections::HashSet::new(),
                selected_index: None,
                scroll_handle: UniformListScrollHandle::new(),
                inline_edit: None,
                filename_editor,
                context_menu: None,
                _subscriptions: vec![editor_subscription],
            };

            for group in &panel.store.groups {
                panel.expanded_groups.insert(group.id.clone());
            }

            panel.rebuild_visible_entries();
            log::info!("[session_panel] panel created with {} visible entries", panel.visible_entries.len());
            panel
        })
    }

    pub async fn load(
        workspace: WeakEntity<Workspace>,
        mut cx: AsyncWindowContext,
    ) -> anyhow::Result<Entity<Self>> {
        log::info!("[session_panel] load() starting");
        let result = workspace.update_in(&mut cx, |workspace, window, cx| {
            Self::new(workspace, window, cx)
        });
        match &result {
            Ok(_) => log::info!("[session_panel] load() succeeded"),
            Err(e) => log::error!("[session_panel] load() failed: {e}"),
        }
        result
    }

    fn default_store_path() -> PathBuf {
        let config_dir = std::env::var("XDG_CONFIG_HOME").unwrap_or_else(|_| {
            let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
            format!("{home}/.config")
        });
        PathBuf::from(config_dir).join("zed-sessions/sessions.json")
    }

    fn default_socket_dir() -> PathBuf {
        let uid = unsafe { libc::getuid() };
        let runtime_dir = std::env::var("XDG_RUNTIME_DIR")
            .unwrap_or_else(|_| format!("/tmp/pty-servers-{uid}"));
        PathBuf::from(runtime_dir).join("pty-servers")
    }

    fn save_store(&self) {
        self.store.save(&self.store_path).ok();
    }

    fn rebuild_visible_entries(&mut self) {
        self.visible_entries.clear();

        for group in &self.store.groups {
            let is_expanded = self.expanded_groups.contains(&group.id);

            self.visible_entries.push(VisibleEntry {
                id: EntryId::Group(group.id.clone()),
                depth: 0,
                is_group: true,
                is_expanded,
                label: group.name.clone(),
                status: None,
                preset: None,
            });

            if is_expanded {
                for session in &group.sessions {
                    if session.status == SessionStatus::Archived {
                        continue;
                    }
                    self.visible_entries.push(VisibleEntry {
                        id: EntryId::Session {
                            group_id: group.id.clone(),
                            session_id: session.id.clone(),
                        },
                        depth: 1,
                        is_group: false,
                        is_expanded: false,
                        label: session.name.clone(),
                        status: Some(session.status),
                        preset: session.preset.clone(),
                    });
                }
            }
        }
    }

    fn toggle_group(&mut self, group_id: &str, cx: &mut Context<Self>) {
        if self.expanded_groups.contains(group_id) {
            self.expanded_groups.remove(group_id);
        } else {
            self.expanded_groups.insert(group_id.to_string());
        }
        self.rebuild_visible_entries();
        cx.notify();
    }

    fn start_inline_edit(
        &mut self,
        edit: InlineEdit,
        initial_text: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.inline_edit = Some(edit);
        self.filename_editor.update(cx, |editor, cx| {
            editor.set_text(initial_text, window, cx);
            editor.select_all(&editor::actions::SelectAll, window, cx);
        });
        window.focus(&self.filename_editor.focus_handle(cx), cx);
        cx.notify();
    }

    fn new_group(&mut self, _: &NewGroup, window: &mut Window, cx: &mut Context<Self>) {
        self.start_inline_edit(InlineEdit::NewGroup, "", window, cx);
    }

    fn new_session(&mut self, _: &NewSession, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(group_id) = self.selected_group_id() {
            self.start_inline_edit(InlineEdit::NewSession { group_id }, "", window, cx);
        }
    }

    fn rename(&mut self, _: &Rename, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(index) = self.selected_index {
            if let Some(entry) = self.visible_entries.get(index) {
                let label = entry.label.clone();
                match &entry.id {
                    EntryId::Group(group_id) => {
                        self.start_inline_edit(
                            InlineEdit::RenameGroup {
                                group_id: group_id.clone(),
                            },
                            &label,
                            window,
                            cx,
                        );
                    }
                    EntryId::Session {
                        group_id,
                        session_id,
                    } => {
                        self.start_inline_edit(
                            InlineEdit::RenameSession {
                                group_id: group_id.clone(),
                                session_id: session_id.clone(),
                            },
                            &label,
                            window,
                            cx,
                        );
                    }
                }
            }
        }
    }

    fn confirm(&mut self, _: &Confirm, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.filename_editor.read(cx).text(cx).trim().to_string();
        if text.is_empty() {
            self.inline_edit = None;
            cx.notify();
            return;
        }

        match self.inline_edit.take() {
            Some(InlineEdit::NewGroup) => {
                let group = self.store.create_group(&text, PathBuf::from("."));
                self.expanded_groups.insert(group.id.clone());
                self.save_store();
                self.rebuild_visible_entries();
            }
            Some(InlineEdit::RenameGroup { group_id }) => {
                if let Some(group) = self.store.find_group_mut(&group_id) {
                    group.name = text;
                    self.save_store();
                    self.rebuild_visible_entries();
                }
            }
            Some(InlineEdit::NewSession { group_id }) => {
                let preset = self.store.presets.first().cloned();
                let command = preset
                    .as_ref()
                    .map(|p| p.launch_command())
                    .unwrap_or_else(|| vec!["bash".into()]);
                let preset_name = preset.map(|p| p.name.clone());

                let socket_path = self.socket_dir.join(format!("{text}.sock"));
                let working_dir = self
                    .store
                    .find_group(&group_id)
                    .map(|g| g.path.clone())
                    .unwrap_or_else(|| PathBuf::from("."));

                // Spawn the persistent PTY daemon
                match SessionServer::create(&text, &command, &working_dir, &self.socket_dir) {
                    Ok(actual_socket_path) => {
                        if let Ok(_session) = self.store.add_session(
                            &group_id,
                            &text,
                            command.clone(),
                            working_dir.clone(),
                            actual_socket_path,
                            preset_name,
                        ) {
                            self.save_store();
                            self.rebuild_visible_entries();
                        }
                    }
                    Err(_) => {
                        // Session server failed to start — still store metadata as dead
                        if let Ok(_session) = self.store.add_session(
                            &group_id,
                            &text,
                            command,
                            working_dir,
                            socket_path,
                            preset_name,
                        ) {
                            self.save_store();
                            self.rebuild_visible_entries();
                        }
                    }
                }
            }
            Some(InlineEdit::RenameSession {
                group_id,
                session_id,
            }) => {
                if let Some(group) = self.store.find_group_mut(&group_id) {
                    if let Some(session) = group
                        .sessions
                        .iter_mut()
                        .find(|s| s.id == session_id)
                    {
                        session.name = text;
                        self.save_store();
                        self.rebuild_visible_entries();
                    }
                }
            }
            None => {}
        }
        window.focus(&self.focus_handle, cx);
        cx.notify();
    }

    fn cancel(&mut self, _: &Cancel, window: &mut Window, cx: &mut Context<Self>) {
        self.inline_edit = None;
        window.focus(&self.focus_handle, cx);
        cx.notify();
    }

    fn delete(&mut self, _: &Delete, _window: &mut Window, cx: &mut Context<Self>) {
        if let Some(index) = self.selected_index {
            if let Some(entry) = self.visible_entries.get(index) {
                match &entry.id {
                    EntryId::Group(group_id) => {
                        self.store.groups.retain(|g| g.id != *group_id);
                        self.expanded_groups.remove(group_id);
                    }
                    EntryId::Session {
                        group_id,
                        session_id,
                    } => {
                        if let Some(group) = self.store.find_group_mut(group_id) {
                            // Kill the session server if it's running
                            if let Some(session) =
                                group.sessions.iter().find(|s| s.id == *session_id)
                            {
                                if session.status == SessionStatus::Running {
                                    SessionServer::kill_session(&session.socket_path).ok();
                                }
                            }
                            group.sessions.retain(|s| s.id != *session_id);
                        }
                    }
                }
                self.save_store();
                self.rebuild_visible_entries();
                self.selected_index = None;
                cx.notify();
            }
        }
    }

    fn selected_group_id(&self) -> Option<String> {
        let index = self.selected_index?;
        let entry = self.visible_entries.get(index)?;
        match &entry.id {
            EntryId::Group(id) => Some(id.clone()),
            EntryId::Session { group_id, .. } => Some(group_id.clone()),
        }
    }

    fn open_session(
        &mut self,
        group_id: &str,
        session_id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Refresh statuses first so we know if the session is alive.
        self.store.refresh_statuses();
        self.rebuild_visible_entries();

        let session_status = self
            .store
            .find_group(group_id)
            .and_then(|g| g.sessions.iter().find(|s| s.id == session_id))
            .map(|s| s.status);

        match session_status {
            Some(SessionStatus::Running) => {
                self.spawn_attach_terminal(group_id, session_id, None, window, cx);
            }
            Some(SessionStatus::Dead) => {
                // Capture the frozen scrollback file BEFORE resuming, since
                // the new daemon will start overwriting it. Snapshot it to a
                // sibling `.replay` file that the attach client will consume
                // and delete after dumping. The `.scrollback` file (vs
                // `.state`) flattens any prior alt-screen content into plain
                // scrollable text so it survives the alt-screen exit.
                let replay_path = self
                    .session_attach_info(group_id, session_id)
                    .and_then(|(name, socket_path, _)| {
                        let scrollback_path = socket_path.with_extension("scrollback");
                        if !scrollback_path.exists() {
                            return None;
                        }
                        let replay_path = self.socket_dir.join(format!("{name}.replay"));
                        std::fs::copy(&scrollback_path, &replay_path).ok()?;
                        Some(replay_path)
                    });

                if !self.try_resume_session(group_id, session_id) {
                    log::warn!("[session_panel] resume failed; nothing to attach to");
                    self.rebuild_visible_entries();
                    cx.notify();
                    return;
                }
                self.rebuild_visible_entries();
                self.spawn_attach_terminal(group_id, session_id, replay_path, window, cx);
            }
            _ => {}
        }
        cx.notify();
    }

    /// Spawn a terminal tab attached to a live session via `pty-server attach`.
    /// If `replay_path` is provided, the attach client dumps that file's bytes
    /// as scrollback before connecting — used to carry forward the previous
    /// session's frozen state when resuming a dead session.
    fn spawn_attach_terminal(
        &self,
        group_id: &str,
        session_id: &str,
        replay_path: Option<PathBuf>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some((label, socket_path, working_dir)) =
            self.session_attach_info(group_id, session_id)
        else {
            return;
        };
        let pty_server_bin = locate_pty_server_binary();

        let mut args: Vec<String> = vec!["attach".into()];
        if let Some(path) = replay_path.as_ref() {
            args.push("--replay".into());
            args.push(path.to_string_lossy().into_owned());
            args.push("--delete-replay".into());
        }
        args.push(socket_path.to_string_lossy().into_owned());

        let command_label = format!("{} {}", pty_server_bin.display(), args.join(" "));
        log::info!(
            "[session_panel] attaching to '{}' via {} ({})",
            label,
            pty_server_bin.display(),
            socket_path.display()
        );
        self.run_in_terminal_panel(
            SpawnInTerminal {
                id: TaskId(format!("attach:{label}").into()),
                full_label: label.clone(),
                label,
                command: Some(pty_server_bin.to_string_lossy().into_owned()),
                args,
                command_label,
                cwd: Some(working_dir),
                env: HashMap::default(),
                use_new_terminal: true,
                allow_concurrent_runs: true,
                reveal: RevealStrategy::Always,
                reveal_target: RevealTarget::Dock,
                hide: task::HideStrategy::Never,
                shell: Shell::System,
                show_summary: false,
                show_command: false,
                show_rerun: false,
                save: task::SaveStrategy::None,
            },
            window,
            cx,
        );
    }

    fn session_attach_info(
        &self,
        group_id: &str,
        session_id: &str,
    ) -> Option<(String, PathBuf, PathBuf)> {
        let group = self.store.find_group(group_id)?;
        let session = group.sessions.iter().find(|s| s.id == session_id)?;
        Some((
            session.name.clone(),
            session.socket_path.clone(),
            session.working_dir.clone(),
        ))
    }

    fn run_in_terminal_panel(
        &self,
        spawn: SpawnInTerminal,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        let terminal_panel = workspace.read(cx).panel::<TerminalPanel>(cx);
        let Some(terminal_panel) = terminal_panel else {
            log::warn!("[session_panel] terminal panel not available");
            return;
        };
        terminal_panel
            .update(cx, |panel, cx| panel.spawn_task(&spawn, window, cx))
            .detach_and_log_err(cx);
    }

    fn resume(&mut self, _: &Resume, _window: &mut Window, cx: &mut Context<Self>) {
        let Some(index) = self.selected_index else {
            return;
        };
        let Some(entry) = self.visible_entries.get(index).cloned() else {
            return;
        };
        let EntryId::Session {
            group_id,
            session_id,
        } = entry.id
        else {
            return;
        };
        if self.try_resume_session(&group_id, &session_id) {
            self.rebuild_visible_entries();
            cx.notify();
        }
    }

    /// Try to resume a dead session using its preset's resume strategy
    /// (or relaunch with the original command if no preset). Returns true
    /// if the session is now alive.
    fn try_resume_session(&mut self, group_id: &str, session_id: &str) -> bool {
        // Snapshot needed values
        let socket_dir = self.socket_dir.clone();
        let (session_name, working_dir, original_command, preset_name, old_socket) = {
            let Some(group) = self.store.find_group(group_id) else {
                return false;
            };
            let Some(session) = group.sessions.iter().find(|s| s.id == session_id) else {
                return false;
            };
            (
                session.name.clone(),
                session.working_dir.clone(),
                session.command.clone(),
                session.preset.clone(),
                session.socket_path.clone(),
            )
        };

        // Choose command: preset's resume command if available, else the original command.
        let command = preset_name
            .as_deref()
            .and_then(|name| self.store.find_preset(name))
            .and_then(|preset| preset.resume_command(&session_name, &working_dir))
            .map(|mut cmd| {
                for arg in cmd.iter_mut() {
                    *arg = arg.replace("{session_id}", &session_name);
                }
                cmd
            })
            .unwrap_or(original_command);

        // Clean up any stale socket file
        std::fs::remove_file(&old_socket).ok();

        log::info!(
            "[session_panel] resuming session '{session_name}' with cmd={command:?}"
        );

        match SessionServer::create(&session_name, &command, &working_dir, &socket_dir) {
            Ok(new_socket_path) => {
                if let Some(group) = self.store.find_group_mut(group_id) {
                    if let Some(session) =
                        group.sessions.iter_mut().find(|s| s.id == session_id)
                    {
                        session.socket_path = new_socket_path;
                        session.status = SessionStatus::Running;
                        session.command = command;
                    }
                }
                self.save_store();
                true
            }
            Err(e) => {
                log::error!("[session_panel] resume failed for '{session_name}': {e}");
                false
            }
        }
    }

    fn refresh_statuses(&mut self, cx: &mut Context<Self>) {
        self.store.refresh_statuses();
        self.rebuild_visible_entries();
        cx.notify();
    }

    fn deploy_preset_picker(
        &mut self,
        position: gpui::Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let presets: Vec<(String, Option<String>)> = self
            .store
            .presets
            .iter()
            .map(|p| (p.name.clone(), p.description.clone()))
            .collect();

        let weak_self = cx.weak_entity();

        let context_menu = ContextMenu::build(window, cx, |menu, _window, _cx| {
            let mut menu = menu;
            for (preset_name, description) in presets {
                let label = match description {
                    Some(desc) => format!("{preset_name} — {desc}"),
                    None => preset_name.clone(),
                };
                let weak = weak_self.clone();
                menu = menu.entry(label, None, move |window, cx| {
                    let preset_name = preset_name.clone();
                    weak.update(cx, |this, cx| {
                        this.create_session_with_preset(&preset_name, window, cx);
                    })
                    .ok();
                });
            }
            menu
        });

        window.focus(&context_menu.focus_handle(cx), cx);
        let subscription = cx.subscribe(&context_menu, |this, _, _: &DismissEvent, cx| {
            this.context_menu.take();
            cx.notify();
        });
        self.context_menu = Some((context_menu, position, subscription));
        cx.notify();
    }

    fn create_session_with_preset(
        &mut self,
        preset_name: &str,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let preset = match self.store.find_preset(preset_name).cloned() {
            Some(p) => p,
            None => {
                log::warn!("[session_panel] preset not found: {preset_name}");
                return;
            }
        };

        // Ensure a default group exists
        let group_id = if let Some(group) = self.store.groups.first() {
            group.id.clone()
        } else {
            let group = self.store.create_group("Sessions", PathBuf::from("."));
            self.expanded_groups.insert(group.id.clone());
            group.id.clone()
        };

        let working_dir = self
            .store
            .find_group(&group_id)
            .map(|g| g.path.clone())
            .unwrap_or_else(|| PathBuf::from("."));

        // Auto-generate a name like "claude-1", "claude-2"
        let session_name = self.next_session_name(&preset.binary, &group_id);

        let mut command = preset.launch_command();
        // Replace {session_id} placeholders
        for arg in command.iter_mut() {
            *arg = arg.replace("{session_id}", &session_name);
        }

        log::info!(
            "[session_panel] creating session '{}' with preset '{}', cmd={:?}",
            session_name,
            preset.name,
            command
        );

        match SessionServer::create(&session_name, &command, &working_dir, &self.socket_dir) {
            Ok(socket_path) => {
                if let Ok(_) = self.store.add_session(
                    &group_id,
                    &session_name,
                    command,
                    working_dir,
                    socket_path,
                    Some(preset.name.clone()),
                ) {
                    self.save_store();
                    self.rebuild_visible_entries();
                    cx.notify();
                }
            }
            Err(e) => {
                log::error!(
                    "[session_panel] failed to spawn session server '{session_name}': {e}"
                );
            }
        }
    }

    fn next_session_name(&self, binary: &str, group_id: &str) -> String {
        let prefix = std::path::Path::new(binary)
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("session")
            .to_string();
        let group = match self.store.find_group(group_id) {
            Some(g) => g,
            None => return format!("{prefix}-1"),
        };
        let mut n = 1;
        loop {
            let candidate = format!("{prefix}-{n}");
            if !group.sessions.iter().any(|s| s.name == candidate) {
                return candidate;
            }
            n += 1;
        }
    }

    fn deploy_context_menu(
        &mut self,
        position: gpui::Point<Pixels>,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.selected_index = Some(index);
        let entry = self.visible_entries.get(index).cloned();

        let context_menu = ContextMenu::build(window, cx, |menu, _window, _cx| {
            let menu = menu.when_some(entry, |menu, entry| {
                if entry.is_group {
                    menu.action("New Session", Box::new(NewSession))
                        .action("Rename Group", Box::new(Rename))
                        .separator()
                        .action("Delete Group", Box::new(Delete))
                } else {
                    let menu = menu.action("Rename Session", Box::new(Rename));
                    let menu = if entry.status == Some(SessionStatus::Dead) {
                        menu.separator().action("Resume Session", Box::new(Resume))
                    } else {
                        menu
                    };
                    menu.separator().action("Delete Session", Box::new(Delete))
                }
            });
            menu
        });

        window.focus(&context_menu.focus_handle(cx), cx);
        let subscription = cx.subscribe(&context_menu, |this, _, _: &DismissEvent, cx| {
            this.context_menu.take();
            cx.notify();
        });
        self.context_menu = Some((context_menu, position, subscription));
        cx.notify();
    }

    fn render_entry(
        &self,
        index: usize,
        entry: &VisibleEntry,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let is_selected = self.selected_index == Some(index);
        let entry_id = entry.id.clone();
        let is_group = entry.is_group;
        let is_expanded = entry.is_expanded;
        let depth = entry.depth;

        let status_color = match entry.status {
            Some(SessionStatus::Running) => Some(gpui::green()),
            Some(SessionStatus::Dead) => Some(gpui::red()),
            _ => None,
        };

        let indent = px(depth as f32 * 16.0 + 8.0);

        div()
            .id(SharedString::from(format!("entry-{index}")))
            .w_full()
            .h(px(28.))
            .pl(indent)
            .flex()
            .items_center()
            .gap_1()
            .cursor_pointer()
            .when(is_selected, |d| {
                d.bg(cx.theme().colors().ghost_element_selected)
            })
            .hover(|d| d.bg(cx.theme().colors().ghost_element_hover))
            .on_click(cx.listener(move |this, _event, window, cx| {
                this.selected_index = Some(index);
                match &entry_id {
                    EntryId::Group(id) => {
                        this.toggle_group(id, cx);
                    }
                    EntryId::Session {
                        group_id,
                        session_id,
                    } => {
                        this.open_session(group_id, session_id, window, cx);
                    }
                }
                cx.notify();
            }))
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                    this.deploy_context_menu(event.position, index, window, cx);
                }),
            )
            .when(is_group, |d| {
                d.child(
                    Icon::new(if is_expanded {
                        IconName::ChevronDown
                    } else {
                        IconName::ChevronRight
                    })
                    .size(IconSize::Small)
                    .color(Color::Muted),
                )
            })
            .when(!is_group, |d| {
                d.child(
                    div()
                        .w(px(8.))
                        .h(px(8.))
                        .rounded_full()
                        .when_some(status_color, |d, color| d.bg(color)),
                )
            })
            .child(
                Label::new(entry.label.clone())
                    .size(LabelSize::Small)
                    .when(is_group, |l| l.weight(gpui::FontWeight::BOLD)),
            )
            .when_some(entry.preset.as_ref(), |d, preset| {
                d.child(
                    Label::new(preset.clone())
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
                )
            })
    }

    fn render_inline_editor(&self, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .w_full()
            .h(px(28.))
            .px_2()
            .flex()
            .items_center()
            .child(self.filename_editor.clone())
    }

    fn render_header(&self, cx: &mut Context<Self>) -> impl IntoElement {
        h_flex()
            .w_full()
            .h(px(32.))
            .px_2()
            .flex()
            .items_center()
            .justify_between()
            .child(
                Label::new("Sessions")
                    .size(LabelSize::Small)
                    .weight(gpui::FontWeight::BOLD)
                    .color(Color::Muted),
            )
            .child(
                h_flex()
                    .gap_1()
                    .child(
                        IconButton::new("refresh", IconName::ArrowCircle)
                            .icon_size(IconSize::Small)
                            .on_click(cx.listener(|this, _, _window, cx| {
                                this.refresh_statuses(cx);
                            })),
                    )
                    .child(
                        IconButton::new("new-session", IconName::Plus)
                            .icon_size(IconSize::Small)
                            .tooltip(ui::Tooltip::text("New Session"))
                            .on_click(cx.listener(|this, event: &gpui::ClickEvent, window, cx| {
                                this.deploy_preset_picker(event.position(), window, cx);
                            })),
                    ),
            )
    }

    fn render_empty(&self, _cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .size_full()
            .items_center()
            .justify_center()
            .gap_2()
            .child(
                Label::new("No session groups yet")
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            )
            .child(
                Label::new("Click + to create one")
                    .size(LabelSize::XSmall)
                    .color(Color::Muted),
            )
    }
}

impl EventEmitter<PanelEvent> for SessionPanel {}

impl Focusable for SessionPanel {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Panel for SessionPanel {
    fn persistent_name() -> &'static str {
        "SessionPanel"
    }

    fn panel_key() -> &'static str {
        "SessionPanel"
    }

    fn position(&self, _window: &Window, _cx: &App) -> DockPosition {
        DockPosition::Right
    }

    fn position_is_valid(&self, position: DockPosition) -> bool {
        matches!(position, DockPosition::Left | DockPosition::Right)
    }

    fn set_position(
        &mut self,
        _position: DockPosition,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) {
    }

    fn default_size(&self, _window: &Window, _cx: &App) -> Pixels {
        px(260.)
    }

    fn icon(&self, _window: &Window, _cx: &App) -> Option<IconName> {
        Some(IconName::ListTree)
    }

    fn icon_tooltip(&self, _window: &Window, _cx: &App) -> Option<&'static str> {
        Some("Session Panel")
    }

    fn toggle_action(&self) -> Box<dyn Action> {
        Box::new(zed_actions::session_panel::ToggleFocus)
    }

    fn starts_open(&self, _window: &Window, _cx: &App) -> bool {
        false
    }

    fn activation_priority(&self) -> u32 {
        3
    }
}

impl Render for SessionPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        log::info!("[session_panel] render() called, entries={}", self.visible_entries.len());
        let entry_count = self.visible_entries.len();
        let has_entries = entry_count > 0;
        let has_inline_edit = self.inline_edit.is_some();

        v_flex()
            .size_full()
            .track_focus(&self.focus_handle(cx))
            .on_action(cx.listener(Self::new_group))
            .on_action(cx.listener(Self::new_session))
            .on_action(cx.listener(Self::rename))
            .on_action(cx.listener(Self::delete))
            .on_action(cx.listener(Self::resume))
            .on_action(cx.listener(Self::confirm))
            .on_action(cx.listener(Self::cancel))
            .child(self.render_header(cx))
            .when(!has_entries && !has_inline_edit, |d| {
                d.child(self.render_empty(cx))
            })
            .when(has_entries, |d| {
                let entries: Vec<VisibleEntry> = self.visible_entries.clone();
                d.child(
                    uniform_list(
                        "session-entries",
                        entry_count,
                        cx.processor(move |this, range: std::ops::Range<usize>, _window, cx| {
                            range
                                .into_iter()
                                .filter_map(|ix| {
                                    entries.get(ix).map(|entry| {
                                        this.render_entry(ix, entry, cx).into_any_element()
                                    })
                                })
                                .collect()
                        }),
                    )
                    .flex_grow()
                    .track_scroll(&self.scroll_handle),
                )
            })
            .when(has_inline_edit, |d| d.child(self.render_inline_editor(cx)))
            .children(self.context_menu.as_ref().map(|(menu, _position, _)| {
                deferred(menu.clone().into_any_element())
            }))
    }
}

/// Locate the pty-server binary. Looks for it next to the running zed binary
/// first (development build), then falls back to PATH lookup.
fn locate_pty_server_binary() -> PathBuf {
    if let Ok(current_exe) = std::env::current_exe() {
        if let Some(dir) = current_exe.parent() {
            let candidate = dir.join("pty-server");
            if candidate.exists() {
                return candidate;
            }
        }
    }
    PathBuf::from("pty-server")
}
