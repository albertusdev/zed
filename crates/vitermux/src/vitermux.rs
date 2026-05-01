use anyhow::{Context as _, Result, anyhow, bail};
use futures::AsyncReadExt as _;
use gpui::{App, AppContext as _, Context, Entity, Global, SharedString, Task};
use http_client::{AsyncBody, HttpClient, Request};
use serde::Deserialize;
use std::{env, sync::Arc, time::Duration};
use url::Url;

const DEFAULT_DAEMON_ADDR: &str = "http://localhost:8401";
const POLL_INTERVAL: Duration = Duration::from_secs(3);

pub fn init(cx: &mut App) {
    VitermuxStore::init_global(cx);
}

#[derive(Clone)]
pub struct VitermuxClient {
    base_url: Arc<str>,
    http_client: Arc<dyn HttpClient>,
}

impl VitermuxClient {
    pub fn new(base_url: impl Into<Arc<str>>, http_client: Arc<dyn HttpClient>) -> Self {
        Self {
            base_url: base_url.into(),
            http_client,
        }
    }

    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    pub async fn fetch_tmux_tree(&self) -> Result<TmuxTreeSnapshot> {
        self.fetch_json("/api/tmux/tree", None).await
    }

    pub async fn fetch_open_plan(&self, session_id: &str) -> Result<ZedOpenPlan> {
        self.fetch_json("/api/zed/open-plan", Some(&[("session_id", session_id)]))
            .await
    }

    async fn fetch_json<T: for<'de> Deserialize<'de>>(
        &self,
        path: &str,
        query: Option<&[(&str, &str)]>,
    ) -> Result<T> {
        let url = self.build_url(path, query)?;
        let request = Request::builder()
            .uri(url.as_str())
            .body(AsyncBody::empty())?;
        let mut response = self
            .http_client
            .send(request)
            .await
            .with_context(|| format!("request failed: {}", url))?;
        let mut body = String::new();
        response.body_mut().read_to_string(&mut body).await?;
        if !response.status().is_success() {
            bail!(
                "daemon returned {} for {}: {}",
                response.status(),
                url,
                body
            );
        }
        serde_json::from_str(&body).with_context(|| format!("invalid JSON from {}", url))
    }

    fn build_url(&self, path: &str, query: Option<&[(&str, &str)]>) -> Result<Url> {
        let joined = format!("{}{}", self.base_url, path);
        let mut url =
            Url::parse(&joined).with_context(|| format!("invalid daemon URL: {joined}"))?;
        if let Some(query) = query {
            url.query_pairs_mut().extend_pairs(query.iter().copied());
        }
        Ok(url)
    }
}

struct GlobalVitermuxStore(Entity<VitermuxStore>);

impl Global for GlobalVitermuxStore {}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VitermuxConnectionState {
    Connecting,
    Ready,
    Error,
}

pub struct VitermuxStore {
    client: VitermuxClient,
    snapshot: Option<TmuxTreeSnapshot>,
    connection_state: VitermuxConnectionState,
    last_error: Option<SharedString>,
    is_refreshing: bool,
    refresh_task: Task<()>,
    poll_task: Task<()>,
}

impl VitermuxStore {
    pub fn init_global(cx: &mut App) {
        if cx.has_global::<GlobalVitermuxStore>() {
            return;
        }

        let base_url = env::var("VITERMUX_ADDR")
            .or_else(|_| env::var("AGENT_HUD_ADDR"))
            .unwrap_or_else(|_| DEFAULT_DAEMON_ADDR.to_string());
        let client = VitermuxClient::new(Arc::<str>::from(base_url), cx.http_client());
        let store = cx.new(|cx| Self::new(client, cx));
        cx.set_global(GlobalVitermuxStore(store));
    }

    pub fn global(cx: &App) -> Entity<Self> {
        cx.global::<GlobalVitermuxStore>().0.clone()
    }

    pub fn try_global(cx: &App) -> Option<Entity<Self>> {
        cx.try_global::<GlobalVitermuxStore>()
            .map(|store| store.0.clone())
    }

    fn new(client: VitermuxClient, cx: &mut Context<Self>) -> Self {
        let mut this = Self {
            client,
            snapshot: None,
            connection_state: VitermuxConnectionState::Connecting,
            last_error: None,
            is_refreshing: false,
            refresh_task: Task::ready(()),
            poll_task: Task::ready(()),
        };
        this.refresh(cx);
        this.start_polling(cx);
        this
    }

    pub fn client(&self) -> VitermuxClient {
        self.client.clone()
    }

    pub fn snapshot(&self) -> Option<&TmuxTreeSnapshot> {
        self.snapshot.as_ref()
    }

    pub fn connection_state(&self) -> VitermuxConnectionState {
        self.connection_state
    }

    pub fn last_error(&self) -> Option<&SharedString> {
        self.last_error.as_ref()
    }

    pub fn is_refreshing(&self) -> bool {
        self.is_refreshing
    }

    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        if self.is_refreshing {
            return;
        }
        self.is_refreshing = true;
        let client = self.client.clone();
        self.refresh_task = cx.spawn(async move |this, cx| {
            let result = client.fetch_tmux_tree().await;
            let _ = this.update(cx, |this, cx| {
                this.is_refreshing = false;
                match result {
                    Ok(snapshot) => {
                        this.snapshot = Some(snapshot);
                        this.connection_state = VitermuxConnectionState::Ready;
                        this.last_error = None;
                    }
                    Err(error) => {
                        this.connection_state = VitermuxConnectionState::Error;
                        this.last_error = Some(error.to_string().into());
                    }
                }
                cx.notify();
            });
        });
    }

    fn start_polling(&mut self, cx: &mut Context<Self>) {
        self.poll_task = cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(POLL_INTERVAL).await;
                if this
                    .update(cx, |this, cx| {
                        this.refresh(cx);
                    })
                    .is_err()
                {
                    break;
                }
            }
        });
    }
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct TmuxTreeSnapshot {
    #[serde(default)]
    pub revision: u64,
    #[serde(default)]
    pub nodes: Vec<TmuxNode>,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct TmuxNode {
    #[serde(default)]
    pub node_key: String,
    #[serde(default)]
    pub node_id: String,
    #[serde(default)]
    pub node_name: String,
    #[serde(default)]
    pub accounts: Vec<TmuxAccount>,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct TmuxAccount {
    #[serde(default)]
    pub account_key: String,
    #[serde(default)]
    pub account_name: String,
    #[serde(default)]
    pub account_user: String,
    #[serde(default)]
    pub sessions: Vec<TmuxSession>,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct TmuxSession {
    #[serde(default)]
    pub session_key: String,
    #[serde(default)]
    pub session_name: String,
    #[serde(default)]
    pub project_group: ProjectGroupBinding,
    #[serde(default)]
    pub windows: Vec<TmuxWindow>,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct TmuxWindow {
    #[serde(default)]
    pub window_key: String,
    #[serde(default)]
    pub target_key: String,
    #[serde(default)]
    pub window_id: String,
    #[serde(default)]
    pub window_name: String,
    #[serde(default)]
    pub window_target: String,
    #[serde(default)]
    pub dedupe_key: String,
    #[serde(default)]
    pub binding: WorktreeBinding,
    #[serde(default)]
    pub harness: HarnessBinding,
    #[serde(default)]
    pub agents: Vec<AgentBinding>,
    #[serde(default)]
    pub attention: String,
    #[serde(default)]
    pub attach_state: String,
}

impl TmuxWindow {
    pub fn primary_session_key(&self) -> Option<&str> {
        if !self.harness.session_key.trim().is_empty() {
            Some(self.harness.session_key.as_str())
        } else if !self.harness.session_id.trim().is_empty() {
            Some(self.harness.session_id.as_str())
        } else {
            self.agents.iter().find_map(|agent| {
                if !agent.session_key.trim().is_empty() {
                    Some(agent.session_key.as_str())
                } else {
                    (!agent.session_id.trim().is_empty()).then_some(agent.session_id.as_str())
                }
            })
        }
    }

    pub fn stable_row_key(&self) -> &str {
        if !self.dedupe_key.is_empty() {
            self.dedupe_key.as_str()
        } else if !self.target_key.is_empty() {
            self.target_key.as_str()
        } else {
            self.window_key.as_str()
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct ProjectGroupBinding {
    #[serde(default)]
    pub codebase_label: String,
    #[serde(default)]
    pub workspace_label: String,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct WorktreeBinding {
    #[serde(default)]
    pub codebase_label: String,
    #[serde(default)]
    pub workspace_label: String,
    #[serde(default)]
    pub cwd: String,
    #[serde(default)]
    pub confidence: String,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct HarnessBinding {
    #[serde(default)]
    pub session_key: String,
    #[serde(default)]
    pub session_id: String,
    #[serde(default)]
    pub primary_agent_id: String,
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub provider: String,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct AgentBinding {
    #[serde(default)]
    pub session_key: String,
    #[serde(default)]
    pub session_id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub provider: String,
    #[serde(default)]
    pub review_requested: bool,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct ZedOpenPlan {
    #[serde(default)]
    pub plan_id: String,
    #[serde(default)]
    pub operator_key: String,
    #[serde(default)]
    pub target_ref: TargetRef,
    #[serde(default)]
    pub project: ZedProjectTarget,
    #[serde(default)]
    pub attach: ZedAttachSpec,
    #[serde(default)]
    pub failure: Option<OpenPlanFailure>,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct TargetRef {
    #[serde(default)]
    pub session_id: String,
    #[serde(default)]
    pub node_id: String,
    #[serde(default)]
    pub account_user: String,
    #[serde(default)]
    pub tmux_session: String,
    #[serde(default)]
    pub tmux_window: String,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct ZedProjectTarget {
    #[serde(default)]
    pub mode: String,
    #[serde(default)]
    pub node: String,
    #[serde(default)]
    pub ssh_address: String,
    #[serde(default)]
    pub remote_path: String,
    #[serde(default)]
    pub workspace_name: String,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct ZedAttachSpec {
    #[serde(default)]
    pub mode: String,
    #[serde(default)]
    pub target: String,
    #[serde(default)]
    pub command: String,
    #[serde(default)]
    pub argv: Vec<String>,
    #[serde(default)]
    pub cwd: String,
    #[serde(default)]
    pub env: std::collections::HashMap<String, String>,
    #[serde(default)]
    pub pane_id: String,
    #[serde(default)]
    pub dedupe_key: String,
}

impl ZedAttachSpec {
    pub fn command_or_error(&self) -> Result<&str> {
        if !self.command.trim().is_empty() {
            Ok(self.command.as_str())
        } else {
            Err(anyhow!(
                "daemon open plan did not include an attach command"
            ))
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct OpenPlanFailure {
    #[serde(default)]
    pub reason: String,
    #[serde(default)]
    pub message: String,
    #[serde(default)]
    pub retryable: bool,
}

impl OpenPlanFailure {
    pub fn display_message(&self) -> String {
        if !self.message.trim().is_empty() {
            self.message.clone()
        } else if !self.reason.trim().is_empty() {
            self.reason.clone()
        } else {
            "unknown daemon open-plan failure".to_string()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn primary_session_key_prefers_harness_session_key() {
        let window = TmuxWindow {
            harness: HarnessBinding {
                session_key: "codex:poros:acct:abc".into(),
                session_id: "raw-session-id".into(),
                ..Default::default()
            },
            agents: vec![AgentBinding {
                session_key: "agent-session-key".into(),
                session_id: "agent-session-id".into(),
                ..Default::default()
            }],
            ..Default::default()
        };

        assert_eq!(window.primary_session_key(), Some("codex:poros:acct:abc"));
    }

    #[test]
    fn primary_session_key_falls_back_to_agent_keys() {
        let window = TmuxWindow {
            agents: vec![AgentBinding {
                session_key: "claude:poros:acct:def".into(),
                ..Default::default()
            }],
            ..Default::default()
        };

        assert_eq!(window.primary_session_key(), Some("claude:poros:acct:def"));
    }

    #[test]
    fn stable_row_key_prefers_dedupe_key() {
        let window = TmuxWindow {
            dedupe_key: "node:poros/acct:albertus/sess:main/win:2".into(),
            target_key: "target-key".into(),
            window_key: "window-key".into(),
            ..Default::default()
        };

        assert_eq!(
            window.stable_row_key(),
            "node:poros/acct:albertus/sess:main/win:2"
        );
    }

    #[test]
    fn stable_row_key_falls_back_when_dedupe_key_missing() {
        let window = TmuxWindow {
            target_key: "target-key".into(),
            window_key: "window-key".into(),
            ..Default::default()
        };

        assert_eq!(window.stable_row_key(), "target-key");
    }
}
