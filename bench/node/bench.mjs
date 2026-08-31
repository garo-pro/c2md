// Reference numbers for the JavaScript markdown engines, so the choice of a compiled hook is backed by data.
// Mirrors crates/mdbench: same corpus shape, warmup pass, median of N timed runs.

import { marked } from "marked";
import MarkdownIt from "markdown-it";

const md = new MarkdownIt({ html: false, linkify: true });
marked.setOptions({ gfm: true });

const BLOCK = `
## Fixing the token refresh race

The refresh path had two callers racing on the same \`RefreshToken\`, so the second one always got a
\`401\` back and *silently* dropped the session. The fix is a single-flight guard around the refresh.

### What changed

1. \`auth/session.rs\` now holds a \`Mutex<Option<JoinHandle<..>>>\` for the in-flight refresh.
2. Callers \`await\` the existing handle instead of starting a second request.
3. Added a **jittered** backoff so a thundering herd after a deploy does not re-create the race.

\`\`\`rust
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
\`\`\`

| Case | Before | After |
| --- | ---: | ---: |
| Single caller | 120 ms | 118 ms |
| Ten concurrent | 9 failures | 0 failures |
| Cold start | 340 ms | 210 ms |

- [ ] Backfill the metric for \`auth.refresh.single_flight\`
- [x] Land the guard
- [x] Add the regression test in \`tests/refresh_race.rs\`

See the [RFC](https://example.invalid/rfc/0042) for why we did not just widen the lock. ~~Widening~~
would have serialized every unrelated read, and the p99 on \`GET /me\` was already at 80 ms.

> Note: this does not address the separate clock-skew problem on the \`exp\` claim. That is tracked
> separately and needs a server-side fix.
`;

const doc = (reps) => "# Session answer\n" + BLOCK.repeat(reps);

const iters = Number(process.argv[2] ?? 400);

function measure(fn, input) {
  fn(input);
  const s = [];
  for (let i = 0; i < iters; i++) {
    const t = process.hrtime.bigint();
    const out = fn(input);
    s.push(Number(process.hrtime.bigint() - t) / 1000);
    if (out.length === -1) throw new Error("unreachable");
  }
  s.sort((a, b) => a - b);
  return s[s.length >> 1];
}

console.log(`node ${process.version}: ${iters} timed iterations per engine, median reported\n`);

for (const [name, reps] of [["answer-small", 1], ["answer-typical", 8], ["answer-huge", 80]]) {
  const text = doc(reps);
  console.log(`== ${name} (${(text.length / 1024) | 0} KiB) ==`);
  const rows = [
    ["marked", measure((t) => marked.parse(t), text)],
    ["markdown-it", measure((t) => md.render(t), text)],
  ].sort((a, b) => a[1] - b[1]);
  for (const [engine, us] of rows) {
    const mibs = text.length / 1024 / 1024 / (us / 1e6);
    console.log(`${engine.padEnd(16)} ${us.toFixed(3).padStart(9)}us ${mibs.toFixed(1).padStart(12)}`);
  }
  console.log();
}
