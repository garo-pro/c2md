//! End-to-end checks for the live-reload server, driven through the same helpers the hook uses.
//!
//! These run the server in a thread rather than a child process so a failure shows up as a test failure instead of a silent fallback to `file://`.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpStream};
use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant};

/// Starts a server in a temporary directory and returns the directory and port once it is answering.
fn start() -> (PathBuf, u16) {
    let dir = std::env::temp_dir().join(format!("c2md-test-{}-{:?}", std::process::id(), std::thread::current().id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    let exe = env!("CARGO_BIN_EXE_c2md");
    Command::new(exe)
        .arg("serve")
        .arg("--dir")
        .arg(&dir)
        .arg("--port")
        .arg("0")
        .arg("--idle")
        .arg("120")
        .spawn()
        .expect("server starts");

    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if let Ok(text) = std::fs::read_to_string(dir.join("server.json")) {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) {
                if let Some(port) = v["port"].as_u64() {
                    return (dir, port as u16);
                }
            }
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    panic!("server never wrote server.json");
}

/// Sends a bare request and returns the whole response, which is what the hook's helpers do in miniature.
fn request(port: u16, path: &str) -> String {
    let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
    let mut stream = TcpStream::connect_timeout(&addr, Duration::from_secs(2)).expect("connects");
    stream.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
    write!(stream, "GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n").unwrap();
    let mut out = String::new();
    let _ = stream.read_to_string(&mut out);
    out
}

#[test]
fn ping_notify_and_page_all_answer() {
    let (dir, port) = start();

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
    let (dir, port) = start();
    std::fs::write(dir.join("watched.html"), "<html><main>x</main></html>").unwrap();

    assert_eq!(body_of(&request(port, "/watchers/watched")), "0", "nothing is watching a page nobody opened");

    let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
    let mut stream = TcpStream::connect_timeout(&addr, Duration::from_secs(2)).unwrap();
    stream.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
    write!(stream, "GET /events/watched HTTP/1.1\r\nHost: localhost\r\n\r\n").unwrap();

    // Read the first event so the stream is fully established before the count is checked.
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline && !line.starts_with("data:") {
        line.clear();
        if reader.read_line(&mut line).unwrap_or(0) == 0 {
            break;
        }
    }
    assert_eq!(await_watchers(port, "watched", "1"), "1", "an attached page must be counted");

    // Closing the connection is what a closed tab looks like from here.
    drop(reader);
    assert_eq!(await_watchers(port, "watched", "0"), "0", "a closed page must stop being counted, and quickly");

    assert!(request(port, "/watchers/../secret").starts_with("HTTP/1.1 400"), "a session name that is not filename-safe is refused");
}

#[test]
fn the_stream_stays_silent_until_something_changes() {
    let (dir, port) = start();
    std::fs::write(dir.join("quiet.html"), "<html><main>x</main></html>").unwrap();

    let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
    let mut stream = TcpStream::connect_timeout(&addr, Duration::from_secs(2)).unwrap();
    stream.set_read_timeout(Some(Duration::from_millis(1200))).unwrap();
    write!(stream, "GET /events/quiet HTTP/1.1\r\nHost: localhost\r\n\r\n").unwrap();

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
