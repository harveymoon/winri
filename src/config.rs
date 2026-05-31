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
    pub api: ApiConfig,
    pub monitors: MonitorsConfig,
    /// Per-app display overrides. Apps whose `process_name` doesn't
    /// uniquely identify them (notably Electron apps, which all report
    /// `electron.exe`) can be matched by exe path substring and given
    /// a friendlier display name and custom icon.
    #[serde(default)]
    pub app_overrides: Vec<AppOverride>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppOverride {
    /// Substring that must appear in the window's full exe path for
    /// this override to apply. Case-insensitive. Example:
    /// `"cool_browser"` matches `C:/CODE/cool_browser/build/electron.exe`.
    pub exe_path_contains: String,
    /// Display name to show in the overview and settings instead of the
    /// (often misleading) executable basename.
    pub display_name: String,
    /// Optional path to a `.ico` (or any `LoadImageW`-readable image)
    /// to render as the app icon in overview tiles. If omitted, falls
    /// back to whatever the window itself exposes via `WM_GETICON`
    /// (which for Electron dev builds is usually the generic Electron
    /// icon — exactly what this override is fixing).
    #[serde(default)]
    pub icon_path: Option<String>,
}

impl AppOverride {
    /// Does this override apply to `exe_path`? Match is case-insensitive
    /// because the user might type `cool_browser` but the actual path
    /// has `Cool_Browser` or vice versa.
    pub fn matches(&self, exe_path: &str) -> bool {
        exe_path
            .to_lowercase()
            .contains(&self.exe_path_contains.to_lowercase())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct MonitorsConfig {
    /// Which monitor hosts the tile strip. `"primary"` or a Win32 device
    /// name like `\\.\DISPLAY1`. Windows on other monitors are left
    /// untouched by winri.
    pub tiling_monitor: String,
}

impl Default for MonitorsConfig {
    fn default() -> Self {
        Self {
            tiling_monitor: "primary".to_string(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ApiConfig {
    /// Whether to start the local HTTP control API on launch.
    pub enabled: bool,
    /// Bind address. Default is loopback only — exposing winri's control API
    /// to other hosts is almost always a mistake.
    pub bind: String,
    /// TCP port for the HTTP API.
    pub port: u16,
}

impl Default for ApiConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            bind: "127.0.0.1".to_string(),
            port: 47812,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct TilingConfig {
    pub padding: f32,
    pub resize_increment: f32,
    /// Animate scroll/focus changes instead of snapping. When false the
    /// tiler still functions identically — values just jump.
    pub smooth_scroll: bool,
    /// Per-tick interpolation factor in [0.05, 1.0]. Higher = snappier,
    /// lower = floatier. At 60 fps, 0.2 settles to within 1% in ~22 frames
    /// (~360 ms); 0.4 in ~9 frames (~150 ms).
    pub smooth_scroll_factor: f32,
    /// Throttle SetWindowPos rate for known-slow apps (currently
    /// `explorer.exe`) during scroll animations so their UI thread can
    /// keep up. With this off, Explorer trails the strip visually by
    /// hundreds-to-thousands of pixels during fast scrolls. Other apps
    /// are unaffected either way.
    pub throttle_slow_apps: bool,
}

impl Default for TilingConfig {
    fn default() -> Self {
        Self {
            padding: 10.0,
            resize_increment: 20.0,
            smooth_scroll: true,
            smooth_scroll_factor: 0.25,
            throttle_slow_apps: true,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct FilterConfig {
    pub ignored_processes: Vec<String>,
    pub ignored_classes: Vec<String>,
    /// Per-window persistent ignore. Each entry exempts a specific window
    /// of a specific app — e.g. an app's settings popup that you never
    /// want tiled. A window is ignored when its `process_name` equals
    /// `process` and its current title is exactly `title`.
    pub ignored_window_titles: Vec<IgnoredWindowTitle>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IgnoredWindowTitle {
    pub process: String,
    /// Exact title match. Set this OR `title_starts_with`, not both.
    /// `Option` (rather than required `String`) so a config entry can
    /// use the prefix-match form instead.
    #[serde(default)]
    pub title: Option<String>,
    /// Title prefix match. Use for apps whose ignored window has a
    /// constant prefix and varying suffix — e.g. Sigma File Manager's
    /// Quick View popup titles as `"Sigma File Manager | Quick View
    /// - IMG_6135.jpg"` where the filename changes per-file.
    #[serde(default)]
    pub title_starts_with: Option<String>,
    /// Optional Win32 window class. When present the match also requires
    /// `class` equality, which keeps a popup from accidentally silencing
    /// its app's main window when both share the same class but differ
    /// only by title. Older config entries without `class` fall back to
    /// the previous (process, title)-only behaviour.
    #[serde(default)]
    pub class: Option<String>,
}

impl IgnoredWindowTitle {
    /// Does this rule match the given window? `process` and `class` are
    /// strict (when `class` is `Some`); title is either exact or
    /// prefix-matched per the rule's configuration.
    pub fn matches(&self, process: &str, title: &str, class: &str) -> bool {
        if self.process != process {
            return false;
        }
        let title_ok = if let Some(prefix) = &self.title_starts_with {
            title.starts_with(prefix.as_str())
        } else if let Some(exact) = &self.title {
            exact == title
        } else {
            // Neither title nor title_starts_with set — invalid rule;
            // treat as non-matching so users see a stale config behave
            // safely (window stays tiled) rather than nuking everything
            // from this process.
            false
        };
        if !title_ok {
            return false;
        }
        self.class.as_deref().is_none_or(|c| c == class)
    }
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
# Animate scroll & focus jumps instead of snapping.
smooth_scroll = true
# Snappiness of the smoothing (0.05 = floaty, 1.0 = effectively snap).
smooth_scroll_factor = 0.25
# Throttle SetWindowPos rate for known-slow apps (File Explorer) so they
# stay in sync with the strip during fast scrolls. Other apps unaffected.
throttle_slow_apps = true

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

# Persistent per-window ignores: a window is exempted from tiling when
# its process AND title both match an entry below. Use the overview's
# right-click "Ignore this window" action to add these.
ignored_window_titles = [
    # { process = "Code.exe", title = "Welcome - Visual Studio Code" },
]

[api]
# Local HTTP control API. When enabled, exposes endpoints to enumerate
# tiled windows, focus them, scroll, fire keyboard-equivalent actions, and
# fetch live thumbnails — useful for Stream Deck plugins, Python macros,
# voice control, etc. See INTEGRATION.md.
enabled = false
bind = "127.0.0.1"
port = 47812

[monitors]
# Which monitor hosts the tile strip. "primary" follows the system primary
# monitor; alternatively use a Win32 device name like "\\.\DISPLAY2".
# Windows on other monitors are left as normal floating windows.
tiling_monitor = "primary"

# Per-app display overrides — give Electron apps (and any others whose
# process name is generic) a friendlier label and custom icon in the
# overview. Match is case-insensitive substring on the full exe path.
# Example:
# [[app_overrides]]
# exe_path_contains = "cool_browser"
# display_name = "cool"
# icon_path = "C:/CODE/cool_browser/build/icon.ico"
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
///
/// The write is atomic from the reader's perspective: we write to a
/// sibling temp file and then `rename` it over the real config. If
/// winri crashes mid-write the original config stays intact and the
/// `.tmp` file can be cleaned up on next launch.
pub fn save(cfg: Config) -> anyhow::Result<()> {
    let path = config_path()?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating config dir {}", parent.display()))?;
    }
    let body =
        toml::to_string_pretty(&cfg).with_context(|| "serializing config to TOML".to_string())?;

    let tmp_path = path.with_extension("toml.tmp");
    std::fs::write(&tmp_path, &body)
        .with_context(|| format!("writing temp config to {}", tmp_path.display()))?;
    std::fs::rename(&tmp_path, &path).with_context(|| {
        format!(
            "rename {} -> {}",
            tmp_path.display(),
            path.display()
        )
    })?;
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
