//! Markdown to HTML rendering and the surrounding page template.
//!
//! pulldown-cmark was picked after benchmarking it against comrak and markdown-rs; see `crates/mdbench` and the numbers in the README. It is a pull parser, so it streams straight into the output buffer with no intermediate AST allocation, which is where most of its lead comes from.

use pulldown_cmark::{html, Options, Parser};

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

/// Converts a markdown document to an HTML fragment.
pub fn markdown_to_html(md: &str) -> String {
    // Rendered HTML runs a little over the source length, and pre-sizing avoids a handful of reallocations.
    let mut out = String::with_capacity(md.len() * 3 / 2 + 256);
    html::push_html(&mut out, Parser::new_ext(md, options()));
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

/// Placeholder the caller substitutes once the render it is timing has actually finished.
pub const RENDER_MICROS: &str = "__RENDER_MICROS__";

/// Builds the complete standalone HTML page for one answer.
pub fn page(answer: &Answer, cfg: &Config, meta: &PageMeta) -> String {
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
        format!("<meta http-equiv=\"refresh\" content=\"{}\">", cfg.auto_refresh_secs)
    } else {
        String::new()
    };

    let highlight = if cfg.code_highlight { highlight_snippet(cfg.theme) } else { String::new() };

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
    footer.push_str(&format!("<span>rendered in {RENDER_MICROS} \u{b5}s</span>"));

    format!(
        r#"<!doctype html>
<html lang="en"{theme_attr}>
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
{refresh}
<title>{title}</title>
<style>
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
<script>
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

  var version = "{version_token}";
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
        version_token = VERSION_TOKEN,
        theme_attr = theme_attr,
        color_scheme = color_scheme,
        refresh = refresh,
        body = body,
        footer = footer,
        highlight = highlight,
    )
}

/// highlight.js pulled from a CDN, placed at the end of the body so it never delays the first paint of the answer.
///
/// Under `theme: auto` both stylesheets are linked and the media query decides which one applies, which costs one extra cached request and keeps the toggle instant.
fn highlight_snippet(theme: Theme) -> String {
    const BASE: &str = "https://cdnjs.cloudflare.com/ajax/libs/highlight.js/11.11.1";
    let styles = match theme {
        Theme::Light => format!("<link rel=\"stylesheet\" href=\"{BASE}/styles/github.min.css\">"),
        Theme::Dark => format!("<link rel=\"stylesheet\" href=\"{BASE}/styles/github-dark.min.css\">"),
        Theme::Auto => format!(
            "<link rel=\"stylesheet\" href=\"{BASE}/styles/github.min.css\" media=\"(prefers-color-scheme: light)\">\n<link rel=\"stylesheet\" href=\"{BASE}/styles/github-dark.min.css\" media=\"(prefers-color-scheme: dark)\">"
        ),
    };
    format!("{styles}\n<script src=\"{BASE}/highlight.min.js\" onload=\"hljs.highlightAll()\"></script>")
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
