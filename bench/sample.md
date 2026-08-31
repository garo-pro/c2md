# Sample answer

A fixture covering everything the page template has to style, so `c2md render bench/sample.md --open` is enough to eyeball a change to the CSS.

## Prose and inline marks

Regular paragraph text with **bold**, *italic*, `inline code`, ~~strikethrough~~ and a [link](https://example.invalid).

> A blockquote, for the notes and caveats Claude tends to end on.
> It runs to a second line.

## Lists

1. First ordered item
2. Second ordered item
   - Nested unordered child
   - Another child with `code`
3. Third

- [x] Completed task
- [ ] Outstanding task

## Code

```rust
pub fn extract(path: &Path, scope: Scope) -> Option<Answer> {
    let mut file = File::open(path).ok()?;
    let len = file.metadata().ok()?.len();
    // Walk backwards from the end until the human message that opened the turn.
    scan(&read_window(&mut file, len.saturating_sub(WINDOW), len)?, scope)
}
```

```bash
c2md install --user
c2md config
```

    An indented code block, which markdown also allows.

## Table

| Engine | Median | Throughput | Relative |
| --- | ---: | ---: | ---: |
| pulldown-cmark | 26.2 us | 437 MiB/s | 1.00x |
| comrak | 133.8 us | 86 MiB/s | 5.11x |
| markdown-rs | 1395.8 us | 8 MiB/s | 53.27x |

---

### Long line handling

A deliberately long line that has to wrap cleanly inside the measure without pushing the page into a horizontal scroll, because answers routinely contain sentences of this length and the layout should not break on them.

`a_very_long_inline_code_span_that_should_not_overflow_the_container_either_even_when_it_has_no_spaces`
