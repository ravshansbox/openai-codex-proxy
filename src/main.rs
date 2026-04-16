mod accounts;
mod admin;
mod cli;
mod config;
mod installation;
mod logins;
mod models;
mod proxy;
mod proxy_auth;

use crate::accounts::AccountRegistry;
use crate::cli::Cli;
use crate::cli::Command;
use crate::config::AppConfig;
use crate::installation::load_or_create_installation_id;
use crate::logins::LoginManager;
use crate::proxy::AppState;
use crate::proxy_auth::ProxyAuth;
use anyhow::Context;
use axum::Router;
use axum::routing::get;
use axum::routing::post;
use clap::Parser;
use std::sync::Arc;
use std::time::Duration;
use tokio::net::TcpListener;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let config = AppConfig::from_env()?;

    match cli.command.unwrap_or(Command::Serve) {
        Command::Serve => {
            init_tracing();
            run_server(config).await
        }
        Command::Login(args) => cli::handle_login_command(&config, args).await,
        Command::ListAccounts(args) => cli::handle_list_accounts_command(&config, args).await,
        Command::SetApiKey(args) => cli::handle_set_api_key_command(&config, args).await,
        Command::ApiKeyStatus => cli::handle_api_key_status_command(&config),
    }
}

async fn run_server(config: AppConfig) -> anyhow::Result<()> {
    let accounts = AccountRegistry::load_or_create(config.data_dir.clone()).await?;
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(config.request_timeout_secs))
        .user_agent(concat!(
            env!("CARGO_PKG_NAME"),
            "/",
            env!("CARGO_PKG_VERSION")
        ))
        .build()
        .context("failed to build HTTP client")?;

    let installation_id = load_or_create_installation_id(&config.data_dir)
        .context("failed to load or create installation id")?;
    let proxy_auth =
        ProxyAuth::load_or_create(&config.data_dir).context("failed to load proxy auth config")?;
    let state = Arc::new(AppState {
        client,
        accounts,
        logins: LoginManager::default(),
        installation_id,
        proxy_auth,
    });
    tokio::spawn(usage_refresh_loop(Arc::clone(&state)));

    let app = Router::new()
        .route("/health", get(proxy::health))
        .route(
            "/accounts",
            get(admin::list_accounts).post(admin::create_account),
        )
        .route(
            "/accounts/{account_id}",
            get(admin::get_account).delete(admin::delete_account),
        )
        .route(
            "/accounts/login/browser/start",
            post(admin::create_and_start_browser_login),
        )
        .route(
            "/accounts/login/device-code/start",
            post(admin::create_and_start_device_code_login),
        )
        .route(
            "/accounts/{account_id}/login/browser/start",
            post(admin::start_browser_login),
        )
        .route(
            "/accounts/{account_id}/login/device-code/start",
            post(admin::start_device_code_login),
        )
        .route("/logins/{login_id}", get(admin::get_login))
        .route("/logins/{login_id}/cancel", post(admin::cancel_login))
        .route("/v1/models", get(models::list_models))
        .route("/v1/responses", post(proxy::proxy_responses))
        .with_state(state.clone());

    let listener = TcpListener::bind(config.listen_addr)
        .await
        .with_context(|| format!("failed to bind {}", config.listen_addr))?;

    tracing::info!("listen_addr={}", config.listen_addr);
    if let Some(api_key) = state.proxy_auth.api_key() {
        tracing::info!("api_key={api_key}");
    } else {
        tracing::warn!("api key: not configured — run `openai-codex-proxy set-api-key`");
    }
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .context("server error")?;
    Ok(())
}

async fn usage_refresh_loop(state: Arc<AppState>) {
    let mut interval = tokio::time::interval(Duration::from_secs(600));
    loop {
        interval.tick().await;
        if let Err(err) = state.accounts.refresh_usage_state().await {
            tracing::warn!(error = %err, "failed to refresh account usage state");
        }
    }
}

fn init_tracing() {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info,codex_client=warn"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .init();
}

async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };

    #[cfg(unix)]
    let terminate = async {
        if let Ok(mut signal) =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
            signal.recv().await;
        }
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {}
        _ = terminate => {}
    }

    tracing::info!("shutdown signal received");
}
