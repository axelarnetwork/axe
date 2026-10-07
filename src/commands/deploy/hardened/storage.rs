use std::fs::{File, OpenOptions};
use std::io::Write;
#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use alloy::primitives::keccak256;
use eyre::{Result, WrapErr};

use crate::state::State;

pub fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    write(path, bytes, false)
}

pub fn atomic_config_write(path: &Path, bytes: &[u8]) -> Result<()> {
    let resolved = std::fs::canonicalize(path)?;
    write(&resolved, bytes, true)
}

fn write(path: &Path, bytes: &[u8], preserve_mode: bool) -> Result<()> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent)?;
    let temporary = path.with_extension(format!("{}.tmp", std::process::id()));
    let mut options = OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use_mode(&mut options);
    }
    let mut file = options.open(&temporary)?;
    if preserve_mode {
        file.set_permissions(std::fs::metadata(path)?.permissions())?;
    }
    file.write_all(bytes)?;
    file.sync_all()?;
    std::fs::rename(&temporary, path)?;
    File::open(parent)?.sync_all()?;
    Ok(())
}

#[cfg(unix)]
fn use_mode(options: &mut OpenOptions) {
    options.mode(0o600);
}

pub fn directory(state: &State) -> Result<PathBuf> {
    eyre::ensure!(
        state
            .axelar_id
            .as_str()
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-'),
        "chain identifier must contain only ASCII letters, digits or hyphens"
    );
    Ok(crate::state::data_dir()?
        .join("deployments")
        .join(state.env.as_str())
        .join(state.axelar_id.as_str()))
}

pub fn lock(path: &Path) -> Result<File> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(path)?;
    file.try_lock()
        .wrap_err("another deployment process holds the lock")?;
    Ok(file)
}

pub fn config_lock(path: &Path) -> Result<File> {
    let canonical = std::fs::canonicalize(path)?;
    let identity = keccak256(canonical.as_os_str().as_encoded_bytes());
    lock(
        &crate::state::data_dir()?
            .join("locks")
            .join(format!("config-{identity}.lock")),
    )
}
