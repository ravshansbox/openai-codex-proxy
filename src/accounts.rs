use anyhow::Context;
use codex_backend_client::Client as BackendClient;
use codex_login::AuthCredentialsStoreMode;
use codex_login::AuthManager;
use serde::Deserialize;
use serde::Serialize;
use std::cmp::Reverse;
use std::collections::HashMap;
use std::collections::HashSet;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::RwLock;
use std::sync::atomic::AtomicI64;
use std::sync::atomic::AtomicU32;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use thiserror::Error;
use tokio::fs;
use tokio::sync::RwLock as AsyncRwLock;
use uuid::Uuid;

const MAX_USED_PERCENT_FOR_NEW_REQUESTS: u8 = 95;

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AccountState {
    Healthy,
    CoolingDown,
    RateLimited,
    NeedsReauth,
    Disabled,
}

impl AccountState {
    fn is_candidate(self) -> bool {
        matches!(self, Self::Healthy | Self::CoolingDown)
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct StoredAccount {
    pub id: String,
}

impl StoredAccount {
    pub fn codex_home(&self, data_dir: &Path) -> PathBuf {
        data_dir.join("accounts").join(&self.id)
    }
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct CreateAccountRequest {}

#[derive(Clone, Debug, Serialize)]
pub struct AccountAuthSnapshot {
    pub authenticated: bool,
    pub auth_mode: Option<String>,
    pub account_id: Option<String>,
    pub email: Option<String>,
    pub user_id: Option<String>,
    pub plan_type: Option<String>,
    pub fedramp: bool,
}

#[derive(Clone, Debug, Serialize)]
pub struct AccountUsageSnapshot {
    pub limit_id: Option<String>,
    pub plan_type: Option<String>,
    pub primary_used_percent: Option<f64>,
    pub primary_window_minutes: Option<i64>,
    pub primary_resets_at: Option<i64>,
    pub secondary_used_percent: Option<f64>,
    pub secondary_window_minutes: Option<i64>,
    pub secondary_resets_at: Option<i64>,
}

#[derive(Clone, Debug, Serialize)]
pub struct AccountSummary {
    pub id: String,
    pub state: AccountState,
    pub used_percent: Option<u8>,
    pub resets_at: Option<i64>,
    pub cooldown_until: Option<i64>,
    pub inflight: usize,
    pub recent_failures: u32,
    pub codex_home: String,
    pub auth: AccountAuthSnapshot,
    pub usage: Option<AccountUsageSnapshot>,
}

#[derive(Clone, Debug)]
struct RuntimeAccountState {
    state: AccountState,
    used_percent: Option<u8>,
    resets_at: Option<i64>,
}

impl Default for RuntimeAccountState {
    fn default() -> Self {
        Self {
            state: AccountState::Healthy,
            used_percent: None,
            resets_at: None,
        }
    }
}

#[derive(Debug)]
pub struct AccountHandle {
    id: String,
    data_dir: PathBuf,
    runtime: RwLock<RuntimeAccountState>,
    inflight: AtomicUsize,
    recent_failures: AtomicU32,
    cooldown_until: AtomicI64,
}

impl AccountHandle {
    fn new(id: String, data_dir: PathBuf) -> Self {
        Self {
            id,
            data_dir,
            runtime: RwLock::new(RuntimeAccountState::default()),
            inflight: AtomicUsize::new(0),
            recent_failures: AtomicU32::new(0),
            cooldown_until: AtomicI64::new(0),
        }
    }

    fn codex_home(&self) -> PathBuf {
        self.data_dir.join("accounts").join(&self.id)
    }

    pub fn id(&self) -> String {
        self.id.clone()
    }

    pub fn record(&self) -> StoredAccount {
        StoredAccount { id: self.id() }
    }

    fn runtime_state(&self) -> RuntimeAccountState {
        self.runtime
            .read()
            .map(|guard| guard.clone())
            .unwrap_or_default()
    }

    fn accepting_new_requests(&self) -> bool {
        let now = current_unix_seconds();
        let cooldown_until = self.cooldown_until.load(Ordering::Relaxed);
        if cooldown_until > now {
            return false;
        }
        self.clear_expired_rate_limit(now);
        let runtime = self.runtime_state();
        runtime.state.is_candidate()
            && runtime
                .used_percent
                .is_none_or(|used_percent| used_percent < MAX_USED_PERCENT_FOR_NEW_REQUESTS)
    }

    fn score(&self) -> Option<i32> {
        if !self.accepting_new_requests() {
            return None;
        }

        let runtime = self.runtime_state();
        let headroom = 100 - i32::from(runtime.used_percent.unwrap_or(0));
        let used_penalty = runtime
            .used_percent
            .map(|used_percent| if used_percent >= 85 { 25 } else { 0 })
            .unwrap_or(0);
        let state_penalty = match runtime.state {
            AccountState::Healthy => 0,
            AccountState::CoolingDown => 20,
            AccountState::RateLimited | AccountState::NeedsReauth | AccountState::Disabled => {
                return None;
            }
        };
        let inflight_penalty = self.inflight.load(Ordering::Relaxed) as i32 * 10;
        let recent_failure_penalty = self.recent_failures.load(Ordering::Relaxed) as i32 * 20;

        Some(headroom - used_penalty - state_penalty - inflight_penalty - recent_failure_penalty)
    }

    pub fn note_success(&self) {
        self.recent_failures.store(0, Ordering::Relaxed);
        self.cooldown_until.store(0, Ordering::Relaxed);
        if let Ok(mut runtime) = self.runtime.write()
            && runtime.state == AccountState::RateLimited
        {
            runtime.state = AccountState::Healthy;
        }
    }

    pub fn note_failure(&self) {
        self.recent_failures.fetch_add(1, Ordering::Relaxed);
    }

    pub fn note_rate_limited(&self, resets_at: Option<i64>, used_percent: Option<u8>) {
        let cooldown_until = resets_at.unwrap_or_else(|| current_unix_seconds() + 300);
        self.cooldown_until.store(cooldown_until, Ordering::Relaxed);
        if let Ok(mut runtime) = self.runtime.write() {
            runtime.state = AccountState::RateLimited;
            if let Some(used_percent) = used_percent {
                runtime.used_percent = Some(used_percent);
            }
            runtime.resets_at = Some(cooldown_until);
        }
    }

    fn clear_expired_rate_limit(&self, now: i64) {
        let cooldown_until = self.cooldown_until.load(Ordering::Relaxed);
        if cooldown_until > now {
            return;
        }
        if let Ok(mut runtime) = self.runtime.write()
            && runtime.state == AccountState::RateLimited
        {
            runtime.state = AccountState::Healthy;
        }
    }

    fn auth_manager(&self) -> AuthManager {
        AuthManager::new(
            self.codex_home(),
            /*enable_codex_api_key_env*/ false,
            AuthCredentialsStoreMode::File,
        )
    }

    pub async fn auth_snapshot(&self) -> AccountAuthSnapshot {
        let auth_manager = self.auth_manager();
        let Some(auth) = auth_manager.auth().await else {
            return AccountAuthSnapshot {
                authenticated: false,
                auth_mode: None,
                account_id: None,
                email: None,
                user_id: None,
                plan_type: None,
                fedramp: false,
            };
        };

        let plan_type = auth
            .account_plan_type()
            .and_then(|plan_type| serde_json::to_string(&plan_type).ok())
            .map(|json| json.trim_matches('"').to_string());

        AccountAuthSnapshot {
            authenticated: true,
            auth_mode: Some(format!("{:?}", auth.auth_mode()).to_ascii_lowercase()),
            account_id: auth.get_account_id(),
            email: auth.get_account_email(),
            user_id: auth.get_chatgpt_user_id(),
            plan_type,
            fedramp: auth.is_fedramp_account(),
        }
    }

    async fn resolve_upstream_auth(&self) -> Result<ResolvedUpstreamAuth, ResolveAuthError> {
        let auth_manager = self.auth_manager();
        let auth = auth_manager
            .auth()
            .await
            .ok_or(ResolveAuthError::NotAuthenticated)?;
        let bearer_token = auth.get_token().map_err(ResolveAuthError::Token)?;
        Ok(ResolvedUpstreamAuth {
            bearer_token,
            chatgpt_account_id: auth.get_account_id(),
            is_fedramp_account: auth.is_fedramp_account(),
        })
    }

    async fn usage_snapshot(&self) -> Option<AccountUsageSnapshot> {
        let auth = self.auth_manager().auth().await?;
        let client = BackendClient::from_auth("https://chatgpt.com", &auth).ok()?;
        let snapshot = client.get_rate_limits().await.ok()?;
        let plan_type = snapshot
            .plan_type
            .and_then(|plan_type| serde_json::to_string(&plan_type).ok())
            .map(|json| json.trim_matches('"').to_string());
        Some(AccountUsageSnapshot {
            limit_id: snapshot.limit_id,
            plan_type,
            primary_used_percent: snapshot.primary.as_ref().map(|window| window.used_percent),
            primary_window_minutes: snapshot
                .primary
                .as_ref()
                .and_then(|window| window.window_minutes),
            primary_resets_at: snapshot
                .primary
                .as_ref()
                .and_then(|window| window.resets_at),
            secondary_used_percent: snapshot
                .secondary
                .as_ref()
                .map(|window| window.used_percent),
            secondary_window_minutes: snapshot
                .secondary
                .as_ref()
                .and_then(|window| window.window_minutes),
            secondary_resets_at: snapshot
                .secondary
                .as_ref()
                .and_then(|window| window.resets_at),
        })
    }

    async fn refresh_usage_state(&self) {
        let Some(usage) = self.usage_snapshot().await else {
            return;
        };
        if let Ok(mut runtime) = self.runtime.write() {
            runtime.used_percent = usage.primary_used_percent.map(|value| value.round() as u8);
            runtime.resets_at = usage.primary_resets_at;
            let cooldown_until = self.cooldown_until.load(Ordering::Relaxed);
            if cooldown_until <= current_unix_seconds()
                && runtime.state == AccountState::RateLimited
            {
                runtime.state = AccountState::Healthy;
            }
        }
    }

    async fn summary(&self) -> AccountSummary {
        let runtime = self.runtime_state();
        let cooldown_until = self.cooldown_until.load(Ordering::Relaxed);
        AccountSummary {
            id: self.id(),
            state: runtime.state,
            used_percent: runtime.used_percent,
            resets_at: runtime.resets_at,
            cooldown_until: (cooldown_until > current_unix_seconds()).then_some(cooldown_until),
            inflight: self.inflight.load(Ordering::Relaxed),
            recent_failures: self.recent_failures.load(Ordering::Relaxed),
            codex_home: self.codex_home().display().to_string(),
            auth: self.auth_snapshot().await,
            usage: self.usage_snapshot().await,
        }
    }
}

#[derive(Debug, Error)]
pub enum ResolveAuthError {
    #[error("account is not authenticated")]
    NotAuthenticated,
    #[error("failed to resolve bearer token: {0}")]
    Token(std::io::Error),
}

#[derive(Clone, Debug)]
pub struct AccountRegistry {
    data_dir: PathBuf,
    accounts: Arc<AsyncRwLock<HashMap<String, Arc<AccountHandle>>>>,
    session_affinity: Arc<AsyncRwLock<HashMap<String, String>>>,
}

impl AccountRegistry {
    pub async fn load_or_create(data_dir: PathBuf) -> anyhow::Result<Self> {
        fs::create_dir_all(data_dir.join("accounts"))
            .await
            .with_context(|| format!("failed to create data dir {}", data_dir.display()))?;

        let registry = Self {
            data_dir,
            accounts: Arc::new(AsyncRwLock::new(HashMap::new())),
            session_affinity: Arc::new(AsyncRwLock::new(HashMap::new())),
        };
        registry.sync_from_disk_inner().await?;
        Ok(registry)
    }

    pub fn data_dir(&self) -> &Path {
        &self.data_dir
    }

    pub async fn list_summaries(&self) -> Vec<AccountSummary> {
        self.sync_from_disk().await;
        let accounts = self.accounts_snapshot().await;
        let mut summaries = Vec::with_capacity(accounts.len());
        for account in accounts {
            summaries.push(account.summary().await);
        }
        summaries
    }

    pub async fn get_summary(&self, account_id: &str) -> Option<AccountSummary> {
        self.sync_from_disk().await;
        let account = self.find_handle(account_id).await?;
        Some(account.summary().await)
    }

    pub async fn create_account(
        &self,
        _request: CreateAccountRequest,
    ) -> anyhow::Result<StoredAccount> {
        self.sync_from_disk().await;

        let account_id = Uuid::new_v4().to_string();
        let codex_home = self.data_dir.join("accounts").join(&account_id);
        fs::create_dir_all(&codex_home)
            .await
            .with_context(|| format!("failed to create {}", codex_home.display()))?;

        let account = StoredAccount { id: account_id };
        self.accounts.write().await.insert(
            account.id.clone(),
            Arc::new(AccountHandle::new(
                account.id.clone(),
                self.data_dir.clone(),
            )),
        );
        Ok(account)
    }

    pub async fn delete_account(&self, account_id: &str) -> anyhow::Result<bool> {
        self.sync_from_disk().await;

        let path = self.data_dir.join("accounts").join(account_id);
        if !fs::try_exists(&path).await.unwrap_or(false) {
            return Ok(false);
        }

        self.accounts.write().await.remove(account_id);
        self.clear_account_affinity(account_id).await;
        fs::remove_dir_all(&path)
            .await
            .with_context(|| format!("failed to remove {}", path.display()))?;
        Ok(true)
    }

    pub async fn get_record(&self, account_id: &str) -> Option<StoredAccount> {
        self.sync_from_disk().await;
        self.find_handle(account_id)
            .await
            .map(|account| account.record())
    }

    pub async fn select_account(
        &self,
        requested_account_id: Option<&str>,
        session_id: Option<&str>,
        excluded_account_ids: &[String],
    ) -> Result<SelectedAccount, RouteError> {
        self.sync_from_disk().await;

        let accounts = self.accounts_snapshot().await;
        if accounts.is_empty() {
            return Err(RouteError::NoAccountsConfigured);
        }

        let bound_account_id = if requested_account_id.is_none() {
            match session_id {
                Some(sid) => self.session_affinity.read().await.get(sid).cloned(),
                None => None,
            }
        } else {
            None
        };

        if let Some(bound_account_id) = bound_account_id
            && !excluded_account_ids
                .iter()
                .any(|id| id == &bound_account_id)
            && let Some(account) = accounts
                .iter()
                .find(|account| account.id() == bound_account_id)
            && let Some(score) = account.score()
        {
            match account.resolve_upstream_auth().await {
                Ok(auth) => {
                    account.inflight.fetch_add(1, Ordering::Relaxed);
                    return Ok(SelectedAccount {
                        lease: AccountLease {
                            account: Arc::clone(account),
                            score,
                        },
                        auth,
                    });
                }
                Err(ResolveAuthError::NotAuthenticated) | Err(ResolveAuthError::Token(_)) => {
                    if let Some(sid) = session_id {
                        self.clear_session_affinity(sid).await;
                    }
                }
            }
        }

        let candidate_accounts = accounts
            .into_iter()
            .filter(|account| {
                requested_account_id
                    .map(|account_id| account.id() == account_id)
                    .unwrap_or(true)
            })
            .filter(|account| !excluded_account_ids.iter().any(|id| id == &account.id()))
            .filter_map(|account| account.score().map(|score| (score, account)))
            .collect::<Vec<_>>();

        let mut candidates = Vec::new();
        for (score, account) in candidate_accounts {
            let usage = if !excluded_account_ids.is_empty() && requested_account_id.is_none() {
                account.usage_snapshot().await
            } else {
                None
            };
            candidates.push((score, account, usage));
        }

        candidates.sort_by_key(|(score, account, usage)| {
            if !excluded_account_ids.is_empty() && requested_account_id.is_none() {
                let secondary_reset = usage
                    .as_ref()
                    .and_then(|usage| usage.secondary_resets_at)
                    .unwrap_or(i64::MAX);
                let secondary_used = usage
                    .as_ref()
                    .and_then(|usage| usage.secondary_used_percent)
                    .map(|value| (value * 10.0) as i64)
                    .unwrap_or(-1);
                (
                    secondary_reset,
                    Reverse(secondary_used),
                    Reverse(*score),
                    account.id(),
                )
            } else {
                (i64::MIN, Reverse(-1), Reverse(*score), account.id())
            }
        });

        for (score, account, _usage) in candidates {
            match account.resolve_upstream_auth().await {
                Ok(auth) => {
                    account.inflight.fetch_add(1, Ordering::Relaxed);
                    if let Some(sid) = session_id {
                        self.bind_session_affinity(sid, &account.id()).await;
                    }
                    return Ok(SelectedAccount {
                        lease: AccountLease { account, score },
                        auth,
                    });
                }
                Err(ResolveAuthError::NotAuthenticated) => {
                    if let Some(sid) = session_id {
                        self.clear_session_affinity(sid).await;
                    }
                    continue;
                }
                Err(ResolveAuthError::Token(_)) => {
                    return Err(RouteError::AccountAuthFailed {
                        account_id: account.id(),
                    });
                }
            }
        }

        Err(RouteError::NoEligibleAccounts {
            requested_account_id: requested_account_id.map(str::to_string),
        })
    }

    pub async fn mark_rate_limited(
        &self,
        account_id: &str,
        session_id: Option<&str>,
        resets_at: Option<i64>,
        used_percent: Option<u8>,
    ) {
        self.sync_from_disk().await;
        if let Some(account) = self.find_handle(account_id).await {
            account.note_rate_limited(resets_at, used_percent);
        }
        if let Some(sid) = session_id {
            self.clear_session_affinity(sid).await;
        }
    }

    async fn find_handle(&self, account_id: &str) -> Option<Arc<AccountHandle>> {
        self.accounts.read().await.get(account_id).cloned()
    }

    async fn accounts_snapshot(&self) -> Vec<Arc<AccountHandle>> {
        let mut accounts = self
            .accounts
            .read()
            .await
            .values()
            .cloned()
            .collect::<Vec<_>>();
        accounts.sort_by_key(|account| account.id());
        accounts
    }

    async fn bind_session_affinity(&self, session_id: &str, account_id: &str) {
        self.session_affinity
            .write()
            .await
            .insert(session_id.to_string(), account_id.to_string());
    }

    async fn clear_session_affinity(&self, session_id: &str) {
        self.session_affinity.write().await.remove(session_id);
    }

    async fn clear_account_affinity(&self, account_id: &str) {
        self.session_affinity
            .write()
            .await
            .retain(|_, bound_account_id| bound_account_id != account_id);
    }

    pub async fn refresh_usage_state(&self) -> anyhow::Result<()> {
        self.sync_from_disk_inner().await?;
        let accounts = self.accounts_snapshot().await;
        for account in accounts {
            account.refresh_usage_state().await;
        }
        Ok(())
    }

    async fn sync_from_disk(&self) {
        if let Err(err) = self.sync_from_disk_inner().await {
            tracing::warn!(error = %err, "failed to sync accounts from disk");
        }
    }

    async fn sync_from_disk_inner(&self) -> anyhow::Result<()> {
        let discovered_ids = self.discover_account_ids().await?;
        let mut accounts = self.accounts.write().await;
        accounts.retain(|account_id, _| discovered_ids.contains(account_id));
        for account_id in discovered_ids {
            accounts
                .entry(account_id.clone())
                .or_insert_with(|| Arc::new(AccountHandle::new(account_id, self.data_dir.clone())));
        }
        Ok(())
    }

    async fn discover_account_ids(&self) -> anyhow::Result<HashSet<String>> {
        let accounts_dir = self.data_dir.join("accounts");
        let mut entries = fs::read_dir(&accounts_dir)
            .await
            .with_context(|| format!("failed to read {}", accounts_dir.display()))?;
        let mut account_ids = HashSet::new();

        while let Some(entry) = entries
            .next_entry()
            .await
            .with_context(|| format!("failed to iterate accounts dir {}", accounts_dir.display()))?
        {
            if !entry
                .file_type()
                .await
                .with_context(|| format!("failed to stat {}", entry.path().display()))?
                .is_dir()
            {
                continue;
            }

            let account_id = entry.file_name().to_string_lossy().into_owned();
            if Uuid::try_parse(&account_id).is_ok() {
                account_ids.insert(account_id);
            }
        }

        Ok(account_ids)
    }
}

#[derive(Debug)]
pub struct AccountLease {
    account: Arc<AccountHandle>,
    score: i32,
}

impl AccountLease {
    pub fn account_id(&self) -> String {
        self.account.id()
    }

    pub fn score(&self) -> i32 {
        self.score
    }

    pub fn note_success(&self) {
        self.account.note_success();
    }

    pub fn note_failure(&self) {
        self.account.note_failure();
    }
}

impl Drop for AccountLease {
    fn drop(&mut self) {
        self.account.inflight.fetch_sub(1, Ordering::Relaxed);
    }
}

#[derive(Debug)]
pub struct SelectedAccount {
    pub lease: AccountLease,
    pub auth: ResolvedUpstreamAuth,
}

#[derive(Clone, Debug)]
pub struct ResolvedUpstreamAuth {
    pub bearer_token: String,
    pub chatgpt_account_id: Option<String>,
    pub is_fedramp_account: bool,
}

fn current_unix_seconds() -> i64 {
    chrono::Utc::now().timestamp()
}

#[derive(Debug, Error)]
pub enum RouteError {
    #[error("no upstream accounts are configured")]
    NoAccountsConfigured,
    #[error("failed to resolve auth for account {account_id}")]
    AccountAuthFailed { account_id: String },
    #[error("no eligible authenticated account found for account id {requested_account_id:?}")]
    NoEligibleAccounts {
        requested_account_id: Option<String>,
    },
}
