use serde::Deserialize;
use serde::Serialize;
use sha2::Digest;
use sha2::Sha256;
use std::path::Path;
use std::path::PathBuf;
use uuid::Uuid;

#[derive(Clone, Debug)]
pub struct ProxyAuth {
    path: PathBuf,
    key_hash: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
struct ProxyAuthFile {
    key_hash: String,
}

impl ProxyAuth {
    pub fn load_or_create(data_dir: &Path) -> anyhow::Result<Self> {
        std::fs::create_dir_all(data_dir)?;
        let path = data_dir.join("proxy_auth.json");
        let key_hash = std::fs::read_to_string(&path)
            .ok()
            .and_then(|raw| serde_json::from_str::<ProxyAuthFile>(&raw).ok())
            .map(|file| file.key_hash);
        Ok(Self { path, key_hash })
    }

    pub fn set_api_key(&mut self, api_key: &str) -> anyhow::Result<()> {
        let key_hash = hash_api_key(api_key);
        let file = ProxyAuthFile {
            key_hash: key_hash.clone(),
        };
        std::fs::write(&self.path, serde_json::to_string_pretty(&file)?)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&self.path, std::fs::Permissions::from_mode(0o600))?;
        }
        self.key_hash = Some(key_hash);
        Ok(())
    }

    pub fn verify_bearer_token(&self, bearer_token: &str) -> bool {
        self.key_hash
            .as_ref()
            .is_some_and(|key_hash| *key_hash == hash_api_key(bearer_token))
    }

    pub fn is_configured(&self) -> bool {
        self.key_hash.is_some()
    }
}

pub fn generate_api_key() -> String {
    format!("ocp_{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple())
}

fn hash_api_key(api_key: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(api_key.as_bytes());
    let digest = hasher.finalize();
    let mut output = String::with_capacity(digest.len() * 2);
    for byte in digest {
        use std::fmt::Write as _;
        let _ = write!(output, "{byte:02x}");
    }
    output
}
