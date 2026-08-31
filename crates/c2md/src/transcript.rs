//! Pulls the answer out of a Claude Code session transcript, which is a JSONL file appended to as the turn runs.
//!
//! The file grows without bound over a long session, so we never read it whole. We map a window off the end and walk backwards line by line until we hit the human message that started the turn, growing the window only if that boundary is further back than expected.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use crate::config::Scope;

/// How much of the tail we read on the first attempt, which covers all but the most enormous single turns.
const INITIAL_WINDOW: u64 = 64 * 1024;

/// Ceiling on window growth, so a pathological transcript cannot make the hook read hundreds of megabytes.
const MAX_WINDOW: u64 = 32 * 1024 * 1024;

/// One contiguous run of assistant output, tagged by whether it was visible prose or reasoning.
pub struct Segment {
    pub thinking: bool,
    pub body: String,
}

/// Everything worth putting on the page, in the order it was produced.
pub struct Answer {
    pub segments: Vec<Segment>,
    /// The human message that started this turn, used as the page subtitle.
    pub prompt: Option<String>,
    /// Model id reported by the last assistant message, shown in the page footer.
    pub model: Option<String>,
    /// Bytes actually read off the end of the transcript, reported by `c2md bench`.
    pub bytes_read: usize,
}

impl Answer {
    /// Total length of the visible prose, which is what `min_chars` is compared against.
    pub fn visible_len(&self) -> usize {
        self.segments.iter().filter(|s| !s.thinking).map(|s| s.body.len()).sum()
    }
}

/// Reads the transcript tail and returns the assistant output for the current turn, or `None` if there is none.
pub fn extract(path: &Path, scope: Scope, include_thinking: bool) -> Option<Answer> {
    let mut file = File::open(path).ok()?;
    let len = file.metadata().ok()?.len();
    let mut window = INITIAL_WINDOW;

    loop {
        let start = len.saturating_sub(window);
        let text = read_window(&mut file, start, len)?;
        let (answer, hit_boundary) = scan(&text, scope, include_thinking);

        // A found boundary means the whole turn was inside the window, so the answer is complete.
        if hit_boundary || start == 0 || window >= MAX_WINDOW {
            let mut answer = answer?;
            answer.bytes_read = text.len();
            return Some(answer);
        }
        window *= 4;
    }
}

/// Reads `[start, len)` and drops any partial first line, so the caller always gets whole JSONL records.
fn read_window(file: &mut File, start: u64, len: u64) -> Option<String> {
    file.seek(SeekFrom::Start(start)).ok()?;
    let mut buf = Vec::with_capacity((len - start) as usize);
    file.take(len - start).read_to_end(&mut buf).ok()?;

    // Slicing at a newline is always a safe UTF-8 boundary, so this cannot split a multi-byte character.
    let body = if start > 0 {
        match buf.iter().position(|&b| b == b'\n') {
            Some(nl) => &buf[nl + 1..],
            None => &buf[..],
        }
    } else {
        &buf[..]
    };
    Some(String::from_utf8_lossy(body).into_owned())
}

/// Walks the window backwards, collecting assistant output until the human message that opened the turn.
///
/// Returns the answer together with whether the human boundary was actually reached, which tells the caller if a larger window is needed.
fn scan(text: &str, scope: Scope, include_thinking: bool) -> (Option<Answer>, bool) {
    let mut segments: Vec<Segment> = Vec::new();
    let mut prompt = None;
    let mut model = None;
    let mut hit_boundary = false;

    for line in text.lines().rev() {
        // Cheap pre-filter: most lines in a busy transcript are tool results and file snapshots we never want.
        let is_assistant = line.contains("\"type\":\"assistant\"");
        let is_user = !is_assistant && line.contains("\"type\":\"user\"");
        if !is_assistant && !is_user {
            continue;
        }

        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else { continue };

        // Subagent transcripts are interleaved into the same file; their output is not this turn's answer.
        if v["isSidechain"].as_bool() == Some(true) {
            continue;
        }

        if is_user {
            if !is_human_turn(&v) {
                continue;
            }
            prompt = user_prompt_text(&v);
            hit_boundary = true;
            break;
        }

        if model.is_none() {
            model = v["message"]["model"].as_str().map(str::to_string);
        }

        let before = segments.len();
        collect_blocks(&v["message"]["content"], include_thinking, &mut segments);

        // In `last` mode a single assistant message is the whole answer, so stop as soon as one yielded prose.
        if scope == Scope::Last && segments[before..].iter().any(|s| !s.thinking) {
            break;
        }
    }

    segments.reverse();
    let answer = if segments.iter().any(|s| !s.body.trim().is_empty()) {
        Some(Answer { segments, prompt, model, bytes_read: 0 })
    } else {
        None
    };
    (answer, hit_boundary)
}

/// Appends the text (and optionally thinking) blocks of one assistant message, newest first.
fn collect_blocks(content: &serde_json::Value, include_thinking: bool, out: &mut Vec<Segment>) {
    let Some(blocks) = content.as_array() else {
        if let Some(s) = content.as_str() {
            out.push(Segment { thinking: false, body: s.to_string() });
        }
        return;
    };
    for block in blocks.iter().rev() {
        match block["type"].as_str() {
            Some("text") => {
                if let Some(s) = block["text"].as_str() {
                    out.push(Segment { thinking: false, body: s.to_string() });
                }
            }
            Some("thinking") if include_thinking => {
                if let Some(s) = block["thinking"].as_str() {
                    out.push(Segment { thinking: true, body: s.to_string() });
                }
            }
            _ => {}
        }
    }
}

/// Distinguishes a real human turn from the tool-result records that Claude Code also stores with `type: "user"`.
fn is_human_turn(v: &serde_json::Value) -> bool {
    if v["origin"]["kind"].as_str() == Some("human") {
        return true;
    }
    let content = &v["message"]["content"];
    if content.is_string() {
        return true;
    }
    match content.as_array() {
        Some(blocks) => !blocks.iter().any(|b| b["type"].as_str() == Some("tool_result")),
        None => false,
    }
}

/// Extracts the prompt text of a human turn, flattening the block form the API also accepts.
fn user_prompt_text(v: &serde_json::Value) -> Option<String> {
    let content = &v["message"]["content"];
    if let Some(s) = content.as_str() {
        return Some(s.to_string());
    }
    let blocks = content.as_array()?;
    let mut out = String::new();
    for b in blocks {
        if b["type"].as_str() == Some("text") {
            if let Some(s) = b["text"].as_str() {
                if !out.is_empty() {
                    out.push('\n');
                }
                out.push_str(s);
            }
        }
    }
    if out.is_empty() {
        None
    } else {
        Some(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds a transcript shaped like a real one: a human turn, a tool round trip, then the answer.
    fn fixture() -> String {
        [
            r#"{"type":"user","isSidechain":false,"origin":{"kind":"human"},"message":{"role":"user","content":"older question"}}"#,
            r#"{"type":"assistant","isSidechain":false,"message":{"model":"claude-opus-5","content":[{"type":"text","text":"stale answer"}]}}"#,
            r#"{"type":"user","isSidechain":false,"origin":{"kind":"human"},"message":{"role":"user","content":"the real question"}}"#,
            r#"{"type":"assistant","isSidechain":false,"message":{"model":"claude-opus-5","content":[{"type":"thinking","thinking":"private reasoning"},{"type":"text","text":"first half"}]}}"#,
            r#"{"type":"assistant","isSidechain":false,"message":{"model":"claude-opus-5","content":[{"type":"tool_use","name":"Bash","input":{}}]}}"#,
            r#"{"type":"user","isSidechain":false,"message":{"role":"user","content":[{"type":"tool_result","content":"output"}]}}"#,
            r#"{"type":"assistant","isSidechain":true,"message":{"content":[{"type":"text","text":"subagent chatter"}]}}"#,
            r#"{"type":"assistant","isSidechain":false,"message":{"model":"claude-opus-5","content":[{"type":"text","text":"second half"}]}}"#,
        ]
        .join("\n")
    }

    #[test]
    fn turn_scope_spans_the_whole_turn_and_stops_at_the_human_message() {
        let (answer, boundary) = scan(&fixture(), Scope::Turn, false);
        let answer = answer.expect("an answer");
        assert!(boundary);
        let bodies: Vec<&str> = answer.segments.iter().map(|s| s.body.as_str()).collect();
        assert_eq!(bodies, vec!["first half", "second half"]);
        assert_eq!(answer.prompt.as_deref(), Some("the real question"));
        assert_eq!(answer.model.as_deref(), Some("claude-opus-5"));
    }

    #[test]
    fn last_scope_keeps_only_the_final_message() {
        let (answer, _) = scan(&fixture(), Scope::Last, false);
        let answer = answer.expect("an answer");
        assert_eq!(answer.segments.len(), 1);
        assert_eq!(answer.segments[0].body, "second half");
    }

    #[test]
    fn thinking_is_opt_in_and_ordered_before_the_prose_it_preceded() {
        let (answer, _) = scan(&fixture(), Scope::Turn, true);
        let answer = answer.expect("an answer");
        let kinds: Vec<(bool, &str)> = answer.segments.iter().map(|s| (s.thinking, s.body.as_str())).collect();
        assert_eq!(kinds, vec![(true, "private reasoning"), (false, "first half"), (false, "second half")]);
    }

    #[test]
    fn tool_results_are_not_mistaken_for_the_human_turn() {
        let line = r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","content":"x"}]}}"#;
        let v: serde_json::Value = serde_json::from_str(line).unwrap();
        assert!(!is_human_turn(&v));
    }

    #[test]
    fn a_turn_with_no_prose_yields_nothing_to_open() {
        let only_tools = r#"{"type":"user","isSidechain":false,"origin":{"kind":"human"},"message":{"role":"user","content":"go"}}
{"type":"assistant","isSidechain":false,"message":{"content":[{"type":"tool_use","name":"Bash","input":{}}]}}"#;
        let (answer, boundary) = scan(only_tools, Scope::Turn, false);
        assert!(boundary);
        assert!(answer.is_none());
    }

    #[test]
    fn a_missing_boundary_reports_so_the_window_can_grow() {
        let partial = r#"{"type":"assistant","isSidechain":false,"message":{"content":[{"type":"text","text":"tail only"}]}}"#;
        let (answer, boundary) = scan(partial, Scope::Turn, false);
        assert!(!boundary);
        assert!(answer.is_some());
    }
}
