use std::path::PathBuf;

use gpui::{
    actions, deferred, div, px, uniform_list, Action, App, AsyncWindowContext, Context,
    DismissEvent, Entity, EventEmitter, FocusHandle, Focusable, InteractiveElement, IntoElement,
    MouseButton, MouseDownEvent, ParentElement, Pixels, Render, StatefulInteractiveElement,
    Styled, Subscription, UniformListScrollHandle, WeakEntity, Window,
};
use pty_server::{SessionStatus, SessionStore};
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
    inline_edit_text: String,

    context_menu: Option<(Entity<ContextMenu>, gpui::Point<Pixels>, Subscription)>,

    _subscriptions: Vec<Subscription>,
}

impl SessionPanel {
    pub fn new(
        workspace: &mut Workspace,
        _window: &mut Window,
        cx: &mut Context<Workspace>,
    ) -> Entity<Self> {
        let workspace_weak = workspace.weak_handle();

        cx.new(|cx| {
            let focus_handle = cx.focus_handle();

            let store_path = Self::default_store_path();
            let socket_dir = Self::default_socket_dir();
            let store = SessionStore::load(&store_path).unwrap_or_default();

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
                inline_edit_text: String::new(),
                context_menu: None,
                _subscriptions: Vec::new(),
            };

            // Expand all groups by default
            for group in &panel.store.groups {
                panel.expanded_groups.insert(group.id.clone());
            }

            panel.rebuild_visible_entries();
            panel
        })
    }

    pub async fn load(
        workspace: WeakEntity<Workspace>,
        mut cx: AsyncWindowContext,
    ) -> anyhow::Result<Entity<Self>> {
        workspace.update_in(&mut cx, |workspace, window, cx| {
            Self::new(workspace, window, cx)
        })
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

    fn new_group(&mut self, _: &NewGroup, _window: &mut Window, cx: &mut Context<Self>) {
        self.inline_edit = Some(InlineEdit::NewGroup);
        self.inline_edit_text.clear();
        cx.notify();
    }

    fn new_session(&mut self, _: &NewSession, _window: &mut Window, cx: &mut Context<Self>) {
        if let Some(group_id) = self.selected_group_id() {
            self.inline_edit = Some(InlineEdit::NewSession { group_id });
            self.inline_edit_text.clear();
            cx.notify();
        }
    }

    fn rename(&mut self, _: &Rename, _window: &mut Window, cx: &mut Context<Self>) {
        if let Some(index) = self.selected_index {
            if let Some(entry) = self.visible_entries.get(index) {
                match &entry.id {
                    EntryId::Group(group_id) => {
                        self.inline_edit_text = entry.label.clone();
                        self.inline_edit = Some(InlineEdit::RenameGroup {
                            group_id: group_id.clone(),
                        });
                    }
                    EntryId::Session {
                        group_id,
                        session_id,
                    } => {
                        self.inline_edit_text = entry.label.clone();
                        self.inline_edit = Some(InlineEdit::RenameSession {
                            group_id: group_id.clone(),
                            session_id: session_id.clone(),
                        });
                    }
                }
                cx.notify();
            }
        }
    }

    fn confirm(&mut self, _: &Confirm, _window: &mut Window, cx: &mut Context<Self>) {
        let text = self.inline_edit_text.trim().to_string();
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
                // Use first preset (Claude Code) by default
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
        cx.notify();
    }

    fn cancel(&mut self, _: &Cancel, _window: &mut Window, cx: &mut Context<Self>) {
        self.inline_edit = None;
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

    fn refresh_statuses(&mut self, cx: &mut Context<Self>) {
        self.store.refresh_statuses();
        self.rebuild_visible_entries();
        cx.notify();
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
                    menu.action("Rename Session", Box::new(Rename))
                        .separator()
                        .action("Delete Session", Box::new(Delete))
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
            .when(is_selected, |d| d.bg(cx.theme().colors().ghost_element_selected))
            .hover(|d| d.bg(cx.theme().colors().ghost_element_hover))
            .on_click(cx.listener(move |this, _event, _window, cx| {
                this.selected_index = Some(index);
                if is_group {
                    if let EntryId::Group(ref id) = entry_id {
                        this.toggle_group(id, cx);
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

    fn render_inline_editor(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let placeholder = match &self.inline_edit {
            Some(InlineEdit::NewGroup) => "Group name...",
            Some(InlineEdit::NewSession { .. }) => "Session name...",
            Some(InlineEdit::RenameGroup { .. }) => "Rename group...",
            Some(InlineEdit::RenameSession { .. }) => "Rename session...",
            None => "",
        };

        div()
            .w_full()
            .h(px(28.))
            .px_2()
            .flex()
            .items_center()
            .child(
                Label::new(if self.inline_edit_text.is_empty() {
                    placeholder.to_string()
                } else {
                    self.inline_edit_text.clone()
                })
                .size(LabelSize::Small)
                .color(if self.inline_edit_text.is_empty() {
                    Color::Muted
                } else {
                    Color::Default
                }),
            )
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
                IconButton::new("add-group", IconName::Plus)
                    .icon_size(IconSize::Small)
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.new_group(&NewGroup, window, cx);
                    })),
            )
    }

    fn render_empty(&self, cx: &mut Context<Self>) -> impl IntoElement {
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
        Some(IconName::TerminalAlt)
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
                                    entries
                                        .get(ix)
                                        .map(|entry| this.render_entry(ix, entry, cx).into_any_element())
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
