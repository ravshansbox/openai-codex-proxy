use serde::Deserialize;
use serde::Serialize;
use std::path::Path;
use std::path::PathBuf;
use uuid::Uuid;

#[derive(Clone, Debug)]
pub struct ProxyAuth {
    path: PathBuf,
    api_key: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
struct ProxyAuthFile {
    api_key: String,
}

impl ProxyAuth {
    pub fn load_or_create(data_dir: &Path) -> anyhow::Result<Self> {
        std::fs::create_dir_all(data_dir)?;
        let path = data_dir.join("proxy_auth.json");
        let api_key = std::fs::read_to_string(&path)
            .ok()
            .and_then(|raw| serde_json::from_str::<ProxyAuthFile>(&raw).ok())
            .map(|file| file.api_key);
        Ok(Self { path, api_key })
    }

    pub fn set_api_key(&mut self, api_key: &str) -> anyhow::Result<()> {
        let file = ProxyAuthFile {
            api_key: api_key.to_string(),
        };
        std::fs::write(&self.path, serde_json::to_string_pretty(&file)?)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&self.path, std::fs::Permissions::from_mode(0o600))?;
        }
        self.api_key = Some(api_key.to_string());
        Ok(())
    }

    pub fn verify_bearer_token(&self, bearer_token: &str) -> bool {
        self.api_key.as_ref().is_some_and(|key| key == bearer_token)
    }

    pub fn is_configured(&self) -> bool {
        self.api_key.is_some()
    }

    pub fn api_key(&self) -> Option<&str> {
        self.api_key.as_deref()
    }
}

pub fn generate_api_key() -> String {
    format!("ocp_{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple())
}
