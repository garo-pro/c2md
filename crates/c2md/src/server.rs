//! A loopback HTTP server that serves the rendered pages and pushes an event when one changes.
//!
//! The point is silence. A `file://` page can only stay current by reloading itself on a timer, which redraws the whole document whether or not anything changed. Served over HTTP the page can hold an `EventSource` open, sit completely idle while nothing happens, and swap in only the new content when an answer actually lands.
//!
//! It is deliberately a hand-rolled HTTP/1.1 subset. The whole surface is five routes on 127.0.0.1, and a real framework would cost more in binary size and startup than the hook can afford.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Placeholder the server fills in with the current version as it serves a page.
///
/// Stamping it at serve time rather than render time closes a race: the page always knows exactly which version it is showing, so it cannot miss an update that landed between the render and the browser connecting.
pub const VERSION_TOKEN: &str = "__C2MD_VERSION__";

/// How often an event stream checks for a new version, and how long a poll blocks before sending a keepalive.
const POLL: Duration = Duration::from_millis(200);
const KEEPALIVE: Duration = Duration::from_secs(15);

/// The liveness probe, kept as a constant so tests exercise byte-for-byte what the hook sends.
const PING_REQUEST: &[u8] = b"GET /ping HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n";

/// The shutdown request, used by `c2md stop`.
const QUIT_REQUEST: &[u8] = b"GET /quit HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n";

/// Details of a running server, as recorded in `server.json` for later hook runs to find.
pub struct Handle {
    pub port: u16,
}

/// Shared between the accept loop and every connection thread.
struct State {
    dir: PathBuf,
    /// Bumped for a session each time the hook reports a new answer.
    versions: Mutex<HashMap<String, u64>>,
    /// Event streams currently attached, counted per session. The total keeps the idle reaper from killing a page someone is reading, and the per-session number is how the hook tells an open tab from one the user closed.
    watchers: Mutex<HashMap<String, usize>>,
    /// Milliseconds since start of the last request of any kind.
    last_activity: AtomicU64,
    started: Instant,
}

impl State {
    fn touch(&self) {
        self.last_activity.store(self.started.elapsed().as_millis() as u64, Ordering::Relaxed);
    }

    fn version(&self, session: &str) -> u64 {
        *self.versions.lock().unwrap().get(session).unwrap_or(&0)
    }

    fn watchers_of(&self, session: &str) -> usize {
        *self.watchers.lock().unwrap().get(session).unwrap_or(&0)
    }

    fn total_watchers(&self) -> usize {
        self.watchers.lock().unwrap().values().sum()
    }
}

/// Reads `server.json` and returns the port if a server is recorded there and still answering.
pub fn running(dir: &Path) -> Option<Handle> {
    let text = std::fs::read_to_string(state_path(dir)).ok()?;
    let v: serde_json::Value = serde_json::from_str(&text).ok()?;
    let port = v["port"].as_u64()? as u16;
    ping(port).then_some(Handle { port })
}

/// Asks a possibly dead server whether it is alive, with a short timeout so a stale record cannot stall the hook.
pub fn ping(port: u16) -> bool {
    let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
    let Ok(mut stream) = TcpStream::connect_timeout(&addr, Duration::from_millis(250)) else {
        return false;
    };
    let _ = stream.set_read_timeout(Some(Duration::from_millis(400)));
    if stream.write_all(PING_REQUEST).is_err() {
        return false;
    }
    status_is_ok(&mut stream)
}

/// Tells a running server that a session has a new answer, so it can wake the page.
pub fn notify(port: u16, session: &str) -> bool {
    let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
    let Ok(mut stream) = TcpStream::connect_timeout(&addr, Duration::from_millis(250)) else {
        return false;
    };
    let _ = stream.set_read_timeout(Some(Duration::from_millis(400)));
    let request = format!("GET /notify/{session} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n");
    if stream.write_all(request.as_bytes()).is_err() {
        return false;
    }
    status_is_ok(&mut stream)
}

/// Asks how many pages are currently holding an event stream open for `session`.
///
/// `None` means the question could not be answered — no server, or one too old to know the route — and the caller must not read that as "nobody is watching".
pub fn watchers(port: u16, session: &str) -> Option<usize> {
    let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
    let mut stream = TcpStream::connect_timeout(&addr, Duration::from_millis(250)).ok()?;
    let _ = stream.set_read_timeout(Some(Duration::from_millis(400)));
    let request = format!("GET /watchers/{session} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n");
    stream.write_all(request.as_bytes()).ok()?;

    let mut response = String::new();
    let _ = stream.read_to_string(&mut response);
    if !response.starts_with("HTTP/1.1 200") {
        return None;
    }
    // The body is a bare count, and the response closes the connection, so everything after the blank line is it.
    let body = response.split("\r\n\r\n").nth(1)?;
    body.trim().parse().ok()
}

/// Reads far enough into a response to judge its status line.
///
/// A single `read` is not enough: TCP is free to hand back as little as one byte, and it was in fact splitting `HTTP/1.1 200` after nine characters, which made every liveness check quietly report a dead server.
fn status_is_ok(stream: &mut TcpStream) -> bool {
    const WANTED: &[u8] = b"HTTP/1.1 200";
    let mut seen = Vec::with_capacity(32);
    let mut chunk = [0u8; 32];

    while seen.len() < WANTED.len() {
        match stream.read(&mut chunk) {
            Ok(0) | Err(_) => return false,
            Ok(n) => seen.extend_from_slice(&chunk[..n]),
        }
    }
    seen.starts_with(WANTED)
}

/// Asks a running server to exit, which is also how you release the lock Windows holds on a running executable.
pub fn stop(dir: &Path) -> Option<u16> {
    let handle = running(dir)?;
    let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, handle.port));
    let mut stream = TcpStream::connect_timeout(&addr, Duration::from_millis(250)).ok()?;
    let _ = stream.set_read_timeout(Some(Duration::from_millis(400)));
    stream.write_all(QUIT_REQUEST).ok()?;
    status_is_ok(&mut stream).then_some(handle.port)
}

/// The URL a browser should open for a session.
pub fn page_url(port: u16, session: &str) -> String {
    format!("http://127.0.0.1:{port}/{session}")
}

fn state_path(dir: &Path) -> PathBuf {
    dir.join("server.json")
}

/// Runs the server until it has been idle, with nothing watching, for `idle_timeout`.
pub fn serve(dir: PathBuf, port: u16, idle_timeout: Duration) -> std::io::Result<()> {
    let listener = TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, port)))?;
    let bound = listener.local_addr()?.port();

    let state = Arc::new(State {
        dir: dir.clone(),
        versions: Mutex::new(HashMap::new()),
        watchers: Mutex::new(HashMap::new()),
        last_activity: AtomicU64::new(0),
        started: Instant::now(),
    });

    let record = serde_json::json!({ "port": bound, "pid": std::process::id() });
    std::fs::write(state_path(&dir), record.to_string())?;

    // The reaper owns process lifetime: nothing else knows when the last page went away.
    {
        let state = Arc::clone(&state);
        let dir = dir.clone();
        std::thread::spawn(move || loop {
            std::thread::sleep(Duration::from_secs(5));
            if state.total_watchers() > 0 {
                state.touch();
                continue;
            }
            let idle_ms = state.started.elapsed().as_millis() as u64 - state.last_activity.load(Ordering::Relaxed);
            if idle_ms > idle_timeout.as_millis() as u64 {
                let _ = std::fs::remove_file(state_path(&dir));
                std::process::exit(0);
            }
        });
    }

    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        let state = Arc::clone(&state);
        std::thread::spawn(move || {
            let _ = handle(stream, &state);
        });
    }
    Ok(())
}

fn handle(mut stream: TcpStream, state: &Arc<State>) -> std::io::Result<()> {
    state.touch();

    let mut reader = BufReader::new(stream.try_clone()?);
    let mut request_line = String::new();
    reader.read_line(&mut request_line)?;

    // Headers are read and discarded; nothing this server does depends on them.
    let mut header = String::new();
    loop {
        header.clear();
        if reader.read_line(&mut header)? == 0 || header == "\r\n" || header == "\n" {
            break;
        }
    }

    let path = request_line.split_whitespace().nth(1).unwrap_or("/").to_string();
    let path = path.split('?').next().unwrap_or("/");

    if path == "/ping" {
        return respond(&mut stream, "200 OK", "text/plain", b"ok");
    }
    if path == "/quit" {
        respond(&mut stream, "200 OK", "text/plain", b"stopping")?;
        let _ = std::fs::remove_file(state_path(&state.dir));
        std::process::exit(0);
    }
    if let Some(session) = path.strip_prefix("/notify/") {
        let Some(session) = safe_session(session) else {
            return respond(&mut stream, "400 Bad Request", "text/plain", b"bad session");
        };
        *state.versions.lock().unwrap().entry(session).or_insert(0) += 1;
        return respond(&mut stream, "200 OK", "text/plain", b"ok");
    }
    if let Some(session) = path.strip_prefix("/watchers/") {
        let Some(session) = safe_session(session) else {
            return respond(&mut stream, "400 Bad Request", "text/plain", b"bad session");
        };
        let count = state.watchers_of(&session).to_string();
        return respond(&mut stream, "200 OK", "text/plain", count.as_bytes());
    }
    if let Some(session) = path.strip_prefix("/events/") {
        let Some(session) = safe_session(session) else {
            return respond(&mut stream, "400 Bad Request", "text/plain", b"bad session");
        };
        return stream_events(stream, state, &session);
    }

    let Some(session) = safe_session(path.trim_start_matches('/').trim_end_matches(".html")) else {
        return respond(&mut stream, "404 Not Found", "text/plain", b"not found");
    };
    match std::fs::read_to_string(state.dir.join(format!("{session}.html"))) {
        Ok(html) => {
            let stamped = html.replace(VERSION_TOKEN, &state.version(&session).to_string());
            respond(&mut stream, "200 OK", "text/html; charset=utf-8", stamped.as_bytes())
        }
        Err(_) => respond(&mut stream, "404 Not Found", "text/plain", b"no page for that session yet"),
    }
}

/// Holds the connection open, sending the version whenever it changes and a comment otherwise so the stream stays alive.
fn stream_events(mut stream: TcpStream, state: &Arc<State>, session: &str) -> std::io::Result<()> {
    stream.write_all(
        b"HTTP/1.1 200 OK\r\n\
          Content-Type: text/event-stream\r\n\
          Cache-Control: no-store\r\n\
          Connection: keep-alive\r\n\r\n",
    )?;
    stream.flush()?;

    *state.watchers.lock().unwrap().entry(session.to_string()).or_insert(0) += 1;
    let result = pump(&mut stream, state, session);
    {
        let mut watchers = state.watchers.lock().unwrap();
        if let Some(count) = watchers.get_mut(session) {
            *count = count.saturating_sub(1);
        }
    }
    result
}

fn pump(stream: &mut TcpStream, state: &Arc<State>, session: &str) -> std::io::Result<()> {
    let mut sent = state.version(session);
    write!(stream, "data: {sent}\n\n")?;
    stream.flush()?;

    // Waiting inside a read is what makes a closed tab visible straight away. Sleeping instead would leave the watcher counted until the next keepalive write failed, up to fifteen seconds later, and the hook would take that stale count for a page that is still open.
    let _ = stream.set_read_timeout(Some(POLL));

    let mut since_keepalive = Duration::ZERO;
    loop {
        if !still_connected(stream) {
            return Ok(());
        }
        since_keepalive += POLL;

        let current = state.version(session);
        if current != sent {
            sent = current;
            // A failed write is how a closed tab announces itself; ending the thread releases the watcher slot.
            write!(stream, "data: {sent}\n\n")?;
            stream.flush()?;
            since_keepalive = Duration::ZERO;
            continue;
        }
        if since_keepalive >= KEEPALIVE {
            stream.write_all(b": keepalive\n\n")?;
            stream.flush()?;
            since_keepalive = Duration::ZERO;
        }
    }
}

/// Blocks for one poll interval and reports whether the page is still there.
///
/// A browser never sends anything on an event stream, so the peek is really a wait: it times out while the tab is open, and returns end of file the moment the tab closes.
fn still_connected(stream: &mut TcpStream) -> bool {
    let mut probe = [0u8; 1];
    match stream.peek(&mut probe) {
        // End of file: the page is gone.
        Ok(0) => false,
        // Unexpected, but harmless. Sleep out the rest of the interval so a chatty client cannot spin this thread.
        Ok(_) => {
            std::thread::sleep(POLL);
            true
        }
        Err(e) => matches!(
            e.kind(),
            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut | std::io::ErrorKind::Interrupted
        ),
    }
}

/// Writes a complete response in a single `write_all`.
///
/// Assembling it first is not just tidiness: writing the headers piecemeal let TCP split the status line across segments, which is what broke the liveness check.
fn respond(stream: &mut TcpStream, status: &str, content_type: &str, body: &[u8]) -> std::io::Result<()> {
    let head = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n",
        body.len()
    );
    let mut out = Vec::with_capacity(head.len() + body.len());
    out.extend_from_slice(head.as_bytes());
    out.extend_from_slice(body);
    stream.write_all(&out)?;
    stream.flush()
}

/// Accepts a session name only if it is already filename-safe, which is what keeps a request from walking out of the output directory.
fn safe_session(raw: &str) -> Option<String> {
    if raw.is_empty() || raw.len() > 64 {
        return None;
    }
    raw.chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        .then(|| raw.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_path_traversal_and_accepts_slugs() {
        assert_eq!(safe_session("abc-123_x").as_deref(), Some("abc-123_x"));
        assert!(safe_session("../../etc/passwd").is_none());
        assert!(safe_session("a/b").is_none());
        assert!(safe_session("a.html").is_none());
        assert!(safe_session("").is_none());
    }

    #[test]
    fn page_url_points_at_loopback_only() {
        assert_eq!(page_url(4321, "sess"), "http://127.0.0.1:4321/sess");
    }
}

#[cfg(test)]
mod helper_tests {
    use super::*;

    /// Runs a server in a thread and returns its directory and port, exercising the same helpers the hook calls.
    fn spawn_local() -> (PathBuf, u16) {
        let dir = std::env::temp_dir().join(format!("c2md-unit-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let d = dir.clone();
        std::thread::spawn(move || {
            let _ = serve(d, 0, Duration::from_secs(300));
        });

        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if let Ok(text) = std::fs::read_to_string(state_path(&dir)) {
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) {
                    if let Some(p) = v["port"].as_u64() {
                        return (dir, p as u16);
                    }
                }
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        panic!("no server.json");
    }

    #[test]
    fn ping_running_and_notify_agree_with_a_live_server() {
        let (dir, port) = spawn_local();
        assert!(ping(port), "ping must see the server it just started");
        assert!(running(&dir).is_some(), "running must find the server from its record");
        assert!(notify(port, "sess"), "notify must be accepted");
    }
}
