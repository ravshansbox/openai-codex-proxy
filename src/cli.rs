use crate::accounts::AccountRegistry;
use crate::accounts::CreateAccountRequest;
use crate::accounts::StoredAccount;
use crate::config::AppConfig;
use crate::proxy_auth::ProxyAuth;
use crate::proxy_auth::generate_api_key;
use anyhow::Context;
use chrono::DateTime;
use chrono::Utc;
use clap::Parser;
use clap::Subcommand;
use codex_login::AuthCredentialsStoreMode;
use codex_login::CLIENT_ID;
use codex_login::ServerOptions;
use codex_login::run_device_code_login;
use codex_login::run_login_server;

#[derive(Debug, Parser)]
#[command(name = "openai-codex-proxy")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    Serve,
    Login(LoginArgs),
    ListAccounts(ListAccountsArgs),
    SetApiKey(SetApiKeyArgs),
    ApiKeyStatus,
}

#[derive(Debug, clap::Args)]
pub struct LoginArgs {
    #[arg(long, conflicts_with = "browser")]
    pub device_auth: bool,
    #[arg(long, default_value_t = false)]
    pub browser: bool,
}

#[derive(Debug, clap::Args)]
pub struct ListAccountsArgs {
    #[arg(long, default_value_t = false)]
    pub verbose: bool,
}

#[derive(Debug, clap::Args)]
pub struct SetApiKeyArgs {
    pub value: Option<String>,
}

pub async fn handle_login_command(config: &AppConfig, args: LoginArgs) -> anyhow::Result<()> {
    let registry = AccountRegistry::load_or_create(config.data_dir.clone()).await?;
    let account = registry
        .create_account(CreateAccountRequest::default())
        .await?;

    let login_result = if args.device_auth {
        login_with_device_code(&account, &config.data_dir).await
    } else {
        login_with_browser(&account, &config.data_dir).await
    };

    match login_result {
        Ok(()) => {
            eprintln!("Successfully logged in account {}", account.id);
            Ok(())
        }
        Err(err) => {
            let _ = registry.delete_account(&account.id).await;
            Err(err)
        }
    }
}

pub async fn handle_set_api_key_command(
    config: &AppConfig,
    args: SetApiKeyArgs,
) -> anyhow::Result<()> {
    let mut proxy_auth = ProxyAuth::load_or_create(&config.data_dir)?;
    let api_key = args.value.unwrap_or_else(generate_api_key);
    proxy_auth.set_api_key(&api_key)?;
    println!("{api_key}");
    Ok(())
}

pub fn handle_api_key_status_command(config: &AppConfig) -> anyhow::Result<()> {
    let proxy_auth = ProxyAuth::load_or_create(&config.data_dir)?;
    if proxy_auth.is_configured() {
        println!("configured");
    } else {
        println!("not configured");
    }
    Ok(())
}

pub async fn handle_list_accounts_command(
    config: &AppConfig,
    args: ListAccountsArgs,
) -> anyhow::Result<()> {
    let registry = AccountRegistry::load_or_create(config.data_dir.clone()).await?;
    let mut accounts = registry.list_summaries().await;
    accounts.sort_by_key(|account| {
        account
            .usage
            .as_ref()
            .and_then(|usage| usage.secondary_resets_at)
            .unwrap_or(i64::MAX)
    });
    if accounts.is_empty() {
        println!("No accounts configured.");
        return Ok(());
    }

    for account in accounts {
        let identity = account
            .auth
            .email
            .clone()
            .unwrap_or_else(|| format!("account {}", account.id));
        let plan_suffix = account
            .auth
            .plan_type
            .clone()
            .map(|plan| format!(" ({plan})"))
            .unwrap_or_default();
        println!("- {identity}{plan_suffix}");

        if let Some(usage) = account.usage.clone() {
            if let Some(used_percent) = usage.primary_used_percent {
                println!(
                    "  {} {}, {}",
                    usage_indicator_bar(used_percent),
                    format_used_percent(used_percent),
                    compact_resets_in(usage.primary_resets_at)
                );
            }
            if let Some(used_percent) = usage.secondary_used_percent {
                println!(
                    "  {} {}, {}",
                    usage_indicator_bar(used_percent),
                    format_used_percent(used_percent),
                    compact_resets_in(usage.secondary_resets_at)
                );
            }
        } else if !account.auth.authenticated {
            println!("  not authenticated");
        }
        if let Some(cooldown_until) = account.cooldown_until {
            println!("  cooldown: {}", compact_resets_in(Some(cooldown_until)));
        }

        if args.verbose {
            println!("  id: {}", account.id);
            println!("  authenticated: {}", account.auth.authenticated);
            if let Some(workspace) = account.auth.account_id {
                println!("  workspace: {}", workspace);
            }
            println!("  state: {:?}", account.state);
            if let Some(usage) = account.usage {
                if let Some(limit_id) = usage.limit_id {
                    println!("  limit: {}", limit_id);
                }
                if let Some(plan_type) = usage.plan_type {
                    println!("  usage plan: {}", plan_type);
                }
            }
        }

        println!();
    }

    Ok(())
}

fn humanize_minutes(minutes: i64) -> String {
    let days = minutes / (60 * 24);
    let hours = (minutes % (60 * 24)) / 60;
    let mins = minutes % 60;

    let mut parts = Vec::new();
    if days > 0 {
        parts.push(format!("{days}d"));
        if hours > 0 {
            parts.push(format!("{hours}h"));
        }
        return parts.join(" ");
    }
    if hours > 0 {
        parts.push(format!("{hours}h"));
    }
    if mins > 0 || parts.is_empty() {
        parts.push(format!("{mins}m"));
    }
    parts.join(" ")
}

fn compact_resets_in(resets_at: Option<i64>) -> String {
    let Some(timestamp) = resets_at else {
        return "unknown".to_string();
    };
    let Some(reset_at) = DateTime::<Utc>::from_timestamp(timestamp, 0) else {
        return "unknown".to_string();
    };
    let delta = reset_at - Utc::now();
    let delta_minutes = delta.num_minutes();
    if delta_minutes > 0 {
        humanize_minutes(delta_minutes)
    } else {
        "now".to_string()
    }
}

fn format_used_percent(used_percent: f64) -> String {
    let clamped_percent = used_percent.clamp(0.0, 100.0);
    let mut formatted = format!("{:.0}%", clamped_percent.round());

    if used_percent >= 100.0 {
        formatted.push_str("(full)");
    }

    formatted
}

fn usage_indicator_bar(used_percent: f64) -> String {
    const BAR_WIDTH: usize = 12;
    let clamped_percent = used_percent.clamp(0.0, 100.0);
    let filled = ((clamped_percent / 100.0) * BAR_WIDTH as f64).round() as usize;
    let empty = BAR_WIDTH.saturating_sub(filled);
    format!("[{}{}]", "#".repeat(filled), "-".repeat(empty))
}

async fn login_with_browser(
    account: &StoredAccount,
    data_dir: &std::path::Path,
) -> anyhow::Result<()> {
    let opts = ServerOptions::new(
        account.codex_home(data_dir),
        CLIENT_ID.to_string(),
        /*forced_chatgpt_workspace_id*/ None,
        AuthCredentialsStoreMode::File,
    );
    let server = run_login_server(opts)?;
    eprintln!(
        "Starting local login server on http://localhost:{}.\nIf your browser did not open, navigate to this URL to authenticate:\n\n{}\n",
        server.actual_port, server.auth_url
    );
    server
        .block_until_done()
        .await
        .context("browser login failed")?;
    Ok(())
}

async fn login_with_device_code(
    account: &StoredAccount,
    data_dir: &std::path::Path,
) -> anyhow::Result<()> {
    let opts = ServerOptions::new(
        account.codex_home(data_dir),
        CLIENT_ID.to_string(),
        /*forced_chatgpt_workspace_id*/ None,
        AuthCredentialsStoreMode::File,
    );
    run_device_code_login(opts)
        .await
        .context("device-code login failed")?;
    Ok(())
}
