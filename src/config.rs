use anyhow::Context;
use std::env;
use std::net::SocketAddr;
use std::path::PathBuf;

#[derive(Clone, Debug)]
pub struct AppConfig {
    pub listen_addr: SocketAddr,
    pub data_dir: PathBuf,
    pub request_timeout_secs: u64,
}

impl AppConfig {
    pub fn from_env() -> anyhow::Result<Self> {
        let listen_addr = env::var("OCP_LISTEN_ADDR")
            .unwrap_or_else(|_| "127.0.0.1:8080".to_string())
            .parse()
            .context("failed to parse OCP_LISTEN_ADDR")?;

        let data_dir = env::var("OCP_DATA_DIR")
            .ok()
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("data"));

        let request_timeout_secs = env::var("OCP_REQUEST_TIMEOUT_SECS")
            .ok()
            .map(|value| value.parse::<u64>())
            .transpose()
            .context("failed to parse OCP_REQUEST_TIMEOUT_SECS")?
            .unwrap_or(600);

        Ok(Self {
            listen_addr,
            data_dir,
            request_timeout_secs,
        })
    }
}
