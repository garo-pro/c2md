//! Wiring the hook into a Claude Code `settings.json`, and taking it back out again.
//!
//! Settings files are hand-edited, so we parse, mutate the one key we own and write back rather than templating a whole file. A `.bak` copy of the previous contents goes next to the original on every change, so the last known-good version is always one rename away.

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
    install_at(&path, command)
}

/// The body of `install`, against an explicit path so it can be exercised without touching a real settings file.
fn install_at(path: &Path, command: &str) -> Result<String, String> {
    let mut root = read_settings(path)?;

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

    write_settings(path, &root)?;
    Ok(format!("Stop hook installed in {}\n  command: {command}", tildify(path)))
}

/// Removes every c2md Stop hook from the settings file, leaving anything else untouched.
pub fn uninstall(target: Target) -> Result<String, String> {
    let path = target.path().ok_or("cannot locate the settings file")?;
    uninstall_at(&path)
}

/// The body of `uninstall`, against an explicit path so it can be exercised without touching a real settings file.
fn uninstall_at(path: &Path) -> Result<String, String> {
    if !path.exists() {
        return Ok(format!("nothing to do, {} does not exist", tildify(path)));
    }
    let mut root = read_settings(path)?;

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

    write_settings(path, &root)?;
    Ok(format!("removed {removed} c2md hook(s) from {}", tildify(path)))
}

/// The c2md command currently registered in a settings file, if there is one.
///
/// This is what `c2md status` reads, so the first question anyone asks — "is the hook even installed?" — can be answered without opening a JSON file by hand.
pub fn installed_command(target: Target) -> Option<String> {
    installed_command_at(&target.path()?)
}

/// The body of `installed_command`, against an explicit path so it can be tested alongside install and uninstall.
fn installed_command_at(path: &Path) -> Option<String> {
    let root = read_settings(path).ok()?;
    let entries = root["hooks"]["Stop"].as_array()?;
    entries
        .iter()
        .filter(|e| is_c2md_entry(e))
        .find_map(|e| e["hooks"].as_array()?.iter().find_map(|h| h["command"].as_str()))
        .map(str::to_string)
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

/// Recognises an entry as ours by the shape of its command.
fn is_c2md_entry(entry: &serde_json::Value) -> bool {
    entry["hooks"]
        .as_array()
        .map(|hooks| hooks.iter().any(|h| h["command"].as_str().map(is_c2md_command).unwrap_or(false)))
        .unwrap_or(false)
}

/// Whether a registered command is one `install` wrote.
///
/// A substring test for `c2md` was tempting and wrong: `uninstall` would have quietly deleted somebody's unrelated `c2md-notify` hook, or any command that merely mentioned this tool's output directory. Every command we write is an executable named `c2md` followed by the `hook` subcommand, so that is what gets matched.
fn is_c2md_command(command: &str) -> bool {
    let Some(exe) = command.trim().strip_suffix(" hook") else { return false };
    let exe = exe.trim().trim_matches('"');
    let name = exe.rsplit(['/', '\\']).next().unwrap_or(exe);
    name.eq_ignore_ascii_case("c2md") || name.eq_ignore_ascii_case("c2md.exe")
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A private settings file for one test, removed and recreated so a rerun starts clean.
    fn scratch(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!("c2md-install-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).unwrap();
        path.join("settings.json")
    }

    fn read(path: &Path) -> serde_json::Value {
        serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
    }

    /// A settings file shaped like a real one: unrelated top-level keys, and a Stop hook that is not ours.
    const EXISTING: &str = r#"{
      "model": "opus",
      "permissions": { "allow": ["Bash(ls:*)"] },
      "hooks": {
        "Stop": [{ "hooks": [{ "type": "command", "command": "notify-send done" }] }],
        "PreToolUse": [{ "hooks": [{ "type": "command", "command": "audit.sh" }] }]
      }
    }"#;

    #[test]
    fn installing_into_a_populated_file_leaves_everything_else_alone() {
        let path = scratch("populated");
        std::fs::write(&path, EXISTING).unwrap();

        install_at(&path, "c2md hook").unwrap();
        let root = read(&path);

        assert_eq!(root["model"], "opus", "unrelated keys survive");
        assert_eq!(root["permissions"]["allow"][0], "Bash(ls:*)");
        assert_eq!(root["hooks"]["PreToolUse"][0]["hooks"][0]["command"], "audit.sh", "other events survive");

        let stop = root["hooks"]["Stop"].as_array().unwrap();
        assert_eq!(stop.len(), 2, "ours is added beside the existing hook, not instead of it");
        assert_eq!(stop[0]["hooks"][0]["command"], "notify-send done");
        assert_eq!(stop[1]["hooks"][0]["command"], "c2md hook");
    }

    #[test]
    fn uninstalling_removes_only_our_entry() {
        let path = scratch("uninstall");
        std::fs::write(&path, EXISTING).unwrap();

        install_at(&path, "c2md hook").unwrap();
        let message = uninstall_at(&path).unwrap();
        assert!(message.contains("removed 1"), "one entry, reported: {message}");

        let root = read(&path);
        let stop = root["hooks"]["Stop"].as_array().unwrap();
        assert_eq!(stop.len(), 1);
        assert_eq!(stop[0]["hooks"][0]["command"], "notify-send done", "the user's own hook is untouched");
        assert_eq!(root["hooks"]["PreToolUse"][0]["hooks"][0]["command"], "audit.sh");
    }

    #[test]
    fn installing_twice_replaces_rather_than_accumulates() {
        let path = scratch("twice");

        install_at(&path, "c2md hook").unwrap();
        install_at(&path, "/opt/bin/c2md hook").unwrap();

        let stop = read(&path)["hooks"]["Stop"].as_array().unwrap().clone();
        assert_eq!(stop.len(), 1, "a reinstall updates the command in place");
        assert_eq!(stop[0]["hooks"][0]["command"], "/opt/bin/c2md hook");
    }

    #[test]
    fn an_emptied_hooks_key_is_dropped_instead_of_left_as_noise() {
        let path = scratch("emptied");

        install_at(&path, "c2md hook").unwrap();
        uninstall_at(&path).unwrap();

        let root = read(&path);
        assert!(root.get("hooks").is_none(), "nothing of ours should linger in a hand-edited file: {root}");
    }

    #[test]
    fn the_previous_contents_are_always_recoverable_from_the_backup() {
        let path = scratch("backup");
        std::fs::write(&path, EXISTING).unwrap();

        install_at(&path, "c2md hook").unwrap();

        let backup = path.with_extension("json.bak");
        assert!(backup.exists(), "a change to a hand-edited file leaves a copy behind");
        assert_eq!(read(&backup), serde_json::from_str::<serde_json::Value>(EXISTING).unwrap());
    }

    #[test]
    fn uninstalling_from_a_file_that_was_never_touched_is_not_an_error() {
        let path = scratch("absent");
        let message = uninstall_at(&path).unwrap();
        assert!(message.contains("does not exist"), "got: {message}");
        assert!(!path.exists(), "and nothing is created on the way past");
    }

    #[test]
    fn a_settings_file_that_is_not_json_is_reported_rather_than_overwritten() {
        let path = scratch("broken");
        std::fs::write(&path, "{ not json").unwrap();

        let err = install_at(&path, "c2md hook").unwrap_err();
        assert!(err.contains("not valid JSON"), "got: {err}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{ not json", "the file is left exactly as it was");
    }

    #[test]
    fn only_our_own_command_shape_is_recognised() {
        for ours in ["c2md hook", "c2md.exe hook", "/home/a/.claude/bin/c2md hook", "\"C:/Program Files/c2md.exe\" hook", "${CLAUDE_PROJECT_DIR}/target/release/c2md.exe hook"] {
            assert!(is_c2md_command(ours), "should be recognised: {ours}");
        }
        // A substring match would have claimed every one of these.
        for theirs in ["c2md-notify hook", "my-c2md hook", "c2md stop", "echo c2md hook", "logger --tag c2md hook"] {
            assert!(!is_c2md_command(theirs), "should not be claimed: {theirs}");
        }
    }

    #[test]
    fn status_can_read_back_what_was_registered() {
        let path = scratch("status");
        assert!(installed_command_at(&path).is_none(), "nothing registered yet");

        install_at(&path, "/opt/bin/c2md hook").unwrap();
        assert_eq!(installed_command_at(&path).as_deref(), Some("/opt/bin/c2md hook"));

        uninstall_at(&path).unwrap();
        assert!(installed_command_at(&path).is_none(), "and gone again afterwards");
    }
}
