//! Benchmarks the candidate Markdown-to-HTML engines on a corpus shaped like a real Claude Code answer.
//!
//! Run with `cargo run --release -p mdbench -- [iterations]`. Each engine gets a warmup pass, then N timed runs; we report the median because it is far less noisy than the mean on a loaded desktop.

use std::time::{Duration, Instant};

/// One markdown document to feed every engine, kept alongside a label for the report.
struct Corpus {
    name: &'static str,
    text: String,
}

fn main() {
    let iters: usize = std::env::args()
        .nth(1)
        .and_then(|a| a.parse().ok())
        .unwrap_or(200);

    let corpora = vec![
        Corpus { name: "answer-small", text: answer_doc(1) },
        Corpus { name: "answer-typical", text: answer_doc(8) },
        Corpus { name: "answer-huge", text: answer_doc(80) },
    ];

    println!("mdbench: {iters} timed iterations per engine, median reported\n");

    for c in &corpora {
        println!("== {} ({} KiB) ==", c.name, c.text.len() / 1024);

        let mut rows = vec![
            ("pulldown-cmark", measure(iters, &c.text, render_pulldown)),
            ("comrak", measure(iters, &c.text, render_comrak)),
            ("markdown-rs", measure(iters, &c.text, render_markdown_rs)),
        ];
        rows.sort_by_key(|r| r.1.median);

        let fastest = rows[0].1.median.as_secs_f64();
        println!("{:<16} {:>11} {:>12} {:>9}", "engine", "median", "MiB/s", "vs best");
        for (name, s) in &rows {
            let secs = s.median.as_secs_f64();
            let throughput = (c.text.len() as f64 / (1024.0 * 1024.0)) / secs;
            println!(
                "{:<16} {:>9.3}us {:>12.1} {:>8.2}x  (out {} B)",
                name,
                secs * 1e6,
                throughput,
                secs / fastest,
                s.out_len
            );
        }
        println!();
    }
}

/// Timing result for a single engine on a single corpus.
struct Stats {
    median: Duration,
    out_len: usize,
}

/// Runs `f` once to warm caches and branch predictors, then `iters` timed times, returning the median.
fn measure(iters: usize, input: &str, f: fn(&str) -> String) -> Stats {
    let warm = f(input);
    let mut samples = Vec::with_capacity(iters);
    for _ in 0..iters {
        let start = Instant::now();
        let out = f(input);
        samples.push(start.elapsed());
        std::hint::black_box(out);
    }
    samples.sort();
    Stats { median: samples[samples.len() / 2], out_len: warm.len() }
}

fn render_pulldown(input: &str) -> String {
    use pulldown_cmark::{html, Options, Parser};
    let mut opts = Options::empty();
    opts.insert(Options::ENABLE_TABLES);
    opts.insert(Options::ENABLE_STRIKETHROUGH);
    opts.insert(Options::ENABLE_TASKLISTS);
    opts.insert(Options::ENABLE_FOOTNOTES);
    let parser = Parser::new_ext(input, opts);
    let mut out = String::with_capacity(input.len() * 3 / 2);
    html::push_html(&mut out, parser);
    out
}

fn render_comrak(input: &str) -> String {
    let mut opts = comrak::Options::default();
    opts.extension.table = true;
    opts.extension.strikethrough = true;
    opts.extension.tasklist = true;
    opts.extension.footnotes = true;
    comrak::markdown_to_html(input, &opts)
}

fn render_markdown_rs(input: &str) -> String {
    markdown::to_html_with_options(input, &markdown::Options::gfm()).unwrap_or_default()
}

/// Builds a synthetic answer by repeating a block that mirrors what Claude Code actually emits: prose, fenced code, nested lists, a table, inline code, links and emphasis.
fn answer_doc(reps: usize) -> String {
    let block = r#"
## Fixing the token refresh race

The refresh path had two callers racing on the same `RefreshToken`, so the second one always got a
`401` back and *silently* dropped the session. The fix is a single-flight guard around the refresh.

### What changed

1. `auth/session.rs` now holds a `Mutex<Option<JoinHandle<..>>>` for the in-flight refresh.
2. Callers `await` the existing handle instead of starting a second request.
3. Added a **jittered** backoff so a thundering herd after a deploy does not re-create the race.

```rust
async fn refresh(&self) -> Result<Token, AuthError> {
    let mut guard = self.inflight.lock().await;
    if let Some(handle) = guard.as_ref() {
        return handle.clone().await?;
    }
    let handle = tokio::spawn(do_refresh(self.client.clone())).shared();
    *guard = Some(handle.clone());
    drop(guard);
    handle.await
}
```

| Case | Before | After |
| --- | ---: | ---: |
| Single caller | 120 ms | 118 ms |
| Ten concurrent | 9 failures | 0 failures |
| Cold start | 340 ms | 210 ms |

- [ ] Backfill the metric for `auth.refresh.single_flight`
- [x] Land the guard
- [x] Add the regression test in `tests/refresh_race.rs`

See the [RFC](https://example.invalid/rfc/0042) for why we did not just widen the lock. ~~Widening~~
would have serialized every unrelated read, and the p99 on `GET /me` was already at 80 ms.

> Note: this does not address the separate clock-skew problem on the `exp` claim. That is tracked
> separately and needs a server-side fix.
"#;
    let mut s = String::with_capacity(block.len() * reps + 64);
    s.push_str("# Session answer\n");
    for _ in 0..reps {
        s.push_str(block);
    }
    s
}
