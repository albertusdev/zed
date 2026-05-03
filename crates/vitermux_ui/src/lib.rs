use anyhow::{Result, anyhow};
use collections::{HashMap, HashSet};
use db::kvp::KeyValueStore;
use editor::{Editor, MultiBufferOffset};
use git_ui::project_diff::ProjectDiff;
use gpui::{
    Action, AnyElement, App, AsyncWindowContext, Context, DismissEvent, Entity, EventEmitter,
    FocusHandle, Focusable, ListAlignment, ListOffset, ListSizingBehavior, ListState,
    ParentElement, Pixels, Render, SharedString, StatefulInteractiveElement, Styled, Subscription,
    Task, WeakEntity, Window, WindowHandle, actions, list, px,
};
use menu::{Cancel, Confirm, SelectFirst, SelectLast, SelectNext, SelectPrevious};
use parking_lot::Mutex;
use project::{ProjectPath, git_store::branch_diff::DiffBase};
use remote::{RemoteConnectionOptions, SshConnectionOptions, same_remote_connection_identity};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use task::{
    HideStrategy, RevealStrategy, RevealTarget, SaveStrategy, Shell, SpawnInTerminal, TaskId,
};
use terminal_view::{TerminalView, terminal_panel::TerminalPanel};
use ui::{
    Color, Disableable, Disclosure, Icon, IconButton, IconName, IconSize, KeyBinding, Label,
    LabelSize, ListItem, ListItemSpacing, Toggleable, Tooltip, prelude::*,
};
use vitermux::{
    OpenPlanFailure, TmuxTreeSnapshot, TmuxWindow, VitermuxClient, VitermuxConnectionState,
    VitermuxStore, ZedOpenPlan,
};
use workspace::{
    ModalView, MultiWorkspace, OpenMode, Pane, PathList, Toast, Workspace,
    dock::{DockPosition, Panel, PanelEvent},
    notifications::{DetachAndPromptErr, NotificationId},
};

const VITERMUX_PANEL_KEY: &str = "VitermuxPanel";
const PROJECT_REPOSITORY_DISCOVERY_ATTEMPTS: usize = 60;
const PROJECT_REPOSITORY_DISCOVERY_DELAY: Duration = Duration::from_millis(50);
const REVIEW_COMPANION_SYNC_ATTEMPTS: usize = 60;
const REVIEW_COMPANION_SYNC_DELAY: Duration = Duration::from_millis(50);
const MUTATION_REFRESH_ATTEMPTS: usize = 3;
const MUTATION_REFRESH_DELAY: Duration = Duration::from_millis(150);
const PENDING_LABEL_TIMEOUT: Duration = Duration::from_secs(5);
const SESSION_SLOT_COUNT: usize = 9;
const SESSION_SLOT_SCOPE_KEY: &str = "vitermux_session_slots";
const OPERATOR_WORKSPACE_SCOPE_KEY: &str = "vitermux_operator_workspace";
const REVIEW_COMPANION_SCOPE_KEY: &str = "vitermux_review_companion";
const COLLAPSED_HOST_SCOPE_KEY: &str = "vitermux_collapsed_hosts";
const OPERATOR_WORKSPACE_CONTEXT_KEY: &str = "VitermuxOperatorWorkspace";

actions!(
    vitermux_panel,
    [
        Toggle,
        ToggleFocus,
        Refresh,
        OpenSelected,
        OpenReviewSelected,
        ToggleReviewCompanion,
        CompleteSelected,
        RenameSelected,
    ]
);

#[derive(Clone, Deserialize, PartialEq, JsonSchema, Action)]
#[action(namespace = vitermux_panel)]
pub struct ActivateSlot(pub usize);

#[derive(Clone, Deserialize, PartialEq, JsonSchema, Action)]
#[action(namespace = vitermux_panel)]
pub struct AssignSelectedToSlot(pub usize);

pub fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _, _| {
        workspace.register_action(|workspace, _: &ToggleFocus, window, cx| {
            workspace.toggle_panel_focus::<VitermuxPanel>(window, cx);
        });
        workspace.register_action(|workspace, _: &Toggle, window, cx| {
            if !workspace.toggle_panel_focus::<VitermuxPanel>(window, cx) {
                workspace.close_panel::<VitermuxPanel>(window, cx);
            }
        });
        workspace.register_action(|workspace, _: &Refresh, _window, cx| {
            if let Some(panel) = workspace.panel::<VitermuxPanel>(cx) {
                panel.update(cx, |panel, cx| panel.refresh(cx));
            }
        });
        workspace.register_action(|workspace, _: &OpenSelected, window, cx| {
            if let Some(panel) = workspace.panel::<VitermuxPanel>(cx) {
                panel.update(cx, |panel, cx| panel.open_selected(window, cx));
            }
        });
        workspace.register_action(|workspace, _: &OpenReviewSelected, window, cx| {
            if let Some(panel) = workspace.panel::<VitermuxPanel>(cx) {
                panel.update(cx, |panel, cx| panel.open_selected_review(window, cx));
            }
        });
        workspace.register_action(|workspace, _: &ToggleReviewCompanion, window, cx| {
            if let Some(panel) = workspace.panel::<VitermuxPanel>(cx) {
                panel.update(cx, |panel, cx| panel.toggle_review_companion(window, cx));
            }
        });
        workspace.register_action(|workspace, _: &CompleteSelected, window, cx| {
            if let Some(panel) = workspace.panel::<VitermuxPanel>(cx) {
                panel.update(cx, |panel, cx| panel.complete_selected(window, cx));
            }
        });
        workspace.register_action(|workspace, _: &RenameSelected, window, cx| {
            if let Some(panel) = workspace.panel::<VitermuxPanel>(cx) {
                panel.update(cx, |panel, cx| panel.rename_selected(window, cx));
            }
        });
        workspace.register_action(|workspace, action: &ActivateSlot, window, cx| {
            if let Some(panel) = workspace.panel::<VitermuxPanel>(cx) {
                panel.update(cx, |panel, cx| panel.activate_slot(action, window, cx));
            }
        });
        workspace.register_action(|workspace, action: &AssignSelectedToSlot, window, cx| {
            if let Some(panel) = workspace.panel::<VitermuxPanel>(cx) {
                panel.update(cx, |panel, cx| {
                    panel.assign_selected_to_slot(action, window, cx)
                });
            }
        });
    })
    .detach();
}

#[derive(Clone)]
struct WorkbenchRow {
    row_key: SharedString,
    node_section_key: SharedString,
    node_label: SharedString,
    node_is_local: bool,
    session_section_key: SharedString,
    session_label: SharedString,
    session_detail: SharedString,
    window_label: SharedString,
    window_detail: SharedString,
    session_key: Option<SharedString>,
    dedupe_key: SharedString,
    attention: SharedString,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct SessionSlotAssignment {
    #[serde(default)]
    terminal_key: String,
    #[serde(default)]
    session_key: String,
    #[serde(default)]
    dedupe_key: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct SessionRenameTarget {
    row_key: String,
    session_key: String,
    current_name: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct PendingSessionLabel {
    label: SharedString,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct SerializedSessionSlotState {
    #[serde(default = "session_slot_state_version")]
    version: u8,
    #[serde(default)]
    slots: Vec<Option<SessionSlotAssignment>>,
}

impl Default for SerializedSessionSlotState {
    fn default() -> Self {
        Self {
            version: session_slot_state_version(),
            slots: empty_session_slots(),
        }
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct SerializedOperatorWorkspaceState {
    #[serde(default)]
    enabled: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct SerializedReviewCompanionState {
    #[serde(default = "default_review_companion_enabled")]
    enabled: bool,
}

impl Default for SerializedReviewCompanionState {
    fn default() -> Self {
        Self {
            enabled: default_review_companion_enabled(),
        }
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct SerializedCollapsedHostState {
    #[serde(default)]
    host_keys: Vec<String>,
}

#[derive(Default)]
struct OpenRequestTracker {
    next_request_id: u64,
    latest_request_id: u64,
}

#[derive(Clone)]
enum DisplayRow {
    HostHeader {
        row_key: SharedString,
        host_key: SharedString,
        label: SharedString,
        local_badge: bool,
        collapsed: bool,
    },
    SessionHeader {
        row_key: SharedString,
        label: SharedString,
        detail: SharedString,
    },
    Window(WorkbenchRow),
}

pub struct VitermuxPanel {
    workspace: WeakEntity<Workspace>,
    persistence_key: Option<String>,
    operator_workspace_enabled: bool,
    store: Entity<VitermuxStore>,
    focus_handle: FocusHandle,
    list_state: ListState,
    open_request_tracker: Arc<Mutex<OpenRequestTracker>>,
    open_plan_cache: Arc<Mutex<HashMap<String, ZedOpenPlan>>>,
    rows: Vec<WorkbenchRow>,
    display_rows: Vec<DisplayRow>,
    row_index_by_key: HashMap<String, usize>,
    row_index_by_session_key: HashMap<String, usize>,
    row_index_by_terminal_key: HashMap<String, usize>,
    display_row_index_by_row_key: HashMap<String, usize>,
    assigned_slot_by_row_key: HashMap<String, usize>,
    session_slots: Vec<Option<SessionSlotAssignment>>,
    collapsed_host_keys: HashSet<String>,
    pending_session_labels: HashMap<String, PendingSessionLabel>,
    selected_row_key: Option<SharedString>,
    last_focused_terminal_key: Option<String>,
    review_companion_enabled: bool,
    active: bool,
    width: Pixels,
    _subscriptions: Vec<Subscription>,
}

impl VitermuxPanel {
    pub async fn load(
        workspace: WeakEntity<Workspace>,
        mut cx: AsyncWindowContext,
    ) -> Result<Entity<Self>> {
        let workspace_handle = workspace.clone();
        workspace.update_in(&mut cx, |workspace, window, cx| {
            let persistence_key = workspace_persistence_key(workspace);
            let operator_workspace_enabled =
                load_operator_workspace_enabled(persistence_key.as_deref(), cx).unwrap_or(false);
            let review_companion_enabled =
                load_review_companion_enabled(persistence_key.as_deref(), cx)
                    .unwrap_or_else(default_review_companion_enabled);
            let session_slots = load_session_slots(persistence_key.as_deref(), cx)
                .unwrap_or_else(empty_session_slots);
            let collapsed_host_keys =
                load_collapsed_host_keys(persistence_key.as_deref(), cx).unwrap_or_default();
            cx.new(|cx| {
                Self::new(
                    workspace_handle.clone(),
                    persistence_key.clone(),
                    operator_workspace_enabled,
                    review_companion_enabled,
                    session_slots.clone(),
                    collapsed_host_keys.clone(),
                    window,
                    cx,
                )
            })
        })
    }

    fn new(
        workspace: WeakEntity<Workspace>,
        persistence_key: Option<String>,
        operator_workspace_enabled: bool,
        review_companion_enabled: bool,
        session_slots: Vec<Option<SessionSlotAssignment>>,
        collapsed_host_keys: HashSet<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let store = VitermuxStore::global(cx);
        // The workbench inbox is operator-critical, so we prefer deterministic
        // far-jump selection reveal over cheaper visible-only measurement.
        let list_state = ListState::new(0, ListAlignment::Top, px(1000.)).measure_all();
        let mut subscriptions = vec![cx.observe(&store, |this, _, cx| {
            this.refresh_view_model(cx);
            this.ensure_selection(cx);
            this.scroll_selection_into_view();
            cx.notify();
        })];
        if let Some(workspace_entity) = workspace.upgrade() {
            subscriptions.push(cx.subscribe_in(
                &workspace_entity,
                window,
                |this, workspace, event: &workspace::Event, window, cx| {
                    if let workspace::Event::ActiveItemChanged = event {
                        this.sync_to_focused_terminal(workspace.clone(), window, cx);
                    }
                },
            ));
        }
        let mut this = Self {
            workspace,
            persistence_key,
            operator_workspace_enabled,
            store: store.clone(),
            focus_handle: cx.focus_handle(),
            list_state,
            open_request_tracker: Arc::default(),
            open_plan_cache: Arc::default(),
            rows: Vec::new(),
            display_rows: Vec::new(),
            row_index_by_key: HashMap::default(),
            row_index_by_session_key: HashMap::default(),
            row_index_by_terminal_key: HashMap::default(),
            display_row_index_by_row_key: HashMap::default(),
            assigned_slot_by_row_key: HashMap::default(),
            session_slots,
            collapsed_host_keys,
            pending_session_labels: HashMap::default(),
            selected_row_key: None,
            last_focused_terminal_key: None,
            review_companion_enabled,
            active: false,
            width: px(336.0),
            _subscriptions: subscriptions,
        };
        this.refresh_view_model(cx);
        this.ensure_selection(cx);
        this.scroll_selection_into_view();
        this.defer_operator_workspace_context_sync(window, cx);
        this
    }

    fn refresh(&mut self, cx: &mut Context<Self>) {
        self.store.update(cx, |store, cx| store.refresh(cx));
    }

    fn refresh_view_model(&mut self, cx: &mut Context<Self>) {
        self.rows = self
            .store
            .read(cx)
            .snapshot()
            .cloned()
            .map(|snapshot| {
                let mut rows = flatten_rows(&snapshot);
                self.apply_pending_session_label_overlays(&mut rows);
                rows
            })
            .unwrap_or_default();
        self.row_index_by_key = build_row_index_by_key(&self.rows);
        self.row_index_by_session_key = build_row_index_by_session_key(&self.rows);
        self.row_index_by_terminal_key = build_row_index_by_terminal_key(&self.rows);
        let seeded_slots = self.maybe_seed_session_slots(cx);
        self.assigned_slot_by_row_key = build_assigned_slot_by_row_key(
            &self.session_slots,
            &self.rows,
            &self.row_index_by_session_key,
            &self.row_index_by_terminal_key,
        );
        self.rebuild_display_rows();
        prune_cached_open_plans(
            &self.open_plan_cache,
            &self.rows,
            &self.row_index_by_session_key,
            &self.row_index_by_terminal_key,
        );
        if seeded_slots {
            cx.notify();
        }
    }

    fn apply_pending_session_label_overlays(&mut self, rows: &mut [WorkbenchRow]) {
        apply_pending_label_overlays(rows, &mut self.pending_session_labels);
    }

    fn ensure_selection(&mut self, _cx: &mut Context<Self>) {
        let visible_rows = self.visible_rows();
        if visible_rows.is_empty() {
            self.selected_row_key = None;
            return;
        }

        let selected_exists = self.selected_row_key.as_ref().is_some_and(|selected| {
            visible_rows
                .iter()
                .any(|row| row.row_key.as_ref() == selected.as_ref())
        });
        if !selected_exists {
            self.selected_row_key = visible_rows.first().map(|row| row.row_key.clone());
        }
    }

    fn selected_row(&self) -> Option<WorkbenchRow> {
        let selected = self.selected_row_key.as_ref()?;
        self.row_index_by_key
            .get(selected.as_ref())
            .and_then(|index| self.rows.get(*index))
            .cloned()
    }

    fn selected_display_row_index(&self) -> Option<usize> {
        self.selected_row_key.as_ref().and_then(|selected| {
            self.display_row_index_by_row_key
                .get(selected.as_ref())
                .copied()
        })
    }

    fn visible_rows(&self) -> Vec<WorkbenchRow> {
        self.display_rows
            .iter()
            .filter_map(|row| match row {
                DisplayRow::Window(row) => Some(row.clone()),
                DisplayRow::HostHeader { .. } | DisplayRow::SessionHeader { .. } => None,
            })
            .collect()
    }

    fn rebuild_display_rows(&mut self) {
        self.display_rows = build_display_rows(&self.rows, &self.collapsed_host_keys);
        self.display_row_index_by_row_key = build_display_row_index_by_row_key(&self.display_rows);
        if self.list_state.item_count() != self.display_rows.len() {
            self.list_state.reset(self.display_rows.len());
        } else {
            self.list_state.remeasure();
        }
    }

    fn ensure_host_expanded_for_row(&mut self, row: &WorkbenchRow, cx: &mut Context<Self>) {
        if self
            .collapsed_host_keys
            .remove(row.node_section_key.as_ref())
        {
            self.save_collapsed_host_keys(cx);
            self.rebuild_display_rows();
        }
    }

    fn toggle_host_collapsed(&mut self, host_key: SharedString, cx: &mut Context<Self>) {
        let host_key = host_key.to_string();
        if !self.collapsed_host_keys.insert(host_key.clone()) {
            self.collapsed_host_keys.remove(host_key.as_str());
        }
        self.save_collapsed_host_keys(cx);
        self.rebuild_display_rows();
        self.ensure_selection(cx);
        self.scroll_selection_into_view();
        cx.notify();
    }

    fn scroll_selection_into_view(&self) {
        if let Some(index) = self.selected_display_row_index() {
            let before = self.list_state.logical_scroll_top();
            self.list_state.scroll_to_reveal_item(index);
            let after = self.list_state.logical_scroll_top();
            if index > after.item_ix
                && after.item_ix == before.item_ix
                && after.offset_in_item == before.offset_in_item
            {
                self.list_state.scroll_to(ListOffset {
                    item_ix: index,
                    offset_in_item: px(0.),
                });
            }
        }
    }

    fn open_selected(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(row) = self.selected_row() {
            self.open_row(row, window, cx);
        }
    }

    fn open_selected_review(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(row) = self.selected_row() {
            self.open_review_for_row(row, window, cx);
        }
    }

    fn complete_selected(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.enable_operator_workspace_mode(window, cx);
        let Some(target) = self.selected_rename_target() else {
            self.show_slot_toast(
                "Select a tracked Vitermux session to complete".to_string(),
                cx,
            );
            return;
        };
        self.complete_session(target, cx);
    }

    fn rename_selected(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.enable_operator_workspace_mode(window, cx);
        let Some(target) = self.selected_rename_target() else {
            self.show_slot_toast(
                "Select a tracked Vitermux session to rename".to_string(),
                cx,
            );
            return;
        };
        self.open_rename_modal(target, window, cx);
    }

    fn activate_slot(
        &mut self,
        action: &ActivateSlot,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(slot_index) = normalize_slot_index(action.0) else {
            return;
        };
        if !self.operator_workspace_enabled {
            if self.focus_handle.contains_focused(window, cx)
                || self.last_focused_terminal_key.is_some()
            {
                self.enable_operator_workspace_mode(window, cx);
            } else {
                window.dispatch_action(workspace::ActivatePane(slot_index).boxed_clone(), cx);
                return;
            }
        }
        let Some(row) = self.resolve_slot_row(slot_index) else {
            self.show_slot_toast(
                format!(
                    "Session slot {} is not currently available in the tmux topology",
                    slot_number(slot_index)
                ),
                cx,
            );
            return;
        };

        self.ensure_host_expanded_for_row(&row, cx);
        self.selected_row_key = Some(row.row_key.clone());
        self.scroll_selection_into_view();
        cx.notify();
        self.open_row(row, window, cx);
    }

    fn assign_selected_to_slot(
        &mut self,
        action: &AssignSelectedToSlot,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(slot_index) = normalize_slot_index(action.0) else {
            return;
        };
        self.enable_operator_workspace_mode(window, cx);
        let Some(row) = self.selected_row() else {
            return;
        };
        if row.session_key.is_none() {
            self.show_slot_toast(
                format!(
                    "{} has no tracked agent session to assign",
                    row.window_label.as_ref()
                ),
                cx,
            );
            return;
        }

        let changed = self.assign_row_to_slot(slot_index, &row);
        if changed {
            self.save_session_slots(cx);
            self.show_slot_toast(
                format!(
                    "Assigned {} to session slot {}",
                    row.window_label.as_ref(),
                    slot_number(slot_index)
                ),
                cx,
            );
            cx.notify();
        }
    }

    fn selected_rename_target(&self) -> Option<SessionRenameTarget> {
        self.selected_row()
            .and_then(|row| rename_target_for_row(&row))
    }

    fn open_rename_for_row(
        &mut self,
        row: WorkbenchRow,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(target) = rename_target_for_row(&row) else {
            self.show_slot_toast(
                format!(
                    "{} has no tracked session to rename",
                    row.window_label.as_ref()
                ),
                cx,
            );
            return;
        };
        self.enable_operator_workspace_mode(window, cx);
        self.ensure_host_expanded_for_row(&row, cx);
        self.selected_row_key = Some(row.row_key.clone());
        self.scroll_selection_into_view();
        self.open_rename_modal(target, window, cx);
        cx.notify();
    }

    fn open_rename_modal(
        &self,
        target: SessionRenameTarget,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        let panel = cx.entity().downgrade();
        workspace.update(cx, |workspace, cx| {
            if let Some(modal) = workspace.active_modal::<RenameSessionModal>(cx) {
                modal.update(cx, |modal, cx| {
                    modal.set_target(target.clone(), window, cx);
                });
            } else {
                workspace.toggle_modal(window, cx, |window, cx| {
                    RenameSessionModal::new(panel.clone(), target.clone(), window, cx)
                });
            }
        });
    }

    fn confirm(&mut self, _: &Confirm, window: &mut Window, cx: &mut Context<Self>) {
        self.open_selected(window, cx);
    }

    fn select_first(&mut self, _: &SelectFirst, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(row) = self.visible_rows().first() {
            self.selected_row_key = Some(row.row_key.clone());
            self.scroll_selection_into_view();
            cx.notify();
        }
    }

    fn select_last(&mut self, _: &SelectLast, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(row) = self.visible_rows().last() {
            self.selected_row_key = Some(row.row_key.clone());
            self.scroll_selection_into_view();
            cx.notify();
        }
    }

    fn select_next(&mut self, _: &SelectNext, window: &mut Window, cx: &mut Context<Self>) {
        let visible_rows = self.visible_rows();
        if visible_rows.is_empty() {
            self.selected_row_key = None;
            cx.notify();
            return;
        }

        let next_index = self
            .selected_row_key
            .as_ref()
            .and_then(|selected| {
                visible_rows
                    .iter()
                    .position(|row| row.row_key.as_ref() == selected.as_ref())
            })
            .map(|selected_index| (selected_index + 1) % visible_rows.len())
            .unwrap_or(0);
        self.selected_row_key = Some(visible_rows[next_index].row_key.clone());
        self.scroll_selection_into_view();

        if !self.focus_handle.contains_focused(window, cx) {
            cx.focus_self(window);
        }
        cx.notify();
    }

    fn select_previous(&mut self, _: &SelectPrevious, window: &mut Window, cx: &mut Context<Self>) {
        let visible_rows = self.visible_rows();
        if visible_rows.is_empty() {
            self.selected_row_key = None;
            cx.notify();
            return;
        }

        let previous_index = self
            .selected_row_key
            .as_ref()
            .and_then(|selected| {
                visible_rows
                    .iter()
                    .position(|row| row.row_key.as_ref() == selected.as_ref())
            })
            .map(|selected_index| {
                if selected_index == 0 {
                    visible_rows.len() - 1
                } else {
                    selected_index - 1
                }
            })
            .unwrap_or(visible_rows.len() - 1);
        self.selected_row_key = Some(visible_rows[previous_index].row_key.clone());
        self.scroll_selection_into_view();

        if !self.focus_handle.contains_focused(window, cx) {
            cx.focus_self(window);
        }
        cx.notify();
    }

    fn select_and_open(&mut self, row: WorkbenchRow, window: &mut Window, cx: &mut Context<Self>) {
        self.ensure_host_expanded_for_row(&row, cx);
        self.selected_row_key = Some(row.row_key.clone());
        self.open_row(row, window, cx);
    }

    fn open_row(&mut self, row: WorkbenchRow, window: &mut Window, cx: &mut Context<Self>) {
        self.enable_operator_workspace_mode(window, cx);
        if row.session_key.is_none() {
            if let Some(workspace) = self.workspace.upgrade() {
                workspace.update(cx, |workspace, cx| {
                    workspace.show_toast(
                        Toast::new(
                            NotificationId::unique::<MissingSessionToast>(),
                            format!(
                                "{} has no tracked agent session to attach",
                                row.window_label
                            ),
                        ),
                        cx,
                    );
                });
            }
            return;
        }

        self.fetch_and_open(row, window, cx).detach_and_prompt_err(
            "Vitermux Open Failed",
            window,
            cx,
            |_, _, _| None,
        );
    }

    fn open_review_for_row(
        &mut self,
        row: WorkbenchRow,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.enable_operator_workspace_mode(window, cx);
        if row.session_key.is_none() {
            return;
        }

        self.fetch_and_open_review(row, ReviewOpenMode::ExplicitOpen, window, cx)
            .detach_and_prompt_err("Vitermux Review Open Failed", window, cx, |_, _, _| None);
    }

    fn resolve_slot_row(&self, slot_index: usize) -> Option<WorkbenchRow> {
        let slot = self.session_slots.get(slot_index)?.as_ref()?;
        row_for_session_slot(
            slot,
            &self.rows,
            &self.row_index_by_session_key,
            &self.row_index_by_terminal_key,
        )
    }

    fn assign_row_to_slot(&mut self, slot_index: usize, row: &WorkbenchRow) -> bool {
        let Some(session_key) = row.session_key.as_ref() else {
            return false;
        };
        if self.session_slots.len() != SESSION_SLOT_COUNT {
            self.session_slots = normalize_session_slots(&self.session_slots);
        }

        let assignment = SessionSlotAssignment {
            terminal_key: row.row_key.to_string(),
            session_key: session_key.to_string(),
            dedupe_key: row.dedupe_key.to_string(),
        };
        let already_assigned = self
            .session_slots
            .get(slot_index)
            .and_then(|slot| slot.as_ref())
            == Some(&assignment);
        if already_assigned {
            return false;
        }

        for slot in &mut self.session_slots {
            if slot.as_ref().is_some_and(|existing| {
                existing.terminal_key == assignment.terminal_key
                    || existing.dedupe_key == assignment.dedupe_key
            }) {
                *slot = None;
            }
        }
        self.session_slots[slot_index] = Some(assignment);
        self.assigned_slot_by_row_key = build_assigned_slot_by_row_key(
            &self.session_slots,
            &self.rows,
            &self.row_index_by_session_key,
            &self.row_index_by_terminal_key,
        );
        true
    }

    fn maybe_seed_session_slots(&mut self, cx: &mut Context<Self>) -> bool {
        if !self.operator_workspace_enabled {
            self.session_slots = normalize_session_slots(&self.session_slots);
            return false;
        }
        if self.session_slots.iter().any(Option::is_some) {
            self.session_slots = normalize_session_slots(&self.session_slots);
            return false;
        }

        let seeded_slots = seed_session_slots(&self.rows);
        if seeded_slots.iter().all(Option::is_none) {
            self.session_slots = seeded_slots;
            return false;
        }

        self.session_slots = seeded_slots;
        self.save_session_slots(cx);
        true
    }

    fn save_session_slots(&self, cx: &mut Context<Self>) {
        let Some(workspace_key) = self.persistence_key.clone() else {
            return;
        };

        let kvp = KeyValueStore::global(cx);
        let session_slots = self.session_slots.clone();
        cx.background_spawn(async move {
            let scope = kvp.scoped(SESSION_SLOT_SCOPE_KEY);
            let state = SerializedSessionSlotState {
                version: session_slot_state_version(),
                slots: normalize_session_slots(&session_slots),
            };
            let Ok(json) = serde_json::to_string(&state) else {
                return;
            };
            let _ = scope.write(workspace_key, json).await;
        })
        .detach();
    }

    fn enable_operator_workspace_mode(&mut self, window: &Window, cx: &mut Context<Self>) {
        if self.operator_workspace_enabled {
            return;
        }
        self.operator_workspace_enabled = true;
        self.defer_operator_workspace_context_sync(window, cx);
        self.save_operator_workspace_mode(cx);
        if self.maybe_seed_session_slots(cx) {
            cx.notify();
        }
    }

    fn defer_operator_workspace_context_sync(&mut self, window: &Window, cx: &mut Context<Self>) {
        cx.defer_in(window, |_this, _window, cx| {
            _this.sync_operator_workspace_context(cx);
        });
    }

    fn sync_operator_workspace_context(&self, cx: &mut Context<Self>) {
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        let enabled = self.operator_workspace_enabled;
        workspace.update(cx, |workspace, cx| {
            workspace.set_extra_key_context(OPERATOR_WORKSPACE_CONTEXT_KEY, enabled, cx);
        });
    }

    fn save_operator_workspace_mode(&self, cx: &mut Context<Self>) {
        let Some(workspace_key) = self.persistence_key.clone() else {
            return;
        };

        let kvp = KeyValueStore::global(cx);
        let enabled = self.operator_workspace_enabled;
        cx.background_spawn(async move {
            let scope = kvp.scoped(OPERATOR_WORKSPACE_SCOPE_KEY);
            let state = SerializedOperatorWorkspaceState { enabled };
            let Ok(json) = serde_json::to_string(&state) else {
                return;
            };
            let _ = scope.write(workspace_key, json).await;
        })
        .detach();
    }

    fn toggle_review_companion(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.enable_operator_workspace_mode(window, cx);
        self.review_companion_enabled = !self.review_companion_enabled;
        self.save_review_companion_enabled(cx);
        self.show_slot_toast(
            if self.review_companion_enabled {
                "Auto review companion enabled".to_string()
            } else {
                "Auto review companion disabled".to_string()
            },
            cx,
        );
        cx.notify();
    }

    fn save_review_companion_enabled(&self, cx: &mut Context<Self>) {
        let Some(workspace_key) = self.persistence_key.clone() else {
            return;
        };

        let kvp = KeyValueStore::global(cx);
        let enabled = self.review_companion_enabled;
        cx.background_spawn(async move {
            let scope = kvp.scoped(REVIEW_COMPANION_SCOPE_KEY);
            let state = SerializedReviewCompanionState { enabled };
            let Ok(json) = serde_json::to_string(&state) else {
                return;
            };
            let _ = scope.write(workspace_key, json).await;
        })
        .detach();
    }

    fn save_collapsed_host_keys(&self, cx: &mut Context<Self>) {
        let Some(workspace_key) = self.persistence_key.clone() else {
            return;
        };

        let kvp = KeyValueStore::global(cx);
        let mut host_keys: Vec<String> = self.collapsed_host_keys.iter().cloned().collect();
        host_keys.sort();
        cx.background_spawn(async move {
            let scope = kvp.scoped(COLLAPSED_HOST_SCOPE_KEY);
            let state = SerializedCollapsedHostState {
                host_keys: host_keys
                    .into_iter()
                    .filter(|host_key| !host_key.trim().is_empty())
                    .collect(),
            };
            let Ok(json) = serde_json::to_string(&state) else {
                return;
            };
            let _ = scope.write(workspace_key, json).await;
        })
        .detach();
    }

    fn rename_session(
        &mut self,
        target: SessionRenameTarget,
        proposed_name: String,
        cx: &mut Context<Self>,
    ) {
        let trimmed_name = proposed_name.trim().to_string();
        if trimmed_name.is_empty() || trimmed_name == target.current_name {
            return;
        }

        let client = self.store.read(cx).client();
        cx.spawn(async move |this, cx| {
            let result = client
                .rename_session(&target.session_key, &trimmed_name)
                .await;
            let _ = this.update(cx, |this, cx| match result {
                Ok(response) => {
                    let canonical_name = response
                        .session
                        .as_ref()
                        .and_then(|session| session.canonical_name())
                        .unwrap_or(trimmed_name.as_str())
                        .trim()
                        .to_string();
                    this.apply_renamed_session_label(&target, &canonical_name, cx);
                    this.refresh_after_mutation(cx);
                    this.show_slot_toast(
                        format!("Renamed {} to {}", target.current_name, canonical_name),
                        cx,
                    );
                }
                Err(error) => {
                    this.refresh_after_mutation(cx);
                    this.show_slot_toast(format!("Rename failed: {}", error), cx);
                }
            });
        })
        .detach();
    }

    fn complete_session(&mut self, target: SessionRenameTarget, cx: &mut Context<Self>) {
        let client = self.store.read(cx).client();
        cx.spawn(async move |this, cx| {
            let result = client.complete_session(&target.session_key).await;
            let _ = this.update(cx, |this, cx| match result {
                Ok(response) => {
                    this.apply_completed_session_attention(&target.row_key, cx);
                    this.refresh_after_mutation(cx);
                    let completed_name = response
                        .session
                        .as_ref()
                        .and_then(|session| session.canonical_name())
                        .unwrap_or(target.current_name.as_str())
                        .trim()
                        .to_string();
                    this.show_slot_toast(format!("Completed review for {}", completed_name), cx);
                }
                Err(error) => {
                    this.refresh_after_mutation(cx);
                    this.show_slot_toast(format!("Complete failed: {}", error), cx);
                }
            });
        })
        .detach();
    }

    fn apply_renamed_session_label(
        &mut self,
        target: &SessionRenameTarget,
        canonical_name: &str,
        cx: &mut Context<Self>,
    ) {
        if canonical_name.trim().is_empty() {
            return;
        }

        self.pending_session_labels.insert(
            target.row_key.clone(),
            PendingSessionLabel {
                label: SharedString::from(canonical_name.to_string()),
            },
        );
        self.expire_pending_session_label(target.row_key.clone(), canonical_name.to_string(), cx);
        let mut changed = false;
        for row in &mut self.rows {
            if row.row_key.as_ref() == target.row_key && row.window_label.as_ref() != canonical_name
            {
                row.window_label = SharedString::from(canonical_name.to_string());
                changed = true;
            }
        }
        if !changed {
            return;
        }

        self.rebuild_display_rows();
        self.ensure_selection(cx);
        self.scroll_selection_into_view();
        cx.notify();
    }

    fn expire_pending_session_label(
        &self,
        row_key: String,
        expected_label: String,
        cx: &mut Context<Self>,
    ) {
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(PENDING_LABEL_TIMEOUT).await;
            let _ = this.update(cx, |this, cx| {
                let Some(pending_label) = this.pending_session_labels.get(row_key.as_str()) else {
                    return;
                };
                if pending_label.label.as_ref() != expected_label {
                    return;
                }

                this.pending_session_labels.remove(row_key.as_str());
                this.refresh_view_model(cx);
                this.ensure_selection(cx);
                this.scroll_selection_into_view();
                this.show_slot_toast(
                    format!(
                        "Rename did not settle for {}; showing live tmux label",
                        expected_label
                    ),
                    cx,
                );
                cx.notify();
            });
        })
        .detach();
    }

    fn apply_completed_session_attention(&mut self, row_key: &str, cx: &mut Context<Self>) {
        let mut changed = false;
        for row in &mut self.rows {
            if row.row_key.as_ref() == row_key && row.attention.as_ref() != "complete" {
                row.attention = SharedString::from("complete");
                changed = true;
            }
        }
        if !changed {
            return;
        }

        self.rebuild_display_rows();
        self.ensure_selection(cx);
        self.scroll_selection_into_view();
        cx.notify();
    }

    fn refresh_after_mutation(&self, cx: &mut Context<Self>) {
        self.store.update(cx, |store, cx| store.refresh(cx));

        cx.spawn(async move |this, cx| {
            for _ in 0..MUTATION_REFRESH_ATTEMPTS {
                cx.background_executor().timer(MUTATION_REFRESH_DELAY).await;
                if this
                    .update(cx, |this, cx| {
                        this.store.update(cx, |store, cx| store.refresh(cx));
                    })
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();
    }

    fn show_slot_toast(&self, message: String, cx: &mut Context<Self>) {
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        workspace.update(cx, |workspace, cx| {
            workspace.show_toast(
                Toast::new(NotificationId::unique::<SessionSlotToast>(), message),
                cx,
            );
        });
    }

    fn sync_to_focused_terminal(
        &mut self,
        workspace: Entity<Workspace>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(focused_terminal) = focused_terminal_match(
            &self.rows,
            &self.row_index_by_terminal_key,
            workspace.read(cx),
            cx,
        ) else {
            return;
        };
        let terminal_key = focused_terminal.task_label.clone();
        let row = focused_terminal.row;
        let terminal_pane = focused_terminal.pane;

        self.ensure_host_expanded_for_row(&row, cx);
        if self.selected_row_key.as_ref().map(|value| value.as_ref()) != Some(row.row_key.as_ref())
        {
            self.selected_row_key = Some(row.row_key.clone());
            self.scroll_selection_into_view();
            cx.notify();
        }

        let same_terminal_key =
            self.last_focused_terminal_key.as_deref() == Some(terminal_key.as_str());
        self.last_focused_terminal_key = Some(terminal_key.clone());

        if !self.review_companion_enabled {
            return;
        }

        let has_companion_for_terminal = workspace.update(cx, |workspace, cx| {
            find_review_diff_pane_for_terminal(workspace, &terminal_pane, cx).is_some()
        });

        if same_terminal_key && has_companion_for_terminal {
            return;
        }

        self.fetch_and_open_review(
            row,
            ReviewOpenMode::SyncCompanion {
                expected_terminal_key: self
                    .last_focused_terminal_key
                    .clone()
                    .expect("focused terminal key was just set"),
                ensure_companion_pane: !has_companion_for_terminal,
            },
            window,
            cx,
        )
        .detach_and_log_err(cx);
    }

    fn fetch_and_open(
        &self,
        row: WorkbenchRow,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        let client = self.store.read(cx).client();
        let workspace = self
            .workspace
            .upgrade()
            .ok_or_else(|| anyhow!("workspace is no longer available"));
        let open_key = if !row.dedupe_key.trim().is_empty() {
            row.dedupe_key.to_string()
        } else {
            row.row_key.to_string()
        };
        let open_plan_cache = self.open_plan_cache.clone();
        let requesting_window = window.window_handle().downcast::<MultiWorkspace>();
        let preferred_terminal_key = self.last_focused_terminal_key.clone();
        let review_companion_enabled = self.review_companion_enabled;
        let open_request_tracker = self.open_request_tracker.clone();
        let request_id = begin_open_request(&open_request_tracker);
        let session_key = row
            .session_key
            .clone()
            .ok_or_else(|| anyhow!("missing vitermux session key"))
            .map(|id| id.to_string());

        window.spawn(cx, async move |cx| {
            let workspace = workspace?;
            let session_key = session_key?;
            if !is_latest_open_request(&open_request_tracker, request_id) {
                return Ok(());
            }
            let focused_workspace = focus_existing_terminal_in_operator_window(
                workspace.clone(),
                requesting_window.clone(),
                &open_key,
                cx,
            )?;
            if let Some(focused_workspace) = focused_workspace {
                if review_companion_enabled {
                    let plan = match fetch_open_plan_with_cache(
                        &client,
                        &open_plan_cache,
                        &session_key,
                        &row,
                    )
                    .await
                    {
                        Ok(plan) => plan,
                        Err(error) => {
                            if !is_latest_open_request(&open_request_tracker, request_id) {
                                return Ok(());
                            }
                            return Err(error);
                        }
                    };
                    if is_latest_open_request(&open_request_tracker, request_id) {
                        let _ = sync_review_companion_for_terminal_open(
                            focused_workspace,
                            &row,
                            &plan,
                            &open_key,
                            cx,
                        )
                        .await;
                    }
                }
                return Ok(());
            }

            if !is_latest_open_request(&open_request_tracker, request_id) {
                return Ok(());
            }
            let plan =
                match fetch_open_plan_with_cache(&client, &open_plan_cache, &session_key, &row)
                    .await
                {
                    Ok(plan) => plan,
                    Err(error) => {
                        if !is_latest_open_request(&open_request_tracker, request_id) {
                            return Ok(());
                        }
                        return Err(error);
                    }
                };
            if !is_latest_open_request(&open_request_tracker, request_id) {
                return Ok(());
            }
            let resolved_workspace = match resolve_project_workspace_for_plan(
                workspace.clone(),
                requesting_window.clone(),
                &plan,
                cx,
            )
            .await
            {
                Ok(resolution) => resolution,
                Err(error) => {
                    if !is_latest_open_request(&open_request_tracker, request_id) {
                        return Ok(());
                    }
                    let source_workspace_is_remote = workspace
                        .read_with(cx, |workspace, cx| workspace.project().read(cx).is_remote());
                    if !allow_source_workspace_attach_fallback(source_workspace_is_remote) {
                        if !is_latest_open_request(&open_request_tracker, request_id) {
                            return Ok(());
                        }
                        return Err(error.context(
                            "cannot safely fall back to attach from a remote source workspace",
                        ));
                    }
                    ProjectWorkspaceResolution {
                        workspace: workspace.clone(),
                        matched_project_context: false,
                    }
                }
            };
            if !is_latest_open_request(&open_request_tracker, request_id) {
                return Ok(());
            }
            let workspace = resolved_workspace.workspace;
            if resolved_workspace.matched_project_context {
                let _ = sync_workspace_project_context_for_plan(
                    workspace.clone(),
                    &plan,
                    ProjectContextSyncMode::BestEffort,
                    cx,
                )
                .await;
                if !is_latest_open_request(&open_request_tracker, request_id) {
                    return Ok(());
                }
            }
            let workspace_is_remote =
                workspace.read_with(cx, |workspace, cx| workspace.project().read(cx).is_remote());
            let spawn_inside_target_remote_workspace = attach_inside_target_remote_workspace(
                &plan,
                resolved_workspace.matched_project_context,
                workspace_is_remote,
            );
            let spawn_task = build_spawn_task(
                &row,
                &plan,
                spawn_inside_target_remote_workspace,
                workspace_is_remote,
            )?;
            let terminal_key = spawn_task.full_label.clone();
            let terminal_panel =
                workspace.read_with(cx, |workspace, cx| workspace.panel::<TerminalPanel>(cx));
            if !is_latest_open_request(&open_request_tracker, request_id) {
                return Ok(());
            }
            let focused_workspace = focus_existing_terminal_in_operator_window(
                workspace.clone(),
                requesting_window.clone(),
                &terminal_key,
                cx,
            )?;
            if let Some(focused_workspace) = focused_workspace {
                if review_companion_enabled
                    && is_latest_open_request(&open_request_tracker, request_id)
                {
                    let _ = sync_review_companion_for_terminal_open(
                        focused_workspace,
                        &row,
                        &plan,
                        &terminal_key,
                        cx,
                    )
                    .await;
                }
                return Ok(());
            }

            if !is_latest_open_request(&open_request_tracker, request_id) {
                return Ok(());
            }
            let preferred_pane = workspace.update_in(cx, |workspace, _window, cx| {
                preferred_terminal_spawn_pane(workspace, preferred_terminal_key.as_deref(), cx)
            })?;
            let Some(terminal_panel) = terminal_panel else {
                return Err(anyhow!("terminal panel is not available"));
            };
            let terminal_task = terminal_panel.update_in(cx, |panel, window, cx| {
                let open_request_tracker = open_request_tracker.clone();
                panel.spawn_task_in_center_pane_with_guard(
                    &spawn_task,
                    preferred_pane,
                    Arc::new(move || is_latest_open_request(&open_request_tracker, request_id)),
                    window,
                    cx,
                )
            })?;
            if let Err(error) = terminal_task.await {
                if !is_latest_open_request(&open_request_tracker, request_id) {
                    return Ok(());
                }
                return Err(error);
            }
            if review_companion_enabled && is_latest_open_request(&open_request_tracker, request_id)
            {
                let _ = sync_review_companion_for_terminal_open(
                    workspace.clone(),
                    &row,
                    &plan,
                    &terminal_key,
                    cx,
                )
                .await;
            }
            Ok(())
        })
    }

    fn fetch_and_open_review(
        &self,
        row: WorkbenchRow,
        mode: ReviewOpenMode,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        let client = self.store.read(cx).client();
        let workspace = self
            .workspace
            .upgrade()
            .ok_or_else(|| anyhow!("workspace is no longer available"));
        let open_plan_cache = self.open_plan_cache.clone();
        let requesting_window = window.window_handle().downcast::<MultiWorkspace>();
        let session_key = row
            .session_key
            .clone()
            .ok_or_else(|| anyhow!("missing vitermux session key"))
            .map(|id| id.to_string());

        window.spawn(cx, async move |cx| {
            let workspace = workspace?;
            let session_key = session_key?;
            let plan =
                fetch_open_plan_with_cache(&client, &open_plan_cache, &session_key, &row).await?;
            if let Some(failure) = plan.failure.as_ref() {
                return Err(open_plan_failure(failure));
            }
            if project_target_path(&plan).is_none() {
                return Err(anyhow!(
                    "daemon open plan did not include a project path for review"
                ));
            }

            let workspace = match mode {
                ReviewOpenMode::ExplicitOpen => {
                    resolve_project_workspace_for_plan(workspace, requesting_window, &plan, cx)
                        .await?
                        .workspace
                }
                ReviewOpenMode::SyncCompanion { .. } => workspace,
            };
            if let ReviewOpenMode::SyncCompanion {
                expected_terminal_key,
                ..
            } = &mode
            {
                let still_focused = workspace.update_in(cx, |workspace, _window, cx| {
                    focused_terminal_task_label(workspace, cx).as_deref()
                        == Some(expected_terminal_key.as_str())
                })?;
                if !still_focused {
                    return Ok(());
                }
            }
            let Some(project_path) = sync_workspace_project_context_for_plan(
                workspace.clone(),
                &plan,
                ProjectContextSyncMode::RequireGitRepository,
                cx,
            )
            .await?
            else {
                return Err(anyhow!(
                    "daemon open plan did not resolve a project path for review"
                ));
            };
            match mode {
                ReviewOpenMode::ExplicitOpen => {
                    workspace.update_in(cx, |workspace, window, cx| {
                        let review_pane = terminal_pane_for_row(workspace, &row, cx)
                            .map(|terminal_pane| {
                                workspace.adjacent_pane_from(
                                    terminal_pane,
                                    workspace::SplitDirection::Right,
                                    window,
                                    cx,
                                )
                            })
                            .or_else(|| find_review_diff_pane(workspace, cx))
                            .unwrap_or_else(|| workspace.adjacent_pane(window, cx));
                        ProjectDiff::deploy_at_project_path_in_pane(
                            workspace,
                            review_pane,
                            project_path,
                            true,
                            true,
                            true,
                            window,
                            cx,
                        );
                    })?;
                }
                ReviewOpenMode::SyncCompanion {
                    expected_terminal_key,
                    ensure_companion_pane,
                } => {
                    let still_focused = workspace.update_in(cx, |workspace, _window, cx| {
                        focused_terminal_task_label(workspace, cx).as_deref()
                            == Some(expected_terminal_key.as_str())
                    })?;
                    if !still_focused {
                        return Ok(());
                    }
                    deploy_review_companion_until_synced(
                        workspace.clone(),
                        &row,
                        project_path,
                        expected_terminal_key,
                        ensure_companion_pane,
                        cx,
                    )
                    .await?;
                }
            }
            Ok(())
        })
    }

    fn render_header(&self, _window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let store = self.store.read(cx);
        let state_text = match store.connection_state() {
            VitermuxConnectionState::Connecting => "Connecting to vitermuxd",
            VitermuxConnectionState::Ready => "Live tmux topology",
            VitermuxConnectionState::Error => "Daemon unavailable",
        };
        let state_color = match store.connection_state() {
            VitermuxConnectionState::Connecting => Color::Accent,
            VitermuxConnectionState::Ready => Color::Muted,
            VitermuxConnectionState::Error => Color::Error,
        };

        v_flex()
            .gap_1()
            .p_3()
            .border_b_1()
            .border_color(cx.theme().colors().border_variant)
            .child(
                h_flex()
                    .w_full()
                    .justify_between()
                    .items_center()
                    .child(
                        h_flex()
                            .gap_2()
                            .items_center()
                            .child(Icon::new(IconName::TerminalAlt).color(Color::Accent))
                            .child(Label::new("Vitermux Workbench")),
                    )
                    .child(
                        h_flex()
                            .gap_2()
                            .items_center()
                            .child(
                                IconButton::new("vitermux-toggle-review", IconName::Diff)
                                    .toggle_state(self.review_companion_enabled)
                                    .icon_size(IconSize::Small)
                                    .icon_color(if self.review_companion_enabled {
                                        Color::Accent
                                    } else {
                                        Color::Muted
                                    })
                                    .tooltip(Tooltip::text(
                                        "Toggle auto review companion (Option+Cmd+/)",
                                    ))
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.toggle_review_companion(window, cx);
                                    })),
                            )
                            .child(
                                div()
                                    .id("vitermux-refresh")
                                    .cursor_pointer()
                                    .on_click(cx.listener(|this, _, _, cx| this.refresh(cx)))
                                    .tooltip(Tooltip::text("Refresh tmux tree"))
                                    .child(Icon::new(IconName::ArrowCircle).color(Color::Muted)),
                            ),
                    ),
            )
            .child(
                Label::new(state_text)
                    .size(LabelSize::XSmall)
                    .color(state_color),
            )
            .child(
                Label::new(
                    "Cmd+1..9 switch tmux tabs  •  Cmd+Shift+R rename selected  •  Option+Cmd+/ auto review",
                )
                    .size(LabelSize::XSmall)
                    .color(Color::Muted),
            )
            .child(
                Label::new(if self.review_companion_enabled {
                    "Tmux windows are the switching primitive  •  Right diff companion follows focus"
                } else {
                    "Tmux windows are the switching primitive  •  Right diff companion is paused"
                })
                    .size(LabelSize::XSmall)
                    .color(Color::Muted),
            )
            .when_some(store.last_error().cloned(), |this, error| {
                this.child(
                    Label::new(error)
                        .size(LabelSize::XSmall)
                        .color(Color::Error),
                )
            })
            .into_any_element()
    }

    fn render_rows(&self, _window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        if self.display_rows.is_empty() {
            return v_flex()
                .p_3()
                .child(
                    Label::new("No tracked tmux windows yet")
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                )
                .into_any_element();
        }

        div()
            .id("vitermux-panel-scroll")
            .size_full()
            .child(
                list(
                    self.list_state.clone(),
                    cx.processor(|this, ix, window, cx| this.render_display_row(ix, window, cx)),
                )
                .with_sizing_behavior(ListSizingBehavior::Auto)
                .size_full(),
            )
            .into_any_element()
    }

    fn render_display_row(
        &self,
        ix: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(display_row) = self.display_rows.get(ix).cloned() else {
            return div().into_any_element();
        };

        match display_row {
            DisplayRow::HostHeader {
                row_key,
                host_key,
                label,
                local_badge,
                collapsed,
            } => {
                let host_key_for_toggle = host_key.clone();
                let host_key_for_click = host_key.clone();

                h_flex()
                    .id(row_key)
                    .pt_3()
                    .px_2()
                    .gap_1()
                    .items_center()
                    .child(
                        Disclosure::new(format!("vitermux-host-toggle-{}", host_key), !collapsed)
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.toggle_host_collapsed(host_key_for_toggle.clone(), cx);
                            })),
                    )
                    .child(
                        h_flex()
                            .id(format!("vitermux-host-label-{}", host_key))
                            .w_full()
                            .gap_2()
                            .items_center()
                            .cursor_pointer()
                            .hover(|style| style.bg(cx.theme().colors().ghost_element_hover))
                            .rounded_sm()
                            .px_1()
                            .py_0p5()
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.toggle_host_collapsed(host_key_for_click.clone(), cx);
                            }))
                            .child(
                                Label::new(label)
                                    .size(LabelSize::XSmall)
                                    .color(Color::Accent),
                            )
                            .when(local_badge, |this| {
                                this.child(
                                    div()
                                        .px_1p5()
                                        .rounded_sm()
                                        .bg(Color::Accent.color(cx).alpha(0.12))
                                        .child(
                                            Label::new("local")
                                                .size(LabelSize::XSmall)
                                                .color(Color::Accent),
                                        ),
                                )
                            }),
                    )
                    .into_any_element()
            }
            DisplayRow::SessionHeader {
                row_key,
                label,
                detail,
            } => v_flex()
                .id(row_key)
                .px_3()
                .pt_1()
                .pb_1()
                .gap_0p5()
                .child(Label::new(label).size(LabelSize::Small))
                .when(!detail.is_empty(), |this| {
                    this.child(
                        Label::new(detail)
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                    )
                })
                .into_any_element(),
            DisplayRow::Window(row) => {
                let row_key = row.row_key.clone();
                let row_clone = row.clone();
                let review_row = row.clone();
                let complete_row = row.clone();
                let rename_row = row.clone();
                let row_selected = self.selected_row_key.as_ref() == Some(&row.row_key);
                let disabled = row.session_key.is_none();
                let panel_focused = self.active && self.focus_handle.contains_focused(window, cx);
                let show_review = review_available(&row);
                let show_complete = show_review;
                let assigned_slot = self
                    .assigned_slot_by_row_key
                    .get(row.row_key.as_ref())
                    .copied();

                ListItem::new(row.row_key.clone())
                    .toggle_state(row_selected)
                    .focused(panel_focused && row_selected)
                    .inset(true)
                    .spacing(ListItemSpacing::Sparse)
                    .disabled(disabled)
                    .start_slot(
                        Icon::new(attention_icon(row.attention.as_ref()))
                            .color(attention_color(row.attention.as_ref())),
                    )
                    .end_slot(
                        h_flex()
                            .gap_1()
                            .when_some(assigned_slot, |this, slot_index| {
                                this.child(
                                    KeyBinding::for_action_in(
                                        &ActivateSlot(slot_index),
                                        &self.focus_handle,
                                        cx,
                                    )
                                    .disabled(disabled),
                                )
                            })
                            .when(show_complete, |this| {
                                this.child(
                                    IconButton::new(
                                        format!("complete:{}", row_key.as_ref()),
                                        IconName::Check,
                                    )
                                    .icon_size(IconSize::Small)
                                    .icon_color(Color::Muted)
                                    .tooltip(Tooltip::text("Complete current review"))
                                    .on_click(cx.listener(
                                        move |this, _, window, cx| {
                                            let Some(target) = rename_target_for_row(&complete_row)
                                            else {
                                                this.show_slot_toast(
                                                    format!(
                                                        "{} has no tracked session to complete",
                                                        complete_row.window_label.as_ref()
                                                    ),
                                                    cx,
                                                );
                                                return;
                                            };
                                            this.enable_operator_workspace_mode(window, cx);
                                            this.ensure_host_expanded_for_row(&complete_row, cx);
                                            this.selected_row_key =
                                                Some(complete_row.row_key.clone());
                                            this.scroll_selection_into_view();
                                            this.complete_session(target, cx);
                                            cx.notify();
                                        },
                                    )),
                                )
                            })
                            .when(!disabled, |this| {
                                this.child(
                                    IconButton::new(
                                        format!("rename:{}", row_key.as_ref()),
                                        IconName::Pencil,
                                    )
                                    .icon_size(IconSize::Small)
                                    .icon_color(Color::Muted)
                                    .tooltip(Tooltip::text("Rename session (Cmd+Shift+R)"))
                                    .on_click(cx.listener(
                                        move |this, _, window, cx| {
                                            this.open_rename_for_row(
                                                rename_row.clone(),
                                                window,
                                                cx,
                                            );
                                        },
                                    )),
                                )
                            })
                            .when(show_review, |this| {
                                this.child(
                                    IconButton::new(
                                        format!("review:{}", row_key.as_ref()),
                                        IconName::Diff,
                                    )
                                    .icon_size(IconSize::Small)
                                    .icon_color(Color::Muted)
                                    .tooltip(Tooltip::text("Open review diff"))
                                    .on_click(cx.listener(
                                        move |this, _, window, cx| {
                                            this.open_review_for_row(
                                                review_row.clone(),
                                                window,
                                                cx,
                                            );
                                        },
                                    )),
                                )
                            })
                            .child(Icon::new(IconName::ChevronRight).color(Color::Muted)),
                    )
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.select_and_open(row_clone.clone(), window, cx);
                    }))
                    .tooltip(move |_, cx| {
                        let message = if disabled {
                            "No tracked agent session is bound to this tmux window"
                        } else {
                            "Open or focus the center terminal tab for this tmux window"
                        };
                        Tooltip::with_meta(message, None, row_key.clone(), cx)
                    })
                    .child(
                        v_flex()
                            .gap_0p5()
                            .child(Label::new(row.window_label.clone()))
                            .child(
                                Label::new(row.window_detail.clone())
                                    .size(LabelSize::XSmall)
                                    .color(Color::Muted),
                            ),
                    )
                    .into_any_element()
            }
        }
    }
}

impl EventEmitter<PanelEvent> for VitermuxPanel {}

impl Focusable for VitermuxPanel {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for VitermuxPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .key_context("VitermuxPanel menu")
            .size_full()
            .bg(cx.theme().colors().panel_background)
            .border_r_1()
            .border_color(if self.active {
                cx.theme().colors().panel_focused_border
            } else {
                cx.theme().colors().border_variant
            })
            .on_action(cx.listener(Self::select_next))
            .on_action(cx.listener(Self::select_previous))
            .on_action(cx.listener(Self::select_first))
            .on_action(cx.listener(Self::select_last))
            .on_action(cx.listener(Self::confirm))
            .track_focus(&self.focus_handle)
            .child(self.render_header(window, cx))
            .child(self.render_rows(window, cx))
    }
}

impl Panel for VitermuxPanel {
    fn persistent_name() -> &'static str {
        "Vitermux Workbench"
    }

    fn panel_key() -> &'static str {
        VITERMUX_PANEL_KEY
    }

    fn position(&self, _window: &Window, _cx: &App) -> DockPosition {
        DockPosition::Left
    }

    fn position_is_valid(&self, position: DockPosition) -> bool {
        matches!(position, DockPosition::Left)
    }

    fn set_position(
        &mut self,
        _position: DockPosition,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) {
    }

    fn default_size(&self, _window: &Window, _cx: &App) -> Pixels {
        self.width
    }

    fn icon(&self, _window: &Window, _cx: &App) -> Option<IconName> {
        Some(IconName::TerminalAlt)
    }

    fn icon_tooltip(&self, _window: &Window, _cx: &App) -> Option<&'static str> {
        Some("Vitermux Workbench")
    }

    fn toggle_action(&self) -> Box<dyn Action> {
        Box::new(ToggleFocus)
    }

    fn starts_open(&self, _window: &Window, _cx: &App) -> bool {
        true
    }

    fn set_active(&mut self, active: bool, _window: &mut Window, cx: &mut Context<Self>) {
        self.active = active;
        cx.notify();
    }

    fn activation_priority(&self) -> u32 {
        8
    }
}

struct RenameSessionModal {
    panel: WeakEntity<VitermuxPanel>,
    editor: Entity<Editor>,
    target: SessionRenameTarget,
    last_error: Option<SharedString>,
}

impl EventEmitter<DismissEvent> for RenameSessionModal {}
impl ModalView for RenameSessionModal {}

impl Focusable for RenameSessionModal {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.editor.focus_handle(cx)
    }
}

impl RenameSessionModal {
    fn new(
        panel: WeakEntity<VitermuxPanel>,
        target: SessionRenameTarget,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let editor = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text("Session name", window, cx);
            editor
        });

        let mut this = Self {
            panel,
            editor,
            target,
            last_error: None,
        };
        this.reset_editor(window, cx);
        this
    }

    fn set_target(
        &mut self,
        target: SessionRenameTarget,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.target = target;
        self.last_error = None;
        self.reset_editor(window, cx);
        cx.notify();
    }

    fn reset_editor(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let current_name = self.target.current_name.clone();
        let selection_end = current_name.len();
        self.editor.update(cx, |editor, cx| {
            editor.set_text(current_name, window, cx);
            editor.change_selections(Default::default(), window, cx, |selections| {
                selections.select_ranges([MultiBufferOffset(0)..MultiBufferOffset(selection_end)]);
            });
        });
    }

    fn cancel(&mut self, _: &Cancel, _window: &mut Window, cx: &mut Context<Self>) {
        cx.emit(DismissEvent);
    }

    fn confirm(&mut self, _: &Confirm, window: &mut Window, cx: &mut Context<Self>) {
        let proposed_name = self.editor.read(cx).text(cx).trim().to_string();
        if proposed_name.is_empty() {
            self.last_error = Some("Session name cannot be empty".into());
            cx.notify();
            return;
        }

        let Some(panel) = self.panel.upgrade() else {
            self.last_error = Some("Vitermux panel is no longer available".into());
            cx.notify();
            return;
        };

        panel.update(cx, |panel, cx| {
            panel.rename_session(self.target.clone(), proposed_name.clone(), cx);
        });
        window.focus(&panel.focus_handle(cx), cx);
        cx.emit(DismissEvent);
    }
}

impl Render for RenameSessionModal {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();

        v_flex()
            .key_context("VitermuxRenameModal")
            .on_action(cx.listener(Self::cancel))
            .on_action(cx.listener(Self::confirm))
            .elevation_3(cx)
            .w_96()
            .overflow_hidden()
            .child(
                div()
                    .p_2()
                    .border_b_1()
                    .border_color(theme.colors().border_variant)
                    .child(self.editor.clone()),
            )
            .child(
                h_flex()
                    .bg(theme.colors().editor_background)
                    .rounded_b_sm()
                    .w_full()
                    .p_2()
                    .gap_1()
                    .when_some(self.last_error.clone(), |this, error| {
                        this.child(Label::new(error).size(LabelSize::Small).color(Color::Error))
                    })
                    .when(self.last_error.is_none(), |this| {
                        this.child(
                            Label::new("Rename the selected session and keep tmux in sync.")
                                .color(Color::Muted)
                                .size(LabelSize::Small),
                        )
                    }),
            )
    }
}

struct MissingSessionToast;
struct SessionSlotToast;

fn flatten_rows(snapshot: &TmuxTreeSnapshot) -> Vec<WorkbenchRow> {
    let mut rows = Vec::new();
    let self_node_id = snapshot.self_node_id.trim();

    for node in &snapshot.nodes {
        let node_host = first_non_empty([
            node.node_id.as_str(),
            node.node_key.as_str(),
            node.node_name.as_str(),
        ]);
        let node_is_local = !self_node_id.is_empty() && node.node_id.trim() == self_node_id;

        for account in &node.accounts {
            let account_label = host_label(account, node_host);

            for session in &account.sessions {
                let session_label =
                    first_non_empty([session.session_name.as_str(), session.session_key.as_str()]);
                let session_detail = join_detail_parts([
                    non_empty_string(session.project_group.workspace_label.as_str()),
                    non_empty_string(session.project_group.codebase_label.as_str()),
                ]);

                for window in &session.windows {
                    let Some(session_key) = window
                        .primary_session_key()
                        .filter(|session_key| !session_key.trim().is_empty())
                    else {
                        continue;
                    };

                    rows.push(WorkbenchRow {
                        row_key: SharedString::from(window.stable_row_key().to_string()),
                        node_section_key: SharedString::from(format!(
                            "{}:{}",
                            node.node_key, account.account_key
                        )),
                        node_label: SharedString::from(account_label.clone()),
                        node_is_local,
                        session_section_key: SharedString::from(session.session_key.clone()),
                        session_label: SharedString::from(session_label.to_string()),
                        session_detail: SharedString::from(session_detail.clone()),
                        window_label: SharedString::from(window_label(window)),
                        window_detail: SharedString::from(window_detail(window)),
                        session_key: Some(SharedString::from(session_key.to_string())),
                        dedupe_key: SharedString::from(window.stable_row_key().to_string()),
                        attention: SharedString::from(window.attention.clone()),
                    });
                }
            }
        }
    }

    rows
}

fn build_display_rows(
    rows: &[WorkbenchRow],
    collapsed_host_keys: &HashSet<String>,
) -> Vec<DisplayRow> {
    let mut display_rows = Vec::new();
    let mut last_node_key: Option<&str> = None;
    let mut last_session_key: Option<&str> = None;

    for row in rows {
        if last_node_key != Some(row.node_section_key.as_ref()) {
            last_node_key = Some(row.node_section_key.as_ref());
            last_session_key = None;
            display_rows.push(DisplayRow::HostHeader {
                row_key: SharedString::from(format!("host:{}", row.node_section_key.as_ref())),
                host_key: row.node_section_key.clone(),
                label: row.node_label.clone(),
                local_badge: row.node_is_local,
                collapsed: collapsed_host_keys.contains(row.node_section_key.as_ref()),
            });
        }

        if collapsed_host_keys.contains(row.node_section_key.as_ref()) {
            continue;
        }

        if last_session_key != Some(row.session_section_key.as_ref()) {
            last_session_key = Some(row.session_section_key.as_ref());
            display_rows.push(DisplayRow::SessionHeader {
                row_key: SharedString::from(format!(
                    "session:{}:{}",
                    row.node_section_key.as_ref(),
                    row.session_section_key.as_ref()
                )),
                label: row.session_label.clone(),
                detail: row.session_detail.clone(),
            });
        }

        display_rows.push(DisplayRow::Window(row.clone()));
    }

    display_rows
}

fn apply_pending_label_overlays(
    rows: &mut [WorkbenchRow],
    pending_labels: &mut HashMap<String, PendingSessionLabel>,
) {
    if pending_labels.is_empty() {
        return;
    }

    let mut visible_rows = HashSet::default();
    let mut settled_rows = Vec::new();
    for row in rows {
        let row_key = row.row_key.to_string();
        visible_rows.insert(row_key.clone());

        let Some(pending_label) = pending_labels.get(row_key.as_str()) else {
            continue;
        };
        if row.window_label.as_ref() == pending_label.label.as_ref() {
            settled_rows.push(row_key);
            continue;
        }
        row.window_label = pending_label.label.clone();
    }

    pending_labels.retain(|row_key, _| visible_rows.contains(row_key));
    for row_key in settled_rows {
        pending_labels.remove(row_key.as_str());
    }
}

fn host_label(account: &vitermux::TmuxAccount, node_host: &str) -> String {
    if account.account_key.contains('@') {
        return account.account_key.clone();
    }

    let account_user = first_non_empty([
        account.account_user.as_str(),
        account_user_from_account_key(account.account_key.as_str()),
    ]);
    if !account_user.is_empty() && !node_host.is_empty() {
        return format!("{account_user}@{node_host}");
    }

    first_non_empty([
        account.account_name.as_str(),
        account.account_key.as_str(),
        node_host,
    ])
    .to_string()
}

fn account_user_from_account_key(account_key: &str) -> &str {
    account_key
        .split_once('@')
        .map(|(user, _)| user)
        .unwrap_or("")
}

fn session_slot_state_version() -> u8 {
    1
}

fn default_review_companion_enabled() -> bool {
    true
}

fn empty_session_slots() -> Vec<Option<SessionSlotAssignment>> {
    vec![None; SESSION_SLOT_COUNT]
}

fn normalize_collapsed_host_keys(host_keys: &[String]) -> HashSet<String> {
    host_keys
        .iter()
        .filter_map(|host_key| {
            let host_key = host_key.trim();
            (!host_key.is_empty()).then(|| host_key.to_string())
        })
        .collect()
}

fn normalize_slot_index(slot_index: usize) -> Option<usize> {
    (slot_index < SESSION_SLOT_COUNT).then_some(slot_index)
}

fn slot_number(slot_index: usize) -> usize {
    slot_index + 1
}

fn normalize_session_slots(
    session_slots: &[Option<SessionSlotAssignment>],
) -> Vec<Option<SessionSlotAssignment>> {
    let mut normalized = session_slots
        .iter()
        .take(SESSION_SLOT_COUNT)
        .cloned()
        .collect::<Vec<_>>();
    normalized.resize(SESSION_SLOT_COUNT, None);
    normalized
}

fn build_row_index_by_key(rows: &[WorkbenchRow]) -> HashMap<String, usize> {
    rows.iter()
        .enumerate()
        .map(|(index, row)| (row.row_key.to_string(), index))
        .collect()
}

fn build_row_index_by_session_key(rows: &[WorkbenchRow]) -> HashMap<String, usize> {
    rows.iter()
        .enumerate()
        .filter_map(|(index, row)| {
            row.session_key
                .as_ref()
                .map(|session_key| (session_key.to_string(), index))
        })
        .collect()
}

fn build_row_index_by_terminal_key(rows: &[WorkbenchRow]) -> HashMap<String, usize> {
    let mut index_by_terminal_key = HashMap::default();
    for (index, row) in rows.iter().enumerate() {
        insert_terminal_key_aliases(&mut index_by_terminal_key, row.row_key.as_ref(), index);
        insert_terminal_key_aliases(&mut index_by_terminal_key, row.dedupe_key.as_ref(), index);
    }
    index_by_terminal_key
}

fn build_display_row_index_by_row_key(display_rows: &[DisplayRow]) -> HashMap<String, usize> {
    display_rows
        .iter()
        .enumerate()
        .filter_map(|(index, row)| match row {
            DisplayRow::Window(row) => Some((row.row_key.to_string(), index)),
            DisplayRow::HostHeader { .. } | DisplayRow::SessionHeader { .. } => None,
        })
        .collect()
}

fn seed_session_slots(rows: &[WorkbenchRow]) -> Vec<Option<SessionSlotAssignment>> {
    let mut session_slots = empty_session_slots();
    for (slot_index, row) in rows
        .iter()
        .filter(|row| row.session_key.is_some())
        .take(SESSION_SLOT_COUNT)
        .enumerate()
    {
        session_slots[slot_index] = Some(SessionSlotAssignment {
            terminal_key: row.row_key.to_string(),
            session_key: row
                .session_key
                .as_ref()
                .map(|session_key| session_key.to_string())
                .unwrap_or_default(),
            dedupe_key: row.dedupe_key.to_string(),
        });
    }
    session_slots
}

fn row_for_session_slot(
    slot: &SessionSlotAssignment,
    rows: &[WorkbenchRow],
    row_index_by_session_key: &HashMap<String, usize>,
    row_index_by_terminal_key: &HashMap<String, usize>,
) -> Option<WorkbenchRow> {
    row_index_by_terminal_key
        .get(slot.terminal_key.as_str())
        .and_then(|index| rows.get(*index))
        .or_else(|| {
            row_index_by_session_key
                .get(slot.session_key.as_str())
                .and_then(|index| rows.get(*index))
        })
        .or_else(|| {
            row_index_by_terminal_key
                .get(slot.dedupe_key.as_str())
                .and_then(|index| rows.get(*index))
        })
        .cloned()
}

fn build_assigned_slot_by_row_key(
    session_slots: &[Option<SessionSlotAssignment>],
    rows: &[WorkbenchRow],
    row_index_by_session_key: &HashMap<String, usize>,
    row_index_by_terminal_key: &HashMap<String, usize>,
) -> HashMap<String, usize> {
    session_slots
        .iter()
        .enumerate()
        .filter_map(|(slot_index, slot)| {
            let slot = slot.as_ref()?;
            let row = row_for_session_slot(
                slot,
                rows,
                row_index_by_session_key,
                row_index_by_terminal_key,
            )?;
            Some((row.row_key.to_string(), slot_index))
        })
        .collect()
}

fn workspace_persistence_key(workspace: &Workspace) -> Option<String> {
    workspace
        .database_id()
        .map(|id| i64::from(id).to_string())
        .or_else(|| workspace.session_id())
}

fn load_session_slots(
    workspace_key: Option<&str>,
    cx: &App,
) -> Option<Vec<Option<SessionSlotAssignment>>> {
    let workspace_key = workspace_key?;
    let kvp = KeyValueStore::global(cx);
    let scope = kvp.scoped(SESSION_SLOT_SCOPE_KEY);
    let state = scope
        .read(workspace_key)
        .ok()
        .flatten()
        .and_then(|json| serde_json::from_str::<SerializedSessionSlotState>(&json).ok())?;
    Some(normalize_session_slots(&state.slots))
}

fn load_operator_workspace_enabled(workspace_key: Option<&str>, cx: &App) -> Option<bool> {
    let workspace_key = workspace_key?;
    let kvp = KeyValueStore::global(cx);
    let scope = kvp.scoped(OPERATOR_WORKSPACE_SCOPE_KEY);
    let state =
        scope.read(workspace_key).ok().flatten().and_then(|json| {
            serde_json::from_str::<SerializedOperatorWorkspaceState>(&json).ok()
        })?;
    Some(state.enabled)
}

fn load_review_companion_enabled(workspace_key: Option<&str>, cx: &App) -> Option<bool> {
    let workspace_key = workspace_key?;
    let kvp = KeyValueStore::global(cx);
    let scope = kvp.scoped(REVIEW_COMPANION_SCOPE_KEY);
    let state = scope
        .read(workspace_key)
        .ok()
        .flatten()
        .and_then(|json| serde_json::from_str::<SerializedReviewCompanionState>(&json).ok())?;
    Some(state.enabled)
}

fn load_collapsed_host_keys(workspace_key: Option<&str>, cx: &App) -> Option<HashSet<String>> {
    let workspace_key = workspace_key?;
    let kvp = KeyValueStore::global(cx);
    let scope = kvp.scoped(COLLAPSED_HOST_SCOPE_KEY);
    let state = scope
        .read(workspace_key)
        .ok()
        .flatten()
        .and_then(|json| serde_json::from_str::<SerializedCollapsedHostState>(&json).ok())?;
    Some(normalize_collapsed_host_keys(&state.host_keys))
}

fn begin_open_request(open_request_tracker: &Arc<Mutex<OpenRequestTracker>>) -> u64 {
    let mut tracker = open_request_tracker.lock();
    tracker.next_request_id += 1;
    tracker.latest_request_id = tracker.next_request_id;
    tracker.latest_request_id
}

fn is_latest_open_request(
    open_request_tracker: &Arc<Mutex<OpenRequestTracker>>,
    request_id: u64,
) -> bool {
    open_request_tracker.lock().latest_request_id == request_id
}

fn looks_like_vitermux_terminal_label(label: &str) -> bool {
    label.starts_with("vitermux:")
        || (label.starts_with("node:") && label.contains("/sess:") && label.contains("/win:"))
}

fn normalize_vitermux_terminal_label(label: &str) -> &str {
    label.strip_prefix("vitermux:").unwrap_or(label)
}

fn vitermux_terminal_label_matches(left: &str, right: &str) -> bool {
    left == right
        || normalize_vitermux_terminal_label(left) == normalize_vitermux_terminal_label(right)
}

fn insert_terminal_key_aliases(
    index_by_terminal_key: &mut HashMap<String, usize>,
    terminal_key: &str,
    index: usize,
) {
    if terminal_key.trim().is_empty() {
        return;
    }
    index_by_terminal_key.insert(terminal_key.to_string(), index);
    let normalized = normalize_vitermux_terminal_label(terminal_key);
    if normalized != terminal_key {
        index_by_terminal_key.insert(normalized.to_string(), index);
    } else if looks_like_vitermux_terminal_label(terminal_key) {
        index_by_terminal_key.insert(format!("vitermux:{terminal_key}"), index);
    }
}

fn cached_open_plan_matches_row(plan: &ZedOpenPlan, row: &WorkbenchRow) -> bool {
    let dedupe_key = plan.attach.dedupe_key.trim();
    dedupe_key.is_empty()
        || vitermux_terminal_label_matches(dedupe_key, row.dedupe_key.as_ref())
        || vitermux_terminal_label_matches(dedupe_key, row.row_key.as_ref())
}

fn cached_open_plan_for_session(
    open_plan_cache: &Arc<Mutex<HashMap<String, ZedOpenPlan>>>,
    session_key: &str,
    row: &WorkbenchRow,
) -> Option<ZedOpenPlan> {
    open_plan_cache
        .lock()
        .get(session_key)
        .filter(|plan| cached_open_plan_matches_row(plan, row))
        .cloned()
}

fn store_cached_open_plan(
    open_plan_cache: &Arc<Mutex<HashMap<String, ZedOpenPlan>>>,
    session_key: &str,
    plan: &ZedOpenPlan,
) {
    if plan.failure.is_some() {
        return;
    }
    open_plan_cache
        .lock()
        .insert(session_key.to_string(), plan.clone());
}

fn prune_cached_open_plans(
    open_plan_cache: &Arc<Mutex<HashMap<String, ZedOpenPlan>>>,
    rows: &[WorkbenchRow],
    row_index_by_session_key: &HashMap<String, usize>,
    row_index_by_terminal_key: &HashMap<String, usize>,
) {
    open_plan_cache.lock().retain(|session_key, plan| {
        row_index_by_session_key
            .get(session_key.as_str())
            .and_then(|index| rows.get(*index))
            .or_else(|| {
                row_index_by_terminal_key
                    .get(plan.attach.dedupe_key.as_str())
                    .and_then(|index| rows.get(*index))
            })
            .is_some_and(|row| cached_open_plan_matches_row(plan, row))
    });
}

async fn fetch_open_plan_with_cache(
    client: &VitermuxClient,
    open_plan_cache: &Arc<Mutex<HashMap<String, ZedOpenPlan>>>,
    session_key: &str,
    row: &WorkbenchRow,
) -> Result<ZedOpenPlan> {
    if let Some(plan) = cached_open_plan_for_session(open_plan_cache, session_key, row) {
        return Ok(plan);
    }

    let plan = client.fetch_open_plan(session_key).await?;
    store_cached_open_plan(open_plan_cache, session_key, &plan);
    Ok(plan)
}

async fn ensure_project_workspace_for_plan(
    source_workspace: Entity<Workspace>,
    requesting_window: Option<WindowHandle<MultiWorkspace>>,
    plan: &ZedOpenPlan,
    cx: &mut AsyncWindowContext,
) -> Result<Entity<Workspace>> {
    let Some(requesting_window) = requesting_window else {
        return Ok(source_workspace);
    };
    let Some(target_path) = project_target_path(plan) else {
        return Ok(source_workspace);
    };

    let connection_options = project_connection_options(plan)?;
    let target_paths = vec![target_path.clone()];

    if let Some(existing_workspace) =
        requesting_window.update(cx, |multi_workspace, window, cx| {
            let existing_workspace = find_matching_workspace_in_window(
                multi_workspace,
                &target_paths,
                connection_options.as_ref(),
                cx,
            );
            if let Some(workspace) = existing_workspace.as_ref() {
                multi_workspace.activate(workspace.clone(), None, window, cx);
            }
            existing_workspace
        })?
    {
        return Ok(existing_workspace);
    }

    let modal_workspace = source_workspace.clone();
    let open_task = requesting_window.update(cx, |multi_workspace, window, cx| {
        multi_workspace.find_or_create_workspace(
            PathList::new(&target_paths),
            connection_options.clone(),
            None,
            move |connection_options, window, cx| {
                remote_connection::connect_with_modal(
                    &modal_workspace,
                    connection_options,
                    window,
                    cx,
                )
            },
            &[],
            None,
            OpenMode::Activate,
            window,
            cx,
        )
    })?;

    let result = open_task.await;
    remote_connection::dismiss_connection_modal(&source_workspace, cx);
    result
}

struct ProjectWorkspaceResolution {
    workspace: Entity<Workspace>,
    matched_project_context: bool,
}

#[derive(Clone, Copy)]
enum ProjectContextSyncMode {
    BestEffort,
    RequireGitRepository,
}

#[derive(Clone)]
enum ReviewOpenMode {
    ExplicitOpen,
    SyncCompanion {
        expected_terminal_key: String,
        ensure_companion_pane: bool,
    },
}

async fn resolve_project_workspace_for_plan(
    source_workspace: Entity<Workspace>,
    requesting_window: Option<WindowHandle<MultiWorkspace>>,
    plan: &ZedOpenPlan,
    cx: &mut AsyncWindowContext,
) -> Result<ProjectWorkspaceResolution> {
    let has_target_path = project_target_path(plan).is_some();
    if !has_target_path {
        return Ok(ProjectWorkspaceResolution {
            workspace: source_workspace,
            matched_project_context: false,
        });
    }

    if requesting_window.is_none() {
        return Err(anyhow!(
            "cannot resolve target project workspace without a Zed operator window"
        ));
    }

    let workspace =
        ensure_project_workspace_for_plan(source_workspace, requesting_window, plan, cx).await?;
    Ok(ProjectWorkspaceResolution {
        workspace,
        matched_project_context: true,
    })
}

async fn sync_workspace_project_context_for_plan(
    workspace: Entity<Workspace>,
    plan: &ZedOpenPlan,
    mode: ProjectContextSyncMode,
    cx: &mut AsyncWindowContext,
) -> Result<Option<ProjectPath>> {
    let Some(target_path) = project_target_path(plan) else {
        return Ok(None);
    };

    let project_path_task = workspace.update(cx, |workspace, cx| {
        Workspace::project_path_for_path(workspace.project().clone(), &target_path, true, cx)
    });
    let (_, project_path) = project_path_task.await?;

    match mode {
        ProjectContextSyncMode::BestEffort => {
            let _ = set_active_repository_for_project_path(workspace.clone(), &project_path, cx);
        }
        ProjectContextSyncMode::RequireGitRepository => {
            let repository =
                wait_for_project_path_repository(workspace.clone(), &project_path, cx).await?;
            set_active_repository_for_project_path(workspace.clone(), &project_path, cx)?;
            wait_for_repository_barrier(repository, cx).await?;
        }
    }

    Ok(Some(project_path))
}

async fn sync_review_companion_for_terminal_open(
    workspace: Entity<Workspace>,
    row: &WorkbenchRow,
    plan: &ZedOpenPlan,
    terminal_key: &str,
    cx: &mut AsyncWindowContext,
) -> Result<()> {
    if let Some(failure) = plan.failure.as_ref() {
        return Err(open_plan_failure(failure));
    }

    let Some(project_path) = sync_workspace_project_context_for_plan(
        workspace.clone(),
        plan,
        ProjectContextSyncMode::RequireGitRepository,
        cx,
    )
    .await?
    else {
        return Ok(());
    };

    deploy_review_companion_until_synced(
        workspace,
        row,
        project_path,
        terminal_key.to_string(),
        false,
        cx,
    )
    .await?;

    Ok(())
}

async fn deploy_review_companion_until_synced(
    workspace: Entity<Workspace>,
    row: &WorkbenchRow,
    project_path: ProjectPath,
    terminal_key: String,
    ensure_companion_pane: bool,
    cx: &mut AsyncWindowContext,
) -> Result<()> {
    let mut ensure_companion_pane = ensure_companion_pane;

    for attempt in 0..=REVIEW_COMPANION_SYNC_ATTEMPTS {
        let outcome = workspace.update_in(cx, |workspace, window, cx| {
            let focused_terminal_key = focused_terminal_task_label(workspace, cx);
            if !focused_terminal_key
                .as_deref()
                .is_some_and(|focused_terminal_key| {
                    vitermux_terminal_label_matches(focused_terminal_key, terminal_key.as_str())
                })
            {
                return ReviewCompanionSyncOutcome::StaleFocus;
            }

            deploy_review_companion_for_terminal_key(
                workspace,
                row,
                project_path.clone(),
                terminal_key.as_str(),
                ensure_companion_pane,
                window,
                cx,
            );

            if review_companion_matches_terminal_worktree(
                workspace,
                terminal_key.as_str(),
                &project_path,
                cx,
            ) {
                ReviewCompanionSyncOutcome::Matched
            } else {
                ReviewCompanionSyncOutcome::RetryNeeded
            }
        })?;

        match outcome {
            ReviewCompanionSyncOutcome::Matched | ReviewCompanionSyncOutcome::StaleFocus => {
                return Ok(());
            }
            ReviewCompanionSyncOutcome::RetryNeeded => {}
        }

        if attempt == REVIEW_COMPANION_SYNC_ATTEMPTS {
            break;
        }

        ensure_companion_pane = false;
        cx.background_executor()
            .timer(REVIEW_COMPANION_SYNC_DELAY)
            .await;
    }

    Ok(())
}

fn deploy_review_companion_for_terminal_key(
    workspace: &mut Workspace,
    row: &WorkbenchRow,
    project_path: ProjectPath,
    terminal_key: &str,
    ensure_companion_pane: bool,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let focused_terminal_key = focused_terminal_task_label(workspace, cx);
    if !focused_terminal_key
        .as_deref()
        .is_some_and(|focused_terminal_key| {
            vitermux_terminal_label_matches(focused_terminal_key, terminal_key)
        })
    {
        return;
    }

    let terminal_pane = terminal_pane_for_task_label(workspace, terminal_key, cx)
        .or_else(|| terminal_pane_for_row(workspace, row, cx));
    let Some(terminal_pane) = terminal_pane else {
        return;
    };

    let should_create_companion = ensure_companion_pane
        || find_review_diff_pane_for_terminal(workspace, &terminal_pane, cx).is_none();
    let review_pane = if should_create_companion {
        Some(workspace.adjacent_pane_from(
            terminal_pane.clone(),
            workspace::SplitDirection::Right,
            window,
            cx,
        ))
    } else {
        find_review_diff_pane_for_terminal(workspace, &terminal_pane, cx)
    };
    if let Some(review_pane) = review_pane {
        ProjectDiff::deploy_at_project_path_in_pane(
            workspace,
            review_pane,
            project_path,
            should_create_companion,
            false,
            false,
            window,
            cx,
        );
    }
}

enum ReviewCompanionSyncOutcome {
    Matched,
    RetryNeeded,
    StaleFocus,
}

async fn wait_for_project_path_repository(
    workspace: Entity<Workspace>,
    project_path: &ProjectPath,
    cx: &mut AsyncWindowContext,
) -> Result<Entity<project::git_store::Repository>> {
    for attempt in 0..=PROJECT_REPOSITORY_DISCOVERY_ATTEMPTS {
        if let Some(repository) = project_path_repository(workspace.clone(), project_path, cx)? {
            return Ok(repository);
        }

        if attempt == PROJECT_REPOSITORY_DISCOVERY_ATTEMPTS {
            break;
        }

        cx.background_executor()
            .timer(PROJECT_REPOSITORY_DISCOVERY_DELAY)
            .await;
    }

    Err(anyhow!(
        "target project path did not resolve to a git repository"
    ))
}

fn project_path_repository(
    workspace: Entity<Workspace>,
    project_path: &ProjectPath,
    cx: &mut AsyncWindowContext,
) -> Result<Option<Entity<project::git_store::Repository>>> {
    Ok(workspace.update(cx, |workspace, cx| {
        let git_store = workspace.project().read(cx).git_store().clone();
        git_store
            .read(cx)
            .repository_and_path_for_project_path(project_path, cx)
            .map(|(repository, _)| repository.clone())
    }))
}

fn set_active_repository_for_project_path(
    workspace: Entity<Workspace>,
    project_path: &ProjectPath,
    cx: &mut AsyncWindowContext,
) -> Result<()> {
    workspace.update(cx, |workspace, cx| {
        let git_store = workspace.project().read(cx).git_store().clone();
        git_store.update(cx, |git_store, cx| {
            git_store.set_active_repo_for_path(project_path, cx);
        });
    });
    Ok(())
}

async fn wait_for_repository_barrier(
    repository: Entity<project::git_store::Repository>,
    cx: &mut AsyncWindowContext,
) -> Result<()> {
    let barrier = repository.update(cx, |repository, _| repository.barrier());
    barrier
        .await
        .map_err(|_| anyhow!("repository barrier canceled"))
}

fn find_matching_workspace_in_window(
    multi_workspace: &MultiWorkspace,
    target_paths: &[PathBuf],
    connection_options: Option<&RemoteConnectionOptions>,
    cx: &App,
) -> Option<Entity<Workspace>> {
    let target_path_list = PathList::new(target_paths);
    let mut best_match = None;
    let mut matching_workspace = None;

    for workspace in multi_workspace.workspaces() {
        let root_paths = workspace.read(cx).root_paths(cx);
        if PathList::new(&root_paths) == target_path_list {
            let project = workspace.read(cx).project().clone();
            if same_remote_connection_identity(
                project.read(cx).remote_connection_options(cx).as_ref(),
                connection_options,
            ) {
                return Some(workspace.clone());
            }
        }

        let project = workspace.read(cx).project().clone();
        if !same_remote_connection_identity(
            project.read(cx).remote_connection_options(cx).as_ref(),
            connection_options,
        ) {
            continue;
        }

        let visibility = project
            .read(cx)
            .visibility_for_paths(target_paths, false, cx);
        let match_depth = matching_root_depth(&root_paths, target_paths);
        let score = visibility.map(|visible| (visible, match_depth));
        if score > best_match {
            best_match = score;
            matching_workspace = Some(workspace.clone());
        }
    }

    matching_workspace
}

fn matching_root_depth(root_paths: &[Arc<Path>], target_paths: &[PathBuf]) -> usize {
    root_paths
        .iter()
        .filter(|root| {
            target_paths
                .iter()
                .all(|path| path.starts_with(root.as_ref()))
        })
        .map(|root| root.components().count())
        .max()
        .unwrap_or(0)
}

fn project_connection_options(plan: &ZedOpenPlan) -> Result<Option<RemoteConnectionOptions>> {
    if plan.project.mode != "remote" {
        return Ok(None);
    }

    let ssh_address = non_empty_string(plan.project.ssh_address.as_str())
        .ok_or_else(|| anyhow!("daemon open plan did not include an ssh address"))?;
    let ssh_options = SshConnectionOptions::parse_command_line(&ssh_address)?;
    Ok(Some(RemoteConnectionOptions::from(ssh_options)))
}

fn project_target_path(plan: &ZedOpenPlan) -> Option<PathBuf> {
    non_empty_string(plan.project.remote_path.as_str())
        .or_else(|| non_empty_string(plan.attach.cwd.as_str()))
        .map(PathBuf::from)
}

fn build_spawn_task(
    row: &WorkbenchRow,
    plan: &ZedOpenPlan,
    attach_inside_target_remote_workspace: bool,
    workspace_is_remote: bool,
) -> Result<SpawnInTerminal> {
    if let Some(failure) = plan.failure.as_ref() {
        return Err(open_plan_failure(failure));
    }

    let (command, args) = resolve_attach_command(plan, attach_inside_target_remote_workspace)?;
    let (shell, command, args) = spawn_command_parts(plan, command, args);
    let cwd = spawn_cwd(plan, workspace_is_remote);
    let dedupe_key = if !plan.attach.dedupe_key.trim().is_empty() {
        plan.attach.dedupe_key.clone()
    } else if !row.dedupe_key.trim().is_empty() {
        row.dedupe_key.to_string()
    } else {
        row.row_key.to_string()
    };

    Ok(SpawnInTerminal {
        id: TaskId(dedupe_key.clone()),
        full_label: dedupe_key.clone(),
        label: row.window_label.to_string(),
        command_label: command
            .as_ref()
            .map(|command| shell_label(command, &args))
            .unwrap_or_else(|| "attach".to_string()),
        command,
        args,
        cwd,
        env: plan
            .attach
            .env
            .iter()
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect::<HashMap<_, _>>(),
        use_new_terminal: false,
        allow_concurrent_runs: false,
        reveal: RevealStrategy::Always,
        reveal_target: RevealTarget::Center,
        hide: HideStrategy::Never,
        shell,
        show_summary: false,
        show_command: false,
        show_rerun: false,
        save: SaveStrategy::None,
    })
}

fn open_plan_failure(failure: &OpenPlanFailure) -> anyhow::Error {
    anyhow!(failure.display_message())
}

fn resolve_attach_command(
    plan: &ZedOpenPlan,
    attach_inside_target_remote_workspace: bool,
) -> Result<(String, Vec<String>)> {
    if attach_inside_target_remote_workspace && plan.attach.mode == "ssh_shell" {
        return tmux_attach_fallback(plan)
            .ok_or_else(|| anyhow!("daemon remote attach plan did not include a tmux target"));
    }

    if plan.attach.mode == "ssh_shell" {
        return argv_attach_command(plan)
            .ok_or_else(|| anyhow!("daemon SSH attach plan did not include argv"));
    }

    if let Some((command, args)) = argv_attach_command(plan) {
        return Ok((command, args));
    }

    if plan.attach.mode == "local_shell" {
        if let Some((command, args)) = tmux_attach_fallback(plan) {
            return Ok((command, args));
        }
    }

    if !plan.attach.command.trim().is_empty() {
        return Ok((plan.attach.command.clone(), Vec::new()));
    }

    Err(anyhow!(
        "daemon open plan did not include a runnable attach command"
    ))
}

fn argv_attach_command(plan: &ZedOpenPlan) -> Option<(String, Vec<String>)> {
    let mut argv = plan.attach.argv.iter();
    let command = argv.next()?.trim();
    if command.is_empty() {
        return None;
    }
    Some((command.to_string(), argv.cloned().collect()))
}

fn tmux_attach_fallback(plan: &ZedOpenPlan) -> Option<(String, Vec<String>)> {
    let tmux_session = non_empty_string(plan.target_ref.tmux_session.as_str())?;
    let cwd = non_empty_string(plan.project.remote_path.as_str())
        .or_else(|| non_empty_string(plan.attach.cwd.as_str()))?;
    let session_quoted = single_quote(&tmux_session);
    let tmux_target = non_empty_string(plan.target_ref.tmux_window.as_str())
        .map(|window| format!("{tmux_session}:{window}"))
        .unwrap_or_else(|| tmux_session.clone());
    let target_quoted = single_quote(&tmux_target);
    let cwd_quoted = single_quote(&cwd);
    let script = format!(
        "if tmux has-session -t {session} 2>/dev/null; then exec tmux attach-session -t {session} \\; select-window -t {target}; else cd -- {cwd} && exec ${{SHELL:-zsh}} -l; fi",
        session = session_quoted,
        target = target_quoted,
        cwd = cwd_quoted,
    );
    Some(("/bin/sh".to_string(), vec!["-lc".to_string(), script]))
}

fn focus_existing_terminal(
    workspace: &mut Workspace,
    terminal_panel: Option<Entity<TerminalPanel>>,
    full_label: &str,
    window: &mut Window,
    cx: &mut App,
) -> bool {
    if let Some((pane, ix)) =
        find_existing_terminal_target(workspace, terminal_panel, full_label, cx)
    {
        pane.update(cx, |pane, cx| {
            pane.activate_item(ix, true, true, window, cx)
        });
        return true;
    }

    false
}

fn focus_existing_terminal_in_operator_window(
    source_workspace: Entity<Workspace>,
    requesting_window: Option<WindowHandle<MultiWorkspace>>,
    full_label: &str,
    cx: &mut AsyncWindowContext,
) -> Result<Option<Entity<Workspace>>> {
    if let Some(requesting_window) = requesting_window {
        return requesting_window.update(cx, |multi_workspace, window, cx| {
            focus_existing_terminal_in_multi_workspace(multi_workspace, full_label, window, cx)
        });
    }

    if source_workspace.update_in(cx, |workspace, window, cx| {
        let terminal_panel = workspace.panel::<TerminalPanel>(cx);
        focus_existing_terminal(workspace, terminal_panel, full_label, window, cx)
    })? {
        Ok(Some(source_workspace))
    } else {
        Ok(None)
    }
}

fn focus_existing_terminal_in_multi_workspace(
    multi_workspace: &mut MultiWorkspace,
    full_label: &str,
    window: &mut Window,
    cx: &mut Context<MultiWorkspace>,
) -> Option<Entity<Workspace>> {
    let active_workspace = multi_workspace.workspace().clone();
    let active_workspace_id = active_workspace.entity_id();
    let mut workspaces = multi_workspace.workspaces().cloned().collect::<Vec<_>>();
    workspaces.sort_by_key(|workspace| workspace.entity_id() != active_workspace_id);

    let Some((candidate_workspace, pane, ix)) =
        workspaces.into_iter().find_map(|candidate_workspace| {
            let terminal_panel = candidate_workspace
                .read_with(cx, |workspace, cx| workspace.panel::<TerminalPanel>(cx));
            let target = candidate_workspace.read_with(cx, |workspace, cx| {
                find_existing_terminal_target(workspace, terminal_panel.clone(), full_label, cx)
            })?;
            Some((candidate_workspace, target.0, target.1))
        })
    else {
        return None;
    };

    if candidate_workspace.entity_id() != active_workspace_id {
        multi_workspace.activate(candidate_workspace.clone(), None, window, cx);
    }
    pane.update(cx, |pane, cx| {
        pane.activate_item(ix, true, true, window, cx)
    });
    Some(candidate_workspace)
}

fn find_existing_terminal_target(
    workspace: &Workspace,
    terminal_panel: Option<Entity<TerminalPanel>>,
    full_label: &str,
    cx: &App,
) -> Option<(Entity<Pane>, usize)> {
    let mut panes = workspace.panes().iter().cloned().collect::<Vec<_>>();
    if let Some(terminal_panel) = terminal_panel {
        panes.extend(
            terminal_panel
                .read(cx)
                .panes()
                .into_iter()
                .map(|pane| pane.clone()),
        );
    }

    panes.iter().find_map(|pane| {
        pane.read(cx).items().enumerate().find_map(|(ix, item)| {
            let terminal_view = item.act_as::<TerminalView>(cx)?;
            let task = terminal_view.read(cx).terminal().read(cx).task()?;
            vitermux_terminal_label_matches(task.spawned_task.full_label.as_str(), full_label)
                .then_some((pane.clone(), ix))
        })
    })
}

struct FocusedTerminalMatch {
    task_label: String,
    row: WorkbenchRow,
    pane: Entity<Pane>,
}

fn focused_terminal_match(
    rows: &[WorkbenchRow],
    row_index_by_terminal_key: &HashMap<String, usize>,
    workspace: &Workspace,
    cx: &App,
) -> Option<FocusedTerminalMatch> {
    let active_item = workspace.active_item(cx)?;
    let pane = workspace.pane_for(active_item.as_ref())?;
    let terminal_view = active_item.act_as::<TerminalView>(cx)?;
    let task = terminal_view.read(cx).terminal().read(cx).task()?;
    let terminal_key = task.spawned_task.full_label.clone();
    let row = row_index_by_terminal_key
        .get(terminal_key.as_str())
        .and_then(|index| rows.get(*index))
        .cloned()
        .or_else(|| row_for_terminal_task_label(rows, &terminal_key))?;
    Some(FocusedTerminalMatch {
        task_label: terminal_key,
        row,
        pane,
    })
}

fn focused_terminal_task_label(workspace: &Workspace, cx: &App) -> Option<String> {
    let active_item = workspace.active_item(cx)?;
    let terminal_view = active_item.act_as::<TerminalView>(cx)?;
    let task = terminal_view.read(cx).terminal().read(cx).task()?;
    Some(task.spawned_task.full_label.clone())
}

fn row_for_terminal_task_label(rows: &[WorkbenchRow], terminal_key: &str) -> Option<WorkbenchRow> {
    rows.iter()
        .find(|row| {
            vitermux_terminal_label_matches(row.dedupe_key.as_ref(), terminal_key)
                || vitermux_terminal_label_matches(row.row_key.as_ref(), terminal_key)
        })
        .cloned()
}

fn terminal_pane_for_row(
    workspace: &Workspace,
    row: &WorkbenchRow,
    cx: &App,
) -> Option<Entity<Pane>> {
    terminal_pane_for_task_label(workspace, row.dedupe_key.as_ref(), cx)
        .or_else(|| terminal_pane_for_task_label(workspace, row.row_key.as_ref(), cx))
}

fn terminal_pane_for_task_label(
    workspace: &Workspace,
    terminal_key: &str,
    cx: &App,
) -> Option<Entity<Pane>> {
    let mut fallback = None;
    for pane in workspace.panes() {
        let has_match = pane.read(cx).items().into_iter().any(|item| {
            item.act_as::<TerminalView>(cx)
                .is_some_and(|terminal_view| {
                    terminal_view
                        .read(cx)
                        .terminal()
                        .read(cx)
                        .task()
                        .is_some_and(|task| {
                            vitermux_terminal_label_matches(
                                task.spawned_task.full_label.as_str(),
                                terminal_key,
                            )
                        })
                })
        });
        if !has_match {
            continue;
        }
        if !pane_has_review_diff(pane, cx) {
            return Some(pane.clone());
        }
        fallback.get_or_insert_with(|| pane.clone());
    }
    fallback
}

fn preferred_terminal_spawn_pane(
    workspace: &mut Workspace,
    preferred_terminal_key: Option<&str>,
    cx: &App,
) -> Entity<Pane> {
    preferred_terminal_key
        .and_then(|terminal_key| terminal_pane_for_task_label(workspace, terminal_key, cx))
        .or_else(|| {
            let review_pane = find_review_diff_pane(workspace, cx)?;
            workspace.pane_in_direction_from(&review_pane, workspace::SplitDirection::Left, cx)
        })
        .unwrap_or_else(|| workspace.active_pane().clone())
}

fn find_review_diff_pane(workspace: &Workspace, cx: &App) -> Option<Entity<Pane>> {
    workspace
        .panes()
        .iter()
        .rev()
        .find(|pane| pane_has_review_diff(pane, cx))
        .cloned()
}

fn find_review_diff_pane_for_terminal(
    workspace: &mut Workspace,
    terminal_pane: &Entity<Pane>,
    cx: &App,
) -> Option<Entity<Pane>> {
    let review_pane =
        workspace.pane_in_direction_from(terminal_pane, workspace::SplitDirection::Right, cx)?;
    pane_has_review_diff(&review_pane, cx).then_some(review_pane)
}

fn pane_has_review_diff(pane: &Entity<Pane>, cx: &App) -> bool {
    pane.read(cx)
        .items_of_type::<ProjectDiff>()
        .any(|item| matches!(item.read(cx).diff_base(cx), DiffBase::Head))
}

fn review_companion_matches_terminal_worktree(
    workspace: &mut Workspace,
    terminal_key: &str,
    project_path: &ProjectPath,
    cx: &App,
) -> bool {
    let Some(terminal_pane) = terminal_pane_for_task_label(workspace, terminal_key, cx) else {
        return false;
    };
    let Some(review_pane) =
        find_review_diff_pane_for_terminal_in_workspace(workspace, &terminal_pane, cx)
    else {
        return false;
    };
    review_pane
        .read(cx)
        .items_of_type::<ProjectDiff>()
        .find(|item| matches!(item.read(cx).diff_base(cx), DiffBase::Head))
        .and_then(|item| item.read(cx).active_path(cx))
        .is_some_and(|active_project_path| {
            active_project_path.worktree_id == project_path.worktree_id
        })
}

fn find_review_diff_pane_for_terminal_in_workspace(
    workspace: &mut Workspace,
    terminal_pane: &Entity<Pane>,
    cx: &App,
) -> Option<Entity<Pane>> {
    let review_pane =
        workspace.pane_in_direction_from(terminal_pane, workspace::SplitDirection::Right, cx)?;
    pane_has_review_diff(&review_pane, cx).then_some(review_pane)
}

fn window_label(window: &TmuxWindow) -> String {
    first_non_empty([
        primary_agent_name(window),
        window.binding.workspace_label.as_str(),
        window.window_name.as_str(),
        window.window_target.as_str(),
        window.window_id.as_str(),
        window.window_key.as_str(),
    ])
    .to_string()
}

fn primary_agent_name(window: &TmuxWindow) -> &str {
    let primary_session_key = window.primary_session_key().unwrap_or_default();
    if !primary_session_key.is_empty() {
        if let Some(agent_name) = window.agents.iter().find_map(|agent| {
            let matches_primary = (!agent.session_key.trim().is_empty()
                && agent.session_key.trim() == primary_session_key)
                || (!agent.session_id.trim().is_empty()
                    && agent.session_id.trim() == primary_session_key);
            matches_primary
                .then(|| agent.name.trim())
                .filter(|name| !name.is_empty())
        }) {
            return agent_name;
        }
    }

    window
        .agents
        .iter()
        .map(|agent| agent.name.trim())
        .find(|name| !name.is_empty())
        .unwrap_or("")
}

fn window_detail(window: &TmuxWindow) -> String {
    join_detail_parts([
        non_empty_string(window.harness.provider.as_str()),
        non_empty_string(window.harness.status.as_str()),
        non_empty_string(window.binding.cwd.as_str()),
        non_empty_string(window.binding.confidence.as_str()),
        attention_detail(window.attention.as_str()),
        non_empty_string(window.attach_state.as_str()),
    ])
}

fn review_available(row: &WorkbenchRow) -> bool {
    row.session_key.is_some() && matches!(row.attention.as_ref(), "new_review" | "urgent")
}

fn join_detail_parts(parts: impl IntoIterator<Item = Option<String>>) -> String {
    parts
        .into_iter()
        .flatten()
        .filter(|value| !value.trim().is_empty())
        .collect::<Vec<_>>()
        .join("  ")
}

fn rename_target_for_row(row: &WorkbenchRow) -> Option<SessionRenameTarget> {
    let session_key = row.session_key.as_ref()?.trim();
    if session_key.is_empty() {
        return None;
    }

    Some(SessionRenameTarget {
        row_key: row.row_key.to_string(),
        session_key: session_key.to_string(),
        current_name: row.window_label.to_string(),
    })
}

fn attention_detail(attention: &str) -> Option<String> {
    let label = match attention {
        "urgent" | "new_review" => "needs review",
        "watching" => "waiting",
        "snoozed" => "snoozed",
        "working" => "working",
        "idle" => "idle",
        "complete" => "reviewed",
        "blocked" => "parked",
        "archived" => "archived",
        _ => "",
    };

    non_empty_string(label)
}

fn attention_icon(attention: &str) -> IconName {
    match attention {
        "urgent" | "new_review" => IconName::Warning,
        "watching" => IconName::Eye,
        "snoozed" => IconName::Clock,
        "working" => IconName::Terminal,
        "complete" => IconName::Check,
        "blocked" | "archived" => IconName::Archive,
        "idle" => IconName::Circle,
        _ => IconName::SquareDot,
    }
}

fn attention_color(attention: &str) -> Color {
    match attention {
        "urgent" | "new_review" => Color::Warning,
        "working" => Color::Accent,
        "complete" => Color::Success,
        _ => Color::Muted,
    }
}

fn shell_label(command: &str, argv: &[String]) -> String {
    std::iter::once(command.to_string())
        .chain(argv.iter().cloned())
        .collect::<Vec<_>>()
        .join(" ")
}

fn spawn_command_parts(
    plan: &ZedOpenPlan,
    command: String,
    args: Vec<String>,
) -> (Shell, Option<String>, Vec<String>) {
    if !plan.attach.argv.is_empty() || !args.is_empty() {
        return (
            Shell::WithArguments {
                program: command,
                args,
                title_override: None,
            },
            None,
            Vec::new(),
        );
    }

    (Shell::System, Some(command), args)
}

fn spawn_cwd(plan: &ZedOpenPlan, workspace_is_remote: bool) -> Option<PathBuf> {
    if workspace_is_remote || plan.project.mode == "remote" {
        return None;
    }

    non_empty_string(plan.attach.cwd.as_str()).map(PathBuf::from)
}

fn attach_inside_target_remote_workspace(
    plan: &ZedOpenPlan,
    matched_project_context: bool,
    workspace_is_remote: bool,
) -> bool {
    plan.project.mode == "remote" && matched_project_context && workspace_is_remote
}

fn allow_source_workspace_attach_fallback(source_workspace_is_remote: bool) -> bool {
    !source_workspace_is_remote
}

fn single_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn non_empty_string(value: &str) -> Option<String> {
    let trimmed = value.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

fn first_non_empty<'a>(values: impl IntoIterator<Item = &'a str>) -> &'a str {
    values
        .into_iter()
        .find(|value| !value.trim().is_empty())
        .unwrap_or("")
}

#[cfg(test)]
fn selected_row_index_in(rows: &[WorkbenchRow], selected: Option<&SharedString>) -> Option<usize> {
    let selected = selected?;
    rows.iter()
        .position(|row| row.row_key.as_ref() == selected.as_ref())
}

#[cfg(test)]
fn selected_display_row_index_in(
    display_rows: &[DisplayRow],
    selected: Option<&SharedString>,
) -> Option<usize> {
    let selected = selected?;
    display_rows.iter().position(|row| match row {
        DisplayRow::Window(row) => row.row_key.as_ref() == selected.as_ref(),
        DisplayRow::HostHeader { .. } | DisplayRow::SessionHeader { .. } => false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use fs::FakeFs;
    use gpui::{KeyBinding, TestAppContext};
    use project::Project;
    use serde_json::json;
    use settings::SettingsStore;
    use task::TaskId;
    use terminal_view::terminal_panel::TerminalPanel;
    use util::{path, rel_path::rel_path};
    use workspace::MultiWorkspace;

    #[test]
    fn flatten_rows_uses_window_session_key_and_dedupe_identity() {
        let snapshot = TmuxTreeSnapshot {
            self_node_id: "macbook".into(),
            nodes: vec![vitermux::TmuxNode {
                node_key: "poros".into(),
                node_id: "poros".into(),
                node_name: "poros".into(),
                accounts: vec![vitermux::TmuxAccount {
                    account_key: "albertus@poros".into(),
                    account_name: "Albertus".into(),
                    account_user: "albertus".into(),
                    sessions: vec![vitermux::TmuxSession {
                        session_key: "main".into(),
                        session_name: "main".into(),
                        project_group: vitermux::ProjectGroupBinding {
                            workspace_label: "agent-hud".into(),
                            codebase_label: "agent-hud".into(),
                        },
                        windows: vec![TmuxWindow {
                            dedupe_key: "node:poros/acct:albertus@poros/sess:main/win:2".into(),
                            window_name: "frontend-logging-cleanup".into(),
                            binding: vitermux::WorktreeBinding {
                                cwd: "/home/albertus/dev/agent-hud".into(),
                                confidence: "provider_cwd".into(),
                                ..Default::default()
                            },
                            harness: vitermux::HarnessBinding {
                                session_key: "codex:poros:albertus@poros:123".into(),
                                provider: "codex".into(),
                                ..Default::default()
                            },
                            attention: "new_review".into(),
                            ..Default::default()
                        }],
                    }],
                }],
                ..Default::default()
            }],
            ..Default::default()
        };

        let rows = flatten_rows(&snapshot);
        assert_eq!(rows.len(), 1);
        let row = &rows[0];
        assert_eq!(
            row.row_key.as_ref(),
            "node:poros/acct:albertus@poros/sess:main/win:2"
        );
        assert_eq!(
            row.session_key.as_ref().map(|value| value.as_ref()),
            Some("codex:poros:albertus@poros:123")
        );
        assert_eq!(row.node_label.as_ref(), "albertus@poros");
        assert!(!row.node_is_local);
        assert_eq!(row.window_label.as_ref(), "frontend-logging-cleanup");
    }

    #[test]
    fn flatten_rows_marks_local_host_rows_from_snapshot_self_node() {
        let snapshot = TmuxTreeSnapshot {
            self_node_id: "macbook".into(),
            nodes: vec![vitermux::TmuxNode {
                node_key: "macbook".into(),
                node_id: "macbook".into(),
                node_name: "MacBook".into(),
                accounts: vec![vitermux::TmuxAccount {
                    account_key: "albertusangga@macbook".into(),
                    account_name: "Albertus".into(),
                    account_user: "albertusangga".into(),
                    sessions: vec![vitermux::TmuxSession {
                        session_key: "zed-tabs".into(),
                        session_name: "zed-tabs".into(),
                        windows: vec![TmuxWindow {
                            dedupe_key:
                                "node:macbook/acct:albertusangga@macbook/sess:zed-tabs/win:index:1"
                                    .into(),
                            window_name: "vitermux".into(),
                            harness: vitermux::HarnessBinding {
                                session_key: "codex:local:albertusangga@macbook:123".into(),
                                provider: "codex".into(),
                                ..Default::default()
                            },
                            ..Default::default()
                        }],
                        ..Default::default()
                    }],
                }],
                ..Default::default()
            }],
            ..Default::default()
        };

        let rows = flatten_rows(&snapshot);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].node_label.as_ref(), "albertusangga@macbook");
        assert!(rows[0].node_is_local);
    }

    #[test]
    fn flatten_rows_skips_windows_without_bound_agent_session() {
        let snapshot = TmuxTreeSnapshot {
            self_node_id: "macbook".into(),
            nodes: vec![vitermux::TmuxNode {
                node_key: "macbook".into(),
                node_id: "macbook".into(),
                node_name: "MacBook".into(),
                accounts: vec![vitermux::TmuxAccount {
                    account_key: "albertusangga@macbook".into(),
                    account_name: "Albertus".into(),
                    account_user: "albertusangga".into(),
                    sessions: vec![vitermux::TmuxSession {
                        session_key: "zed-tabs".into(),
                        session_name: "zed-tabs".into(),
                        windows: vec![
                            TmuxWindow {
                                dedupe_key: "node:macbook/acct:albertusangga@macbook/sess:zed-tabs/win:index:1".into(),
                                window_name: "tracked".into(),
                                harness: vitermux::HarnessBinding {
                                    session_key: "codex:local:albertusangga@macbook:tracked".into(),
                                    provider: "codex".into(),
                                    ..Default::default()
                                },
                                ..Default::default()
                            },
                            TmuxWindow {
                                dedupe_key: "node:macbook/acct:albertusangga@macbook/sess:zed-tabs/win:index:2".into(),
                                window_name: "unbound".into(),
                                ..Default::default()
                            },
                        ],
                        ..Default::default()
                    }],
                }],
                ..Default::default()
            }],
            ..Default::default()
        };

        let rows = flatten_rows(&snapshot);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].window_label.as_ref(), "tracked");
        assert_eq!(
            rows[0].session_key.as_ref().map(|value| value.as_ref()),
            Some("codex:local:albertusangga@macbook:tracked")
        );
    }

    #[test]
    fn selected_row_index_matches_row_key() {
        let rows = vec![
            WorkbenchRow {
                row_key: SharedString::from("row-1"),
                ..sample_row()
            },
            WorkbenchRow {
                row_key: SharedString::from("row-2"),
                ..sample_row()
            },
        ];

        assert_eq!(
            selected_row_index_in(&rows, Some(&SharedString::from("row-2"))),
            Some(1)
        );
        assert_eq!(
            selected_row_index_in(&rows, Some(&SharedString::from("missing"))),
            None
        );
    }

    #[test]
    fn build_display_rows_inserts_group_headers_once_per_section() {
        let rows = vec![
            WorkbenchRow {
                row_key: SharedString::from("row-1"),
                session_section_key: SharedString::from("main"),
                window_label: SharedString::from("frontend-logging-cleanup"),
                ..sample_row()
            },
            WorkbenchRow {
                row_key: SharedString::from("row-2"),
                session_section_key: SharedString::from("main"),
                window_label: SharedString::from("library-ux-rewrite"),
                ..sample_row()
            },
            WorkbenchRow {
                row_key: SharedString::from("row-3"),
                session_section_key: SharedString::from("review"),
                session_label: SharedString::from("review"),
                ..sample_row()
            },
        ];

        let display_rows = build_display_rows(&rows, &HashSet::default());
        assert_eq!(display_rows.len(), 6);
        assert!(matches!(
            &display_rows[0],
            DisplayRow::HostHeader { label, collapsed, .. } if label.as_ref() == "albertus@poros" && !collapsed
        ));
        assert!(matches!(
            &display_rows[1],
            DisplayRow::SessionHeader { label, .. } if label.as_ref() == "main"
        ));
        assert!(
            matches!(&display_rows[2], DisplayRow::Window(row) if row.row_key.as_ref() == "row-1")
        );
        assert!(
            matches!(&display_rows[3], DisplayRow::Window(row) if row.row_key.as_ref() == "row-2")
        );
        assert!(matches!(
            &display_rows[4],
            DisplayRow::SessionHeader { label, .. } if label.as_ref() == "review"
        ));
        assert!(
            matches!(&display_rows[5], DisplayRow::Window(row) if row.row_key.as_ref() == "row-3")
        );
    }

    #[test]
    fn build_display_rows_hides_children_for_collapsed_host() {
        let rows = vec![
            WorkbenchRow {
                row_key: SharedString::from("row-1"),
                ..sample_row()
            },
            WorkbenchRow {
                row_key: SharedString::from("row-2"),
                node_section_key: SharedString::from("node-section-2"),
                node_label: SharedString::from("albertus@tate"),
                session_section_key: SharedString::from("session-2"),
                session_label: SharedString::from("review"),
                ..sample_row()
            },
        ];
        let collapsed_host_keys: HashSet<String> =
            ["node-section".to_string()].into_iter().collect();

        let display_rows = build_display_rows(&rows, &collapsed_host_keys);
        assert_eq!(display_rows.len(), 4);
        assert!(matches!(
            &display_rows[0],
            DisplayRow::HostHeader {
                label,
                collapsed: true,
                ..
            } if label.as_ref() == "albertus@poros"
        ));
        assert!(matches!(
            &display_rows[1],
            DisplayRow::HostHeader {
                label,
                collapsed: false,
                ..
            } if label.as_ref() == "albertus@tate"
        ));
        assert!(matches!(
            &display_rows[2],
            DisplayRow::SessionHeader { label, .. } if label.as_ref() == "review"
        ));
        assert!(matches!(
            &display_rows[3],
            DisplayRow::Window(row) if row.row_key.as_ref() == "row-2"
        ));
    }

    #[test]
    fn selected_display_row_index_matches_visible_window_row() {
        let display_rows = build_display_rows(
            &[
                WorkbenchRow {
                    row_key: SharedString::from("row-1"),
                    ..sample_row()
                },
                WorkbenchRow {
                    row_key: SharedString::from("row-2"),
                    session_section_key: SharedString::from("review"),
                    session_label: SharedString::from("review"),
                    ..sample_row()
                },
            ],
            &HashSet::default(),
        );

        assert_eq!(
            selected_display_row_index_in(&display_rows, Some(&SharedString::from("row-1"))),
            Some(2)
        );
        assert_eq!(
            selected_display_row_index_in(&display_rows, Some(&SharedString::from("row-2"))),
            Some(4)
        );
        assert_eq!(
            selected_display_row_index_in(&display_rows, Some(&SharedString::from("missing"))),
            None
        );
    }

    #[test]
    fn row_for_terminal_task_label_matches_dedupe_key_and_row_key() {
        let row = sample_row();
        let rows = vec![row.clone()];

        assert_eq!(
            row_for_terminal_task_label(&rows, row.dedupe_key.as_ref())
                .expect("dedupe key should match")
                .row_key,
            row.row_key
        );
        assert_eq!(
            row_for_terminal_task_label(&rows, row.row_key.as_ref())
                .expect("row key should match")
                .dedupe_key,
            row.dedupe_key
        );
        assert!(row_for_terminal_task_label(&rows, "missing").is_none());
    }

    #[test]
    fn row_for_terminal_task_label_matches_vitermux_prefixed_aliases() {
        let row = sample_row();
        let rows = vec![row.clone()];
        let prefixed_row_key = format!("vitermux:{}", row.row_key);
        let prefixed_dedupe_key = format!("vitermux:{}", row.dedupe_key);

        assert_eq!(
            row_for_terminal_task_label(&rows, &prefixed_dedupe_key)
                .expect("prefixed dedupe key should match")
                .row_key,
            row.row_key
        );
        assert_eq!(
            row_for_terminal_task_label(&rows, &prefixed_row_key)
                .expect("prefixed row key should match")
                .dedupe_key,
            row.dedupe_key
        );
    }

    #[test]
    fn latest_open_request_tracker_replaces_older_requests() {
        let tracker = Arc::new(Mutex::new(OpenRequestTracker::default()));

        let request_a = begin_open_request(&tracker);
        assert!(is_latest_open_request(&tracker, request_a));

        let request_b = begin_open_request(&tracker);
        assert!(!is_latest_open_request(&tracker, request_a));
        assert!(is_latest_open_request(&tracker, request_b));

        let request_a_retry = begin_open_request(&tracker);
        assert!(!is_latest_open_request(&tracker, request_b));
        assert!(is_latest_open_request(&tracker, request_a_retry));
    }

    #[test]
    fn seed_session_slots_uses_first_trackable_rows_in_order() {
        let rows = vec![
            WorkbenchRow {
                row_key: SharedString::from("untracked"),
                session_key: None,
                ..sample_row()
            },
            WorkbenchRow {
                row_key: SharedString::from("row-1"),
                session_key: Some(SharedString::from("session-1")),
                dedupe_key: SharedString::from("dedupe-1"),
                ..sample_row()
            },
            WorkbenchRow {
                row_key: SharedString::from("row-2"),
                session_key: Some(SharedString::from("session-2")),
                dedupe_key: SharedString::from("dedupe-2"),
                ..sample_row()
            },
        ];

        let slots = seed_session_slots(&rows);

        assert_eq!(slots.len(), SESSION_SLOT_COUNT);
        assert_eq!(
            slots[0].as_ref().map(|slot| (
                slot.terminal_key.as_str(),
                slot.session_key.as_str(),
                slot.dedupe_key.as_str()
            )),
            Some(("row-1", "session-1", "dedupe-1"))
        );
        assert_eq!(
            slots[1].as_ref().map(|slot| (
                slot.terminal_key.as_str(),
                slot.session_key.as_str(),
                slot.dedupe_key.as_str()
            )),
            Some(("row-2", "session-2", "dedupe-2"))
        );
        assert!(slots[2..].iter().all(Option::is_none));
    }

    #[test]
    fn row_for_session_slot_prefers_terminal_key_then_session_key_then_dedupe_key() {
        let rows = vec![
            WorkbenchRow {
                row_key: SharedString::from("row-1"),
                session_key: Some(SharedString::from("session-shared")),
                dedupe_key: SharedString::from("dedupe-1"),
                ..sample_row()
            },
            WorkbenchRow {
                row_key: SharedString::from("row-2"),
                session_key: Some(SharedString::from("session-shared")),
                dedupe_key: SharedString::from("dedupe-2"),
                ..sample_row()
            },
            WorkbenchRow {
                row_key: SharedString::from("row-3"),
                session_key: Some(SharedString::from("session-3")),
                dedupe_key: SharedString::from("dedupe-3"),
                ..sample_row()
            },
        ];
        let row_index_by_session_key = build_row_index_by_session_key(&rows);
        let row_index_by_terminal_key = build_row_index_by_terminal_key(&rows);

        let prefers_terminal_key = row_for_session_slot(
            &SessionSlotAssignment {
                terminal_key: "row-1".into(),
                session_key: "session-shared".into(),
                dedupe_key: "dedupe-2".into(),
            },
            &rows,
            &row_index_by_session_key,
            &row_index_by_terminal_key,
        )
        .expect("slot should resolve by terminal key");
        assert_eq!(prefers_terminal_key.row_key.as_ref(), "row-1");

        let falls_back_to_session_key = row_for_session_slot(
            &SessionSlotAssignment {
                terminal_key: "missing".into(),
                session_key: "session-3".into(),
                dedupe_key: "dedupe-missing".into(),
            },
            &rows,
            &row_index_by_session_key,
            &row_index_by_terminal_key,
        )
        .expect("slot should resolve by session key");
        assert_eq!(falls_back_to_session_key.row_key.as_ref(), "row-3");

        let falls_back_to_dedupe = row_for_session_slot(
            &SessionSlotAssignment {
                terminal_key: "missing".into(),
                session_key: "session-missing".into(),
                dedupe_key: "dedupe-2".into(),
            },
            &rows,
            &row_index_by_session_key,
            &row_index_by_terminal_key,
        )
        .expect("slot should resolve by dedupe key");
        assert_eq!(falls_back_to_dedupe.row_key.as_ref(), "row-2");
    }

    #[test]
    fn build_assigned_slot_by_row_key_marks_only_visible_slot_rows() {
        let rows = vec![
            WorkbenchRow {
                row_key: SharedString::from("row-1"),
                session_key: Some(SharedString::from("session-1")),
                dedupe_key: SharedString::from("dedupe-1"),
                ..sample_row()
            },
            WorkbenchRow {
                row_key: SharedString::from("row-2"),
                session_key: Some(SharedString::from("session-2")),
                dedupe_key: SharedString::from("dedupe-2"),
                ..sample_row()
            },
        ];
        let row_index_by_session_key = build_row_index_by_session_key(&rows);
        let row_index_by_terminal_key = build_row_index_by_terminal_key(&rows);
        let slots = vec![
            Some(SessionSlotAssignment {
                terminal_key: "row-2".into(),
                session_key: "session-2".into(),
                dedupe_key: "dedupe-2".into(),
            }),
            Some(SessionSlotAssignment {
                terminal_key: "row-missing".into(),
                session_key: "session-missing".into(),
                dedupe_key: "dedupe-missing".into(),
            }),
        ];

        let assigned = build_assigned_slot_by_row_key(
            &slots,
            &rows,
            &row_index_by_session_key,
            &row_index_by_terminal_key,
        );

        assert_eq!(assigned.get("row-2"), Some(&0));
        assert!(!assigned.contains_key("row-1"));
    }

    #[test]
    fn default_keymap_places_operator_workspace_slot_bindings_after_workspace_pane_bindings() {
        let keymap = include_str!("../../../assets/keymaps/default-macos.json");
        let pane_bindings = keymap
            .find(r#""cmd-9": ["workspace::ActivatePane", 8]"#)
            .expect("workspace pane bindings should exist");
        let operator_bindings = keymap
            .find(r#""context": "Workspace && VitermuxOperatorWorkspace""#)
            .expect("operator workspace binding block should exist");

        assert!(
            operator_bindings > pane_bindings,
            "operator workspace slot bindings must come after plain workspace pane bindings"
        );
    }

    #[test]
    fn default_keymap_binds_review_companion_toggle_in_vitermux_contexts() {
        let keymap = include_str!("../../../assets/keymaps/default-macos.json");
        assert!(
            keymap.contains(r#""context": "VitermuxPanel || (Terminal && vitermux_terminal)""#)
                && keymap.contains(r#""alt-cmd-/": "vitermux_panel::ToggleReviewCompanion""#),
            "vitermux panel and terminal contexts should bind the review companion toggle"
        );
        assert!(
            keymap.contains(r#""context": "Workspace && VitermuxOperatorWorkspace""#)
                && keymap.contains(r#""alt-cmd-/": "vitermux_panel::ToggleReviewCompanion""#),
            "operator workspace context should also bind the review companion toggle"
        );
    }

    #[test]
    fn default_linux_keymap_places_operator_workspace_slot_bindings_after_workspace_pane_bindings()
    {
        let keymap = include_str!("../../../assets/keymaps/default-linux.json");
        let pane_bindings = keymap
            .find(r#""alt-9": ["workspace::ActivatePane", 8]"#)
            .expect("workspace pane bindings should exist");
        let operator_bindings = keymap
            .find(r#""context": "Workspace && VitermuxOperatorWorkspace""#)
            .expect("operator workspace binding block should exist");

        assert!(
            operator_bindings > pane_bindings,
            "operator workspace slot bindings must come after plain workspace pane bindings"
        );
    }

    #[test]
    fn default_linux_keymap_binds_review_companion_toggle_in_vitermux_contexts() {
        let keymap = include_str!("../../../assets/keymaps/default-linux.json");
        assert!(
            keymap.contains(r#""context": "VitermuxPanel || (Terminal && vitermux_terminal)""#)
                && keymap.contains(r#""alt-cmd-/": "vitermux_panel::ToggleReviewCompanion""#),
            "vitermux panel and terminal contexts should bind the review companion toggle"
        );
        assert!(
            keymap.contains(r#""context": "Workspace && VitermuxOperatorWorkspace""#)
                && keymap.contains(r#""alt-cmd-/": "vitermux_panel::ToggleReviewCompanion""#),
            "operator workspace context should also bind the review companion toggle"
        );
    }

    #[test]
    fn default_keymap_binds_rename_selected_in_vitermux_panel_and_terminal() {
        let keymap = include_str!("../../../assets/keymaps/default-macos.json");
        assert!(
            keymap.contains(
                r#"  {
    "context": "VitermuxPanel || (Terminal && vitermux_terminal)",
    "bindings": {
      "alt-cmd-/": "vitermux_panel::ToggleReviewCompanion",
      "cmd-shift-r": "vitermux_panel::RenameSelected","#,
            ),
            "vitermux panel and terminal contexts should expose a first-class rename shortcut"
        );
    }

    #[test]
    fn default_linux_keymap_binds_rename_selected_in_vitermux_panel_and_terminal() {
        let keymap = include_str!("../../../assets/keymaps/default-linux.json");
        assert!(
            keymap.contains(
                r#"  {
    "context": "VitermuxPanel || (Terminal && vitermux_terminal)",
    "bindings": {
      "alt-cmd-/": "vitermux_panel::ToggleReviewCompanion",
      "cmd-shift-r": "vitermux_panel::RenameSelected","#,
            ),
            "linux vitermux panel and terminal contexts should expose a first-class rename shortcut"
        );
    }

    #[test]
    fn attention_color_only_warns_for_canonical_new_review_or_urgent() {
        assert_eq!(attention_color("new_review"), Color::Warning);
        assert_eq!(attention_color("urgent"), Color::Warning);
        assert_eq!(attention_color("watching"), Color::Muted);
        assert_eq!(attention_color("blocked"), Color::Muted);
        assert_eq!(attention_color("working"), Color::Accent);
    }

    #[test]
    fn build_spawn_task_uses_raw_argv_for_remote_attach() {
        let row = sample_row();
        let plan = ZedOpenPlan {
            project: vitermux::ZedProjectTarget {
                mode: "remote".into(),
                remote_path: "/home/albertus/dev/agent-hud".into(),
                ..Default::default()
            },
            attach: vitermux::ZedAttachSpec {
                mode: "ssh_shell".into(),
                argv: vec![
                    "ssh".into(),
                    "-t".into(),
                    "albertus@poros".into(),
                    "tmux attach-session -t main \\; select-window -t main:2".into(),
                ],
                cwd: "/home/albertus/dev/agent-hud".into(),
                dedupe_key: "node:poros/acct:albertus@poros/sess:main/win:2".into(),
                ..Default::default()
            },
            ..sample_plan()
        };

        let task =
            build_spawn_task(&row, &plan, false, false).expect("remote argv attach should build");
        assert_eq!(
            task.id,
            TaskId("node:poros/acct:albertus@poros/sess:main/win:2".into())
        );
        assert_eq!(task.cwd, None);
        assert_eq!(task.command, None);
        assert_eq!(task.args, Vec::<String>::new());
        assert_eq!(
            task.shell,
            Shell::WithArguments {
                program: "ssh".into(),
                args: vec![
                    "-t".into(),
                    "albertus@poros".into(),
                    "tmux attach-session -t main \\; select-window -t main:2".into(),
                ],
                title_override: None,
            }
        );
    }

    #[test]
    fn build_spawn_task_rejects_remote_attach_without_argv() {
        let row = sample_row();
        let plan = ZedOpenPlan {
            project: vitermux::ZedProjectTarget {
                mode: "remote".into(),
                ..Default::default()
            },
            attach: vitermux::ZedAttachSpec {
                mode: "ssh_shell".into(),
                command: "tmux attach-session -t main".into(),
                ..Default::default()
            },
            ..sample_plan()
        };

        let error = build_spawn_task(&row, &plan, false, false)
            .expect_err("remote attach should require argv");
        assert!(
            error
                .to_string()
                .contains("daemon SSH attach plan did not include argv")
        );
    }

    #[test]
    fn build_spawn_task_falls_back_to_local_shell_script() {
        let row = sample_row();
        let plan = ZedOpenPlan {
            project: vitermux::ZedProjectTarget {
                mode: "local".into(),
                remote_path: "/tmp/frontend-logging-cleanup".into(),
                ..Default::default()
            },
            attach: vitermux::ZedAttachSpec {
                mode: "local_shell".into(),
                cwd: "/tmp/frontend-logging-cleanup".into(),
                dedupe_key: "node:local/acct:me/sess:main/win:2".into(),
                ..Default::default()
            },
            target_ref: vitermux::TargetRef {
                tmux_session: "main".into(),
                tmux_window: "2".into(),
                ..Default::default()
            },
            ..sample_plan()
        };

        let task =
            build_spawn_task(&row, &plan, false, false).expect("local fallback should build");
        assert_eq!(
            task.cwd,
            Some(PathBuf::from("/tmp/frontend-logging-cleanup"))
        );
        assert_eq!(task.command, None);
        assert_eq!(
            task.shell,
            Shell::WithArguments {
                program: "/bin/sh".into(),
                args: vec![
                    "-lc".into(),
                    "if tmux has-session -t 'main' 2>/dev/null; then exec tmux attach-session -t 'main' \\; select-window -t 'main:2'; else cd -- '/tmp/frontend-logging-cleanup' && exec ${SHELL:-zsh} -l; fi".into(),
                ],
                title_override: None,
            }
        );
    }

    #[test]
    fn build_spawn_task_uses_remote_tmux_attach_inside_remote_workspace() {
        let row = sample_row();
        let plan = ZedOpenPlan {
            project: vitermux::ZedProjectTarget {
                mode: "remote".into(),
                remote_path: "/home/albertus/dev/agent-hud".into(),
                ..Default::default()
            },
            attach: vitermux::ZedAttachSpec {
                mode: "ssh_shell".into(),
                argv: vec![
                    "ssh".into(),
                    "-t".into(),
                    "albertus@poros".into(),
                    "tmux attach-session -t main \\; select-window -t main:2".into(),
                ],
                cwd: "/home/albertus/dev/agent-hud".into(),
                dedupe_key: "node:poros/acct:albertus@poros/sess:main/win:2".into(),
                ..Default::default()
            },
            target_ref: vitermux::TargetRef {
                tmux_session: "main".into(),
                tmux_window: "2".into(),
                ..Default::default()
            },
            ..sample_plan()
        };

        let task = build_spawn_task(&row, &plan, true, true)
            .expect("remote workspace attach should build");
        assert_eq!(task.cwd, None);
        assert_eq!(task.command, None);
        assert_eq!(
            task.shell,
            Shell::WithArguments {
                program: "/bin/sh".into(),
                args: vec![
                    "-lc".into(),
                    "if tmux has-session -t 'main' 2>/dev/null; then exec tmux attach-session -t 'main' \\; select-window -t 'main:2'; else cd -- '/home/albertus/dev/agent-hud' && exec ${SHELL:-zsh} -l; fi".into(),
                ],
                title_override: None,
            }
        );
    }

    #[test]
    fn build_spawn_task_suppresses_local_cwd_when_source_workspace_is_remote() {
        let row = sample_row();
        let plan = ZedOpenPlan {
            project: vitermux::ZedProjectTarget {
                mode: "local".into(),
                remote_path: "/tmp/frontend-logging-cleanup".into(),
                ..Default::default()
            },
            attach: vitermux::ZedAttachSpec {
                mode: "local_shell".into(),
                cwd: "/tmp/frontend-logging-cleanup".into(),
                dedupe_key: "node:local/acct:me/sess:main/win:2".into(),
                ..Default::default()
            },
            target_ref: vitermux::TargetRef {
                tmux_session: "main".into(),
                tmux_window: "2".into(),
                ..Default::default()
            },
            ..sample_plan()
        };

        let task = build_spawn_task(&row, &plan, false, true)
            .expect("fallback attach in a remote source workspace should build");
        assert_eq!(task.cwd, None);
    }

    #[test]
    fn remote_attach_uses_bare_tmux_only_for_matched_target_remote_workspace() {
        let plan = ZedOpenPlan {
            project: vitermux::ZedProjectTarget {
                mode: "remote".into(),
                remote_path: "/home/albertus/dev/agent-hud".into(),
                ..Default::default()
            },
            ..sample_plan()
        };

        assert!(attach_inside_target_remote_workspace(&plan, true, true));
        assert!(!attach_inside_target_remote_workspace(&plan, false, true));
        assert!(!attach_inside_target_remote_workspace(&plan, true, false));
    }

    #[test]
    fn local_attach_never_uses_remote_workspace_tmux_shortcut() {
        let plan = ZedOpenPlan {
            project: vitermux::ZedProjectTarget {
                mode: "local".into(),
                remote_path: "/tmp/frontend-logging-cleanup".into(),
                ..Default::default()
            },
            attach: vitermux::ZedAttachSpec {
                mode: "local_shell".into(),
                cwd: "/tmp/frontend-logging-cleanup".into(),
                ..Default::default()
            },
            ..sample_plan()
        };

        assert!(!attach_inside_target_remote_workspace(&plan, true, true));
    }

    #[test]
    fn source_workspace_attach_fallback_is_only_allowed_from_local_workspace() {
        assert!(allow_source_workspace_attach_fallback(false));
        assert!(!allow_source_workspace_attach_fallback(true));
    }

    #[gpui::test]
    async fn deploy_review_companion_prefers_terminal_key_over_stale_row_identity(
        cx: &mut TestAppContext,
    ) {
        cx.executor().allow_parking();
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            path!("/project_a"),
            json!({
                ".git": {},
                "a.txt": "CHANGED_A\n",
            }),
        )
        .await;
        fs.set_head_and_index_for_repo(
            Path::new(path!("/project_a/.git")),
            &[("a.txt", "original_a\n".to_string())],
        );

        let project = Project::test(fs, [path!("/project_a").as_ref()], cx).await;
        let worktree_id = project.read_with(cx, |project, cx| {
            project
                .worktrees(cx)
                .next()
                .expect("worktree should exist")
                .read(cx)
                .id()
        });

        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());

        let spawn_task = SpawnInTerminal {
            id: TaskId("actual-terminal-key".into()),
            full_label: "actual-terminal-key".into(),
            label: "zed-pristine-a".into(),
            reveal: RevealStrategy::Always,
            reveal_target: RevealTarget::Center,
            hide: HideStrategy::Never,
            ..SpawnInTerminal::default()
        };

        cx.update(|window, cx| {
            workspace.update(cx, |workspace, cx| {
                TerminalPanel::add_center_terminal(workspace, window, cx, {
                    let spawn_task = spawn_task.clone();
                    move |project, cx| project.create_terminal_task(spawn_task, cx)
                })
            })
        })
        .await
        .expect("terminal creation should succeed");
        cx.run_until_parked();

        let row = WorkbenchRow {
            row_key: SharedString::from("stale-row-key"),
            dedupe_key: SharedString::from("stale-dedupe-key"),
            ..sample_row()
        };
        let project_path: ProjectPath = (worktree_id, rel_path("a.txt")).into();

        cx.update(|window, cx| {
            workspace.update(cx, |workspace, cx| {
                deploy_review_companion_for_terminal_key(
                    workspace,
                    &row,
                    project_path,
                    "actual-terminal-key",
                    false,
                    window,
                    cx,
                );
            })
        });
        cx.run_until_parked();

        let (pane_count, has_review_diff) = workspace.read_with(cx, |workspace, cx| {
            let panes = workspace.panes();
            let has_review_diff = panes
                .iter()
                .find(|pane| pane_has_review_diff(pane, cx))
                .and_then(|pane| pane.read(cx).items_of_type::<ProjectDiff>().next())
                .is_some();
            (panes.len(), has_review_diff)
        });

        assert_eq!(pane_count, 2, "review companion should create a right pane");
        assert!(has_review_diff, "review diff should be present");
    }

    #[gpui::test]
    async fn cmd_number_switches_between_existing_vitermux_terminals(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(path!("/project_a"), json!({ ".git": {} }))
            .await;
        let project = Project::test(fs, [path!("/project_a").as_ref()], cx).await;

        let (multi_workspace, window) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = multi_workspace.read_with(window, |mw, _| mw.workspace().clone());

        let persistence_key =
            workspace.read_with(window, |workspace, _| workspace_persistence_key(workspace));
        let panel = window.update(|window, cx| {
            cx.new(|cx| {
                VitermuxPanel::new(
                    workspace.downgrade(),
                    persistence_key.clone(),
                    false,
                    true,
                    empty_session_slots(),
                    HashSet::default(),
                    window,
                    cx,
                )
            })
        });
        window.update(|window, cx| {
            workspace.update(cx, |workspace, cx| {
                workspace.add_panel(panel.clone(), window, cx);
                workspace.toggle_panel_focus::<VitermuxPanel>(window, cx);
            })
        });
        window.run_until_parked();

        let row_a = WorkbenchRow {
            row_key: SharedString::from("node:local/acct:me/sess:zed-tabs/win:index:1"),
            session_key: Some(SharedString::from("codex:local:me@macbook:slot-a")),
            dedupe_key: SharedString::from("node:local/acct:me/sess:zed-tabs/win:index:1"),
            window_label: SharedString::from("vitermux-zed-pristine-a"),
            ..sample_row()
        };
        let row_b = WorkbenchRow {
            row_key: SharedString::from("node:local/acct:me/sess:zed-tabs/win:index:2"),
            session_key: Some(SharedString::from("codex:local:me@macbook:slot-b")),
            dedupe_key: SharedString::from("node:local/acct:me/sess:zed-tabs/win:index:2"),
            window_label: SharedString::from("vitermux-zed-pristine-b"),
            ..sample_row()
        };

        window.update(|_, cx| {
            cx.bind_keys([
                KeyBinding::new(
                    "cmd-1",
                    ActivateSlot(0),
                    Some("Terminal && vitermux_terminal"),
                ),
                KeyBinding::new(
                    "cmd-2",
                    ActivateSlot(1),
                    Some("Terminal && vitermux_terminal"),
                ),
                KeyBinding::new(
                    "cmd-1",
                    ActivateSlot(0),
                    Some("Workspace && VitermuxOperatorWorkspace"),
                ),
                KeyBinding::new(
                    "cmd-2",
                    ActivateSlot(1),
                    Some("Workspace && VitermuxOperatorWorkspace"),
                ),
            ]);
        });

        let spawn_task_a = SpawnInTerminal {
            id: TaskId(row_a.dedupe_key.to_string()),
            full_label: row_a.dedupe_key.to_string(),
            label: row_a.window_label.to_string(),
            reveal: RevealStrategy::Always,
            reveal_target: RevealTarget::Center,
            hide: HideStrategy::Never,
            ..SpawnInTerminal::default()
        };
        let spawn_task_b = SpawnInTerminal {
            id: TaskId(row_b.dedupe_key.to_string()),
            full_label: row_b.dedupe_key.to_string(),
            label: row_b.window_label.to_string(),
            reveal: RevealStrategy::Always,
            reveal_target: RevealTarget::Center,
            hide: HideStrategy::Never,
            ..SpawnInTerminal::default()
        };

        for spawn_task in [spawn_task_a.clone(), spawn_task_b.clone()] {
            window
                .update(|window, cx| {
                    workspace.update(cx, |workspace, cx| {
                        TerminalPanel::add_center_terminal(workspace, window, cx, {
                            let spawn_task = spawn_task.clone();
                            move |project, cx| project.create_terminal_task(spawn_task, cx)
                        })
                    })
                })
                .await
                .expect("terminal creation should succeed");
            window.run_until_parked();
        }

        panel.update(window, |panel, cx| {
            panel.rows = vec![row_a.clone(), row_b.clone()];
            panel.display_rows = build_display_rows(&panel.rows, &HashSet::default());
            panel.selected_row_key = Some(row_a.row_key.clone());
            panel.row_index_by_session_key = build_row_index_by_session_key(&panel.rows);
            panel.row_index_by_terminal_key = build_row_index_by_terminal_key(&panel.rows);
            panel.operator_workspace_enabled = false;
            panel.session_slots = normalize_session_slots(&[
                Some(SessionSlotAssignment {
                    terminal_key: row_a.row_key.to_string(),
                    session_key: row_a
                        .session_key
                        .as_ref()
                        .expect("row a should have a session key")
                        .to_string(),
                    dedupe_key: row_a.dedupe_key.to_string(),
                }),
                Some(SessionSlotAssignment {
                    terminal_key: row_b.row_key.to_string(),
                    session_key: row_b
                        .session_key
                        .as_ref()
                        .expect("row b should have a session key")
                        .to_string(),
                    dedupe_key: row_b.dedupe_key.to_string(),
                }),
            ]);
            panel.assigned_slot_by_row_key = build_assigned_slot_by_row_key(
                &panel.session_slots,
                &panel.rows,
                &panel.row_index_by_session_key,
                &panel.row_index_by_terminal_key,
            );
            cx.notify();
        });
        window.run_until_parked();

        window.update(|window, cx| {
            workspace.update(cx, |workspace, cx| {
                let terminal_panel = workspace.panel::<TerminalPanel>(cx);
                assert!(focus_existing_terminal(
                    workspace,
                    terminal_panel,
                    row_a.dedupe_key.as_ref(),
                    window,
                    cx,
                ));
            })
        });
        window.run_until_parked();

        let before = workspace.read_with(window, |workspace, cx| {
            focused_terminal_task_label(workspace, cx)
        });
        assert_eq!(before.as_deref(), Some(row_a.dedupe_key.as_ref()));

        panel.update_in(window, |panel, window, cx| {
            panel.sync_to_focused_terminal(workspace.clone(), window, cx);
        });
        window.run_until_parked();

        window.simulate_keystrokes("cmd-2");

        let after = workspace.read_with(window, |workspace, cx| {
            focused_terminal_task_label(workspace, cx)
        });
        assert_eq!(after.as_deref(), Some(row_b.dedupe_key.as_ref()));

        let selected_row_key = panel.read_with(window, |panel, _| panel.selected_row_key.clone());
        assert_eq!(selected_row_key.as_deref(), Some(row_b.row_key.as_ref()));
        let operator_workspace_enabled =
            panel.read_with(window, |panel, _| panel.operator_workspace_enabled);
        assert!(operator_workspace_enabled);
    }

    #[gpui::test]
    async fn operator_window_focus_reuses_existing_vitermux_terminal_across_workspaces(
        cx: &mut TestAppContext,
    ) {
        cx.executor().allow_parking();
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(path!("/project_a"), json!({ ".git": {} }))
            .await;
        fs.insert_tree(path!("/project_b"), json!({ ".git": {} }))
            .await;
        let project_a = Project::test(fs.clone(), [path!("/project_a").as_ref()], cx).await;
        let project_b = Project::test(fs, [path!("/project_b").as_ref()], cx).await;

        let (multi_workspace, window) = cx
            .add_window_view(|window, cx| MultiWorkspace::test_new(project_a.clone(), window, cx));
        let workspace_a = multi_workspace.read_with(window, |mw, _| mw.workspace().clone());
        multi_workspace.update_in(window, |mw, window, cx| {
            mw.add(workspace_a.clone(), window, cx);
        });
        let workspace_b = window.update(|window, cx| {
            let workspace_b = cx.new(|cx| Workspace::test_new(project_b.clone(), window, cx));
            multi_workspace.update(cx, |mw, cx| {
                mw.add(workspace_b.clone(), window, cx);
                mw.activate(workspace_b.clone(), None, window, cx);
            });
            workspace_b
        });

        let spawn_task_a = SpawnInTerminal {
            id: TaskId("node:local/acct:me/sess:zed-tabs/win:index:1".into()),
            full_label: "node:local/acct:me/sess:zed-tabs/win:index:1".into(),
            label: "zed-pristine-a".into(),
            reveal: RevealStrategy::Always,
            reveal_target: RevealTarget::Center,
            hide: HideStrategy::Never,
            ..SpawnInTerminal::default()
        };
        let spawn_task_b = SpawnInTerminal {
            id: TaskId("node:local/acct:me/sess:zed-tabs/win:index:2".into()),
            full_label: "node:local/acct:me/sess:zed-tabs/win:index:2".into(),
            label: "zed-pristine-b".into(),
            reveal: RevealStrategy::Always,
            reveal_target: RevealTarget::Center,
            hide: HideStrategy::Never,
            ..SpawnInTerminal::default()
        };

        window
            .update(|window, cx| {
                multi_workspace.update(cx, |mw, cx| {
                    mw.activate(workspace_a.clone(), None, window, cx);
                    workspace_a.update(cx, |workspace, cx| {
                        TerminalPanel::add_center_terminal(workspace, window, cx, {
                            let spawn_task = spawn_task_a.clone();
                            move |project, cx| project.create_terminal_task(spawn_task, cx)
                        })
                    })
                })
            })
            .await
            .expect("workspace A terminal creation should succeed");
        window.run_until_parked();

        window
            .update(|window, cx| {
                multi_workspace.update(cx, |mw, cx| {
                    mw.activate(workspace_b.clone(), None, window, cx);
                    workspace_b.update(cx, |workspace, cx| {
                        TerminalPanel::add_center_terminal(workspace, window, cx, {
                            let spawn_task = spawn_task_b.clone();
                            move |project, cx| project.create_terminal_task(spawn_task, cx)
                        })
                    })
                })
            })
            .await
            .expect("workspace B terminal creation should succeed");
        window.run_until_parked();

        let workspace_a_terminal_count = workspace_a.read_with(window, |workspace, cx| {
            workspace
                .panes()
                .iter()
                .flat_map(|pane| pane.read(cx).items_of_type::<TerminalView>())
                .count()
        });
        let workspace_b_terminal_count = workspace_b.read_with(window, |workspace, cx| {
            workspace
                .panes()
                .iter()
                .flat_map(|pane| pane.read(cx).items_of_type::<TerminalView>())
                .count()
        });
        assert_eq!(
            workspace_a_terminal_count, 1,
            "workspace A should retain one terminal"
        );
        assert_eq!(
            workspace_b_terminal_count, 1,
            "workspace B should retain one terminal"
        );

        let retained_workspace_count =
            multi_workspace.read_with(window, |mw, _| mw.workspaces().count());
        assert_eq!(
            retained_workspace_count, 2,
            "operator window should retain both workspaces"
        );

        let direct_reuse = multi_workspace.update_in(window, |mw, window, cx| {
            mw.activate(workspace_a.clone(), None, window, cx);
            workspace_a.update(cx, |workspace, cx| {
                let terminal_panel = workspace.panel::<TerminalPanel>(cx);
                focus_existing_terminal(
                    workspace,
                    terminal_panel,
                    spawn_task_a.full_label.as_str(),
                    window,
                    cx,
                )
            })
        });
        assert!(
            direct_reuse,
            "workspace-local focus should find the retained terminal"
        );

        multi_workspace.update_in(window, |mw, window, cx| {
            mw.activate(workspace_b.clone(), None, window, cx);
        });
        window.run_until_parked();

        let finds_workspace_a_after_activation =
            multi_workspace.update_in(window, |mw, window, cx| {
                mw.activate(workspace_a.clone(), None, window, cx);
                workspace_a.update(cx, |workspace, cx| {
                    let terminal_panel = workspace.panel::<TerminalPanel>(cx);
                    find_existing_terminal_target(
                        workspace,
                        terminal_panel,
                        spawn_task_a.full_label.as_str(),
                        cx,
                    )
                    .is_some()
                })
            });
        assert!(
            finds_workspace_a_after_activation,
            "workspace A should expose the retained terminal once activated"
        );

        multi_workspace.update_in(window, |mw, window, cx| {
            mw.activate(workspace_b.clone(), None, window, cx);
        });
        window.run_until_parked();

        let reused_workspace = multi_workspace.update_in(window, |mw, window, cx| {
            focus_existing_terminal_in_multi_workspace(
                mw,
                spawn_task_a.full_label.as_str(),
                window,
                cx,
            )
        });
        assert!(
            reused_workspace.is_some(),
            "focus should reuse an existing terminal across workspaces"
        );
        assert_eq!(
            reused_workspace.as_ref().map(Entity::entity_id),
            Some(workspace_a.entity_id())
        );
        window.run_until_parked();

        let active_workspace_id =
            multi_workspace.read_with(window, |mw, _| mw.workspace().entity_id());
        assert_eq!(active_workspace_id, workspace_a.entity_id());

        let focused_label = multi_workspace.read_with(window, |mw, cx| {
            focused_terminal_task_label(mw.workspace().read(cx), cx)
        });
        assert_eq!(
            focused_label.as_deref(),
            Some(spawn_task_a.full_label.as_str())
        );
        assert_eq!(
            workspace_a_terminal_count, 1,
            "reusing a focused tmux tab should not create a second terminal item"
        );
    }

    #[test]
    fn rename_selected_requires_a_selected_session_row() {
        let row = WorkbenchRow {
            session_key: None,
            ..sample_row()
        };

        assert_eq!(rename_target_for_row(&row), None);
    }

    #[test]
    fn rename_selected_targets_primary_agent_for_window_row() {
        let row = sample_row();

        let target = rename_target_for_row(&row).expect("sample row should be renameable");

        assert_eq!(target.session_key, "codex:poros:albertus@poros:123");
        assert_eq!(target.current_name, "frontend-logging-cleanup");
        assert_eq!(target.row_key, "row-key");
    }

    #[test]
    fn pending_label_overlays_apply_only_to_matching_row_key() {
        let session_key = SharedString::from("claude:macbook:acct:shared");
        let mut rows = vec![
            WorkbenchRow {
                row_key: SharedString::from("row-a"),
                session_key: Some(session_key.clone()),
                ..sample_row()
            },
            WorkbenchRow {
                row_key: SharedString::from("row-b"),
                session_key: Some(session_key),
                window_label: SharedString::from("other-window"),
                ..sample_row()
            },
        ];
        let mut pending_labels = HashMap::default();
        pending_labels.insert(
            "row-a".to_string(),
            PendingSessionLabel {
                label: SharedString::from("renamed-window"),
            },
        );

        apply_pending_label_overlays(&mut rows, &mut pending_labels);

        assert_eq!(rows[0].window_label.as_ref(), "renamed-window");
        assert_eq!(rows[1].window_label.as_ref(), "other-window");
        assert!(
            pending_labels.contains_key("row-a"),
            "overlay should remain pending until the daemon tree converges"
        );
    }

    #[test]
    fn pending_label_overlay_clears_once_matching_row_settles() {
        let mut rows = vec![WorkbenchRow {
            row_key: SharedString::from("row-a"),
            window_label: SharedString::from("renamed-window"),
            ..sample_row()
        }];
        let mut pending_labels = HashMap::default();
        pending_labels.insert(
            "row-a".to_string(),
            PendingSessionLabel {
                label: SharedString::from("renamed-window"),
            },
        );

        apply_pending_label_overlays(&mut rows, &mut pending_labels);

        assert!(
            pending_labels.is_empty(),
            "settled row should clear the pending overlay"
        );
    }

    #[test]
    fn window_label_prefers_primary_agent_name_over_workspace_label() {
        let window = TmuxWindow {
            binding: vitermux::WorktreeBinding {
                workspace_label: "workspace-label".into(),
                ..Default::default()
            },
            harness: vitermux::HarnessBinding {
                session_key: "claude:macbook:acct:primary".into(),
                ..Default::default()
            },
            agents: vec![vitermux::AgentBinding {
                session_key: "claude:macbook:acct:primary".into(),
                name: "explicit-display-name".into(),
                ..Default::default()
            }],
            window_name: "tmux-window-name".into(),
            ..Default::default()
        };

        assert_eq!(window_label(&window), "explicit-display-name");
    }

    #[test]
    fn window_label_falls_back_to_workspace_label_without_agent_name() {
        let window = TmuxWindow {
            binding: vitermux::WorktreeBinding {
                workspace_label: "workspace-label".into(),
                ..Default::default()
            },
            window_name: "tmux-window-name".into(),
            ..Default::default()
        };

        assert_eq!(window_label(&window), "workspace-label");
    }

    fn sample_row() -> WorkbenchRow {
        WorkbenchRow {
            row_key: SharedString::from("row-key"),
            node_section_key: SharedString::from("node-section"),
            node_label: SharedString::from("albertus@poros"),
            node_is_local: false,
            session_section_key: SharedString::from("main"),
            session_label: SharedString::from("main"),
            session_detail: SharedString::from("agent-hud"),
            window_label: SharedString::from("frontend-logging-cleanup"),
            window_detail: SharedString::from("codex  /tmp/frontend-logging-cleanup"),
            session_key: Some(SharedString::from("codex:poros:albertus@poros:123")),
            dedupe_key: SharedString::from("node:poros/acct:albertus@poros/sess:main/win:2"),
            attention: SharedString::from("new_review"),
        }
    }

    fn sample_plan() -> ZedOpenPlan {
        ZedOpenPlan {
            target_ref: vitermux::TargetRef {
                tmux_session: "main".into(),
                tmux_window: "2".into(),
                ..Default::default()
            },
            ..Default::default()
        }
    }

    fn init_test(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let store = SettingsStore::test(cx);
            cx.set_global(store);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            editor::init(cx);
            git_ui::init(cx);
            terminal_view::init(cx);
            vitermux::init(cx);
            crate::init(cx);
        });
    }
}
