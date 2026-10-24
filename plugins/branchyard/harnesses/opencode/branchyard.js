// Branchyard for OpenCode (@opencode-ai/plugin 1.19.x): tells a session a person
// started which of the repository's Branchyard branches finished, failed, are blocked
// or ask, as a synthetic text part on the user's message. The first message of a
// session gets `by hook --harness generic session-start`; later ones get
// `user-prompt-submit`, which says something only when there is news. Without `by`
// on PATH, or when `by` is slow (3 s) or fails, it adds nothing and never throws.
// OpenCode has no plugin status line (TUI slots only), so there is none here.
// See docs/harness-hooks.md.

import { randomBytes } from "node:crypto";
import { spawn } from "node:child_process";

const TIMEOUT_MS = 3000;

/** Run `by <args>` with `input` as JSON on stdin; resolve its stdout, or "" on any failure. */
function runBy(args, input) {
  return new Promise((resolve) => {
    let done = false;
    let out = "";
    let timer;
    let child;
    const finish = (text) => {
      if (done) return;
      done = true;
      clearTimeout(timer);
      resolve(text);
    };
    try {
      child = spawn("by", args, { stdio: ["pipe", "pipe", "ignore"] });
    } catch {
      finish("");
      return;
    }
    timer = setTimeout(() => {
      try {
        child.kill("SIGKILL");
        child.stdout.destroy();
        child.unref();
      } catch {
        // already gone
      }
      finish("");
    }, TIMEOUT_MS);
    child.on("error", () => finish(""));
    child.stdout.on("data", (chunk) => {
      out += chunk;
    });
    child.on("close", (code) => finish(code === 0 ? out : ""));
    child.stdin.on("error", () => {});
    child.stdin.end(JSON.stringify(input));
  });
}

/** The context `by hook --harness generic <event>` has for this session, or "". */
async function hookContext(event, cwd, sessionId) {
  const out = await runBy(["hook", "--harness", "generic", event], { cwd, session_id: sessionId });
  if (!out.trim()) return "";
  try {
    const reply = JSON.parse(out);
    return reply && typeof reply.context === "string" ? reply.context.trim() : "";
  } catch {
    return "";
  }
}

/** An ascending part id in OpenCode's shape: `prt_` + 12 hex digits of time + 14 base62. */
function partId() {
  const time = (BigInt(Date.now()) * 0x1000n).toString(16).padStart(12, "0").slice(-12);
  const chars = "0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";
  const bytes = randomBytes(14);
  let tail = "";
  for (const byte of bytes) tail += chars[byte % 62];
  return `prt_${time}${tail}`;
}

/** @type {import("@opencode-ai/plugin").Plugin} */
async function server(input) {
  const cwd = (input && (input.directory || input.worktree)) || process.cwd();
  // Sessions that have had their session-start context; later messages ask only for news.
  const started = new Set();

  return {
    "chat.message": async (hookInput, output) => {
      try {
        const sessionId = String(hookInput && hookInput.sessionID);
        if (!sessionId || !output || !Array.isArray(output.parts)) return;
        const event = started.has(sessionId) ? "user-prompt-submit" : "session-start";
        started.add(sessionId);
        const text = await hookContext(event, cwd, sessionId);
        if (!text) return;
        const messageId = (output.message && output.message.id) || hookInput.messageID;
        output.parts.push({
          id: partId(),
          sessionID: sessionId,
          messageID: messageId,
          type: "text",
          text,
          synthetic: true,
        });
      } catch {
        // never break the user's message
      }
    },
  };
}

export default { id: "branchyard", server };
