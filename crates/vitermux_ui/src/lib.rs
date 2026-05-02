use anyhow::{Result, anyhow};
use collections::{HashMap, HashSet};
use db::kvp::KeyValueStore;
use git_ui::project_diff::ProjectDiff;
use gpui::{
    Action, AnyElement, App, AsyncWindowContext, Context, Entity, EventEmitter, FocusHandle,
    Focusable, ListAlignment, ListOffset, ListSizingBehavior, ListState, ParentElement, Pixels,
    Render, SharedString, StatefulInteractiveElement, Styled, Subscription, Task, WeakEntity,
    Window, WindowHandle, actions, list, px,
};
use menu::{Confirm, SelectFirst, SelectLast, SelectNext, SelectPrevious};
use parking_lot::Mutex;
use project::{ProjectPath, git_store::branch_diff::DiffBase};
use remote::{RemoteConnectionOptions, SshConnectionOptions, same_remote_connection_identity};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;
use task::{
    HideStrategy, RevealStrategy, RevealTarget, SaveStrategy, Shell, SpawnInTerminal, TaskId,
};
use terminal_view::{TerminalView, terminal_panel::TerminalPanel};
use ui::{
    Color, Disableable, Icon, IconButton, IconName, IconSize, Label, LabelSize, ListItem,
    ListItemSpacing, Toggleable, Tooltip, prelude::*,
};
use vitermux::{
    OpenPlanFailure, TmuxTreeSnapshot, TmuxWindow, VitermuxClient, VitermuxConnectionState,
    VitermuxStore, ZedOpenPlan,
};
use workspace::{
    MultiWorkspace, OpenMode, Pane, PathList, Toast, Workspace,
    dock::{DockPosition, Panel, PanelEvent},
    notifications::{DetachAndPromptErr, NotificationId},
};

const VITERMUX_PANEL_KEY: &str = "VitermuxPanel";
const PROJECT_REPOSITORY_DISCOVERY_ATTEMPTS: usize = 60;
const PROJECT_REPOSITORY_DISCOVERY_DELAY: Duration = Duration::from_millis(50);
const REVIEW_COMPANION_SYNC_ATTEMPTS: usize = 60;
const REVIEW_COMPANION_SYNC_DELAY: Duration = Duration::from_millis(50);
const SESSION_SLOT_COUNT: usize = 9;
const SESSION_SLOT_SCOPE_KEY: &str = "vitermux_session_slots";

actions!(
    vitermux_panel,
    [
        Toggle,
        ToggleFocus,
        Refresh,
        OpenSelected,
        OpenReviewSelected,
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
        workspace.register_action(|workspace, action: &ActivateSlot, window, cx| {
            if let Some(panel) = workspace.panel::<VitermuxPanel>(cx) {
                panel.update(cx, |panel, cx| panel.activate_slot(action, window, cx));
            }
        });
        workspace.register_action(|workspace, action: &AssignSelectedToSlot, window, cx| {
            if let Some(panel) = workspace.panel::<VitermuxPanel>(cx) {
                panel.update(cx, |panel, cx| panel.assign_selected_to_slot(action, window, cx));
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
    session_key: String,
    dedupe_key: String,
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

#[derive(Clone)]
enum DisplayRow {
    NodeHeader {
        row_key: SharedString,
        label: SharedString,
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
    store: Entity<VitermuxStore>,
    focus_handle: FocusHandle,
    list_state: ListState,
    pending_open_keys: Arc<Mutex<HashSet<String>>>,
    open_plan_cache: Arc<Mutex<HashMap<String, ZedOpenPlan>>>,
    rows: Vec<WorkbenchRow>,
    display_rows: Vec<DisplayRow>,
    row_index_by_key: HashMap<String, usize>,
    row_index_by_session_key: HashMap<String, usize>,
    row_index_by_terminal_key: HashMap<String, usize>,
    display_row_index_by_row_key: HashMap<String, usize>,
    assigned_slot_by_row_key: HashMap<String, usize>,
    session_slots: Vec<Option<SessionSlotAssignment>>,
    selected_row_key: Option<SharedString>,
    last_focused_terminal_key: Option<String>,
    review_companion_enabled: Arc<AtomicBool>,
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
            let session_slots = load_session_slots(persistence_key.as_deref(), cx)
                .unwrap_or_else(empty_session_slots);
            cx.new(|cx| {
                Self::new(
                    workspace_handle.clone(),
                    persistence_key.clone(),
                    session_slots.clone(),
                    window,
                    cx,
                )
            })
        })
    }

    fn new(
        workspace: WeakEntity<Workspace>,
        persistence_key: Option<String>,
        session_slots: Vec<Option<SessionSlotAssignment>>,
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
            store: store.clone(),
            focus_handle: cx.focus_handle(),
            list_state,
            pending_open_keys: Arc::default(),
            open_plan_cache: Arc::default(),
            rows: Vec::new(),
            display_rows: Vec::new(),
            row_index_by_key: HashMap::default(),
            row_index_by_session_key: HashMap::default(),
            row_index_by_terminal_key: HashMap::default(),
            display_row_index_by_row_key: HashMap::default(),
            assigned_slot_by_row_key: HashMap::default(),
            session_slots,
            selected_row_key: None,
            last_focused_terminal_key: None,
            review_companion_enabled: review_companion_enabled_flag(),
            active: false,
            width: px(336.0),
            _subscriptions: subscriptions,
        };
        this.refresh_view_model(cx);
        this.ensure_selection(cx);
        this.scroll_selection_into_view();
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
            .map(|snapshot| flatten_rows(&snapshot))
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
        self.display_rows = build_display_rows(&self.rows);
        self.display_row_index_by_row_key = build_display_row_index_by_row_key(&self.display_rows);
        prune_cached_open_plans(
            &self.open_plan_cache,
            &self.rows,
            &self.row_index_by_session_key,
            &self.row_index_by_terminal_key,
        );
        if seeded_slots {
            cx.notify();
        }

        if self.list_state.item_count() != self.display_rows.len() {
            self.list_state.reset(self.display_rows.len());
        } else {
            self.list_state.remeasure();
        }
    }

    fn ensure_selection(&mut self, _cx: &mut Context<Self>) {
        if self.rows.is_empty() {
            self.selected_row_key = None;
            return;
        }

        let selected_exists = self
            .selected_row_key
            .as_ref()
            .is_some_and(|selected| self.row_index_by_key.contains_key(selected.as_ref()));
        if !selected_exists {
            self.selected_row_key = self.rows.first().map(|row| row.row_key.clone());
        }
    }

    fn selected_row(&self) -> Option<WorkbenchRow> {
        let selected = self.selected_row_key.as_ref()?;
        self.row_index_by_key
            .get(selected.as_ref())
            .and_then(|index| self.rows.get(*index))
            .cloned()
    }

    fn selected_row_index(&self) -> Option<usize> {
        self.selected_row_key
            .as_ref()
            .and_then(|selected| self.row_index_by_key.get(selected.as_ref()).copied())
    }

    fn selected_display_row_index(&self) -> Option<usize> {
        self.selected_row_key.as_ref().and_then(|selected| {
            self.display_row_index_by_row_key
                .get(selected.as_ref())
                .copied()
        })
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

    fn activate_slot(
        &mut self,
        action: &ActivateSlot,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(slot_index) = normalize_slot_index(action.0) else {
            return;
        };
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

        self.selected_row_key = Some(row.row_key.clone());
        self.scroll_selection_into_view();
        cx.notify();
        self.open_row(row, window, cx);
    }

    fn assign_selected_to_slot(
        &mut self,
        action: &AssignSelectedToSlot,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(slot_index) = normalize_slot_index(action.0) else {
            return;
        };
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

    fn confirm(&mut self, _: &Confirm, window: &mut Window, cx: &mut Context<Self>) {
        self.open_selected(window, cx);
    }

    fn select_first(&mut self, _: &SelectFirst, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(row) = self.rows.first() {
            self.selected_row_key = Some(row.row_key.clone());
            self.scroll_selection_into_view();
            cx.notify();
        }
    }

    fn select_last(&mut self, _: &SelectLast, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(row) = self.rows.last() {
            self.selected_row_key = Some(row.row_key.clone());
            self.scroll_selection_into_view();
            cx.notify();
        }
    }

    fn select_next(&mut self, _: &SelectNext, window: &mut Window, cx: &mut Context<Self>) {
        if self.rows.is_empty() {
            self.selected_row_key = None;
            cx.notify();
            return;
        }

        let next_index = self
            .selected_row_index()
            .map(|selected_index| (selected_index + 1) % self.rows.len())
            .unwrap_or(0);
        self.selected_row_key = Some(self.rows[next_index].row_key.clone());
        self.scroll_selection_into_view();

        if !self.focus_handle.contains_focused(window, cx) {
            cx.focus_self(window);
        }
        cx.notify();
    }

    fn select_previous(&mut self, _: &SelectPrevious, window: &mut Window, cx: &mut Context<Self>) {
        if self.rows.is_empty() {
            self.selected_row_key = None;
            cx.notify();
            return;
        }

        let previous_index = self
            .selected_row_index()
            .map(|selected_index| {
                if selected_index == 0 {
                    self.rows.len() - 1
                } else {
                    selected_index - 1
                }
            })
            .unwrap_or(self.rows.len() - 1);
        self.selected_row_key = Some(self.rows[previous_index].row_key.clone());
        self.scroll_selection_into_view();

        if !self.focus_handle.contains_focused(window, cx) {
            cx.focus_self(window);
        }
        cx.notify();
    }

    fn select_and_open(&mut self, row: WorkbenchRow, window: &mut Window, cx: &mut Context<Self>) {
        self.selected_row_key = Some(row.row_key.clone());
        self.open_row(row, window, cx);
    }

    fn open_row(&mut self, row: WorkbenchRow, window: &mut Window, cx: &mut Context<Self>) {
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
        if row.session_key.is_none() {
            return;
        }

        self.review_companion_enabled.store(true, Ordering::Release);
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
                existing.session_key == assignment.session_key
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

        if self.selected_row_key.as_ref().map(|value| value.as_ref()) != Some(row.row_key.as_ref())
        {
            self.selected_row_key = Some(row.row_key.clone());
            self.scroll_selection_into_view();
            cx.notify();
        }

        if !self.review_companion_enabled.load(Ordering::Acquire) {
            return;
        }

        let has_companion_for_terminal = workspace.update(cx, |workspace, cx| {
            find_review_diff_pane_for_terminal(workspace, &terminal_pane, cx).is_some()
        });
        let same_terminal_key =
            self.last_focused_terminal_key.as_deref() == Some(terminal_key.as_str());
        self.last_focused_terminal_key = Some(terminal_key);

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
        if !self.pending_open_keys.lock().insert(open_key.clone()) {
            return Task::ready(Ok(()));
        }
        let pending_open_keys = self.pending_open_keys.clone();
        let open_plan_cache = self.open_plan_cache.clone();
        let requesting_window = window.window_handle().downcast::<MultiWorkspace>();
        let preferred_terminal_key = self.last_focused_terminal_key.clone();
        let review_companion_enabled = self.review_companion_enabled.load(Ordering::Acquire);
        let session_key = row
            .session_key
            .clone()
            .ok_or_else(|| anyhow!("missing vitermux session key"))
            .map(|id| id.to_string());

        window.spawn(cx, async move |cx| {
            let result = async {
                let workspace = workspace?;
                let session_key = session_key?;
                let terminal_panel =
                    workspace.read_with(cx, |workspace, cx| workspace.panel::<TerminalPanel>(cx));

                let did_focus_existing = workspace.update_in(cx, |workspace, window, cx| {
                    focus_existing_terminal(
                        workspace,
                        terminal_panel.clone(),
                        &open_key,
                        window,
                        cx,
                    )
                })?;
                if did_focus_existing {
                    if review_companion_enabled {
                        let plan = fetch_open_plan_with_cache(
                            &client,
                            &open_plan_cache,
                            &session_key,
                            &row,
                        )
                        .await?;
                        if let Err(error) = sync_review_companion_for_terminal_open(
                            workspace.clone(),
                            &row,
                            &plan,
                            &open_key,
                            cx,
                        )
                        .await
                        {
                            let _ = error;
                        }
                    }
                    return Ok(());
                }

                let plan =
                    fetch_open_plan_with_cache(&client, &open_plan_cache, &session_key, &row)
                        .await?;
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
                        let source_workspace_is_remote = workspace
                            .read_with(cx, |workspace, cx| {
                                workspace.project().read(cx).is_remote()
                            });
                        if !allow_source_workspace_attach_fallback(source_workspace_is_remote) {
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
                let workspace = resolved_workspace.workspace;
                if resolved_workspace.matched_project_context {
                    let _ = sync_workspace_project_context_for_plan(
                        workspace.clone(),
                        &plan,
                        ProjectContextSyncMode::BestEffort,
                        cx,
                    )
                    .await;
                }
                let workspace_is_remote = workspace
                    .read_with(cx, |workspace, cx| workspace.project().read(cx).is_remote());
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
                let did_focus_existing = workspace.update_in(cx, |workspace, window, cx| {
                    focus_existing_terminal(
                        workspace,
                        terminal_panel.clone(),
                        &terminal_key,
                        window,
                        cx,
                    )
                })?;
                if did_focus_existing {
                    if review_companion_enabled {
                        if let Err(error) = sync_review_companion_for_terminal_open(
                            workspace.clone(),
                            &row,
                            &plan,
                            &terminal_key,
                            cx,
                        )
                        .await
                        {
                            let _ = error;
                        }
                    }
                    return Ok(());
                }

                let preferred_pane = workspace.update_in(cx, |workspace, _window, cx| {
                    preferred_terminal_spawn_pane(workspace, preferred_terminal_key.as_deref(), cx)
                })?;
                let Some(terminal_panel) = terminal_panel else {
                    return Err(anyhow!("terminal panel is not available"));
                };
                let terminal_task = terminal_panel.update_in(cx, |panel, window, cx| {
                    panel.spawn_task_in_center_pane(&spawn_task, preferred_pane, window, cx)
                })?;
                terminal_task.await?;
                if review_companion_enabled {
                    if let Err(error) = sync_review_companion_for_terminal_open(
                        workspace.clone(),
                        &row,
                        &plan,
                        &terminal_key,
                        cx,
                    )
                    .await
                    {
                        let _ = error;
                    }
                }
                Ok(())
            }
            .await;

            pending_open_keys.lock().remove(&open_key);
            result
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
                        div()
                            .id("vitermux-refresh")
                            .cursor_pointer()
                            .on_click(cx.listener(|this, _, _, cx| this.refresh(cx)))
                            .tooltip(Tooltip::text("Refresh tmux tree"))
                            .child(Icon::new(IconName::ArrowCircle).color(Color::Muted)),
                    ),
            )
            .child(
                Label::new(state_text)
                    .size(LabelSize::XSmall)
                    .color(state_color),
            )
            .child(
                Label::new("Cmd+1..9 switch sessions  •  Option+Cmd+1..9 assign selected")
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
            DisplayRow::NodeHeader { row_key, label } => v_flex()
                .id(row_key)
                .pt_3()
                .px_3()
                .child(
                    Label::new(label)
                        .size(LabelSize::XSmall)
                        .color(Color::Accent),
                )
                .into_any_element(),
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
                let row_selected = self.selected_row_key.as_ref() == Some(&row.row_key);
                let disabled = row.session_key.is_none();
                let panel_focused = self.active && self.focus_handle.contains_focused(window, cx);
                let show_review = review_available(&row);
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
                                    Label::new(format!("⌘{}", slot_number(slot_index)))
                                        .size(LabelSize::XSmall)
                                        .color(if disabled { Color::Disabled } else { Color::Muted }),
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

struct MissingSessionToast;
struct SessionSlotToast;

fn flatten_rows(snapshot: &TmuxTreeSnapshot) -> Vec<WorkbenchRow> {
    let mut rows = Vec::new();

    for node in &snapshot.nodes {
        let node_label = first_non_empty([
            node.node_name.as_str(),
            node.node_id.as_str(),
            node.node_key.as_str(),
        ]);

        for account in &node.accounts {
            let account_label = first_non_empty([
                account.account_name.as_str(),
                account.account_user.as_str(),
                account.account_key.as_str(),
            ]);
            let node_section_label = if account.account_user.trim().is_empty()
                || account.account_user == account_label
            {
                node_label.to_string()
            } else {
                format!("{node_label} ({account_label})")
            };

            for session in &account.sessions {
                let session_label =
                    first_non_empty([session.session_name.as_str(), session.session_key.as_str()]);
                let session_detail = join_detail_parts([
                    non_empty_string(session.project_group.workspace_label.as_str()),
                    non_empty_string(session.project_group.codebase_label.as_str()),
                ]);

                for window in &session.windows {
                    rows.push(WorkbenchRow {
                        row_key: SharedString::from(window.stable_row_key().to_string()),
                        node_section_key: SharedString::from(format!(
                            "{}:{}",
                            node.node_key, account.account_key
                        )),
                        node_label: SharedString::from(node_section_label.clone()),
                        session_section_key: SharedString::from(session.session_key.clone()),
                        session_label: SharedString::from(session_label.to_string()),
                        session_detail: SharedString::from(session_detail.clone()),
                        window_label: SharedString::from(window_label(window)),
                        window_detail: SharedString::from(window_detail(window)),
                        session_key: window
                            .primary_session_key()
                            .filter(|session_key| !session_key.trim().is_empty())
                            .map(|session_key| SharedString::from(session_key.to_string())),
                        dedupe_key: SharedString::from(window.stable_row_key().to_string()),
                        attention: SharedString::from(window.attention.clone()),
                    });
                }
            }
        }
    }

    rows
}

fn build_display_rows(rows: &[WorkbenchRow]) -> Vec<DisplayRow> {
    let mut display_rows = Vec::new();
    let mut last_node_key: Option<&str> = None;
    let mut last_session_key: Option<&str> = None;

    for row in rows {
        if last_node_key != Some(row.node_section_key.as_ref()) {
            last_node_key = Some(row.node_section_key.as_ref());
            last_session_key = None;
            display_rows.push(DisplayRow::NodeHeader {
                row_key: SharedString::from(format!("node:{}", row.node_section_key.as_ref())),
                label: row.node_label.clone(),
            });
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

fn session_slot_state_version() -> u8 {
    1
}

fn empty_session_slots() -> Vec<Option<SessionSlotAssignment>> {
    vec![None; SESSION_SLOT_COUNT]
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
        index_by_terminal_key.insert(row.row_key.to_string(), index);
        index_by_terminal_key.insert(row.dedupe_key.to_string(), index);
    }
    index_by_terminal_key
}

fn build_display_row_index_by_row_key(display_rows: &[DisplayRow]) -> HashMap<String, usize> {
    display_rows
        .iter()
        .enumerate()
        .filter_map(|(index, row)| match row {
            DisplayRow::Window(row) => Some((row.row_key.to_string(), index)),
            DisplayRow::NodeHeader { .. } | DisplayRow::SessionHeader { .. } => None,
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
        let Some(session_key) = row.session_key.as_ref() else {
            continue;
        };
        session_slots[slot_index] = Some(SessionSlotAssignment {
            session_key: session_key.to_string(),
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
    row_index_by_session_key
        .get(slot.session_key.as_str())
        .and_then(|index| rows.get(*index))
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
            let row = row_for_session_slot(slot, rows, row_index_by_session_key, row_index_by_terminal_key)?;
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

fn cached_open_plan_matches_row(plan: &ZedOpenPlan, row: &WorkbenchRow) -> bool {
    let dedupe_key = plan.attach.dedupe_key.trim();
    dedupe_key.is_empty()
        || dedupe_key == row.dedupe_key.as_ref()
        || dedupe_key == row.row_key.as_ref()
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

    open_task.await
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
            if focused_terminal_task_label(workspace, cx).as_deref() != Some(terminal_key.as_str())
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
    if focused_terminal_task_label(workspace, cx).as_deref() != Some(terminal_key) {
        return;
    }

    let terminal_pane =
        terminal_pane_for_task_label(workspace, terminal_key, cx).or_else(|| {
            terminal_pane_for_row(workspace, row, cx)
        });
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
        if let Some(repository) =
            project_path_repository(workspace.clone(), project_path, cx)?
        {
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

    let target = panes.iter().find_map(|pane| {
        pane.read(cx).items().enumerate().find_map(|(ix, item)| {
            let terminal_view = item.act_as::<TerminalView>(cx)?;
            let task = terminal_view.read(cx).terminal().read(cx).task()?;
            if task.spawned_task.full_label == full_label {
                Some((pane.clone(), ix))
            } else {
                None
            }
        })
    });

    if let Some((pane, ix)) = target {
        pane.update(cx, |pane, cx| {
            pane.activate_item(ix, true, true, window, cx)
        });
        return true;
    }

    false
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
        .find(|row| row.dedupe_key.as_ref() == terminal_key || row.row_key.as_ref() == terminal_key)
        .cloned()
}

fn review_companion_enabled_flag() -> Arc<AtomicBool> {
    static FLAG: OnceLock<Arc<AtomicBool>> = OnceLock::new();
    FLAG.get_or_init(|| Arc::new(AtomicBool::new(false)))
        .clone()
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
                        .is_some_and(|task| task.spawned_task.full_label == terminal_key)
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
        window.binding.workspace_label.as_str(),
        window.window_name.as_str(),
        window.window_target.as_str(),
        window.window_id.as_str(),
        window.window_key.as_str(),
    ])
    .to_string()
}

fn window_detail(window: &TmuxWindow) -> String {
    join_detail_parts([
        non_empty_string(window.harness.provider.as_str()),
        non_empty_string(window.harness.status.as_str()),
        non_empty_string(window.binding.cwd.as_str()),
        non_empty_string(window.binding.confidence.as_str()),
        non_empty_string(window.attention.as_str()),
        non_empty_string(window.attach_state.as_str()),
    ])
}

fn review_available(row: &WorkbenchRow) -> bool {
    row.session_key.is_some() && row.attention.as_ref() == "new_review"
}

fn join_detail_parts(parts: impl IntoIterator<Item = Option<String>>) -> String {
    parts
        .into_iter()
        .flatten()
        .filter(|value| !value.trim().is_empty())
        .collect::<Vec<_>>()
        .join("  ")
}

fn attention_icon(attention: &str) -> IconName {
    match attention {
        "new_review" | "blocked" => IconName::Warning,
        "working" => IconName::Terminal,
        _ => IconName::SquareDot,
    }
}

fn attention_color(attention: &str) -> Color {
    match attention {
        "new_review" => Color::Warning,
        "blocked" => Color::Error,
        "working" => Color::Accent,
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
        DisplayRow::NodeHeader { .. } | DisplayRow::SessionHeader { .. } => false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use fs::FakeFs;
    use gpui::{KeyContext, Keystroke, TestAppContext};
    use project::Project;
    use serde_json::json;
    use settings::{KeymapFile, SettingsStore};
    use task::TaskId;
    use terminal_view::terminal_panel::TerminalPanel;
    use util::{path, rel_path::rel_path};
    use workspace::MultiWorkspace;

    #[test]
    fn flatten_rows_uses_window_session_key_and_dedupe_identity() {
        let snapshot = TmuxTreeSnapshot {
            nodes: vec![vitermux::TmuxNode {
                node_key: "poros".into(),
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
        assert_eq!(row.node_label.as_ref(), "poros (Albertus)");
        assert_eq!(row.window_label.as_ref(), "frontend-logging-cleanup");
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

        let display_rows = build_display_rows(&rows);
        assert_eq!(display_rows.len(), 6);
        assert!(matches!(
            &display_rows[0],
            DisplayRow::NodeHeader { label, .. } if label.as_ref() == "poros"
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
    fn selected_display_row_index_matches_visible_window_row() {
        let display_rows = build_display_rows(&[
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
        ]);

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
            slots[0].as_ref().map(|slot| (slot.session_key.as_str(), slot.dedupe_key.as_str())),
            Some(("session-1", "dedupe-1"))
        );
        assert_eq!(
            slots[1].as_ref().map(|slot| (slot.session_key.as_str(), slot.dedupe_key.as_str())),
            Some(("session-2", "dedupe-2"))
        );
        assert!(slots[2..].iter().all(Option::is_none));
    }

    #[test]
    fn row_for_session_slot_prefers_session_key_then_dedupe_key() {
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

        let prefers_session_key = row_for_session_slot(
            &SessionSlotAssignment {
                session_key: "session-2".into(),
                dedupe_key: "dedupe-missing".into(),
            },
            &rows,
            &row_index_by_session_key,
            &row_index_by_terminal_key,
        )
        .expect("slot should resolve by session key");
        assert_eq!(prefers_session_key.row_key.as_ref(), "row-2");

        let falls_back_to_dedupe = row_for_session_slot(
            &SessionSlotAssignment {
                session_key: "session-missing".into(),
                dedupe_key: "dedupe-1".into(),
            },
            &rows,
            &row_index_by_session_key,
            &row_index_by_terminal_key,
        )
        .expect("slot should resolve by dedupe key");
        assert_eq!(falls_back_to_dedupe.row_key.as_ref(), "row-1");
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
                session_key: "session-2".into(),
                dedupe_key: "dedupe-2".into(),
            }),
            Some(SessionSlotAssignment {
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

    #[gpui::test]
    async fn session_slot_keymap_binds_in_vitermux_context(
        cx: &mut TestAppContext,
    ) {
        cx.executor().allow_parking();
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(path!("/project_a"), json!({ ".git": {} })).await;
        let project = Project::test(fs, [path!("/project_a").as_ref()], cx).await;
        let window = cx.add_window(|window, cx| MultiWorkspace::test_new(project, window, cx));

        cx.update(|cx| {
            cx.bind_keys(
                KeymapFile::load_panic_on_failure(
                    r#"[
                        {
                            "context": "VitermuxPanel || (Terminal && vitermux_terminal)",
                            "bindings": {
                                "cmd-2": ["vitermux_panel::ActivateSlot", 1]
                            }
                        },
                        {
                            "context": "VitermuxPanel",
                            "bindings": {
                                "alt-cmd-3": ["vitermux_panel::AssignSelectedToSlot", 2]
                            }
                        }
                    ]"#,
                    cx,
                ),
            );
        });

        let activate_slot_matches = window.update(cx, |_workspace, window, _cx| {
            let binding = window
                .highest_precedence_binding_for_action_in_context(
                    &ActivateSlot(1),
                    KeyContext::parse("Terminal vitermux_terminal")
                        .expect("key context should parse"),
                )
                .expect("activate slot binding should exist");
            binding.match_keystrokes(&[Keystroke::parse("cmd-2").unwrap()])
        })
        .expect("window update should succeed");
        assert_eq!(activate_slot_matches, Some(false));

        let activate_slot_from_panel_matches = window.update(cx, |_workspace, window, _cx| {
            let binding = window
                .highest_precedence_binding_for_action_in_context(
                    &ActivateSlot(1),
                    KeyContext::parse("VitermuxPanel").expect("key context should parse"),
                )
                .expect("activate slot binding should exist");
            binding.match_keystrokes(&[Keystroke::parse("cmd-2").unwrap()])
        })
        .expect("window update should succeed");
        assert_eq!(activate_slot_from_panel_matches, Some(false));

        let assign_slot_matches = window.update(cx, |_workspace, window, _cx| {
            let binding = window
                .highest_precedence_binding_for_action_in_context(
                    &AssignSelectedToSlot(2),
                    KeyContext::parse("VitermuxPanel").expect("key context should parse"),
                )
                .expect("assign slot binding should exist");
            binding.match_keystrokes(&[Keystroke::parse("alt-cmd-3").unwrap()])
        })
        .expect("window update should succeed");
        assert_eq!(assign_slot_matches, Some(false));
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

    fn sample_row() -> WorkbenchRow {
        WorkbenchRow {
            row_key: SharedString::from("row-key"),
            node_section_key: SharedString::from("node-section"),
            node_label: SharedString::from("poros"),
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
            crate::init(cx);
        });
    }
}
