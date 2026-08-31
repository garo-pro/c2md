# c2md

A Claude Code Stop hook that renders the answer Claude just gave as a styled HTML page and opens it in your browser.

Long answers are easier to read in a browser than in a terminal: real typography, a comfortable measure, syntax-highlighted code, tables that do not wrap into soup. c2md gives you that automatically, one tab per session, updating itself as the session goes on.

## How it works

Claude Code fires the `Stop` hook every time an answer finishes, handing the hook a JSON payload on stdin that includes `transcript_path` — the session's JSONL file.

c2md reads the tail of that file, walks backwards to the human message that started the turn, collects the assistant text blocks in between, renders them to HTML and writes one page per session. On the first turn it launches your browser; on later turns it rewrites the file and tells the page, which swaps in the new answer without reloading.

There is no clipboard involved and no slash command. Hooks cannot drive the interactive copy picker, and reading the transcript directly is both faster and more reliable than a clipboard round trip would be.

## Install

Build the binary, then register the hook.

```bash
cargo build --release
./target/release/c2md install --user     # or --project, or --local
./target/release/c2md init               # writes ~/.claude/c2md.json with every default
```

For a user-wide hook, copy the binary out of the build directory first so the registered command does not depend on `target/` surviving a `cargo clean`. Install from the copy, since `install` registers the location of whichever executable you ran it from.

```bash
mkdir -p ~/.claude/bin
cp target/release/c2md ~/.claude/bin/
~/.claude/bin/c2md install --user
```

`install` edits the `hooks.Stop` array of the chosen settings file in place, backing the original up to `settings.json.bak` first, and leaves any other hooks alone. `c2md uninstall` takes only the c2md entry back out.

### What gets registered

`install` writes the most portable command the binary's location allows, in this order.

| Situation | Registered command |
| --- | --- |
| This exact binary is on your `PATH` | `c2md hook` |
| `--project` or `--local`, binary inside the project | `${CLAUDE_PROJECT_DIR}/target/release/c2md.exe hook` |
| Anything else | the absolute path |

`$CLAUDE_PROJECT_DIR` is expanded by Claude Code itself and always points at the project root, so a project-scoped settings file stays valid when the checkout moves and is safe to commit. The braced spelling is used because bare `$CLAUDE_PROJECT_DIR` is read as an undefined variable when the hook runs under PowerShell.

A bare relative path is deliberately never registered. Claude Code resolves one against the directory `claude` was launched from rather than the project root, so it breaks silently whenever a session starts from a subdirectory.

The `PATH` check compares resolved paths, not names, so a different `c2md` earlier on your `PATH` is never adopted by mistake. Put the binary on your `PATH` before installing to get the shortest form.

## Settings

Settings live in `~/.claude/c2md.json`, or wherever `$C2MD_CONFIG` points. Every key has an environment override named after it in upper case with a `C2MD_` prefix, so `C2MD_AUTO_OPEN=0 claude` disables opening for one session without touching the file.

| Key | Default | What it does |
| --- | --- | --- |
| `enabled` | `true` | Master switch. False makes the hook a no-op. |
| `auto_open` | `true` | Launch a browser. False still writes the HTML, which suits an editor preview pane. |
| `open_once_per_session` | `true` | Open a tab on the first answer only, and let that tab update itself after that. Set false to get a fresh launch every turn. |
| `reopen_if_closed` | `true` | Open the page again when an answer lands and the tab that would have refreshed itself has been closed. Needs `live_reload`, since the server is what knows whether a page is still attached. |
| `scope` | `"turn"` | `turn` renders every assistant message since your prompt; `last` renders only the final one. |
| `include_thinking` | `false` | Add the model's thinking blocks in a collapsed section. |
| `output_dir` | `""` | Where pages are written. Empty means `c2md` inside the system temp directory. |
| `file_mode` | `"overwrite"` | `overwrite` keeps one page per session; `timestamped` writes a new file per turn, building a history. |
| `browser` | `""` | Command to launch. Empty means the OS default handler. |
| `theme` | `"auto"` | `auto`, `light` or `dark`. Auto follows the system setting. |
| `title` | `"Claude Code"` | Page title and header text. |
| `min_chars` | `1` | Skip answers shorter than this, so a one-word reply does not take over a tab. |
| `live_reload` | `true` | Serve the page over loopback and push updates, so the tab changes only when the answer does. |
| `port` | `0` | Port for that server. Zero lets the OS pick a free one. |
| `server_idle_secs` | `1800` | Shut the server down after this long with no page watching and no new answers. |
| `auto_refresh_secs` | `2` | Reload interval for the `file://` fallback only, used when `live_reload` is off or the server cannot start. `0` disables it. |
| `code_highlight` | `true` | Load highlight.js from a CDN for fenced code. Turn off to keep the page fully offline. |
| `log` | `false` | Append a timing line per run to `c2md.log` in the output directory. |
| `notify` | `false` | Echo the page URL back into the Claude Code transcript after each render. |

## Staying current without reloading

The obvious way to keep a page fresh is `<meta http-equiv="refresh">`, and that is what c2md did first. It is unconditional: the document is torn down and rebuilt every couple of seconds whether or not anything changed, which throws away scroll position, text selection, open disclosure triangles and focus, and re-runs syntax highlighting for nothing.

A `file://` page cannot do better on its own. It has no way to ask whether the file changed, because `fetch` and `XHR` against `file://` are blocked, so a blind timer is the only mechanism available to it.

So c2md serves the page from a small HTTP server on 127.0.0.1 instead. The page holds an `EventSource` open and the server says nothing at all while nothing happens. When an answer lands the hook tells the server, the server sends one line, and the page fetches itself and replaces only the `main`, `header` and `footer` elements. Scroll position and selection survive untouched, and the timestamp flashes once so a change that arrives while you are reading is still noticeable.

Idle cost is a single open socket and no repaints. The server starts on the first answer of a session, is reused by every later hook run, and exits on its own after `server_idle_secs` with nothing watching it. `c2md stop` ends it immediately.

If the server cannot start, the page falls back to `file://` with the old reload timer, which is worse but never broken. In that mode it saves and restores scroll position across reloads.

### Closing the tab

`open_once_per_session` assumes the tab it opened is still there. Close it and the next answers go nowhere: the file is written, the server is told, and nothing is listening.

The server already knows, because a closed tab drops its event stream, so it counts the streams attached to each page and answers `/watchers/<page>`. When an answer lands on a page with no watchers, `reopen_if_closed` opens it again.

Two things keep that from firing when it should not. A tab is given twenty seconds to start up and attach before its absence counts, so answering two questions in quick succession does not open a second tab for the first one. And an event stream is detected as gone within a fifth of a second rather than at the next keepalive, so the count the hook reads is about the tab as it is now.

The check is only made when the answer is one a live page would have refreshed into: under `file://`, or with `live_reload` off, nothing can tell an open tab from a closed one, and under `file_mode: timestamped` every turn is a new page that no existing tab was showing anyway. In those cases the tab is assumed to still be open, because a wrong guess costs a tab you did not ask for.

## Commands

```
c2md hook                              read a Stop payload on stdin and render (the default)
c2md install [--user|--project|--local]
c2md uninstall [--user|--project|--local]
c2md init                              write a config file with every setting at its default
c2md config                            print the resolved settings and where they came from
c2md render <file.md> [-o out.html] [--open]
c2md open                              re-open the most recently rendered page
c2md stop                              stop the background live-reload server
c2md bench [transcript.jsonl] [iters]  time the pipeline against a real transcript
```

`c2md render bench/sample.md --open` is the quickest way to see a template change, since the sample exercises every element the CSS styles.

## Performance

A Stop hook runs once per answer, so its cost is paid on every single turn. That budget drove two decisions, both measured rather than assumed.

### Markdown engine

`cargo run --release -p mdbench` renders a corpus shaped like a real Claude Code answer — prose, fenced code, nested lists, a GFM table, task lists, inline marks — and reports the median of 400 runs.

| Engine | 1 KiB | 11 KiB | 117 KiB | Throughput |
| --- | ---: | ---: | ---: | ---: |
| pulldown-cmark | 3.9 us | 26.2 us | 251.9 us | 454 MiB/s |
| comrak | 18.0 us | 133.8 us | 1371.5 us | 84 MiB/s |
| markdown-rs | 225.9 us | 1395.8 us | 18938.9 us | 6 MiB/s |
| marked (Node) | 53.9 us | 371.3 us | 3692.5 us | 31 MiB/s |
| markdown-it (Node) | 63.4 us | 426.6 us | 4286.5 us | 27 MiB/s |

pulldown-cmark wins by 5x over the next Rust engine and 14x over the fastest JavaScript one. It is a pull parser, so it streams events straight into the output buffer instead of building an AST first, which is where most of the lead comes from.

### Runtime

`bench/hook-latency.ps1` times whole process invocations, spawn included, against an equivalent Node implementation in `bench/node/hook.mjs` that does the same four things.

| Implementation | Median | Min |
| --- | ---: | ---: |
| c2md (Rust) | 6.80 ms | 6.35 ms |
| Node + marked | 53.83 ms | 51.01 ms |

The gap is almost entirely interpreter startup, which is why this is a compiled binary. Rendering is not the expensive part of a hook; being a process at all is.

About 2 ms of the c2md figure is live reload: one loopback round trip to check the server is alive before rendering, and a second to tell it the answer changed. The Node column does no such thing, so it is flattered slightly by the comparison.

### Inside the process

`c2md bench` breaks the pipeline down. On a 1 MB transcript with a typical answer, extraction reads a 64 KiB tail in about 180 us and rendering takes about 25 us; the rest is the file write, which is dominated by the OS and on Windows by whatever the antivirus does to a newly created file.

The transcript is never read whole. c2md reads a 64 KiB window off the end and grows it fourfold only if the human message that opened the turn is further back than that, so cost stays flat as a session grows.

## Layout

```
crates/c2md      the hook binary, two dependencies: pulldown-cmark and serde_json
crates/mdbench   the markdown engine comparison
bench/           the cross-runtime latency harness and the CSS sample
```

## Notes

Subagent output is skipped: sidechain entries share the transcript file but are not the answer you were given.

A turn that ends in tool calls with no prose renders nothing, and the hook exits quietly. The same is true of an empty or unreadable transcript — a Stop hook that fails would interrupt the session, so every error path here exits successfully and silently.

The background server is spawned with every inherited handle sealed against inheritance. Windows enables handle inheritance for the whole table when spawning, so without that sweep the server would hold a copy of the pipe Claude Code reads the hook through; because the server outlives the hook, that pipe would never reach end of file and every turn would stall until the hook timeout.

In `timestamped` mode each turn is served under its own filename, so those pages are static snapshots rather than live ones. Live updating only makes sense for `overwrite`, where the session has one page that keeps changing.
