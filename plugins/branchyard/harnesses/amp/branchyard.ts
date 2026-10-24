// Branchyard for Amp: tells a thread a person started which of the repository's
// Branchyard branches finished, failed, are blocked or ask, and keeps a status item.
// It runs `by hook --harness generic <event>` and `by statusline --harness generic`;
// without `by` on PATH, or when `by` is slow (3 s) or fails, it adds nothing and never
// throws. Plain JavaScript in a .ts file, so Amp (Bun) and node load it as it is.
// See docs/harness-hooks.md.

import { spawn } from "node:child_process";

export const description =
  "Tells each thread which Branchyard branches finished, failed, are blocked or ask, and shows a Branchyard status item. Needs by on PATH.";

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

/** The context `by hook --harness generic <event>` has for this thread, or "". */
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

function workspaceOf(amp) {
  try {
    const root = amp.system && amp.system.workspaceRoot;
    if (root) {
      const path = amp.helpers.filePathFromURI(root);
      if (typeof path === "string" && path) return path;
    }
  } catch {
    // no workspace: use the process's directory
  }
  return process.cwd();
}

export default function (amp) {
  // Threads that have had their session-start context; later prompts ask only for news.
  const started = new Set();
  let status = null;
  let shown = "";

  async function refreshStatus() {
    try {
      if (typeof amp.createStatusItem !== "function") return;
      const text = await statusText(workspaceOf(amp));
      if (text === shown) return;
      if (!status) status = amp.createStatusItem();
      status.update({ text });
      shown = text;
    } catch {
      // a status item is never worth an error
    }
  }

  amp.on("session.start", async () => {
    await refreshStatus();
  });

  amp.on("agent.start", async (event) => {
    try {
      const threadId = String((event && event.thread && event.thread.id) || `amp-${process.pid}`);
      const name = started.has(threadId) ? "user-prompt-submit" : "session-start";
      started.add(threadId);
      const text = await hookContext(name, workspaceOf(amp), threadId);
      if (!text) return undefined;
      return { message: { content: text, display: true } };
    } catch {
      return undefined;
    }
  });

  amp.on("agent.end", async () => {
    await refreshStatus();
  });
}
