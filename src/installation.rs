use std::path::Path;
use std::path::PathBuf;
use uuid::Uuid;

const INSTALLATION_ID_FILENAME: &str = "installation_id";

pub fn load_or_create_installation_id(data_dir: &Path) -> std::io::Result<String> {
    std::fs::create_dir_all(data_dir)?;
    let path: PathBuf = data_dir.join(INSTALLATION_ID_FILENAME);
    if let Ok(existing) = std::fs::read_to_string(&path) {
        let trimmed = existing.trim();
        if !trimmed.is_empty() {
            return Ok(trimmed.to_string());
        }
    }
    let installation_id = Uuid::new_v4().to_string();
    std::fs::write(&path, &installation_id)?;
    Ok(installation_id)
}
