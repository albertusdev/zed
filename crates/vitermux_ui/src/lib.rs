use anyhow::{Result, anyhow};
use collections::{HashMap, HashSet};
use gpui::{
    Action, AnyElement, App, AsyncWindowContext, Context, Entity, EventEmitter, FocusHandle,
    Focusable, ParentElement, Pixels, Render, SharedString, StatefulInteractiveElement, Styled,
    Subscription, Task, WeakEntity, Window, actions, px,
};
use parking_lot::Mutex;
use std::path::PathBuf;
use std::sync::Arc;
use task::{
    HideStrategy, RevealStrategy, RevealTarget, SaveStrategy, Shell, SpawnInTerminal, TaskId,
};
use terminal_view::{TerminalView, terminal_panel::TerminalPanel};
use ui::{
    Color, Disableable, Icon, IconName, Label, LabelSize, ListItem, ListItemSpacing, Toggleable,
    Tooltip, prelude::*,
};
use vitermux::{
    OpenPlanFailure, TmuxTreeSnapshot, TmuxWindow, VitermuxConnectionState, VitermuxStore,
    ZedOpenPlan,
};
use workspace::{
    Toast, Workspace,
    dock::{DockPosition, Panel, PanelEvent},
    notifications::{DetachAndPromptErr, NotificationId},
};

const VITERMUX_PANEL_KEY: &str = "VitermuxPanel";

actions!(
    vitermux_panel,
    [Toggle, ToggleFocus, Refresh, OpenSelected,]
);

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

pub struct VitermuxPanel {
    workspace: WeakEntity<Workspace>,
    store: Entity<VitermuxStore>,
    focus_handle: FocusHandle,
    pending_open_keys: Arc<Mutex<HashSet<String>>>,
    selected_row_key: Option<SharedString>,
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
        workspace.update_in(&mut cx, |_workspace, _window, cx| {
            cx.new(|cx| Self::new(workspace_handle.clone(), cx))
        })
    }

    fn new(workspace: WeakEntity<Workspace>, cx: &mut Context<Self>) -> Self {
        let store = VitermuxStore::global(cx);
        let mut this = Self {
            workspace,
            store: store.clone(),
            focus_handle: cx.focus_handle(),
            pending_open_keys: Arc::default(),
            selected_row_key: None,
            active: false,
            width: px(336.0),
            _subscriptions: vec![cx.observe(&store, |this, _, cx| {
                this.ensure_selection(cx);
                cx.notify();
            })],
        };
        this.ensure_selection(cx);
        this
    }

    fn refresh(&mut self, cx: &mut Context<Self>) {
        self.store.update(cx, |store, cx| store.refresh(cx));
    }

    fn rows(&self, cx: &App) -> Vec<WorkbenchRow> {
        let Some(snapshot) = self.store.read(cx).snapshot().cloned() else {
            return Vec::new();
        };
        flatten_rows(&snapshot)
    }

    fn ensure_selection(&mut self, cx: &mut Context<Self>) {
        let rows = self.rows(cx);
        if rows.is_empty() {
            self.selected_row_key = None;
            return;
        }

        let selected_exists = self.selected_row_key.as_ref().is_some_and(|selected| {
            rows.iter()
                .any(|row| row.row_key.as_ref() == selected.as_ref())
        });
        if !selected_exists {
            self.selected_row_key = rows.first().map(|row| row.row_key.clone());
        }
    }

    fn selected_row(&self, cx: &App) -> Option<WorkbenchRow> {
        let selected = self.selected_row_key.as_ref()?;
        self.rows(cx)
            .into_iter()
            .find(|row| row.row_key.as_ref() == selected.as_ref())
    }

    fn open_selected(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(row) = self.selected_row(cx) {
            self.open_row(row, window, cx);
        }
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
        let session_key = row
            .session_key
            .clone()
            .ok_or_else(|| anyhow!("missing vitermux session key"))
            .map(|id| id.to_string());
        let terminal_panel = workspace.as_ref().ok().and_then(|workspace| {
            workspace.read_with(cx, |workspace, cx| workspace.panel::<TerminalPanel>(cx))
        });

        window.spawn(cx, async move |cx| {
            let result = async {
                let workspace = workspace?;
                let session_key = session_key?;

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
                    return Ok(());
                }

                let plan = client.fetch_open_plan(&session_key).await?;
                let spawn_task = build_spawn_task(&row, &plan)?;
                let did_focus_existing = workspace.update_in(cx, |workspace, window, cx| {
                    focus_existing_terminal(
                        workspace,
                        terminal_panel.clone(),
                        &spawn_task.full_label,
                        window,
                        cx,
                    )
                })?;
                if did_focus_existing {
                    return Ok(());
                }

                let Some(terminal_panel) = terminal_panel else {
                    return Err(anyhow!("terminal panel is not available"));
                };
                let terminal_task = terminal_panel.update_in(cx, |panel, window, cx| {
                    panel.spawn_task(&spawn_task, window, cx)
                })?;
                terminal_task.await?;
                Ok(())
            }
            .await;

            pending_open_keys.lock().remove(&open_key);
            result
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
            .when_some(store.last_error().cloned(), |this, error| {
                this.child(
                    Label::new(error)
                        .size(LabelSize::XSmall)
                        .color(Color::Error),
                )
            })
            .into_any_element()
    }

    fn render_rows(&self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let rows = self.rows(cx);
        let selected_key = self.selected_row_key.clone();
        let panel_focused = self.active && self.focus_handle.contains_focused(window, cx);

        if rows.is_empty() {
            return v_flex()
                .p_3()
                .child(
                    Label::new("No tracked tmux windows yet")
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                )
                .into_any_element();
        }

        let mut elements = Vec::new();
        let mut last_node_key: Option<SharedString> = None;
        let mut last_session_key: Option<SharedString> = None;

        for row in rows {
            if last_node_key.as_ref() != Some(&row.node_section_key) {
                last_node_key = Some(row.node_section_key.clone());
                last_session_key = None;
                elements.push(
                    v_flex()
                        .pt_3()
                        .px_3()
                        .child(
                            Label::new(row.node_label.clone())
                                .size(LabelSize::XSmall)
                                .color(Color::Accent),
                        )
                        .into_any_element(),
                );
            }

            if last_session_key.as_ref() != Some(&row.session_section_key) {
                last_session_key = Some(row.session_section_key.clone());
                elements.push(
                    v_flex()
                        .px_3()
                        .pt_1()
                        .pb_1()
                        .gap_0p5()
                        .child(Label::new(row.session_label.clone()).size(LabelSize::Small))
                        .child(
                            Label::new(row.session_detail.clone())
                                .size(LabelSize::XSmall)
                                .color(Color::Muted),
                        )
                        .into_any_element(),
                );
            }

            let row_key = row.row_key.clone();
            let row_clone = row.clone();
            let row_selected = selected_key.as_ref() == Some(&row.row_key);
            let disabled = row.session_key.is_none();
            elements.push(
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
                    .end_slot(Icon::new(IconName::ChevronRight).color(Color::Muted))
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
                    .into_any_element(),
            );
        }

        v_flex().w_full().children(elements).into_any_element()
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
            .key_context("VitermuxPanel")
            .size_full()
            .bg(cx.theme().colors().panel_background)
            .border_r_1()
            .border_color(if self.active {
                cx.theme().colors().panel_focused_border
            } else {
                cx.theme().colors().border_variant
            })
            .track_focus(&self.focus_handle)
            .child(self.render_header(window, cx))
            .child(
                v_flex()
                    .id("vitermux-panel-scroll")
                    .size_full()
                    .overflow_y_scroll()
                    .child(self.render_rows(window, cx)),
            )
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

fn build_spawn_task(row: &WorkbenchRow, plan: &ZedOpenPlan) -> Result<SpawnInTerminal> {
    if let Some(failure) = plan.failure.as_ref() {
        return Err(open_plan_failure(failure));
    }

    let (command, args) = resolve_attach_command(plan)?;
    let (shell, command, args) = spawn_command_parts(plan, command, args);
    let cwd = spawn_cwd(plan);
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

fn resolve_attach_command(plan: &ZedOpenPlan) -> Result<(String, Vec<String>)> {
    if plan.attach.mode == "ssh_shell" {
        return argv_attach_command(plan)
            .ok_or_else(|| anyhow!("daemon SSH attach plan did not include argv"));
    }

    if let Some((command, args)) = argv_attach_command(plan) {
        return Ok((command, args));
    }

    if plan.attach.mode == "local_shell" {
        if let Some((command, args)) = local_shell_fallback(plan) {
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

fn local_shell_fallback(plan: &ZedOpenPlan) -> Option<(String, Vec<String>)> {
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
        pane.read(cx).items().find_map(|item| {
            let terminal_view = item.act_as::<TerminalView>(cx)?;
            let task = terminal_view.read(cx).terminal().read(cx).task()?;
            if task.spawned_task.full_label == full_label {
                Some(terminal_view)
            } else {
                None
            }
        })
    });

    if let Some(terminal_view) = target {
        return workspace.activate_item(&terminal_view, true, true, window, cx);
    }

    false
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

fn spawn_cwd(plan: &ZedOpenPlan) -> Option<PathBuf> {
    if plan.project.mode == "remote" {
        return None;
    }

    non_empty_string(plan.attach.cwd.as_str()).map(PathBuf::from)
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
mod tests {
    use super::*;
    use task::TaskId;

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

        let task = build_spawn_task(&row, &plan).expect("remote argv attach should build");
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

        let error = build_spawn_task(&row, &plan).expect_err("remote attach should require argv");
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

        let task = build_spawn_task(&row, &plan).expect("local fallback should build");
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
}
