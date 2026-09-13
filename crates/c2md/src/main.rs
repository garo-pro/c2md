//! c2md renders the answer Claude Code just gave into a standalone HTML page and opens it in a browser.
//!
//! It runs as a Stop hook, so it is spawned once per turn and its cost is paid on every single answer. That budget is why this is a compiled binary reading the session transcript directly, rather than a script shelling out to a markdown toolchain.

mod browser;
mod config;
mod install;
mod render;
mod server;
mod transcript;

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use config::{Config, FileMode};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let cmd = args.first().map(String::as_str).unwrap_or("hook");

    let code = match cmd {
        "hook" => {
            run_hook();
            // A Stop hook must never fail the turn, so every path here reports success.
            0
        }
        "install" => report(install_cmd(&args[1..])),
        "uninstall" => report(uninstall_cmd(&args[1..])),
        "init" => report(init_cmd()),
        "config" => report(config_cmd()),
        "status" => report(status_cmd()),
        "render" => report(render_cmd(&args[1..])),
        "open" => report(open_cmd()),
        "bench" => report(bench_cmd(&args[1..])),
        "serve" => report(serve_cmd(&args[1..])),
        "stop" => report(stop_cmd(&args[1..])),
        "-h" | "--help" | "help" => {
            println!("{USAGE}");
            0
        }
        "-V" | "--version" | "version" => {
            println!("c2md {}", env!("CARGO_PKG_VERSION"));
            0
        }
        other => {
            eprintln!("c2md: unknown command `{other}`\n\n{USAGE}");
            2
        }
    };
    std::process::exit(code);
}

const USAGE: &str = "\
c2md - open the last Claude Code answer in your browser

USAGE
  c2md hook                      Read a Stop hook payload on stdin and render the answer (default)
  c2md install [--user|--project|--local]
                                 Register the Stop and SessionEnd hooks in a settings file
  c2md uninstall [--user|--project|--local]
                                 Remove both hooks again
  c2md init                      Write a config file with every setting at its default
  c2md config                    Print the resolved settings and where they came from
  c2md status                    Report whether the hooks are registered and the server is running
  c2md render <file.md> [-o out.html] [--open]
                                 Render a markdown file, for previewing the page style
  c2md open                      Re-open the most recently rendered page
  c2md bench [transcript.jsonl] [iters]
                                 Time the full pipeline end to end
  c2md stop [--if-unwatched]     Stop the background live-reload server. --if-unwatched leaves it
                                 alone while a page is still open, which is what the SessionEnd hook
                                 runs so one session ending cannot close another session's page
  c2md serve [--port N] [--dir D] [--idle S]
                                 Run the live-reload server in the foreground; the hook starts its
                                 own in the background, so this is for debugging

SETTINGS
  Config file: ~/.claude/c2md.json, or $C2MD_CONFIG.
  Every key also has an environment override named after it, so C2MD_AUTO_OPEN=0 disables opening
  for a single session without touching the file.";

/// Prints a command result and maps it to a process exit code.
fn report(r: Result<String, String>) -> i32 {
    match r {
        Ok(msg) => {
            if !msg.is_empty() {
                println!("{msg}");
            }
            0
        }
        Err(err) => {
            eprintln!("c2md: {err}");
            1
        }
    }
}

/// The hot path: parse the hook payload, render the answer and hand it to a browser.
fn run_hook() {
    let started = Instant::now();

    let mut input = String::new();
    if std::io::stdin().read_to_string(&mut input).is_err() {
        return;
    }
    // Some shells prepend a UTF-8 BOM when they pipe a string, and it would otherwise fail the parse.
    let payload_text = input.strip_prefix('\u{feff}').unwrap_or(&input);
    let Ok(payload) = serde_json::from_str::<serde_json::Value>(payload_text) else { return };

    let cfg = Config::load();
    if !cfg.enabled {
        return;
    }

    let Some(transcript_path) = payload["transcript_path"].as_str() else { return };
    let session = payload["session_id"].as_str().unwrap_or("session");
    let cwd = payload["cwd"].as_str();

    let t_extract = Instant::now();
    let settle = Duration::from_millis(cfg.settle_ms);
    let Some(answer) = transcript::extract(Path::new(transcript_path), cfg.scope, cfg.include_thinking, settle) else {
        return;
    };
    let extract_us = t_extract.elapsed().as_micros();

    if answer.visible_len() < cfg.min_chars {
        return;
    }

    let dir = cfg.resolved_output_dir();

    // Liveness has to be settled before rendering, because it decides whether the page carries a reload timer.
    let port = if cfg.live_reload { ensure_server(&cfg, &dir) } else { None };

    let meta = render::PageMeta { cwd, epoch_ms: epoch_ms(), live: port.is_some() };
    let page = render::page(&answer, &cfg, &meta);
    let render_us = page.render_us;

    let path = output_path(&dir, session, cfg.file_mode);

    let t_write = Instant::now();
    if write_atomic(&path, &page.html).is_err() {
        return;
    }
    let write_us = t_write.elapsed().as_micros();

    // The served name is the file stem, not the session: in timestamped mode every turn is its own file, and the URL has to name the one just written.
    let served = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| slug(session));

    // Telling the server first means the page already has the new content by the time a browser opens.
    if let Some(port) = port {
        server::notify(port, &served);
    }

    let url = match port {
        Some(port) => server::page_url(port, &served),
        None => browser::file_url(&path),
    };
    let opened = maybe_open(&cfg, &dir, session, &url, port.map(|p| (p, served.as_str())));

    if cfg.notify {
        // Claude Code surfaces `systemMessage` from a Stop hook as a line in the transcript.
        println!("{}", serde_json::json!({ "systemMessage": format!("c2md: {url}") }));
    }

    if cfg.log {
        let line = format!(
            "session={session} bytes={} extract={extract_us}us render={render_us}us write={write_us}us total={}us opened={opened} path={}\n",
            answer.bytes_read,
            started.elapsed().as_micros(),
            path.display()
        );
        append_log(&dir, &line);
    }
}

/// How long after opening a tab its absence is forgiven.
///
/// A browser takes a moment to start, load the page and attach its event stream, and until it does the session looks exactly like one whose tab was closed. Answering two questions inside that window would otherwise open a second tab for the first one.
const REOPEN_GRACE: Duration = Duration::from_secs(20);

/// Opens the page unless the session already has a tab that will update itself into the new content.
///
/// `live` names the server and the page it is serving, when there is one.
fn maybe_open(cfg: &Config, dir: &Path, session: &str, url: &str, live: Option<(u16, &str)>) -> bool {
    if !cfg.auto_open {
        return false;
    }
    let marker = dir.join(format!("{}.opened", slug(session)));
    if cfg.open_once_per_session && marker.exists() && !tab_is_gone(cfg, &marker, live) {
        return false;
    }
    if browser::open_url(url, &cfg.browser).is_err() {
        return false;
    }
    // Rewriting the marker restarts the grace period, so a tab that is still starting up is not mistaken for a closed one.
    let _ = std::fs::write(&marker, b"");
    true
}

/// Whether the tab this session was opened in is provably gone, which is the one case where opening again is right.
///
/// Proof is the point. Only the live-reload server knows whether a page is attached, so a `file://` page, a server that could not start and a server too old to answer the question all mean "assume it is still there" rather than "open another tab".
fn tab_is_gone(cfg: &Config, marker: &Path, live: Option<(u16, &str)>) -> bool {
    if !cfg.reopen_if_closed {
        return false;
    }
    // In timestamped mode each turn is a different page, so an open tab is watching the previous one and would never have refreshed into this answer anyway. Reopening on that basis would mean a new tab every single turn.
    if cfg.file_mode != FileMode::Overwrite {
        return false;
    }
    let Some((port, served)) = live else { return false };
    if opened_within(marker, REOPEN_GRACE) {
        return false;
    }
    server::watchers(port, served) == Some(0)
}

/// Whether the marker was written less than `window` ago, treating an unreadable timestamp as "just now".
fn opened_within(marker: &Path, window: Duration) -> bool {
    let Ok(modified) = std::fs::metadata(marker).and_then(|m| m.modified()) else {
        return true;
    };
    modified.elapsed().map(|age| age < window).unwrap_or(true)
}

/// Returns the port of a live-reload server, starting one in the background if nothing is listening yet.
///
/// Every failure here is soft: `None` just means the page falls back to `file://` with its reload timer, which is worse but still works.
fn ensure_server(cfg: &Config, dir: &Path) -> Option<u16> {
    if let Some(handle) = server::running(dir) {
        return Some(handle.port);
    }
    spawn_server(cfg, dir).ok()?;

    // The child has to bind a port and write its record before we can name a URL, so wait briefly for it.
    let deadline = Instant::now() + Duration::from_millis(1500);
    while Instant::now() < deadline {
        if let Some(handle) = server::running(dir) {
            return Some(handle.port);
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    None
}

/// Starts `c2md serve` as a detached background process that outlives this hook run.
fn spawn_server(cfg: &Config, dir: &Path) -> std::io::Result<()> {
    seal_std_handles();

    let exe = std::env::current_exe()?;
    let mut cmd = Command::new(exe);
    cmd.arg("serve")
        .arg("--dir")
        .arg(dir)
        .arg("--port")
        .arg(cfg.port.to_string())
        .arg("--idle")
        .arg(cfg.server_idle_secs.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    detach(&mut cmd);
    cmd.spawn().map(|_| ())
}

/// Clears the inherit flag on every handle this process holds, so the background server inherits nothing.
///
/// This is what keeps the hook from hanging the turn. The server outlives the hook, so any pipe handle it inherits stays open, and whoever is reading that pipe waits for an end of file that never comes. Claude Code would sit on a hook that already exited until the timeout fired.
///
/// Sealing only the three standard handles is not enough. Windows spawns with inheritance switched on for the whole handle table, so a pipe belonging to a grandparent, inherited by the hook without ever being one of its standard streams, is passed down just the same. Sweeping the table is the only way to catch those, and clearing the flag on a handle costs this process nothing: it stays fully usable here and merely stops being copied into children.
#[cfg(windows)]
fn seal_std_handles() {
    const HANDLE_FLAG_INHERIT: u32 = 0x0000_0001;
    // Windows hands out handle values in multiples of four, and this ceiling is far above what a hook ever holds.
    const MAX_HANDLE: isize = 1 << 16;

    extern "system" {
        fn SetHandleInformation(handle: isize, mask: u32, flags: u32) -> i32;
        fn GetProcessHandleCount(process: isize, count: *mut u32) -> i32;
        fn GetCurrentProcess() -> isize;
    }

    unsafe {
        let mut total = 0u32;
        if GetProcessHandleCount(GetCurrentProcess(), &mut total) == 0 {
            total = 512;
        }

        // Stop as soon as every live handle has been seen, rather than always walking to the ceiling.
        let mut sealed = 0u32;
        let mut handle = 4isize;
        while handle <= MAX_HANDLE && sealed < total {
            if SetHandleInformation(handle, HANDLE_FLAG_INHERIT, 0) != 0 {
                sealed += 1;
            }
            handle += 4;
        }
    }
}

/// Unix keeps the hook's pipes out of the child through `Stdio::null` plus a new session, so nothing extra is needed.
#[cfg(not(windows))]
fn seal_std_handles() {}

/// Detaches the child so it survives this process exiting.
///
/// `CREATE_BREAKAWAY_FROM_JOB` matters because a hook may run inside a job object that kills its children on close, which would take the server down with the hook that started it.
#[cfg(windows)]
fn detach(cmd: &mut Command) {
    use std::os::windows::process::CommandExt;
    const DETACHED_PROCESS: u32 = 0x0000_0008;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    const CREATE_BREAKAWAY_FROM_JOB: u32 = 0x0100_0000;
    cmd.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP | CREATE_BREAKAWAY_FROM_JOB);
}

/// Puts the child in its own session, detaching it from the process group and controlling terminal of the hook.
#[cfg(not(windows))]
fn detach(cmd: &mut Command) {
    use std::os::unix::process::CommandExt;
    extern "C" {
        fn setsid() -> i32;
    }
    unsafe {
        cmd.pre_exec(|| {
            setsid();
            Ok(())
        });
    }
}

/// Builds the output path, one stable file per session or a new timestamped one per turn.
fn output_path(dir: &Path, session: &str, mode: FileMode) -> PathBuf {
    let base = slug(session);
    match mode {
        FileMode::Overwrite => dir.join(format!("{base}.html")),
        FileMode::Timestamped => dir.join(format!("{base}-{}.html", epoch_ms())),
    }
}

/// Writes through a temporary file so the auto-refreshing page never loads a half-written document.
fn write_atomic(path: &Path, contents: &str) -> std::io::Result<()> {
    let tmp = path.with_extension("html.tmp");
    std::fs::write(&tmp, contents)?;
    std::fs::rename(&tmp, path)
}

fn append_log(dir: &Path, line: &str) {
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(dir.join("c2md.log")) {
        let _ = f.write_all(line.as_bytes());
    }
}

/// Reduces an identifier to characters that are safe in a filename on every platform.
fn slug(s: &str) -> String {
    let cleaned: String = s
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
        .take(40)
        .collect();
    if cleaned.is_empty() {
        "session".to_string()
    } else {
        cleaned
    }
}

fn epoch_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn install_cmd(args: &[String]) -> Result<String, String> {
    let target = parse_target(args)?;
    let msg = install::install(target, &install::executable(target))?;
    let cfg_note = match config::config_path() {
        Some(p) if !p.exists() => format!("\n\nRun `c2md init` to write a config file at {}.", config::tildify(&p)),
        _ => String::new(),
    };
    Ok(format!("{msg}{cfg_note}"))
}

fn uninstall_cmd(args: &[String]) -> Result<String, String> {
    install::uninstall(parse_target(args)?)
}

fn parse_target(args: &[String]) -> Result<install::Target, String> {
    match args.first() {
        None => Ok(install::Target::User),
        Some(a) => install::Target::parse(a).ok_or_else(|| format!("unknown target `{a}`, expected --user, --project or --local")),
    }
}

fn init_cmd() -> Result<String, String> {
    let path = config::config_path().ok_or("cannot locate a config directory")?;
    if path.exists() {
        return Err(format!("{} already exists, edit it or delete it first", config::tildify(&path)));
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    std::fs::write(&path, Config::default().to_json() + "\n").map_err(|e| e.to_string())?;
    Ok(format!("wrote defaults to {}", config::tildify(&path)))
}

fn config_cmd() -> Result<String, String> {
    let cfg = Config::load();
    let path = config::config_path();
    let source = match &path {
        Some(p) if p.exists() => format!("file: {}", config::tildify(p)),
        Some(p) => format!("file: {} (not present, using defaults)", config::tildify(p)),
        None => "file: unavailable".to_string(),
    };
    Ok(format!(
        "{source}\noutput: {}\n\n{}",
        config::tildify(&cfg.resolved_output_dir()),
        cfg.to_json()
    ))
}

/// Answers the questions someone asks when nothing opened, in the order they would ask them.
///
/// A Stop hook is invisible by design: it fails silently so it cannot interrupt a session, which means a misconfiguration produces no output anywhere. Everything this prints was previously only discoverable by opening a JSON file by hand and guessing.
fn status_cmd() -> Result<String, String> {
    let cfg = Config::load();
    let dir = cfg.resolved_output_dir();
    let mut out = String::new();

    let installed = [
        ("user", install::Target::User),
        ("project", install::Target::Project),
        ("local", install::Target::Local),
    ]
    .into_iter()
    .map(|(label, target)| (label, target, install::installed_commands(target)))
    .filter(|(_, _, hooks)| !hooks.is_empty())
    .collect::<Vec<_>>();

    if installed.is_empty() {
        out.push_str("hooks:     not registered  (run `c2md install --user`)\n");
    } else {
        for (label, target, hooks) in &installed {
            let path = target.path().map(|p| config::tildify(&p)).unwrap_or_default();
            out.push_str(&format!("hooks:     {label:<8} in {path}\n"));
            for (event, cmd) in hooks {
                out.push_str(&format!("           {event:<11} {cmd}\n"));
            }
        }
    }

    out.push_str(&format!("enabled:   {}\n", if cfg.enabled { "yes" } else { "no  (enabled is false)" }));

    let config_note = match config::config_path() {
        Some(p) if p.exists() => config::tildify(&p),
        Some(p) => format!("{} (not present, using defaults)", config::tildify(&p)),
        None => "unavailable".to_string(),
    };
    out.push_str(&format!("config:    {config_note}\n"));
    out.push_str(&format!("output:    {}\n", config::tildify(&dir)));

    let server = match (cfg.live_reload, server::running(&dir)) {
        (false, _) => "off  (live_reload is false, pages open as file://)".to_string(),
        (true, Some(handle)) => format!("running on http://127.0.0.1:{}", handle.port),
        (true, None) => "not running  (it starts on the next answer)".to_string(),
    };
    out.push_str(&format!("server:    {server}\n"));

    let newest = newest_page(&dir);
    out.push_str(&format!(
        "page:      {}\n",
        newest.map(|p| config::tildify(&p)).unwrap_or_else(|| "none rendered yet".to_string())
    ));

    Ok(out.trim_end().to_string())
}

fn render_cmd(args: &[String]) -> Result<String, String> {
    let mut input = None;
    let mut output = None;
    let mut open_after = false;

    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "-o" | "--output" => output = it.next().map(PathBuf::from),
            "--open" => open_after = true,
            other => input = Some(PathBuf::from(other)),
        }
    }
    let input = input.ok_or("usage: c2md render <file.md> [-o out.html] [--open]")?;
    let md = std::fs::read_to_string(&input).map_err(|e| format!("cannot read {}: {e}", input.display()))?;

    let cfg = Config::load();
    let answer = transcript::Answer {
        segments: vec![transcript::Segment { thinking: false, body: md }],
        prompt: None,
        model: None,
        bytes_read: 0,
    };

    let meta = render::PageMeta { cwd: None, epoch_ms: epoch_ms(), live: false };
    let page = render::page(&answer, &cfg, &meta);

    let out = output.unwrap_or_else(|| cfg.resolved_output_dir().join("render.html"));
    write_atomic(&out, &page.html).map_err(|e| format!("cannot write {}: {e}", out.display()))?;

    if open_after {
        browser::open(&out, &cfg.browser).map_err(|e| e.to_string())?;
    }
    Ok(format!("{} ({} bytes, {} us)", out.display(), page.html.len(), page.render_us))
}

fn open_cmd() -> Result<String, String> {
    let cfg = Config::load();
    let dir = cfg.resolved_output_dir();
    let newest = newest_page(&dir).ok_or_else(|| format!("no rendered pages in {}", dir.display()))?;

    browser::open(&newest, &cfg.browser).map_err(|e| e.to_string())?;
    Ok(format!("opened {}", newest.display()))
}

/// The most recently written page in the output directory, which is the one `c2md open` means.
fn newest_page(dir: &Path) -> Option<PathBuf> {
    std::fs::read_dir(dir)
        .ok()?
        .filter_map(Result::ok)
        .filter(|e| e.path().extension().map(|x| x == "html").unwrap_or(false))
        .max_by_key(|e| e.metadata().and_then(|m| m.modified()).ok())
        .map(|e| e.path())
}

/// Times the real pipeline against a real transcript, separating the read, the render and the write.
fn bench_cmd(args: &[String]) -> Result<String, String> {
    let path = args
        .first()
        .map(PathBuf::from)
        .or_else(newest_transcript)
        .ok_or("no transcript given and none found under ~/.claude/projects")?;
    let iters: usize = args.get(1).and_then(|a| a.parse().ok()).unwrap_or(100);

    let cfg = Config::load();
    let dir = cfg.resolved_output_dir();
    let out = dir.join("bench.html");

    let mut extract = Vec::with_capacity(iters);
    let mut render = Vec::with_capacity(iters);
    let mut write = Vec::with_capacity(iters);
    let mut html_len = 0;
    let mut bytes_read = 0;

    for _ in 0..iters {
        let t = Instant::now();
        // A transcript on disk is finished, so the settle wait has nothing to wait for and costs nothing.
        let answer = transcript::extract(&path, cfg.scope, cfg.include_thinking, Duration::ZERO)
            .ok_or("no assistant answer found in that transcript")?;
        extract.push(t.elapsed().as_micros());
        bytes_read = answer.bytes_read;

        let t = Instant::now();
        let meta = render::PageMeta { cwd: None, epoch_ms: epoch_ms(), live: false };
        let page = render::page(&answer, &cfg, &meta);
        render.push(t.elapsed().as_micros());
        html_len = page.html.len();

        let t = Instant::now();
        write_atomic(&out, &page.html).map_err(|e| e.to_string())?;
        write.push(t.elapsed().as_micros());
    }

    let _ = std::fs::remove_file(&out);
    let total = median(&extract) + median(&render) + median(&write);
    Ok(format!(
        "transcript: {}\n  tail read     {} KiB\n  page          {} KiB\n\n  extract       {:>7} us\n  render        {:>7} us\n  write         {:>7} us\n  in-process    {:>7} us  (median of {iters})",
        path.display(),
        bytes_read / 1024,
        html_len / 1024,
        median(&extract),
        median(&render),
        median(&write),
        total
    ))
}

/// Runs the live-reload server in the foreground, which is how you watch its behaviour while debugging.
fn serve_cmd(args: &[String]) -> Result<String, String> {
    let cfg = Config::load();
    let mut dir = cfg.resolved_output_dir();
    let mut port = cfg.port;
    let mut idle = cfg.server_idle_secs;

    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--dir" => {
                if let Some(v) = it.next() {
                    dir = PathBuf::from(v);
                }
            }
            "--port" => {
                if let Some(v) = it.next().and_then(|v| v.parse().ok()) {
                    port = v;
                }
            }
            "--idle" => {
                if let Some(v) = it.next().and_then(|v| v.parse().ok()) {
                    idle = v;
                }
            }
            other => return Err(format!("unknown option `{other}`")),
        }
    }
    let _ = std::fs::create_dir_all(&dir);
    server::serve(dir, port, Duration::from_secs(idle)).map_err(|e| e.to_string())?;
    Ok(String::new())
}

/// Stops the background server, if one is running.
///
/// Nothing here is an error. This runs as a SessionEnd hook, where a non-zero exit is reported to the user as a failed hook, and every way this can go wrong — no server, one that has already gone, one still serving somebody — is a perfectly ordinary outcome.
fn stop_cmd(args: &[String]) -> Result<String, String> {
    let mut if_unwatched = false;
    for arg in args {
        match arg.as_str() {
            "--if-unwatched" => if_unwatched = true,
            other => return Err(format!("unknown option `{other}`")),
        }
    }

    let dir = Config::load().resolved_output_dir();
    if !if_unwatched {
        return Ok(match server::stop(&dir) {
            Some(port) => format!("stopped the server on port {port}"),
            None => "no server running".to_string(),
        });
    }

    Ok(match server::stop_if_unwatched(&dir) {
        server::StopResult::NotRunning => "no server running".to_string(),
        server::StopResult::Stopped(port) => format!("stopped the server on port {port}"),
        server::StopResult::Watched { port, pages } => {
            format!("left the server on port {port} running, {pages} page(s) still open")
        }
        server::StopResult::Unreachable(port) => {
            format!("left the server on port {port} alone, it did not answer")
        }
    })
}

fn median(v: &[u128]) -> u128 {
    let mut s = v.to_vec();
    s.sort_unstable();
    s.get(s.len() / 2).copied().unwrap_or(0)
}

/// Finds the most recently touched transcript so `c2md bench` works with no arguments.
fn newest_transcript() -> Option<PathBuf> {
    let projects = config::claude_dir()?.join("projects");
    let mut best: Option<(std::time::SystemTime, PathBuf)> = None;
    for project in std::fs::read_dir(projects).ok()?.filter_map(Result::ok) {
        let Ok(files) = std::fs::read_dir(project.path()) else { continue };
        for f in files.filter_map(Result::ok) {
            let p = f.path();
            if p.extension().map(|e| e != "jsonl").unwrap_or(true) {
                continue;
            }
            let Ok(modified) = f.metadata().and_then(|m| m.modified()) else { continue };
            if best.as_ref().map(|(t, _)| modified > *t).unwrap_or(true) {
                best = Some((modified, p));
            }
        }
    }
    best.map(|(_, p)| p)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slug_strips_path_separators() {
        assert_eq!(slug("a/b\\c:d"), "a_b_c_d");
        assert_eq!(slug(""), "session");
    }

    /// Writes a marker whose age is what `tab_is_gone` reads, so the grace period can be tested without waiting it out.
    fn marker_aged(name: &str, age: Duration) -> PathBuf {
        let path = std::env::temp_dir().join(format!("c2md-marker-{}-{name}.opened", std::process::id()));
        std::fs::write(&path, b"").unwrap();
        let when = std::time::SystemTime::now() - age;
        // Windows only lets a handle opened for writing change a timestamp, which is why this is not a plain `File::open`.
        std::fs::OpenOptions::new()
            .write(true)
            .open(&path)
            .and_then(|f| f.set_modified(when))
            .expect("the test needs to age its own marker file");
        path
    }

    #[test]
    fn a_page_nobody_is_watching_is_reopened_but_only_after_the_grace_period() {
        let cfg = Config::default();
        assert!(cfg.reopen_if_closed, "reopening a closed page is the default");

        // No server means no way to know, so the page is left alone.
        let old = marker_aged("old", Duration::from_secs(120));
        assert!(!tab_is_gone(&cfg, &old, None), "without a server the tab is assumed to still be open");

        // A tab opened moments ago has not had time to attach its event stream yet.
        let fresh = marker_aged("fresh", Duration::ZERO);
        assert!(opened_within(&fresh, REOPEN_GRACE), "a just-opened tab is inside the grace period");
        assert!(!opened_within(&old, REOPEN_GRACE), "a tab opened two minutes ago is not");

        let off = Config { reopen_if_closed: false, ..Config::default() };
        assert!(!tab_is_gone(&off, &old, Some((1, "sess"))), "the setting switches the whole behaviour off");

        let timestamped = Config { file_mode: FileMode::Timestamped, ..Config::default() };
        assert!(
            !tab_is_gone(&timestamped, &old, Some((1, "sess"))),
            "timestamped pages are never refreshed in place, so a missing watcher proves nothing"
        );
    }

    #[test]
    fn overwrite_mode_is_stable_across_turns() {
        let dir = Path::new("/tmp");
        let a = output_path(dir, "abc", FileMode::Overwrite);
        let b = output_path(dir, "abc", FileMode::Overwrite);
        assert_eq!(a, b);
    }
}
