//! End-to-end checks for the live-reload server, driven through the same helpers the hook uses.
//!
//! Each test spawns the real `c2md serve` binary, so what is exercised is the process the hook actually starts rather than a thread standing in for it. The server is owned by the test and killed when it ends.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpStream};
use std::path::PathBuf;
use std::process::{Child, Command};
use std::time::{Duration, Instant};

/// A server running for the length of one test, killed when the test ends.
///
/// The child has to be owned rather than spawned and forgotten. Its idle timeout is minutes long, so a forgotten one outlives the test run and the next one starts against a directory some previous server still has opinions about.
struct Server {
    dir: PathBuf,
    port: u16,
    child: Child,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// Starts a server in a temporary directory and returns it once it is answering.
fn start() -> Server {
    let dir = std::env::temp_dir().join(format!("c2md-test-{}-{:?}", std::process::id(), std::thread::current().id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    let exe = env!("CARGO_BIN_EXE_c2md");
    let child = Command::new(exe)
        .arg("serve")
        .arg("--dir")
        .arg(&dir)
        .arg("--port")
        .arg("0")
        .arg("--idle")
        .arg("120")
        .spawn()
        .expect("server starts");

    // Owned before it is waited for, so the child is killed on the way out of the panic below too.
    let mut server = Server { dir, port: 0, child };

    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if let Ok(text) = std::fs::read_to_string(server.dir.join("server.json")) {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) {
                if let Some(port) = v["port"].as_u64() {
                    server.port = port as u16;
                    return server;
                }
            }
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    panic!("server never wrote server.json");
}

/// Sends a bare request and returns the whole response, which is what the hook's helpers do in miniature.
fn request(port: u16, path: &str) -> String {
    request_from(port, path, &format!("127.0.0.1:{port}"), "")
}

/// Sends a request with a chosen `Host` and `Origin`, which is how the loopback guard gets exercised.
fn request_from(port: u16, path: &str, host: &str, origin: &str) -> String {
    let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
    let mut stream = TcpStream::connect_timeout(&addr, Duration::from_secs(2)).expect("connects");
    stream.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
    let origin_header = if origin.is_empty() { String::new() } else { format!("Origin: {origin}\r\n") };
    write!(stream, "GET {path} HTTP/1.1\r\nHost: {host}\r\n{origin_header}Connection: close\r\n\r\n").unwrap();
    let mut out = String::new();
    let _ = stream.read_to_string(&mut out);
    out
}

#[test]
fn ping_notify_and_page_all_answer() {
    let server = start();
    let (dir, port) = (&server.dir, server.port);

    assert!(request(port, "/ping").starts_with("HTTP/1.1 200"), "ping should answer 200");

    std::fs::write(dir.join("sess.html"), "<html><main>hello __C2MD_VERSION__</main></html>").unwrap();
    let page = request(port, "/sess");
    assert!(page.starts_with("HTTP/1.1 200"), "page should answer 200");
    assert!(page.contains("hello 0"), "version placeholder should be stamped, got: {page}");

    assert!(request(port, "/notify/sess").starts_with("HTTP/1.1 200"));
    assert!(request(port, "/sess").contains("hello 1"), "notify should bump the version");

    assert!(request(port, "/nope").starts_with("HTTP/1.1 404"));
    assert!(request(port, "/../secret").starts_with("HTTP/1.1 404"));
}

/// Reads the body of a short response, which for `/watchers` is a bare count.
fn body_of(response: &str) -> String {
    response.split("\r\n\r\n").nth(1).unwrap_or("").trim().to_string()
}

/// Polls `/watchers` until it reports `want`, so the assertion does not race the server noticing a closed connection.
fn await_watchers(port: u16, session: &str, want: &str) -> String {
    let deadline = Instant::now() + Duration::from_secs(3);
    let mut last = String::new();
    while Instant::now() < deadline {
        last = body_of(&request(port, &format!("/watchers/{session}")));
        if last == want {
            return last;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    last
}

#[test]
fn watchers_counts_an_attached_page_and_forgets_a_closed_one() {
    let server = start();
    let (dir, port) = (&server.dir, server.port);
    std::fs::write(dir.join("watched.html"), "<html><main>x</main></html>").unwrap();

    assert_eq!(body_of(&request(port, "/watchers/watched")), "0", "nothing is watching a page nobody opened");

    let reader = attach(port, "watched");
    assert_eq!(await_watchers(port, "watched", "1"), "1", "an attached page must be counted");

    // Closing the connection is what a closed tab looks like from here.
    drop(reader);
    assert_eq!(await_watchers(port, "watched", "0"), "0", "a closed page must stop being counted, and quickly");

    assert!(request(port, "/watchers/../secret").starts_with("HTTP/1.1 400"), "a session name that is not filename-safe is refused");
}

/// Opens an event stream and reads far enough into it that the server has certainly counted the watcher.
fn attach(port: u16, session: &str) -> BufReader<TcpStream> {
    let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
    let mut stream = TcpStream::connect_timeout(&addr, Duration::from_secs(2)).unwrap();
    stream.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
    write!(stream, "GET /events/{session} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n\r\n").unwrap();

    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline && !line.starts_with("data:") {
        line.clear();
        if reader.read_line(&mut line).unwrap_or(0) == 0 {
            break;
        }
    }
    reader
}

/// The SessionEnd hook, end to end: a session going away must not take a page somebody else is reading with it.
#[test]
fn the_conditional_quit_spares_a_watched_server_and_stops_an_unwatched_one() {
    let server = start();
    let (dir, port) = (&server.dir, server.port);
    std::fs::write(dir.join("open.html"), "<html><main>x</main></html>").unwrap();

    let reader = attach(port, "open");
    assert_eq!(await_watchers(port, "open", "1"), "1", "the page must be attached before the question means anything");

    assert_eq!(body_of(&request(port, "/quit-if-unwatched")), "watching 1", "a page is open, so the server stays");
    assert!(request(port, "/ping").starts_with("HTTP/1.1 200"), "and it is still serving afterwards");

    // Closing the tab is the only thing that changes the answer.
    drop(reader);
    assert_eq!(await_watchers(port, "open", "0"), "0");

    assert_eq!(body_of(&request(port, "/quit-if-unwatched")), "stopping", "with nothing watching, the server goes");

    let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline && TcpStream::connect_timeout(&addr, Duration::from_millis(200)).is_ok() {
        std::thread::sleep(Duration::from_millis(25));
    }
    assert!(TcpStream::connect_timeout(&addr, Duration::from_millis(200)).is_err(), "the port must stop answering");
    assert!(!dir.join("server.json").exists(), "and the record of it must not outlive it");
}

#[test]
fn the_stream_stays_silent_until_something_changes() {
    let server = start();
    let (dir, port) = (&server.dir, server.port);
    std::fs::write(dir.join("quiet.html"), "<html><main>x</main></html>").unwrap();

    let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
    let mut stream = TcpStream::connect_timeout(&addr, Duration::from_secs(2)).unwrap();
    stream.set_read_timeout(Some(Duration::from_millis(1200))).unwrap();
    write!(stream, "GET /events/quiet HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n\r\n").unwrap();

    let mut reader = BufReader::new(stream);
    let mut data_lines = Vec::new();
    let mut line = String::new();

    // Drain for a second: the only data line should be the initial version.
    let deadline = Instant::now() + Duration::from_millis(1000);
    while Instant::now() < deadline {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) | Err(_) => break,
            Ok(_) => {
                if line.starts_with("data:") {
                    data_lines.push(line.trim().to_string());
                }
            }
        }
    }
    assert_eq!(data_lines, vec!["data: 0"], "an idle stream must say nothing beyond the initial version");

    // Now change something and confirm exactly one new version arrives.
    request(port, "/notify/quiet");
    let deadline = Instant::now() + Duration::from_millis(1500);
    while Instant::now() < deadline && data_lines.len() < 2 {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) | Err(_) => break,
            Ok(_) => {
                if line.starts_with("data:") {
                    data_lines.push(line.trim().to_string());
                }
            }
        }
    }
    assert_eq!(data_lines, vec!["data: 0", "data: 1"], "a change must push exactly one new version");
}

#[test]
fn a_request_that_names_someone_elses_host_is_refused() {
    let server = start();
    let (dir, port) = (&server.dir, server.port);
    std::fs::write(dir.join("private.html"), "<html><main>secret</main></html>").unwrap();

    // The shape of a DNS rebinding attack: a name the attacker controls, pointed at 127.0.0.1, so the
    // request arrives over loopback like any other and only the Host header gives it away.
    for host in ["evil.example", "evil.example:80", &format!("evil.example:{port}")] {
        let response = request_from(port, "/private", host, "");
        assert!(response.starts_with("HTTP/1.1 403"), "Host {host} should be refused, got: {response}");
        assert!(!response.contains("secret"), "and must not leak the page: {response}");
    }

    // A loopback name that does not name this server's port is not this server either.
    let elsewhere = request_from(port, "/private", "127.0.0.1:1", "");
    assert!(elsewhere.starts_with("HTTP/1.1 403"), "got: {elsewhere}");

    // The spellings a browser or the hook actually uses all still work.
    for host in [format!("127.0.0.1:{port}"), format!("localhost:{port}"), format!("[::1]:{port}")] {
        let response = request_from(port, "/private", &host, "");
        assert!(response.starts_with("HTTP/1.1 200"), "Host {host} should be accepted, got: {response}");
    }
}

#[test]
fn a_request_driven_by_another_site_is_refused() {
    let server = start();
    let (dir, port) = (&server.dir, server.port);
    std::fs::write(dir.join("private.html"), "<html><main>secret</main></html>").unwrap();
    let host = format!("127.0.0.1:{port}");

    for origin in ["https://evil.example", "http://evil.example", "null"] {
        let response = request_from(port, "/private", &host, origin);
        assert!(response.starts_with("HTTP/1.1 403"), "Origin {origin} should be refused, got: {response}");
        assert!(!response.contains("secret"), "and must not leak the page: {response}");
    }

    // The page's own fetch and EventSource carry this one.
    let own = request_from(port, "/private", &host, &format!("http://127.0.0.1:{port}"));
    assert!(own.starts_with("HTTP/1.1 200"), "the page's own origin must work, got: {own}");
}

#[test]
fn the_index_lists_the_pages_that_exist_and_nothing_else() {
    let server = start();
    let (dir, port) = (&server.dir, server.port);
    std::fs::write(dir.join("alpha.html"), "<html><main>a</main></html>").unwrap();
    std::fs::write(dir.join("beta.html"), "<html><main>b</main></html>").unwrap();
    // Neither of these is a page: one is the server's own record, the other a half-written render.
    std::fs::write(dir.join("server.json"), "{}").unwrap();
    std::fs::write(dir.join("gamma.html.tmp"), "<html></html>").unwrap();

    let response = request(port, "/");
    assert!(response.starts_with("HTTP/1.1 200"), "the index should answer 200, got: {response}");
    assert!(response.contains("href=\"/alpha\""), "alpha should be listed: {response}");
    assert!(response.contains("href=\"/beta\""), "beta should be listed: {response}");
    assert!(!response.contains("server.json"), "the server's own record is not a page");
    assert!(!response.contains("gamma"), "a temporary file is not a page either");
}

#[test]
fn an_empty_output_directory_still_gives_a_usable_index() {
    let server = start();
    let port = server.port;
    let response = request(port, "/");
    assert!(response.starts_with("HTTP/1.1 200"));
    assert!(response.contains("Nothing rendered yet"), "got: {response}");
}
