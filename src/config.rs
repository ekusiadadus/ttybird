use crate::model::Binding;
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    path::{Path, PathBuf},
};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Host {
    pub name: String,
    pub binary: String,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub hosts: Vec<Host>,
}

pub struct MutationGuard(PathBuf);
impl Drop for MutationGuard {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

/// Covers the complete read/modify/write transaction, not just file replacement.
pub fn mutation_guard(dir: &Path) -> Result<MutationGuard> {
    fs::create_dir_all(dir)?;
    let path = dir.join("mutation.lock");
    fs::OpenOptions::new().write(true).create_new(true).open(&path)
        .context("another configuration mutation or interrupted transaction; inspect mutation.lock before removing")?;
    Ok(MutationGuard(path))
}

pub fn home() -> Result<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .context("HOME is not set; specify --config-dir")
}

pub fn directory() -> Result<PathBuf> {
    if let Some(p) = std::env::var_os("TTYBIRD_CONFIG_DIR") {
        return Ok(p.into());
    }
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or(home()?.join(".config"));
    Ok(base.join("ttybird"))
}

pub fn read(dir: &Path) -> Result<Config> {
    let path = dir.join("config.toml");
    if !path.exists() {
        return Ok(Config::default());
    }
    let text = fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
    toml::from_str(&text).context("invalid config.toml")
}

pub fn write(dir: &Path, config: &Config) -> Result<()> {
    atomic_write(
        &dir.join("config.toml"),
        toml::to_string_pretty(config)?.as_bytes(),
    )
}

pub fn bindings(dir: &Path) -> Result<Vec<Binding>> {
    let path = dir.join("bindings.json");
    if !path.exists() {
        return Ok(vec![]);
    }
    serde_json::from_slice(&fs::read(path)?).context("invalid bindings.json")
}

pub fn save_bindings(dir: &Path, bindings: &[Binding]) -> Result<()> {
    atomic_write(
        &dir.join("bindings.json"),
        &serde_json::to_vec_pretty(bindings)?,
    )
}

/// Atomic replacement prevents interrupted writes from leaving partially valid records.
/// Mutations are explicit CLI actions; concurrent mutations are refused with a lock file.
pub fn atomic_write(path: &Path, data: &[u8]) -> Result<()> {
    use std::io::Write;
    let parent = path.parent().context("path has no parent")?;
    fs::create_dir_all(parent)?;
    let lock = path.with_extension("lock");
    let guard = fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&lock)
        .with_context(|| {
            format!(
                "another writer or interrupted write: {}; inspect before removing",
                lock.display()
            )
        })?;
    let tmp = path.with_extension(format!("{}.tmp", std::process::id()));
    let result = (|| -> Result<()> {
        #[cfg(unix)]
        use std::os::unix::fs::OpenOptionsExt;
        let mut options = fs::OpenOptions::new();
        options.create_new(true).write(true);
        #[cfg(unix)]
        options.mode(0o600);
        let mut file = options.open(&tmp)?;
        file.write_all(data)?;
        file.sync_all()?;
        fs::rename(&tmp, path)?;
        fs::File::open(parent)?.sync_all()?;
        Ok(())
    })();
    drop(guard);
    let _ = fs::remove_file(&tmp);
    let _ = fs::remove_file(&lock);
    result
}

pub fn add_host(config: &mut Config, name: String, binary: String) -> Result<()> {
    if config.hosts.iter().any(|h| h.name == name) {
        bail!("host already registered: {name}");
    }
    config.hosts.push(Host { name, binary });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn refuses_existing_lock_and_preserves_data() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("state.json");
        atomic_write(&p, b"old").unwrap();
        fs::write(p.with_extension("lock"), b"").unwrap();
        assert!(atomic_write(&p, b"new").is_err());
        assert_eq!(fs::read(p).unwrap(), b"old");
    }
}
