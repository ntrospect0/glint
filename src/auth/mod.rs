pub mod google;

use std::path::PathBuf;

use anyhow::{Context, Result};

use crate::config;

/// Returns `~/.config/glint/credentials/`, creating it (mode 0700 on Unix) on
/// first use.
pub fn credentials_dir() -> Result<PathBuf> {
    let dir = config::config_dir()?.join("credentials");
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("failed to create credentials dir at {}", dir.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700));
    }
    Ok(dir)
}
