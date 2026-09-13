//! Configuration loading for the hook, resolved from a JSON file with environment-variable overrides on top.

use std::path::{Path, PathBuf};

/// Which part of the turn gets rendered.
#[derive(Clone, Copy, PartialEq)]
pub enum Scope {
    /// Every assistant text block since the last human message, in order. This is the whole visible turn.
    Turn,
    /// Only the final assistant message, which is usually the summary Claude ends the turn on.
    Last,
}

/// How the output file is named on each turn.
#[derive(Clone, Copy, PartialEq)]
pub enum FileMode {
    /// Reuse one file per session so a single browser tab can keep showing the newest answer.
    Overwrite,
    /// Write a new timestamped file every turn, building a browsable history of the session.
    Timestamped,
}

/// Colour scheme baked into the generated page.
#[derive(Clone, Copy, PartialEq)]
pub enum Theme {
    Auto,
    Light,
    Dark,
}

/// The fully resolved settings the rest of the program reads.
pub struct Config {
    /// Master switch. When false the hook exits immediately without doing any work.
    pub enabled: bool,
    /// Whether to launch a browser at all. False still writes the HTML file, which is handy with an editor preview.
    pub auto_open: bool,
    /// Launch the browser only on the first render of a session, letting later turns refresh the open tab instead.
    pub open_once_per_session: bool,
    /// Open the page again when a new answer lands and the tab that would have refreshed itself is gone.
    ///
    /// Only the live-reload server can tell an open tab from a closed one, so this does nothing when `live_reload` is off or the server could not start.
    pub reopen_if_closed: bool,
    pub scope: Scope,
    /// Include the model thinking blocks above the answer.
    pub include_thinking: bool,
    /// Directory for generated HTML. Empty means a `c2md` folder inside the system temp directory.
    pub output_dir: PathBuf,
    pub file_mode: FileMode,
    /// Browser command to spawn. Empty means whatever the OS has registered for the file.
    pub browser: String,
    pub theme: Theme,
    /// Text shown in the page title and header.
    pub title: String,
    /// Answers shorter than this are not worth a browser tab, so they are skipped.
    pub min_chars: usize,
    /// How long the hook will wait, in milliseconds, for Claude Code to finish writing the turn's last message before rendering what it can see.
    ///
    /// The hook is started off the same event as that write, so it regularly gets there first. Nothing waits when the answer is already on disk, which is the ordinary case; this only bounds how long a turn that ends without any prose at all can stall the hook.
    pub settle_ms: u64,
    /// Serve the page from a loopback server that pushes updates, so the tab changes only when the answer actually does.
    pub live_reload: bool,
    /// Port for that server. Zero lets the OS pick a free one, which is what you want unless a firewall rule needs a fixed number.
    pub port: u16,
    /// Shut the server down after this many seconds with no page watching it and no new answers.
    pub server_idle_secs: u64,
    /// Seconds between page self-refreshes in the `file://` fallback, used only when `live_reload` is off or the server cannot start. Zero disables it.
    pub auto_refresh_secs: u32,
    /// Load highlight.js from a CDN to colour fenced code blocks.
    pub code_highlight: bool,
    /// Append a compact timing line per run to `c2md.log` in the output directory.
    pub log: bool,
    /// Print the page URL back into the Claude Code transcript after each render.
    pub notify: bool,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            enabled: true,
            auto_open: true,
            open_once_per_session: true,
            reopen_if_closed: true,
            scope: Scope::Turn,
            include_thinking: false,
            output_dir: PathBuf::new(),
            file_mode: FileMode::Overwrite,
            browser: String::new(),
            theme: Theme::Auto,
            title: "Claude Code".to_string(),
            min_chars: 1,
            settle_ms: 2000,
            live_reload: true,
            port: 0,
            server_idle_secs: 1800,
            auto_refresh_secs: 2,
            code_highlight: true,
            log: false,
            notify: false,
        }
    }
}

impl Config {
    /// Reads the config file if present, then applies `C2MD_*` environment overrides, which always win.
    pub fn load() -> Config {
        let mut cfg = Config::default();
        if let Some(path) = config_path() {
            if let Ok(text) = std::fs::read_to_string(&path) {
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) {
                    cfg.apply_json(&v);
                }
            }
        }
        cfg.apply_env();
        cfg
    }

    fn apply_json(&mut self, v: &serde_json::Value) {
        if let Some(b) = v["enabled"].as_bool() { self.enabled = b; }
        if let Some(b) = v["auto_open"].as_bool() { self.auto_open = b; }
        if let Some(b) = v["open_once_per_session"].as_bool() { self.open_once_per_session = b; }
        if let Some(b) = v["reopen_if_closed"].as_bool() { self.reopen_if_closed = b; }
        if let Some(b) = v["include_thinking"].as_bool() { self.include_thinking = b; }
        if let Some(b) = v["code_highlight"].as_bool() { self.code_highlight = b; }
        if let Some(b) = v["log"].as_bool() { self.log = b; }
        if let Some(b) = v["notify"].as_bool() { self.notify = b; }
        if let Some(b) = v["live_reload"].as_bool() { self.live_reload = b; }
        if let Some(n) = v["port"].as_u64() { self.port = n as u16; }
        if let Some(n) = v["server_idle_secs"].as_u64() { self.server_idle_secs = n; }
        if let Some(s) = v["scope"].as_str() { self.scope = parse_scope(s).unwrap_or(self.scope); }
        if let Some(s) = v["file_mode"].as_str() { self.file_mode = parse_file_mode(s).unwrap_or(self.file_mode); }
        if let Some(s) = v["theme"].as_str() { self.theme = parse_theme(s).unwrap_or(self.theme); }
        if let Some(s) = v["output_dir"].as_str() { if !s.is_empty() { self.output_dir = PathBuf::from(s); } }
        if let Some(s) = v["browser"].as_str() { self.browser = s.to_string(); }
        if let Some(s) = v["title"].as_str() { self.title = s.to_string(); }
        if let Some(n) = v["min_chars"].as_u64() { self.min_chars = n as usize; }
        if let Some(n) = v["settle_ms"].as_u64() { self.settle_ms = n; }
        if let Some(n) = v["auto_refresh_secs"].as_u64() { self.auto_refresh_secs = n as u32; }
    }

    fn apply_env(&mut self) {
        if let Some(b) = env_bool("C2MD_ENABLED") { self.enabled = b; }
        if let Some(b) = env_bool("C2MD_AUTO_OPEN") { self.auto_open = b; }
        if let Some(b) = env_bool("C2MD_OPEN_ONCE_PER_SESSION") { self.open_once_per_session = b; }
        if let Some(b) = env_bool("C2MD_REOPEN_IF_CLOSED") { self.reopen_if_closed = b; }
        if let Some(b) = env_bool("C2MD_INCLUDE_THINKING") { self.include_thinking = b; }
        if let Some(b) = env_bool("C2MD_CODE_HIGHLIGHT") { self.code_highlight = b; }
        if let Some(b) = env_bool("C2MD_LOG") { self.log = b; }
        if let Some(b) = env_bool("C2MD_NOTIFY") { self.notify = b; }
        if let Some(b) = env_bool("C2MD_LIVE_RELOAD") { self.live_reload = b; }
        if let Ok(s) = std::env::var("C2MD_PORT") { if let Ok(n) = s.parse() { self.port = n; } }
        if let Ok(s) = std::env::var("C2MD_SERVER_IDLE_SECS") { if let Ok(n) = s.parse() { self.server_idle_secs = n; } }
        if let Ok(s) = std::env::var("C2MD_SCOPE") { if let Some(v) = parse_scope(&s) { self.scope = v; } }
        if let Ok(s) = std::env::var("C2MD_FILE_MODE") { if let Some(v) = parse_file_mode(&s) { self.file_mode = v; } }
        if let Ok(s) = std::env::var("C2MD_THEME") { if let Some(v) = parse_theme(&s) { self.theme = v; } }
        if let Ok(s) = std::env::var("C2MD_OUTPUT_DIR") { if !s.is_empty() { self.output_dir = PathBuf::from(s); } }
        if let Ok(s) = std::env::var("C2MD_BROWSER") { self.browser = s; }
        if let Ok(s) = std::env::var("C2MD_TITLE") { self.title = s; }
        if let Ok(s) = std::env::var("C2MD_MIN_CHARS") { if let Ok(n) = s.parse() { self.min_chars = n; } }
        if let Ok(s) = std::env::var("C2MD_SETTLE_MS") { if let Ok(n) = s.parse() { self.settle_ms = n; } }
        if let Ok(s) = std::env::var("C2MD_AUTO_REFRESH_SECS") { if let Ok(n) = s.parse() { self.auto_refresh_secs = n; } }
    }

    /// Resolves the directory that generated pages are written to, creating it if it does not exist yet.
    pub fn resolved_output_dir(&self) -> PathBuf {
        let dir = if self.output_dir.as_os_str().is_empty() {
            std::env::temp_dir().join("c2md")
        } else {
            self.output_dir.clone()
        };
        let _ = std::fs::create_dir_all(&dir);
        dir
    }

    /// Renders the settings back to pretty JSON, used by `c2md init` and `c2md config`.
    pub fn to_json(&self) -> String {
        let v = serde_json::json!({
            "enabled": self.enabled,
            "auto_open": self.auto_open,
            "open_once_per_session": self.open_once_per_session,
            "reopen_if_closed": self.reopen_if_closed,
            "scope": match self.scope { Scope::Turn => "turn", Scope::Last => "last" },
            "include_thinking": self.include_thinking,
            "output_dir": self.output_dir.to_string_lossy(),
            "file_mode": match self.file_mode { FileMode::Overwrite => "overwrite", FileMode::Timestamped => "timestamped" },
            "browser": self.browser,
            "theme": match self.theme { Theme::Auto => "auto", Theme::Light => "light", Theme::Dark => "dark" },
            "title": self.title,
            "min_chars": self.min_chars,
            "settle_ms": self.settle_ms,
            "live_reload": self.live_reload,
            "port": self.port,
            "server_idle_secs": self.server_idle_secs,
            "auto_refresh_secs": self.auto_refresh_secs,
            "code_highlight": self.code_highlight,
            "log": self.log,
            "notify": self.notify,
        });
        serde_json::to_string_pretty(&v).unwrap_or_default()
    }
}

/// Location of the config file, kept next to the Claude Code settings so everything lives in one place.
pub fn config_path() -> Option<PathBuf> {
    if let Ok(explicit) = std::env::var("C2MD_CONFIG") {
        if !explicit.is_empty() {
            return Some(PathBuf::from(explicit));
        }
    }
    claude_dir().map(|d| d.join("c2md.json"))
}

/// The `~/.claude` directory, which is where Claude Code keeps settings, projects and transcripts.
pub fn claude_dir() -> Option<PathBuf> {
    if let Ok(d) = std::env::var("CLAUDE_CONFIG_DIR") {
        if !d.is_empty() {
            return Some(PathBuf::from(d));
        }
    }
    home_dir().map(|h| h.join(".claude"))
}

/// Resolves the home directory without pulling in a crate, covering both Unix and Windows conventions.
pub fn home_dir() -> Option<PathBuf> {
    if let Ok(h) = std::env::var("HOME") {
        if !h.is_empty() {
            return Some(PathBuf::from(h));
        }
    }
    if let Ok(p) = std::env::var("USERPROFILE") {
        if !p.is_empty() {
            return Some(PathBuf::from(p));
        }
    }
    match (std::env::var("HOMEDRIVE"), std::env::var("HOMEPATH")) {
        (Ok(d), Ok(p)) if !d.is_empty() => Some(PathBuf::from(format!("{d}{p}"))),
        _ => None,
    }
}

/// Shortens a path for display by folding the home directory back into a `~`.
pub fn tildify(p: &Path) -> String {
    if let Some(home) = home_dir() {
        if let Ok(rest) = p.strip_prefix(&home) {
            return format!("~{}{}", std::path::MAIN_SEPARATOR, rest.display());
        }
    }
    p.display().to_string()
}

fn parse_scope(s: &str) -> Option<Scope> {
    match s.to_ascii_lowercase().as_str() {
        "turn" | "all" => Some(Scope::Turn),
        "last" | "final" | "message" => Some(Scope::Last),
        _ => None,
    }
}

fn parse_file_mode(s: &str) -> Option<FileMode> {
    match s.to_ascii_lowercase().as_str() {
        "overwrite" | "single" => Some(FileMode::Overwrite),
        "timestamped" | "history" => Some(FileMode::Timestamped),
        _ => None,
    }
}

fn parse_theme(s: &str) -> Option<Theme> {
    match s.to_ascii_lowercase().as_str() {
        "auto" | "system" => Some(Theme::Auto),
        "light" => Some(Theme::Light),
        "dark" => Some(Theme::Dark),
        _ => None,
    }
}

/// Reads a boolean environment variable, accepting the usual spellings people reach for.
fn env_bool(key: &str) -> Option<bool> {
    let raw = std::env::var(key).ok()?;
    match raw.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Some(true),
        "0" | "false" | "no" | "off" => Some(false),
        _ => None,
    }
}
