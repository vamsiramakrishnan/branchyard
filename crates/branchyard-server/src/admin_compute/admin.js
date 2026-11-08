// The admin compute console: reads /v1/admin/compute and posts plans to
// /v1/admin/compute/plan with the admin token the operator pastes. Every
// value is written with textContent, never as HTML.
"use strict";

const RUNTIMES = {
  gke: [["gvisor", "gVisor (GKE Sandbox)"], ["kata", "Kata Containers (microVM)"]],
  gce: [["gvisor", "gVisor (Docker runsc)"], ["microsandbox", "Microsandbox (KVM microVM)"], ["process", "Process (no isolation)"]],
};

const $ = (id) => document.getElementById(id);

function token() {
  try { return sessionStorage.getItem("by-admin-token") || ""; } catch { return ""; }
}

async function call(method, path, body) {
  const response = await fetch(path, {
    method,
    headers: { authorization: `Bearer ${token()}`, "content-type": "application/json" },
    body: body === undefined ? undefined : JSON.stringify(body),
  });
  const value = await response.json().catch(() => ({}));
  if (!response.ok) throw new Error(value.message || value.error || `HTTP ${response.status}`);
  return value;
}

function cell(row, text, className) {
  const td = row.insertCell();
  td.textContent = text;
  if (className) td.className = className;
  return td;
}

function header(table, names) {
  table.replaceChildren();
  const row = table.createTHead().insertRow();
  for (const name of names) {
    const th = document.createElement("th");
    th.textContent = name;
    row.appendChild(th);
  }
  return table.createTBody();
}

function render(report) {
  $("origin").textContent = report.origin === "default"
    ? "No execution_presets are configured: the server offers its implicit local preset."
    : "From the server's execution_presets.";
  const targets = header($("targets"), ["Environment", "Provider", "Runs on", "Isolation", "Network", "Labels", "Workers", "State"]);
  for (const t of report.targets) {
    const row = targets.insertRow();
    cell(row, `${t.label} (${t.preset})`);
    cell(row, t.provider);
    cell(row, t.runs_on);
    cell(row, t.isolation);
    cell(row, t.network || "");
    cell(row, t.require_labels.join(", "));
    cell(row, String(t.workers.length));
    const state = cell(row, t.state.replaceAll("_", " "), `state-${t.state}`);
    state.title = t.next;
  }
  const workers = header($("workers"), ["Worker", "Host", "Labels", "Seen", "Can claim"]);
  for (const w of report.workers) {
    const row = workers.insertRow();
    cell(row, w.this ? `${w.id} (this server)` : w.id);
    cell(row, w.host);
    cell(row, w.labels.join(", "));
    cell(row, `${Math.round(w.seen_ms_ago / 1000)} s ago`);
    cell(row, w.targets.join(", "));
  }
}

function shellWord(word) {
  return /^[A-Za-z0-9_@%+=:,./-]+$/.test(word) ? word : `'${word.replaceAll("'", "'\\''")}'`;
}

function showPlan(plan) {
  const steps = $("steps");
  steps.replaceChildren();
  for (const step of plan.steps) {
    const li = document.createElement("li");
    const title = document.createElement("p");
    title.textContent = `${step.title} (on the ${step.on})`;
    const pre = document.createElement("pre");
    pre.textContent = step.argv.map(shellWord).join(" ");
    li.append(title, pre);
    steps.appendChild(li);
  }
  $("allow").textContent = JSON.stringify(plan.allow_provider);
  $("preset").textContent = JSON.stringify(plan.preset, null, 2);
  const notes = $("notes");
  notes.replaceChildren(...plan.notes.map((n) => { const li = document.createElement("li"); li.textContent = n; return li; }));
  $("plan").hidden = false;
}

function syncForm() {
  const form = $("plan-form");
  const platform = form.platform.value;
  const runtime = form.runtime;
  const current = runtime.value;
  runtime.replaceChildren(...RUNTIMES[platform].map(([value, text]) => new Option(text, value)));
  if (RUNTIMES[platform].some(([value]) => value === current)) runtime.value = current;
  for (const label of form.querySelectorAll("[data-for]")) {
    const kind = label.dataset.for;
    label.hidden = kind === "image" ? platform === "gce" && runtime.value === "process" : kind !== platform;
  }
}

async function load() {
  try {
    render(await call("GET", "/v1/admin/compute"));
    $("login").hidden = true;
    $("console").hidden = false;
  } catch (error) {
    $("login").querySelector(".muted").textContent = `Could not open the console: ${error.message}`;
  }
}

$("token-form").addEventListener("submit", (event) => {
  event.preventDefault();
  try { sessionStorage.setItem("by-admin-token", $("token").value.trim()); } catch { /* kept nowhere */ }
  load();
});

$("plan-form").addEventListener("change", syncForm);
$("plan-form").addEventListener("submit", async (event) => {
  event.preventDefault();
  const form = event.target;
  const data = Object.fromEntries(new FormData(form));
  const body = {
    id: data.id, platform: data.platform, runtime: data.runtime, project: data.project,
    zone: data.zone, name: data.name, version: data.version,
    tunnel_through_iap: form.tunnel_through_iap.checked,
    harnesses: (data.harnesses || "").split(",").map((h) => h.trim()).filter(Boolean),
  };
  for (const key of ["image", "gke_version", "service_account", "master_cidr"]) {
    if (data[key]) body[key] = data[key].trim();
  }
  $("plan-error").hidden = true;
  try {
    showPlan(await call("POST", "/v1/admin/compute/plan", body));
  } catch (error) {
    $("plan").hidden = true;
    $("plan-error").textContent = error.message;
    $("plan-error").hidden = false;
  }
});

syncForm();
if (token()) load();
