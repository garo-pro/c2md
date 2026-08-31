//! Markdown to HTML rendering and the surrounding page template.
//!
//! pulldown-cmark was picked after benchmarking it against comrak and markdown-rs; see `crates/mdbench` and the numbers in the README. It is a pull parser, so it streams straight into the output buffer with no intermediate AST allocation, which is where most of its lead comes from.

use std::time::Instant;

use pulldown_cmark::{html, Event, Options, Parser};

use crate::config::{Config, Theme};
use crate::server::VERSION_TOKEN;
use crate::transcript::Answer;

/// Markdown extensions enabled for rendering, chosen to match what Claude Code actually emits.
fn options() -> Options {
    let mut opts = Options::empty();
    opts.insert(Options::ENABLE_TABLES);
    opts.insert(Options::ENABLE_STRIKETHROUGH);
    opts.insert(Options::ENABLE_TASKLISTS);
    opts.insert(Options::ENABLE_FOOTNOTES);
    opts.insert(Options::ENABLE_HEADING_ATTRIBUTES);
    opts
}

/// Converts a markdown document to an HTML fragment, with any raw HTML in the source shown as text rather than emitted as markup.
///
/// pulldown-cmark passes HTML through untouched by default, which is the right call for a document you wrote and the wrong one here. An answer routinely quotes material Claude did not author — a file it was asked to summarise, a page it fetched, a diff — and this page is served from `127.0.0.1`, an origin shared by every other session the server is hosting. Markup arriving that way must not become an element, so the raw-HTML events are turned back into text and pulldown-cmark escapes them on the way out.
pub fn markdown_to_html(md: &str) -> String {
    // Rendered HTML runs a little over the source length, and pre-sizing avoids a handful of reallocations.
    let mut out = String::with_capacity(md.len() * 3 / 2 + 256);
    let events = Parser::new_ext(md, options()).map(|event| match event {
        Event::Html(raw) | Event::InlineHtml(raw) => Event::Text(raw),
        other => other,
    });
    html::push_html(&mut out, events);
    out
}

/// Metadata shown in the page header and footer.
pub struct PageMeta<'a> {
    pub cwd: Option<&'a str>,
    /// When the answer was rendered. The page turns this into a local time in the browser, which keeps every timezone concern out of the binary.
    pub epoch_ms: u64,
    /// Whether the page will be served over HTTP with a live event stream, which decides if the reload timer is needed at all.
    pub live: bool,
}

/// A finished page and how long building it took.
pub struct Page {
    pub html: String,
    /// Microseconds spent turning the answer into HTML, which the footer also reports.
    pub render_us: u128,
}

/// Builds the complete standalone HTML page for one answer.
///
/// The render is timed in here rather than by the caller because the footer quotes the figure. Substituting it afterwards meant a `replace` across the whole document, which would have rewritten the placeholder wherever it appeared — including inside an answer that happened to quote it.
pub fn page(answer: &Answer, cfg: &Config, meta: &PageMeta) -> Page {
    let started = Instant::now();
    let mut body = String::with_capacity(8 * 1024);

    if let Some(prompt) = &answer.prompt {
        body.push_str("<details class=\"prompt\"><summary>Prompt</summary>\n");
        body.push_str(&markdown_to_html(prompt));
        body.push_str("</details>\n");
    }

    for segment in &answer.segments {
        if segment.thinking {
            body.push_str("<details class=\"thinking\"><summary>Thinking</summary>\n");
            body.push_str(&markdown_to_html(&segment.body));
            body.push_str("</details>\n");
        } else {
            body.push_str(&markdown_to_html(&segment.body));
            body.push('\n');
        }
    }

    // The reload timer is the `file://` fallback only. A served page is told when something changed, so a timer would be pure waste.
    let refresh = if !meta.live && cfg.auto_refresh_secs > 0 {
        format!("<meta http-equiv=\"refresh\" content=\"{}\">\n", cfg.auto_refresh_secs)
    } else {
        String::new()
    };

    // Only a served page has a version to carry, and putting the placeholder in the head means the server can substitute the first occurrence and be certain it found the right one.
    let version_meta = if meta.live {
        format!("<meta name=\"c2md-version\" content=\"{VERSION_TOKEN}\">\n")
    } else {
        String::new()
    };

    let nonce = nonce();
    let highlight = if cfg.code_highlight { highlight_snippet(cfg.theme, &nonce) } else { String::new() };

    let color_scheme = match cfg.theme {
        Theme::Auto => "light dark",
        Theme::Light => "light",
        Theme::Dark => "dark",
    };
    let theme_attr = match cfg.theme {
        Theme::Auto => "",
        Theme::Light => " data-theme=\"light\"",
        Theme::Dark => " data-theme=\"dark\"",
    };

    let mut footer = String::new();
    if let Some(cwd) = meta.cwd {
        footer.push_str(&format!("<span>{}</span>", escape(cwd)));
    }
    if let Some(model) = &answer.model {
        footer.push_str(&format!("<span>{}</span>", escape(model)));
    }
    // The index only exists on a served page; under file:// there is no route to link to.
    if meta.live {
        footer.push_str("<span><a href=\"/\">all pages</a></span>");
    }

    let render_us = started.elapsed().as_micros();
    footer.push_str(&format!("<span>rendered in {render_us} \u{b5}s</span>"));

    let html = format!(
        r#"<!doctype html>
<html lang="en"{theme_attr}>
<head>
<meta charset="utf-8">
<meta http-equiv="Content-Security-Policy" content="{csp}">
<meta name="viewport" content="width=device-width, initial-scale=1">
{version_meta}{refresh}<title>{title}</title>
<style nonce="{nonce}">
:root {{ color-scheme: {color_scheme}; --bg:#fdfdfc; --fg:#20201d; --muted:#6b6a63; --rule:#e4e2dd; --code-bg:#f4f3f0; --accent:#b85c2e; --link:#1a6b8f; }}
@media (prefers-color-scheme: dark) {{ :root:not([data-theme="light"]) {{ --bg:#161614; --fg:#e6e4de; --muted:#9a978d; --rule:#2e2d29; --code-bg:#201f1c; --accent:#e08a55; --link:#66b8d8; }} }}
:root[data-theme="dark"] {{ --bg:#161614; --fg:#e6e4de; --muted:#9a978d; --rule:#2e2d29; --code-bg:#201f1c; --accent:#e08a55; --link:#66b8d8; }}
* {{ box-sizing: border-box; }}
body {{ margin:0; background:var(--bg); color:var(--fg); font:16px/1.65 -apple-system, BlinkMacSystemFont, "Segoe UI", Roboto, Helvetica, Arial, sans-serif; }}
.wrap {{ max-width: 46rem; margin: 0 auto; padding: 2rem 1.5rem 6rem; }}
header {{ display:flex; align-items:baseline; gap:.75rem; flex-wrap:wrap; padding-bottom:.75rem; margin-bottom:1.75rem; border-bottom:1px solid var(--rule); }}
header h1 {{ font-size:.95rem; font-weight:600; letter-spacing:.02em; margin:0; color:var(--accent); }}
header time {{ font-size:.8rem; color:var(--muted); }}
h1,h2,h3,h4 {{ line-height:1.3; margin:2rem 0 .6rem; }}
h1 {{ font-size:1.6rem; }} h2 {{ font-size:1.3rem; }} h3 {{ font-size:1.1rem; }} h4 {{ font-size:1rem; }}
p, ul, ol, blockquote, table, pre {{ margin:0 0 1rem; }}
a {{ color:var(--link); }}
code {{ font-family: ui-monospace, "SF Mono", "Cascadia Code", Consolas, monospace; font-size:.875em; background:var(--code-bg); padding:.12em .35em; border-radius:4px; }}
pre {{ background:var(--code-bg); padding:.9rem 1rem; border-radius:8px; overflow-x:auto; border:1px solid var(--rule); }}
pre code {{ background:none; padding:0; font-size:.85rem; line-height:1.55; }}
blockquote {{ margin-left:0; padding-left:1rem; border-left:3px solid var(--rule); color:var(--muted); }}
table {{ border-collapse:collapse; width:100%; display:block; overflow-x:auto; font-size:.92rem; }}
th, td {{ border:1px solid var(--rule); padding:.4rem .65rem; text-align:left; }}
th {{ background:var(--code-bg); }}
hr {{ border:none; border-top:1px solid var(--rule); margin:2rem 0; }}
img {{ max-width:100%; }}
ul, ol {{ padding-left:1.4rem; }}
li {{ margin:.2rem 0; }}
li input[type=checkbox] {{ margin-right:.4rem; }}
details {{ border:1px solid var(--rule); border-radius:8px; padding:.5rem .9rem; margin:0 0 1.5rem; background:var(--code-bg); }}
details summary {{ cursor:pointer; font-size:.8rem; letter-spacing:.06em; text-transform:uppercase; color:var(--muted); font-weight:600; }}
details[open] summary {{ margin-bottom:.6rem; }}
details.thinking {{ opacity:.85; }}
@keyframes c2md-flash {{ from {{ background: var(--code-bg); }} to {{ background: transparent; }} }}
body.c2md-updated header time {{ animation: c2md-flash 1.2s ease-out; border-radius:4px; }}
@media (prefers-reduced-motion: reduce) {{ body.c2md-updated header time {{ animation: none; }} }}
footer {{ margin-top:3rem; padding-top:.75rem; border-top:1px solid var(--rule); display:flex; gap:1rem; flex-wrap:wrap; font-size:.75rem; color:var(--muted); }}
</style>
</head>
<body>
<div class="wrap">
<header><h1>{title}</h1><time data-epoch="{epoch_ms}"></time></header>
<main>
{body}</main>
<footer>{footer}</footer>
</div>
<script nonce="{nonce}">
// Two ways to stay current, picked by how the page was opened.
//
// Over HTTP the server holds an event stream open and says nothing at all until an answer actually changes; the page then fetches itself, swaps the article element and leaves everything else, including scroll position and text selection, exactly where it was. Over file:// there is no server to ask, so the page falls back to reloading itself on a timer and restores scroll by hand.
(function () {{
  var KEY = "c2md:" + location.pathname;

  function saveScroll() {{
    try {{ sessionStorage.setItem(KEY, String(window.scrollY)); }} catch (e) {{}}
  }}
  addEventListener("scroll", saveScroll, {{ passive: true }});

  function stamp(root) {{
    var t = (root || document).querySelector("time[data-epoch]");
    if (!t) return;
    var d = new Date(Number(t.dataset.epoch));
    t.textContent = d.toLocaleString(undefined, {{ dateStyle: "medium", timeStyle: "medium" }});
    t.dateTime = d.toISOString();
  }}
  stamp(document);

  if (location.protocol !== "http:") {{
    // file:// cannot ask anything about the file, so the timer in the head is the only option and scroll has to be restored across the reload.
    try {{
      var y = sessionStorage.getItem(KEY);
      if (y !== null) window.scrollTo(0, parseInt(y, 10) || 0);
    }} catch (e) {{}}
    return;
  }}

  // The server stamps the current version into the head as it serves the page, so the page always knows exactly which one it is showing and cannot miss an update that landed while the browser was connecting.
  var tag = document.querySelector('meta[name="c2md-version"]');
  var version = tag ? tag.content : "";
  var busy = false;

  async function refresh() {{
    if (busy) return;
    busy = true;
    try {{
      var res = await fetch(location.pathname, {{ cache: "no-store" }});
      if (!res.ok) return;
      var doc = new DOMParser().parseFromString(await res.text(), "text/html");
      var next = doc.querySelector("main");
      var header = doc.querySelector("header");
      var footer = doc.querySelector("footer");
      if (!next) return;

      document.querySelector("main").replaceWith(next);
      if (header) document.querySelector("header").replaceWith(header);
      if (footer) document.querySelector("footer").replaceWith(footer);
      stamp(document);
      if (window.hljs) hljs.highlightAll();

      document.body.classList.remove("c2md-updated");
      void document.body.offsetWidth;
      document.body.classList.add("c2md-updated");
    }} catch (e) {{
    }} finally {{
      busy = false;
    }}
  }}

  function listen() {{
    var es = new EventSource("/events/" + location.pathname.replace(/^\//, ""));
    es.onmessage = function (e) {{
      if (e.data === version) return;
      version = e.data;
      refresh();
    }};
    // EventSource reconnects on its own, but not if the server exited; a manual retry picks up a server that came back on the same port.
    es.onerror = function () {{
      es.close();
      setTimeout(listen, 3000);
    }};
  }}
  listen();
}})();
</script>
{highlight}
</body>
</html>
"#,
        title = escape(&cfg.title),
        epoch_ms = meta.epoch_ms,
        csp = csp(cfg, &nonce),
        nonce = nonce,
        version_meta = version_meta,
        theme_attr = theme_attr,
        color_scheme = color_scheme,
        refresh = refresh,
        body = body,
        footer = footer,
        highlight = highlight,
    );

    Page { html, render_us }
}

/// One rendered page, as the index lists it.
pub struct IndexEntry {
    pub session: String,
    pub epoch_ms: u64,
    pub bytes: u64,
}

/// The directory listing served at `/`.
///
/// This is what makes `file_mode: timestamped` usable. That mode writes a page per turn and builds a real history of the session, which until now nothing could browse — the tab only ever showed the newest one and the older files just accumulated.
pub fn index(entries: &[IndexEntry]) -> String {
    let nonce = nonce();

    let rows = if entries.is_empty() {
        "<li class=\"empty\">Nothing rendered yet.</li>".to_string()
    } else {
        entries
            .iter()
            .map(|e| {
                format!(
                    "<li><a href=\"/{session}\">{session}</a><time data-epoch=\"{epoch}\"></time><span>{kib} KiB</span></li>",
                    session = escape(&e.session),
                    epoch = e.epoch_ms,
                    kib = e.bytes / 1024,
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    };

    format!(
        r#"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta http-equiv="Content-Security-Policy" content="default-src 'none'; script-src 'nonce-{nonce}'; style-src 'nonce-{nonce}'; base-uri 'none'; form-action 'none'">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>c2md</title>
<style nonce="{nonce}">
:root {{ color-scheme: light dark; --bg:#fdfdfc; --fg:#20201d; --muted:#6b6a63; --rule:#e4e2dd; --accent:#b85c2e; --link:#1a6b8f; }}
@media (prefers-color-scheme: dark) {{ :root {{ --bg:#161614; --fg:#e6e4de; --muted:#9a978d; --rule:#2e2d29; --accent:#e08a55; --link:#66b8d8; }} }}
body {{ margin:0; background:var(--bg); color:var(--fg); font:16px/1.65 -apple-system, BlinkMacSystemFont, "Segoe UI", Roboto, Helvetica, Arial, sans-serif; }}
.wrap {{ max-width:46rem; margin:0 auto; padding:2rem 1.5rem 6rem; }}
h1 {{ font-size:.95rem; font-weight:600; letter-spacing:.02em; margin:0 0 1.75rem; padding-bottom:.75rem; border-bottom:1px solid var(--rule); color:var(--accent); }}
ul {{ list-style:none; margin:0; padding:0; }}
li {{ display:flex; align-items:baseline; gap:1rem; padding:.5rem 0; border-bottom:1px solid var(--rule); }}
li a {{ color:var(--link); flex:1; font-family: ui-monospace, "SF Mono", "Cascadia Code", Consolas, monospace; font-size:.9rem; overflow-wrap:anywhere; }}
li time, li span {{ font-size:.75rem; color:var(--muted); white-space:nowrap; }}
li.empty {{ color:var(--muted); font-size:.9rem; border:none; }}
</style>
</head>
<body>
<div class="wrap">
<h1>c2md</h1>
<ul>
{rows}
</ul>
</div>
<script nonce="{nonce}">
// Timestamps are shipped as epoch milliseconds and localised here, which keeps every timezone concern out of the binary.
document.querySelectorAll("time[data-epoch]").forEach(function (t) {{
  var d = new Date(Number(t.dataset.epoch));
  t.textContent = d.toLocaleString(undefined, {{ dateStyle: "medium", timeStyle: "short" }});
  t.dateTime = d.toISOString();
}});
</script>
</body>
</html>
"#
    )
}

/// The page's Content-Security-Policy, which is the second line of defence behind escaping raw HTML.
///
/// Nothing loads by default. The page's own style and script carry the nonce; highlight.js is allowed by origin, and only when it is switched on. Inline event handlers — the `onerror` an injected `<img>` would rely on — match neither, so they do not run even if markup ever reaches the document.
///
/// Images stay wide open because markdown images are a real feature of an answer, and a remote image is a tracking pixel at worst rather than code.
fn csp(cfg: &Config, nonce: &str) -> String {
    let cdn = if cfg.code_highlight { " https://cdnjs.cloudflare.com" } else { "" };
    format!(
        "default-src 'none'; script-src 'nonce-{nonce}'{cdn}; style-src 'nonce-{nonce}'{cdn}; img-src * data:; font-src{cdn} data:; connect-src 'self'; base-uri 'none'; form-action 'none'"
    )
}

/// A fresh nonce for each rendered page.
///
/// This does not have to be unguessable by a local attacker, who can simply read the file. It has to be unguessable by the *content*, which was written before the page existed: markup quoted inside an answer cannot carry an attribute matching a value chosen after it was quoted.
fn nonce() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let mut state = nanos ^ ((std::process::id() as u64) << 32);

    // SplitMix64, so two renders a nanosecond apart do not produce neighbouring nonces.
    let mut out = String::with_capacity(32);
    for _ in 0..2 {
        state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^= z >> 31;
        out.push_str(&format!("{z:016x}"));
    }
    out
}

/// highlight.js pulled from a CDN, placed at the end of the body so it never delays the first paint of the answer.
///
/// Under `theme: auto` both stylesheets are linked and the media query decides which one applies, which costs one extra cached request and keeps the toggle instant.
///
/// The call used to ride on an `onload` attribute, which the page's own CSP now refuses to run. A nonced script after the library does the same job: a classic external script blocks the inline script behind it until it has finished executing.
fn highlight_snippet(theme: Theme, nonce: &str) -> String {
    const BASE: &str = "https://cdnjs.cloudflare.com/ajax/libs/highlight.js/11.11.1";
    let styles = match theme {
        Theme::Light => format!("<link rel=\"stylesheet\" href=\"{BASE}/styles/github.min.css\">"),
        Theme::Dark => format!("<link rel=\"stylesheet\" href=\"{BASE}/styles/github-dark.min.css\">"),
        Theme::Auto => format!(
            "<link rel=\"stylesheet\" href=\"{BASE}/styles/github.min.css\" media=\"(prefers-color-scheme: light)\">\n<link rel=\"stylesheet\" href=\"{BASE}/styles/github-dark.min.css\" media=\"(prefers-color-scheme: dark)\">"
        ),
    };
    format!(
        "{styles}\n<script src=\"{BASE}/highlight.min.js\" nonce=\"{nonce}\"></script>\n<script nonce=\"{nonce}\">hljs.highlightAll();</script>"
    )
}

/// Escapes the few characters that matter for text and double-quoted attributes.
pub fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            _ => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transcript::Segment;

    fn answer(md: &str) -> Answer {
        Answer {
            segments: vec![Segment { thinking: false, body: md.to_string() }],
            prompt: None,
            model: None,
            bytes_read: 0,
        }
    }

    fn meta(live: bool) -> PageMeta<'static> {
        PageMeta { cwd: None, epoch_ms: 0, live }
    }

    #[test]
    fn raw_html_in_an_answer_is_shown_as_text_not_run_as_markup() {
        let html = markdown_to_html("<script>alert(1)</script>\n\n<img src=x onerror=\"alert(2)\">");
        // The text of the attack survives as text — that is the point, the answer still reads correctly — so the assertions are about tags, not about the characters in them.
        assert!(!html.contains("<script"), "a script element must never reach the document: {html}");
        assert!(!html.contains("<img"), "nor an element that can carry an event handler: {html}");
        assert!(html.contains("&lt;script&gt;"), "the markup is still visible, as text: {html}");
        assert!(html.contains("&lt;img src=x onerror="), "all of it, verbatim: {html}");
    }

    #[test]
    fn markdown_features_still_render_as_markup() {
        let html = markdown_to_html("# Title\n\n| a | b |\n| - | - |\n| 1 | 2 |\n\n- [x] done\n\n~~gone~~");
        for tag in ["<h1>", "<table>", "<td>", "<del>", "type=\"checkbox\""] {
            assert!(html.contains(tag), "expected {tag} in: {html}");
        }
    }

    #[test]
    fn the_policy_blocks_inline_handlers_and_only_names_the_cdn_when_highlighting_is_on() {
        let default = page(&answer("hi"), &Config::default(), &meta(false));
        assert!(default.html.contains("Content-Security-Policy"));
        assert!(!default.html.contains("'unsafe-inline'"), "unsafe-inline would defeat the whole policy");
        assert!(default.html.contains("https://cdnjs.cloudflare.com"), "highlighting is on by default");

        let cfg = Config { code_highlight: false, ..Config::default() };
        let offline = page(&answer("hi"), &cfg, &meta(false));
        assert!(!offline.html.contains("cdnjs.cloudflare.com"), "nothing should be fetched with highlighting off");
    }

    #[test]
    fn every_script_and_style_carries_the_page_nonce() {
        let rendered = page(&answer("hi"), &Config::default(), &meta(false));
        let nonce = rendered
            .html
            .split("'nonce-")
            .nth(1)
            .and_then(|rest| rest.split('\'').next())
            .expect("the policy names a nonce")
            .to_string();
        assert_eq!(nonce.len(), 32, "nonce is two 64-bit words of hex");

        // Every tag the browser will execute or apply has to match, or the page renders unstyled and dead.
        let executable = rendered.html.matches("<script").count() + rendered.html.matches("<style").count();
        assert_eq!(
            rendered.html.matches(&format!("nonce=\"{nonce}\"")).count(),
            executable,
            "every script and style tag needs the nonce"
        );
    }

    #[test]
    fn nonces_differ_between_renders() {
        assert_ne!(nonce(), nonce());
    }

    #[test]
    fn the_reload_timer_and_version_stamp_are_exclusive() {
        let cfg = Config::default();

        let served = page(&answer("hi"), &cfg, &meta(true));
        assert!(!served.html.contains("http-equiv=\"refresh\""), "a served page is told when to update");
        assert!(served.html.contains(VERSION_TOKEN), "and carries a version for the server to stamp");

        let local = page(&answer("hi"), &cfg, &meta(false));
        assert!(local.html.contains("http-equiv=\"refresh\""), "a file:// page has only the timer");
        assert!(!local.html.contains(VERSION_TOKEN), "and no server to stamp a version");
    }

    #[test]
    fn the_version_placeholder_sits_ahead_of_the_answer_so_the_server_can_stamp_the_first_one() {
        // An answer is free to quote the token; the server replaces one occurrence, so the head's must come first.
        let rendered = page(&answer(VERSION_TOKEN), &Config::default(), &meta(true));
        let first = rendered.html.find(VERSION_TOKEN).unwrap();
        assert!(rendered.html[..first].contains("<head>"), "the placeholder must be in the head");
        assert!(!rendered.html[..first].contains("<main>"), "and ahead of the answer");
    }

    #[test]
    fn the_title_is_escaped_into_both_the_tag_and_the_heading() {
        let cfg = Config { title: "a<b>&\"c".to_string(), ..Config::default() };
        let rendered = page(&answer("hi"), &cfg, &meta(false));
        assert!(rendered.html.contains("a&lt;b&gt;&amp;&quot;c"));
        assert!(!rendered.html.contains("a<b>"));
    }

    #[test]
    fn the_theme_setting_reaches_both_the_root_attribute_and_the_colour_scheme() {
        let cfg = Config { theme: Theme::Dark, ..Config::default() };
        let dark = page(&answer("hi"), &cfg, &meta(false));
        assert!(dark.html.contains("<html lang=\"en\" data-theme=\"dark\">"));
        assert!(dark.html.contains("color-scheme: dark;"));

        let cfg = Config { theme: Theme::Auto, ..Config::default() };
        let auto = page(&answer("hi"), &cfg, &meta(false));
        assert!(auto.html.contains("<html lang=\"en\">"), "auto pins nothing and lets the media query decide");
        assert!(auto.html.contains("color-scheme: light dark;"));
    }

    #[test]
    fn the_index_is_linked_only_when_there_is_a_server_to_serve_it() {
        assert!(page(&answer("hi"), &Config::default(), &meta(true)).html.contains("href=\"/\""));
        assert!(!page(&answer("hi"), &Config::default(), &meta(false)).html.contains("href=\"/\""));
    }
}
