// Branchyard for Pi and Oh My Pi: tells a session a person started which of the
// repository's Branchyard branches finished, failed, are blocked or ask, and keeps a
// status-line entry. It runs `by hook --harness generic <event>` and
// `by statusline --harness generic`; without `by` on PATH, or when `by` is slow
// (3 s) or fails, it adds nothing and never throws. Plain JavaScript in a .ts file
// so Pi (jiti), Oh My Pi (Bun) and node all load it without a build step.
// See docs/harness-hooks.md.

import { spawn } from "node:child_process";

const TIMEOUT_MS = 3000;
const STATUS_KEY = "branchyard";

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

/** One status line from `by statusline --harness generic`, or "". */
async function statusText(cwd) {
  const out = await runBy(["statusline", "--harness", "generic"], { cwd });
  return out.split("\n")[0].trim();
}

function sessionIdOf(ctx) {
  try {
    const manager = ctx && ctx.sessionManager;
    const id =
      (manager && typeof manager.getSessionId === "function" && manager.getSessionId()) ||
      (manager && typeof manager.getSessionFile === "function" && manager.getSessionFile());
    if (typeof id === "string" && id) return id;
  } catch {
    // fall through to the process-wide id
  }
  return `pi-${process.pid}`;
}

function cwdOf(ctx) {
  return (ctx && typeof ctx.cwd === "string" && ctx.cwd) || process.cwd();
}

async function refreshStatus(ctx) {
  try {
    if (!ctx || !ctx.ui || typeof ctx.ui.setStatus !== "function") return;
    const text = await statusText(cwdOf(ctx));
    ctx.ui.setStatus(STATUS_KEY, text || undefined);
  } catch {
    // a status line is never worth an error
  }
}

export default function branchyard(pi) {
  // Sessions that have had their session-start context; later turns ask only for news.
  const started = new Set();

  pi.on("session_start", async (_event, ctx) => {
    await refreshStatus(ctx);
  });

  pi.on("before_agent_start", async (_event, ctx) => {
    try {
      const sessionId = sessionIdOf(ctx);
      const event = started.has(sessionId) ? "user-prompt-submit" : "session-start";
      started.add(sessionId);
      const text = await hookContext(event, cwdOf(ctx), sessionId);
      void refreshStatus(ctx);
      if (!text) return undefined;
      return { message: { customType: "branchyard", content: text, display: true } };
    } catch {
      return undefined;
    }
  });

  pi.on("agent_end", async (_event, ctx) => {
    await refreshStatus(ctx);
  });
}
