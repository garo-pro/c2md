// A deliberately equivalent Node implementation of the hook, used only to measure what the runtime choice costs.
//
// It does the same four things c2md does: read the payload, tail-read the transcript, render the answer, write the page. The gap between the two is almost entirely interpreter startup, which a Stop hook pays on every single turn.

import { readFileSync, writeFileSync, openSync, readSync, fstatSync, closeSync } from "node:fs";
import { marked } from "marked";

const payload = JSON.parse(readFileSync(0, "utf8").replace(/^﻿/, ""));
const WINDOW = 64 * 1024;

const fd = openSync(payload.transcript_path, "r");
const size = fstatSync(fd).size;
const start = Math.max(0, size - WINDOW);
const buf = Buffer.allocUnsafe(size - start);
readSync(fd, buf, 0, buf.length, start);
closeSync(fd);

let text = buf.toString("utf8");
if (start > 0) text = text.slice(text.indexOf("\n") + 1);

const lines = text.split("\n");
const segments = [];
for (let i = lines.length - 1; i >= 0; i--) {
  const line = lines[i];
  const isAssistant = line.includes('"type":"assistant"');
  const isUser = !isAssistant && line.includes('"type":"user"');
  if (!isAssistant && !isUser) continue;

  let o;
  try { o = JSON.parse(line); } catch { continue; }
  if (o.isSidechain === true) continue;

  if (isUser) {
    const c = o.message?.content;
    const human = o.origin?.kind === "human" || typeof c === "string" ||
      (Array.isArray(c) && !c.some((b) => b.type === "tool_result"));
    if (human) break;
    continue;
  }
  const blocks = o.message?.content;
  if (!Array.isArray(blocks)) continue;
  for (let j = blocks.length - 1; j >= 0; j--) {
    if (blocks[j].type === "text") segments.push(blocks[j].text);
  }
}
segments.reverse();

const html = `<!doctype html><html><head><meta charset="utf-8"><title>Claude Code</title></head><body>${
  segments.map((s) => marked.parse(s)).join("\n")
}</body></html>`;

writeFileSync(process.env.C2MD_NODE_OUT ?? "node-hook.html", html);
