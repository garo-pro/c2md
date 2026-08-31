//! Launching the rendered page in a browser, detached from the hook process.
//!
//! The child gets null stdio on purpose. Claude Code reads the hook to completion, so handing a long-lived browser process our inherited pipes would keep them open and stall the turn.

use std::path::Path;
use std::process::{Command, Stdio};

/// Spawns a browser for `path` and returns immediately, without waiting for it to exit.
///
/// `browser` is a command name or path; an empty string means whatever the OS has registered for HTML.
pub fn open(path: &Path, browser: &str) -> std::io::Result<()> {
    open_url(&file_url(path), browser)
}

/// Spawns a browser for an already-formed URL, which is how a served page is opened.
pub fn open_url(url: &str, browser: &str) -> std::io::Result<()> {
    let mut cmd = if !browser.is_empty() {
        let mut c = Command::new(browser);
        c.arg(url);
        c
    } else {
        default_opener(url)
    };

    cmd.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    no_window(&mut cmd);
    cmd.spawn().map(|_| ())
}

#[cfg(windows)]
fn default_opener(url: &str) -> Command {
    // `start` needs an explicit empty title argument, otherwise it treats a quoted URL as the window title.
    let mut c = Command::new("cmd");
    c.args(["/C", "start", "", url]);
    c
}

#[cfg(target_os = "macos")]
fn default_opener(url: &str) -> Command {
    let mut c = Command::new("open");
    c.arg(url);
    c
}

#[cfg(all(unix, not(target_os = "macos")))]
fn default_opener(url: &str) -> Command {
    let mut c = Command::new("xdg-open");
    c.arg(url);
    c
}

/// Suppresses the console window that `cmd.exe` would otherwise flash on screen for a moment.
#[cfg(windows)]
fn no_window(cmd: &mut Command) {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    cmd.creation_flags(CREATE_NO_WINDOW);
}

#[cfg(not(windows))]
fn no_window(_cmd: &mut Command) {}

/// Turns a filesystem path into a `file://` URL, normalising separators and percent-encoding the unsafe bytes.
pub fn file_url(path: &Path) -> String {
    let raw = path.to_string_lossy().replace('\\', "/");
    let mut url = String::with_capacity(raw.len() + 16);
    url.push_str("file:///");

    // An absolute POSIX path already starts with the slash that `file:///` supplied.
    let body = raw.strip_prefix('/').unwrap_or(&raw);

    for byte in body.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' | b'/' | b':' => {
                url.push(byte as char)
            }
            _ => url.push_str(&format!("%{byte:02X}")),
        }
    }
    url
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_windows_path() {
        let url = file_url(Path::new(r"C:\Users\a b\c2md\answer.html"));
        assert_eq!(url, "file:///C:/Users/a%20b/c2md/answer.html");
    }

    #[test]
    fn keeps_single_root_slash_on_posix_paths() {
        let url = file_url(Path::new("/tmp/c2md/answer.html"));
        assert_eq!(url, "file:///tmp/c2md/answer.html");
    }
}
