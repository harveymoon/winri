//! User configuration loaded from `<config_dir>/config.toml`.
//!
//! Config is read at startup and cached in a process-wide `RwLock`. It can be
//! re-read at runtime via `reload()` (currently triggered by the Win+R force
//! refresh action). Filter changes (ignored processes/classes) apply on the
//! next window enumeration; padding/resize_increment changes require a winri
//! restart because the tiler captures them at construction.

use std::{
    path::PathBuf,
    sync::{OnceLock, RwLock, RwLockReadGuard},
};

use anyhow::Context;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub tiling: TilingConfig,
    pub filter: FilterConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct TilingConfig {
    pub padding: f32,
    pub resize_increment: f32,
}

impl Default for TilingConfig {
    fn default() -> Self {
        Self {
            padding: 10.0,
            resize_increment: 20.0,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct FilterConfig {
    pub ignored_processes: Vec<String>,
    pub ignored_classes: Vec<String>,
}

const DEFAULT_CONFIG_TOML: &str = r#"# Winri configuration
#
# Reload at runtime: Win+R (filter changes apply immediately, padding/
# resize_increment require a winri restart).
# Manual edits: this file lives at %APPDATA%\winri\config.toml.

[tiling]
# Gap between tiled windows, in pixels.
padding = 10.0
# How much Win+Shift+Left/Right resizes the focused window by.
resize_increment = 20.0

[filter]
# Extra process executables (.exe filename, case-sensitive) to exempt from
# tiling. Exempt apps float freely above the tile strip.
ignored_processes = [
    # "MyOverlayApp.exe",
]

# Extra Win32 window class names to exempt from tiling.
ignored_classes = [
    # "SomeOverlayClass",
]
"#;

static CONFIG: OnceLock<RwLock<Config>> = OnceLock::new();

fn config_path() -> anyhow::Result<PathBuf> {
    Ok(crate::root_dir()?.join("config.toml"))
}

fn load_from_disk() -> anyhow::Result<Config> {
    let path = config_path()?;
    if !path.exists() {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating config dir {}", parent.display()))?;
        }
        std::fs::write(&path, DEFAULT_CONFIG_TOML)
            .with_context(|| format!("writing default config to {}", path.display()))?;
        log::info!("Wrote default config to {}", path.display());
        return Ok(Config::default());
    }
    let body = std::fs::read_to_string(&path)
        .with_context(|| format!("reading config {}", path.display()))?;
    let cfg: Config =
        toml::from_str(&body).with_context(|| format!("parsing config {}", path.display()))?;
    log::info!(
        "Loaded config from {} (ignored_processes={}, ignored_classes={})",
        path.display(),
        cfg.filter.ignored_processes.len(),
        cfg.filter.ignored_classes.len(),
    );
    Ok(cfg)
}

/// Load the config from disk and store it in the global slot. Idempotent —
/// safe to call repeatedly; subsequent calls behave like `reload()`.
pub fn init() -> anyhow::Result<()> {
    let cfg = load_from_disk()?;
    match CONFIG.set(RwLock::new(cfg.clone())) {
        Ok(()) => Ok(()),
        Err(_) => {
            *CONFIG.get().expect("set above").write().expect("poisoned") = cfg;
            Ok(())
        }
    }
}

/// Re-read the config from disk into the global slot. On failure, the
/// previous config is kept unchanged.
pub fn reload() -> anyhow::Result<()> {
    let cfg = load_from_disk()?;
    let lock = CONFIG.get_or_init(|| RwLock::new(Config::default()));
    *lock.write().expect("config rwlock poisoned") = cfg;
    Ok(())
}

/// Write the given config to disk and replace the in-memory copy.
pub fn save(cfg: Config) -> anyhow::Result<()> {
    let path = config_path()?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating config dir {}", parent.display()))?;
    }
    let body =
        toml::to_string_pretty(&cfg).with_context(|| "serializing config to TOML".to_string())?;
    std::fs::write(&path, body)
        .with_context(|| format!("writing config to {}", path.display()))?;
    log::info!("Saved config to {}", path.display());

    let lock = CONFIG.get_or_init(|| RwLock::new(Config::default()));
    *lock.write().expect("config rwlock poisoned") = cfg;
    Ok(())
}

/// Read-locked view of the current config.
pub fn current() -> RwLockReadGuard<'static, Config> {
    CONFIG
        .get_or_init(|| RwLock::new(Config::default()))
        .read()
        .expect("config rwlock poisoned")
}
