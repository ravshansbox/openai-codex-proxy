use crate::accounts::StoredAccount;
use codex_login::AuthCredentialsStoreMode;
use codex_login::DeviceCode;
use codex_login::LoginServer;
use codex_login::ServerOptions;
use codex_login::complete_device_code_login;
use codex_login::request_device_code;
use codex_login::run_login_server;
use serde::Serialize;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;
use uuid::Uuid;

#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LoginKind {
    Browser,
    DeviceCode,
}

#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LoginPhase {
    Pending,
    Succeeded,
    Failed,
    Cancelled,
}

#[derive(Clone, Debug, Serialize)]
pub struct LoginStatus {
    pub login_id: String,
    pub account_id: String,
    pub kind: LoginKind,
    pub phase: LoginPhase,
    pub auth_url: Option<String>,
    pub verification_url: Option<String>,
    pub user_code: Option<String>,
    pub error: Option<String>,
}

struct LoginEntry {
    status: Arc<RwLock<LoginStatus>>,
    cancel: Option<codex_login::ShutdownHandle>,
}

#[derive(Clone, Default)]
pub struct LoginManager {
    entries: Arc<RwLock<HashMap<String, LoginEntry>>>,
}

impl LoginManager {
    pub async fn start_browser_login(
        &self,
        account: &StoredAccount,
        data_dir: &std::path::Path,
    ) -> anyhow::Result<LoginStatus> {
        let mut options = server_options(account.codex_home(data_dir));
        options.open_browser = false;
        options.port = 0;
        let login_server = run_login_server(options)?;
        let login_id = Uuid::new_v4().to_string();
        let status = LoginStatus {
            login_id: login_id.clone(),
            account_id: account.id.clone(),
            kind: LoginKind::Browser,
            phase: LoginPhase::Pending,
            auth_url: Some(login_server.auth_url.clone()),
            verification_url: None,
            user_code: None,
            error: None,
        };
        let status_ref = Arc::new(RwLock::new(status.clone()));
        self.entries.write().await.insert(
            login_id.clone(),
            LoginEntry {
                status: Arc::clone(&status_ref),
                cancel: Some(login_server.cancel_handle()),
            },
        );
        tokio::spawn(wait_for_browser_login(status_ref, login_server));
        Ok(status)
    }

    pub async fn start_device_code_login(
        &self,
        account: &StoredAccount,
        data_dir: &std::path::Path,
    ) -> anyhow::Result<LoginStatus> {
        let options = server_options(account.codex_home(data_dir));
        let device_code = request_device_code(&options).await?;
        let login_id = Uuid::new_v4().to_string();
        let status = LoginStatus {
            login_id: login_id.clone(),
            account_id: account.id.clone(),
            kind: LoginKind::DeviceCode,
            phase: LoginPhase::Pending,
            auth_url: None,
            verification_url: Some(device_code.verification_url.clone()),
            user_code: Some(device_code.user_code.clone()),
            error: None,
        };
        let status_ref = Arc::new(RwLock::new(status.clone()));
        self.entries.write().await.insert(
            login_id.clone(),
            LoginEntry {
                status: Arc::clone(&status_ref),
                cancel: None,
            },
        );
        tokio::spawn(wait_for_device_code_login(status_ref, options, device_code));
        Ok(status)
    }

    pub async fn get(&self, login_id: &str) -> Option<LoginStatus> {
        let status = self
            .entries
            .read()
            .await
            .get(login_id)
            .map(|entry| Arc::clone(&entry.status))?;
        Some(status.read().await.clone())
    }

    pub async fn cancel(&self, login_id: &str) -> Option<LoginStatus> {
        let entry = self
            .entries
            .read()
            .await
            .get(login_id)
            .map(|entry| LoginEntry {
                status: Arc::clone(&entry.status),
                cancel: entry.cancel.clone(),
            })?;
        if let Some(cancel) = entry.cancel {
            cancel.shutdown();
        }
        {
            let mut status = entry.status.write().await;
            status.phase = LoginPhase::Cancelled;
            status.error = Some("login cancelled".to_string());
        }
        Some(entry.status.read().await.clone())
    }
}

fn server_options(codex_home: std::path::PathBuf) -> ServerOptions {
    ServerOptions::new(
        codex_home,
        codex_login::CLIENT_ID.to_string(),
        /*forced_chatgpt_workspace_id*/ None,
        AuthCredentialsStoreMode::File,
    )
}

async fn wait_for_browser_login(status_ref: Arc<RwLock<LoginStatus>>, login_server: LoginServer) {
    match login_server.block_until_done().await {
        Ok(()) => {
            let mut status = status_ref.write().await;
            status.phase = LoginPhase::Succeeded;
            status.error = None;
        }
        Err(err) => {
            let mut status = status_ref.write().await;
            if !matches!(status.phase, LoginPhase::Cancelled) {
                status.phase = LoginPhase::Failed;
                status.error = Some(err.to_string());
            }
        }
    }
}

async fn wait_for_device_code_login(
    status_ref: Arc<RwLock<LoginStatus>>,
    options: ServerOptions,
    device_code: DeviceCode,
) {
    match complete_device_code_login(options, device_code).await {
        Ok(()) => {
            let mut status = status_ref.write().await;
            status.phase = LoginPhase::Succeeded;
            status.error = None;
        }
        Err(err) => {
            let mut status = status_ref.write().await;
            status.phase = LoginPhase::Failed;
            status.error = Some(err.to_string());
        }
    }
}
