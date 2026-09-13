//! Pulls the answer out of a Claude Code session transcript, which is a JSONL file appended to as the turn runs.
//!
//! The file grows without bound over a long session, so we never read it whole. We read a window off the end and walk backwards line by line until the answer is complete, widening the window only if it was not. Widening only ever reads the bytes it adds, so a turn that takes several passes still parses every line exactly once.
//!
//! The first turn of a session usually does take several passes. Claude Code writes its prompt snapshots, skill and tool listings and environment blocks into the transcript as attachment records between the human message and the answer, which on a current version is a couple of hundred KiB standing where the 64 KiB window used to reach.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;
use std::time::{Duration, Instant};

use crate::config::Scope;

/// How much of the tail we read on the first attempt, which covers all but the most enormous single turns.
const INITIAL_WINDOW: u64 = 64 * 1024;

/// Ceiling on window growth, so a pathological transcript cannot make the hook read hundreds of megabytes.
const MAX_WINDOW: u64 = 32 * 1024 * 1024;

/// How often the tail is re-read while waiting for the turn's last message to land.
const POLL_INTERVAL: Duration = Duration::from_millis(20);

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
    /// Total length of the visible prose in characters, which is what `min_chars` is compared against.
    ///
    /// Counted in characters rather than bytes because the setting is named for characters, and a short answer in a non-Latin script would otherwise clear a threshold the same answer in English would not.
    pub fn visible_len(&self) -> usize {
        self.segments.iter().filter(|s| !s.thinking).map(|s| s.body.chars().count()).sum()
    }
}

/// Reads the transcript tail and returns the assistant output for the current turn, or `None` if there is none.
///
/// A Stop hook can outrun the transcript. Claude Code stamps a message with the time it was produced but appends it to the JSONL a moment later, and the hook is launched off the same end-of-turn event as that write, so a scan starting immediately can land on a file whose newest record is still the second-to-last one. That is why the answer would go missing while the tool calls that preceded it rendered perfectly: everything except the final message was already on disk.
///
/// So the read is retried for up to `settle` until the tail proves the turn is over. Nothing waits in the ordinary case, where the answer is already there on the first read.
pub fn extract(path: &Path, scope: Scope, include_thinking: bool, settle: Duration) -> Option<Answer> {
    let deadline = Instant::now() + settle;
    let mut bytes_read = 0;
    loop {
        let (answer, ended) = read_turn(path, scope, include_thinking);
        // Every attempt costs a read, and `c2md bench` is the one caller that wants to know about all of them.
        bytes_read += answer.as_ref().map(|a| a.bytes_read).unwrap_or(0);

        if ended || Instant::now() >= deadline {
            return answer.map(|a| Answer { bytes_read, ..a });
        }
        std::thread::sleep(POLL_INTERVAL);
    }
}

/// One pass over the tail, returning the answer so far and whether the turn's last message has landed.
fn read_turn(path: &Path, scope: Scope, include_thinking: bool) -> (Option<Answer>, bool) {
    match scan_tail(path, scope, include_thinking) {
        Some(acc) => {
            let ended = acc.turn_ended();
            (acc.finish(), ended)
        }
        // A transcript that cannot be read will not become readable by being read again.
        None => (None, true),
    }
}

/// Walks the tail backwards, widening the window until the turn boundary is in hand.
fn scan_tail(path: &Path, scope: Scope, include_thinking: bool) -> Option<Scan> {
    let mut file = File::open(path).ok()?;
    let len = file.metadata().ok()?.len();

    let mut acc = Scan::default();
    let mut window = INITIAL_WINDOW;
    // Start of the oldest whole line already scanned, so a wider window only has to read what it adds.
    let mut scanned_from = len;

    loop {
        let start = len.saturating_sub(window);
        let (text, text_start) = read_lines(&mut file, start, scanned_from)?;
        acc.bytes_read += text.len();
        let complete = acc.absorb(&text, scope, include_thinking);
        scanned_from = text_start;

        if complete || start == 0 || window >= MAX_WINDOW {
            return Some(acc);
        }
        window = (window * 4).min(MAX_WINDOW);
    }
}

/// Reads `[start, end)` and drops any partial first line, returning the text and the offset it begins at.
///
/// `end` is always either the end of the file or the start of a line a previous pass already read, so the last line is never cut in half.
fn read_lines(file: &mut File, start: u64, end: u64) -> Option<(String, u64)> {
    file.seek(SeekFrom::Start(start)).ok()?;
    let mut buf = Vec::with_capacity((end - start) as usize);
    file.take(end - start).read_to_end(&mut buf).ok()?;

    // Slicing at a newline is always a safe UTF-8 boundary, so this cannot split a multi-byte character.
    // A window landing inside one enormous line has no newline to cut at; that line fails to parse and is skipped.
    let skipped = match (start > 0).then(|| buf.iter().position(|&b| b == b'\n')).flatten() {
        Some(nl) => nl + 1,
        None => 0,
    };
    Some((String::from_utf8_lossy(&buf[skipped..]).into_owned(), start + skipped as u64))
}

/// What the newest assistant record in the file says about whether the turn is over.
struct Tail {
    /// `stop_reason` of that record: `tool_use` on every message that hands off to a tool, something else once the model has stopped talking.
    stop_reason: Option<String>,
    /// Whether it carried prose, which is what proves the record holding the answer itself has been written.
    has_text: bool,
}

/// The answer being assembled as the window walks backwards, holding segments newest first.
#[derive(Default)]
struct Scan {
    segments: Vec<Segment>,
    prompt: Option<String>,
    model: Option<String>,
    bytes_read: usize,
    /// Set from the first assistant record the backwards walk meets, which is the newest one in the file.
    tail: Option<Tail>,
}

impl Scan {
    /// Walks one chunk backwards, newest line first, and reports whether the answer is now complete.
    ///
    /// Chunks arrive newest first and each is walked backwards, so appending to a newest-first list stays ordered across however many passes it takes.
    fn absorb(&mut self, text: &str, scope: Scope, include_thinking: bool) -> bool {
        for line in text.lines().rev() {
            // Cheap pre-filter: most lines in a busy transcript are attachments, tool results and file snapshots we never want.
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
                self.prompt = user_prompt_text(&v);
                // The message that opened the turn: everything the answer could contain is already in hand.
                return true;
            }

            if self.model.is_none() {
                self.model = v["message"]["model"].as_str().map(str::to_string);
            }

            let before = self.segments.len();
            collect_blocks(&v["message"]["content"], include_thinking, &mut self.segments);
            let produced_text = self.segments[before..].iter().any(|s| !s.thinking);

            if self.tail.is_none() {
                self.tail = Some(Tail {
                    stop_reason: v["message"]["stop_reason"].as_str().map(str::to_string),
                    has_text: produced_text,
                });
            }

            // In `last` mode a single assistant message is the whole answer, so one that yielded prose
            // finishes the job. Reporting that as complete is what keeps the window from growing to the
            // whole file looking for a boundary this mode never reads.
            if scope == Scope::Last && produced_text {
                return true;
            }
        }
        false
    }

    /// Whether the newest assistant record in the file is the one that finished the turn.
    ///
    /// Two things have to hold, because Claude Code splits a single message across records. `stop_reason` rules out a message that is only handing off to a tool, and the text block rules out having caught the message's thinking record while the prose record behind it is still unwritten.
    ///
    /// An empty tail means no assistant record was found at all, which is the reading a Stop hook gets when it beats the whole message to disk.
    fn turn_ended(&self) -> bool {
        self.tail.as_ref().is_some_and(|t| t.has_text && t.stop_reason.as_deref() != Some("tool_use"))
    }

    /// Puts the segments back in the order they were produced, or reports that the turn held no prose.
    fn finish(mut self) -> Option<Answer> {
        self.segments.reverse();
        if !self.segments.iter().any(|s| !s.body.trim().is_empty()) {
            return None;
        }
        Some(Answer { segments: self.segments, prompt: self.prompt, model: self.model, bytes_read: self.bytes_read })
    }
}

/// Scans one self-contained window, for tests that do not want to go through a file.
#[cfg(test)]
fn scan(text: &str, scope: Scope, include_thinking: bool) -> (Option<Answer>, bool) {
    let mut acc = Scan::default();
    let complete = acc.absorb(text, scope, include_thinking);
    (acc.finish(), complete)
}

/// Whether a self-contained window reads as a finished turn, which is what the settle wait is watching for.
#[cfg(test)]
fn scan_turn_ended(text: &str) -> bool {
    let mut acc = Scan::default();
    acc.absorb(text, Scope::Turn, false);
    acc.turn_ended()
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

/// Local slash commands (`/copy`, `/exit`, ...) are echoed into the transcript as their own
/// `type: "user"` records — a caveat, the command name, its stdout — each carrying plain string
/// content just like a real prompt does. Only the first of the three is marked `isMeta`, so the
/// other two are indistinguishable from a real human message by shape alone; these prefixes are
/// the stable wrapper Claude Code puts around all three. Without filtering them out, a scan
/// landing on one stops short of the real turn boundary and reports the turn as empty.
const SYNTHETIC_COMMAND_ECHO_PREFIXES: [&str; 3] =
    ["<local-command-caveat>", "<command-name>", "<local-command-stdout>"];

/// Distinguishes a real human turn from the tool-result and slash-command-echo records that Claude
/// Code also stores with `type: "user"`.
fn is_human_turn(v: &serde_json::Value) -> bool {
    if v["isMeta"].as_bool() == Some(true) {
        return false;
    }
    if v["origin"]["kind"].as_str() == Some("human") {
        return true;
    }
    let content = &v["message"]["content"];
    if let Some(s) = content.as_str() {
        return !SYNTHETIC_COMMAND_ECHO_PREFIXES.iter().any(|p| s.starts_with(p));
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

    /// A local slash command (`/copy`, `/exit`, ...) echoes a caveat, the command name and its stdout
    /// into the transcript as separate `type: "user"` records with plain string content — the same
    /// shape a real human message has. Only the caveat carries `isMeta: true`; the other two do not,
    /// so both signals are needed. Trusting shape alone stops the scan short of the real boundary.
    #[test]
    fn slash_command_echoes_are_not_mistaken_for_the_human_turn() {
        let caveat = r#"{"type":"user","isMeta":true,"message":{"role":"user","content":"<local-command-caveat>Caveat...</local-command-caveat>"}}"#;
        let command_name = r#"{"type":"user","message":{"role":"user","content":"<command-name>/copy</command-name>\n<command-message>copy</command-message>\n<command-args></command-args>"}}"#;
        let stdout = r#"{"type":"user","message":{"role":"user","content":"<local-command-stdout>Catch you later!</local-command-stdout>"}}"#;
        for line in [caveat, command_name, stdout] {
            let v: serde_json::Value = serde_json::from_str(line).unwrap();
            assert!(!is_human_turn(&v), "should not treat `{line}` as the human turn boundary");
        }
    }

    /// The end-to-end failure this caused: trailing slash-command echoes after the answer made a
    /// scan starting from the true end of the file stop immediately, before collecting anything.
    #[test]
    fn a_trailing_slash_command_does_not_swallow_the_answer_before_it() {
        let transcript = [
            r#"{"type":"user","isSidechain":false,"origin":{"kind":"human"},"message":{"role":"user","content":"7COPY"}}"#,
            r#"{"type":"assistant","isSidechain":false,"message":{"model":"claude-opus-5","content":[{"type":"text","text":"the answer"}]}}"#,
            r#"{"type":"user","isMeta":true,"message":{"role":"user","content":"<local-command-caveat>...</local-command-caveat>"}}"#,
            r#"{"type":"user","message":{"role":"user","content":"<command-name>/copy</command-name>"}}"#,
            r#"{"type":"user","message":{"role":"user","content":"<local-command-stdout>Copied.</local-command-stdout>"}}"#,
        ]
        .join("\n");
        let (answer, boundary) = scan(&transcript, Scope::Turn, false);
        assert!(boundary);
        let answer = answer.expect("an answer, not swallowed by the trailing /copy echo");
        assert_eq!(answer.segments[0].body, "the answer");
        assert_eq!(answer.prompt.as_deref(), Some("7COPY"));
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
        let (answer, complete) = scan(partial, Scope::Turn, false);
        assert!(!complete);
        assert!(answer.is_some());
    }

    /// `last` mode never reads as far back as the human message, so completeness cannot be a report of
    /// having seen one. Saying otherwise sent the window off to swallow the whole transcript looking for
    /// a boundary this mode stops short of on purpose.
    #[test]
    fn last_scope_is_complete_without_reaching_the_human_message() {
        let tail = r#"{"type":"assistant","isSidechain":false,"message":{"content":[{"type":"text","text":"the answer"}]}}"#;
        let (answer, complete) = scan(tail, Scope::Last, false);
        assert!(complete);
        assert_eq!(answer.expect("an answer").segments[0].body, "the answer");
    }

    /// Writes `body` to a scratch file and returns the path, keeping the name unique per test.
    fn scratch(name: &str, body: &str) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!("c2md-transcript-{}-{name}.jsonl", std::process::id()));
        std::fs::write(&path, body).expect("write the fixture");
        path
    }

    /// One turn padded out past the first window, the shape every session's opening turn now has.
    fn padded(pad_lines: usize) -> String {
        let filler = format!(
            r#"{{"type":"attachment","isSidechain":false,"attachment":{{"type":"prompt_snapshot","systemPrompt":[{{"type":"text","text":"{}"}}]}}}}"#,
            "x".repeat(4000)
        );
        let mut out = String::new();
        for _ in 0..pad_lines {
            out.push_str(&filler);
            out.push('\n');
        }
        out.push_str(r#"{"type":"user","isSidechain":false,"origin":{"kind":"human"},"message":{"role":"user","content":"the question"}}"#);
        out.push('\n');
        for half in ["first half", "second half"] {
            out.push_str(&format!(
                r#"{{"type":"assistant","isSidechain":false,"message":{{"model":"claude-opus-5","content":[{{"type":"text","text":"{half}"}}]}}}}"#
            ));
            out.push('\n');
        }
        out
    }

    /// A boundary past the first window is reached by widening, and the answer comes back whole and in order.
    #[test]
    fn a_turn_wider_than_the_first_window_is_still_assembled_in_order() {
        let path = scratch("wide", &padded(40));
        let answer = extract(&path, Scope::Turn, false, Duration::ZERO).expect("an answer");
        let bodies: Vec<&str> = answer.segments.iter().map(|s| s.body.as_str()).collect();
        assert_eq!(bodies, vec!["first half", "second half"]);
        assert_eq!(answer.prompt.as_deref(), Some("the question"));
        assert_eq!(answer.model.as_deref(), Some("claude-opus-5"));
        let _ = std::fs::remove_file(&path);
    }

    /// Widening reads only what it adds, so the passes together cost about one read of the turn rather
    /// than one of every window tried.
    #[test]
    fn widening_reads_each_byte_once() {
        let body = padded(40);
        let path = scratch("once", &body);
        let answer = extract(&path, Scope::Turn, false, Duration::ZERO).expect("an answer");
        assert!(
            answer.bytes_read <= body.len(),
            "read {} bytes of a {} byte transcript",
            answer.bytes_read,
            body.len()
        );
        let _ = std::fs::remove_file(&path);
    }

    /// One assistant record in the shape Claude Code writes it, where `stop_reason` is what separates a
    /// message handing off to a tool from the one that ended the turn.
    fn assistant(text: &str, stop_reason: &str) -> String {
        format!(
            r#"{{"type":"assistant","isSidechain":false,"message":{{"model":"claude-opus-5","stop_reason":"{stop_reason}","content":[{{"type":"text","text":"{text}"}}]}}}}"#
        )
    }

    /// A transcript caught mid-turn: the model has spoken once and gone off to a tool, and the answer is still to come.
    fn mid_turn() -> String {
        [
            r#"{"type":"user","isSidechain":false,"origin":{"kind":"human"},"message":{"role":"user","content":"the question"}}"#.to_string(),
            assistant("looking into it", "tool_use"),
            r#"{"type":"user","isSidechain":false,"message":{"role":"user","content":[{"type":"tool_result","content":"output"}]}}"#.to_string(),
        ]
        .join("\n")
            + "\n"
    }

    /// `stop_reason` is the signal that the model has stopped talking, and a tool hand-off is not that.
    #[test]
    fn a_tail_that_is_still_calling_tools_is_not_a_finished_turn() {
        assert!(!scan_turn_ended(&mid_turn()), "a message that handed off to a tool has more to come");
        assert!(
            scan_turn_ended(&(mid_turn() + &assistant("the answer", "end_turn"))),
            "an end_turn message carrying prose is the end of the turn"
        );
    }

    /// Claude Code writes one message's thinking and its prose as two records, so an `end_turn` record
    /// with no text is the first half of an answer whose second half has not been written yet.
    #[test]
    fn an_end_turn_record_without_prose_is_only_half_an_answer() {
        let thinking = r#"{"type":"assistant","isSidechain":false,"message":{"model":"claude-opus-5","stop_reason":"end_turn","content":[{"type":"thinking","thinking":"nearly there"}]}}"#;
        assert!(!scan_turn_ended(&(mid_turn() + thinking + "\n")));
        assert!(scan_turn_ended(&(mid_turn() + thinking + "\n" + &assistant("the answer", "end_turn"))));
    }

    /// The bug this whole mechanism exists for. A Stop hook is launched off the same event as the write of
    /// the turn's last message and regularly beats it to disk, which is why the answer went missing while
    /// the messages before it rendered perfectly. Waiting for the tail to say the turn is over fixes it.
    #[test]
    fn an_answer_still_being_written_is_waited_for_rather_than_missed() {
        let path = scratch("late", &mid_turn());

        let appending = path.clone();
        let writer = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(120));
            use std::io::Write;
            let mut f = std::fs::OpenOptions::new().append(true).open(&appending).expect("append the answer");
            f.write_all(assistant("the real answer", "end_turn").as_bytes()).expect("append the answer");
        });

        let answer = extract(&path, Scope::Turn, false, Duration::from_secs(5)).expect("an answer");
        writer.join().expect("the writer thread");

        let bodies: Vec<&str> = answer.segments.iter().map(|s| s.body.as_str()).collect();
        assert_eq!(bodies, vec!["looking into it", "the real answer"]);
        let _ = std::fs::remove_file(&path);
    }

    /// A turn can end without ever producing prose — an interrupted one, say — and the wait has to be a
    /// bound rather than a promise, since a Stop hook that never returns is worse than a missing page.
    #[test]
    fn a_turn_that_never_finishes_gives_up_when_the_budget_runs_out() {
        let path = scratch("budget", &mid_turn());
        let started = Instant::now();
        let answer = extract(&path, Scope::Turn, false, Duration::from_millis(150));
        let waited = started.elapsed();

        assert_eq!(answer.expect("what prose there is").segments.len(), 1);
        assert!(waited >= Duration::from_millis(150), "it gave up after {waited:?}, before the budget");
        assert!(waited < Duration::from_secs(2), "it waited {waited:?}, far past the budget");
        let _ = std::fs::remove_file(&path);
    }

    /// The whole point of the `last` fix: the answer sits in the first window, so nothing behind it is read.
    #[test]
    fn last_scope_does_not_read_past_the_answer() {
        let body = padded(400);
        let path = scratch("last", &body);
        let answer = extract(&path, Scope::Last, false, Duration::ZERO).expect("an answer");
        assert_eq!(answer.segments.len(), 1);
        assert_eq!(answer.segments[0].body, "second half");
        assert!(
            answer.bytes_read <= INITIAL_WINDOW as usize,
            "read {} bytes of a {} byte transcript to find an answer in the last window",
            answer.bytes_read,
            body.len()
        );
        let _ = std::fs::remove_file(&path);
    }
}
