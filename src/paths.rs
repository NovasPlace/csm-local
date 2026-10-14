use std::env;
use std::path::PathBuf;

use crate::{Error, Result};

/// Root of everything csm stores: `$CSM_HOME`, otherwise `~/.csm`.
///
/// Deliberately not under AppData: the Claude and Codex desktop apps are
/// MSIX packages, and Windows redirects their AppData writes into private
/// per-app stores, which would split one user's memory into several
/// databases. The home directory is shared by every host.
pub fn data_dir() -> Result<PathBuf> {
    if let Some(home) = env::var_os("CSM_HOME").filter(|v| !v.is_empty()) {
        let dir = PathBuf::from(home);
        if !dir.is_absolute() {
            return Err(Error::Invalid("CSM_HOME must be an absolute path".into()));
        }
        return Ok(dir);
    }
    home_dir()
        .map(|home| home.join(".csm"))
        .ok_or_else(|| Error::Invalid("cannot find the home directory; set CSM_HOME".into()))
}

/// `$CSM_DB` when set (must be absolute), otherwise `csm.db` in the data dir.
pub fn default_db_path() -> Result<PathBuf> {
    if let Some(db) = env::var_os("CSM_DB").filter(|v| !v.is_empty()) {
        let path = PathBuf::from(db);
        if !path.is_absolute() {
            return Err(Error::Invalid("CSM_DB must be an absolute path".into()));
        }
        return Ok(path);
    }
    Ok(data_dir()?.join("csm.db"))
}

pub fn hook_log_path() -> Result<PathBuf> {
    Ok(data_dir()?.join("hook-errors.log"))
}

pub fn home_dir() -> Option<PathBuf> {
    let var = |name: &str| env::var_os(name).filter(|v| !v.is_empty()).map(PathBuf::from);
    if cfg!(windows) {
        var("USERPROFILE").or_else(|| var("HOME"))
    } else {
        var("HOME")
    }
}
