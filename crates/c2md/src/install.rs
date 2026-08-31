//! Wiring the hook into a Claude Code `settings.json`, and taking it back out again.
//!
//! Settings files are hand-edited, so we parse, mutate the one key we own and write back rather than templating a whole file. A timestamped backup goes next to the original on every change.

use std::path::{Path, PathBuf};

use crate::config::{claude_dir, tildify};

/// Which settings file to edit.
#[derive(Clone, Copy, PartialEq)]
pub enum Target {
    /// `~/.claude/settings.json`, applying to every project.
    User,
    /// `.claude/settings.json` in the current directory, meant to be committed.
    Project,
    /// `.claude/settings.local.json` in the current directory, personal and usually gitignored.
    Local,
}

impl Target {
    pub fn parse(s: &str) -> Option<Target> {
        match s {
            "--user" | "user" => Some(Target::User),
            "--project" | "project" => Some(Target::Project),
            "--local" | "local" => Some(Target::Local),
            _ => None,
        }
    }

    pub fn path(self) -> Option<PathBuf> {
        match self {
            Target::User => claude_dir().map(|d| d.join("settings.json")),
            Target::Project => Some(PathBuf::from(".claude").join("settings.json")),
            Target::Local => Some(PathBuf::from(".claude").join("settings.local.json")),
        }
    }
}

/// Adds a Stop hook invoking this executable, replacing any c2md entry that is already there.
pub fn install(target: Target, command: &str) -> Result<String, String> {
    let path = target.path().ok_or("cannot locate the settings file")?;
    let mut root = read_settings(&path)?;

    let entries = stop_entries(&mut root);
    entries.retain(|e| !is_c2md_entry(e));
    entries.push(serde_json::json!({
        "hooks": [{
            "type": "command",
            "command": command,
            "timeout": 10,
            "statusMessage": "Opening answer in browser"
        }]
    }));

    write_settings(&path, &root)?;
    Ok(format!("Stop hook installed in {}\n  command: {command}", tildify(&path)))
}

/// Removes every c2md Stop hook from the settings file, leaving anything else untouched.
pub fn uninstall(target: Target) -> Result<String, String> {
    let path = target.path().ok_or("cannot locate the settings file")?;
    if !path.exists() {
        return Ok(format!("nothing to do, {} does not exist", tildify(&path)));
    }
    let mut root = read_settings(&path)?;

    let entries = stop_entries(&mut root);
    let before = entries.len();
    entries.retain(|e| !is_c2md_entry(e));
    let removed = before - entries.len();

    // An emptied list is noise in a hand-edited file, so drop the key entirely.
    if entries.is_empty() {
        if let Some(hooks) = root.get_mut("hooks").and_then(|h| h.as_object_mut()) {
            hooks.remove("Stop");
            if hooks.is_empty() {
                root.as_object_mut().map(|o| o.remove("hooks"));
            }
        }
    }

    write_settings(&path, &root)?;
    Ok(format!("removed {removed} c2md hook(s) from {}", tildify(&path)))
}

/// Reads a settings file, treating a missing file as an empty object.
fn read_settings(path: &Path) -> Result<serde_json::Value, String> {
    if !path.exists() {
        return Ok(serde_json::json!({}));
    }
    let text = std::fs::read_to_string(path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    if text.trim().is_empty() {
        return Ok(serde_json::json!({}));
    }
    serde_json::from_str(&text).map_err(|e| format!("{} is not valid JSON: {e}", path.display()))
}

/// Writes settings back, keeping a `.bak` copy of whatever was there before.
fn write_settings(path: &Path, root: &serde_json::Value) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("cannot create {}: {e}", parent.display()))?;
    }
    if path.exists() {
        let backup = path.with_extension("json.bak");
        std::fs::copy(path, &backup).map_err(|e| format!("cannot back up to {}: {e}", backup.display()))?;
    }
    let text = serde_json::to_string_pretty(root).map_err(|e| e.to_string())?;
    std::fs::write(path, text + "\n").map_err(|e| format!("cannot write {}: {e}", path.display()))
}

/// Borrows the `hooks.Stop` array, creating the intermediate objects if the file has never had hooks.
fn stop_entries(root: &mut serde_json::Value) -> &mut Vec<serde_json::Value> {
    let obj = root.as_object_mut().expect("settings root is an object");
    let hooks = obj
        .entry("hooks")
        .or_insert_with(|| serde_json::json!({}))
        .as_object_mut()
        .expect("hooks is an object");
    hooks
        .entry("Stop")
        .or_insert_with(|| serde_json::json!([]))
        .as_array_mut()
        .expect("Stop is an array")
}

/// Recognises an entry as ours by looking for the executable name in any of its commands.
fn is_c2md_entry(entry: &serde_json::Value) -> bool {
    entry["hooks"]
        .as_array()
        .map(|hooks| {
            hooks.iter().any(|h| {
                h["command"]
                    .as_str()
                    .map(|c| c.contains("c2md"))
                    .unwrap_or(false)
            })
        })
        .unwrap_or(false)
}

/// The command string to register, in the most portable form the executable's location allows.
///
/// An absolute path breaks the moment the checkout moves or the settings file is shared, so we prefer, in order: the bare name when this exact binary is already on `PATH`, then `${CLAUDE_PROJECT_DIR}` when it lives inside the project being configured, and only then an absolute path.
///
/// A bare relative path is deliberately not used. Claude Code resolves one against the directory `claude` was launched from, not the project root, so it silently breaks whenever the session starts from a subdirectory.
pub fn hook_command(target: Target) -> String {
    let exe = std::env::current_exe().ok().and_then(|p| p.canonicalize().ok());

    let Some(exe) = exe else { return "c2md hook".to_string() };

    if same_file_on_path(&exe) {
        return "c2md hook".to_string();
    }

    if target != Target::User {
        if let Some(rel) = project_relative(&exe) {
            return quote(&format!("${{CLAUDE_PROJECT_DIR}}/{rel}"));
        }
    }

    quote(&normalize(&exe))
}

/// Expresses the executable relative to the project root, or `None` when it lives outside the checkout.
fn project_relative(exe: &Path) -> Option<String> {
    let root = std::env::current_dir().ok()?.canonicalize().ok()?;
    let rel = exe.strip_prefix(&root).ok()?;
    Some(normalize(rel))
}

/// Renders a path with forward slashes, which every shell on every platform accepts, and without the Windows verbatim prefix that `canonicalize` adds.
fn normalize(p: &Path) -> String {
    let s = p.to_string_lossy().replace('\\', "/");
    s.strip_prefix("//?/").unwrap_or(&s).to_string()
}

/// Quotes only when needed, since an unquoted command reads better in a hand-edited settings file.
fn quote(command: &str) -> String {
    if command.contains(' ') {
        format!("\"{command}\" hook")
    } else {
        format!("{command} hook")
    }
}

/// Reports whether `PATH` already resolves `c2md` to this very binary.
///
/// Matching on the resolved path rather than the name matters: a different c2md earlier on `PATH` would otherwise be the thing we silently registered.
fn same_file_on_path(exe: &Path) -> bool {
    let Ok(path) = std::env::var("PATH") else { return false };
    let separator = if cfg!(windows) { ';' } else { ':' };
    let names: &[&str] = if cfg!(windows) { &["c2md.exe", "c2md"] } else { &["c2md"] };

    path.split(separator)
        .filter(|dir| !dir.is_empty())
        .flat_map(|dir| names.iter().map(move |name| PathBuf::from(dir).join(name)))
        .filter_map(|candidate| candidate.canonicalize().ok())
        .any(|candidate| candidate == exe)
}
