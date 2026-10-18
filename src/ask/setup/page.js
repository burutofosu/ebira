"use strict";

// The token from the address `ebira ask-setup` printed; every call to this page's server carries it.
const token = new URLSearchParams(location.search).get("t") || "";
const byId = (id) => document.getElementById(id);
const controls = ["api", "url", "key", "toggle", "check", "delete", "strong", "cheap", "save", "reset", "quit"].map(byId);
const unreachable = "The page's server is gone: run `ebira ask-setup` again.";
let status = null;

async function call(path, body) {
  const response = await fetch(path, {
    method: body === undefined ? "GET" : "POST",
    headers: { "X-Ebira-Token": token, "Content-Type": "application/json" },
    body: body === undefined ? undefined : JSON.stringify(body),
    cache: "no-store",
  });
  return response.json();
}

function setBusy(busy) {
  for (const control of controls) control.disabled = busy;
}

function say(text, ok) {
  const box = byId("result");
  box.textContent = text || "";
  box.className = "result" + (ok === true ? " ok" : ok === false ? " bad" : "");
}

// The service chosen on the page, as the server describes it.
function chosen() {
  const name = byId("api").value;
  return status.services.find((service) => service.name === name) || status.services[0];
}

function choice() {
  return {
    api: byId("api").value,
    url: byId("url").value.trim(),
    key: byId("key").value.trim(),
    strong: byId("strong").value.trim(),
    cheap: byId("cheap").value.trim(),
  };
}

function showModelList(models) {
  byId("model-list").replaceChildren(...(models || []).map((id) => new Option(id, id)));
}

// The chosen service's key and URL field, and, unless the person is choosing them, its models:
// the saved ones when it is the saved service.
function showService(keepModels) {
  const service = chosen();
  const anthropic = service.name === "anthropic";
  byId("url-field").hidden = service.name !== "compatible";
  byId("key").placeholder = service.placeholder;
  byId("key-label").textContent = (service.masked ? "Replace it with a new key" : "Enter the key") + " (" + service.variable + ")";
  byId("key-state").textContent = service.masked
    ? (service.removable ? "Saved:" : "Set:")
    : service.name === "compatible"
      ? "No key saved; most local servers need none."
      : "No key saved yet.";
  byId("masked").textContent = service.masked;
  byId("key-where").textContent = service.masked ? "in " + service.where : "";
  byId("delete").hidden = !service.removable;
  byId("anthropic-note").hidden = !anthropic;
  byId("service-note").textContent = anthropic
    ? "The default. Ebira's instructions were tuned with Claude models."
    : "Ebira's instructions were tuned with Claude models; whichever model answers, its quotes are checked against the records. Prices are known for Claude models only: other calls are recorded with their tokens and no price.";
  if (!keepModels) {
    const saved = service.name === status.api;
    byId("strong").value = saved ? status.strong : "";
    byId("cheap").value = saved ? status.cheap : "";
    showModelList(null);
  }
  const pick = "check the key, then choose from the list";
  byId("strong").placeholder = anthropic ? status.default_strong + " (the default)" : pick;
  byId("cheap").placeholder = anthropic ? status.default_cheap + " (the default)" : pick;
  byId("models-note").textContent = anthropic
    ? "Leave a field empty for the default. Checking the key lists the models it can use."
    : "Checking the key lists the service's models. One model may do both.";
}

async function refresh(keepChoice) {
  status = await call("/api/status");
  if (!status.ok) {
    say(status.message, false);
    return;
  }
  const select = byId("api");
  if (select.options.length === 0) {
    for (const service of status.services) select.append(new Option(service.label, service.name));
  }
  if (!keepChoice) {
    select.value = status.api;
    byId("url").value = status.url;
  }
  showService(keepChoice);
  byId("notice").textContent = status.notice;
  byId("notice").hidden = !status.notice;
  byId("save").textContent = status.windows ? "Check and save" : "Check, and show what to set";
  byId("reset").hidden = !(status.windows && status.customized);
  byId("spending").textContent = status.spending || "";
}

// Runs an action, then shows the saved state again: the saved choice when the action changed it,
// else the choice the person is making.
async function act(waiting, action, changesSaved) {
  setBusy(true);
  say(waiting);
  try {
    const result = await action();
    await refresh(!(changesSaved || result.saved));
    say(result.message, result.ok);
    if (result.models) showModelList(result.models);
  } catch (error) {
    say(unreachable, false);
  } finally {
    setBusy(false);
  }
}

byId("api").addEventListener("change", () => {
  say("");
  showService(false);
});

byId("toggle").addEventListener("click", () => {
  const input = byId("key");
  const show = input.type === "password";
  input.type = show ? "text" : "password";
  byId("toggle").textContent = show ? "Hide" : "Show";
  byId("toggle").setAttribute("aria-pressed", String(show));
});

byId("check").addEventListener("click", () =>
  act("Checking…", () => call("/api/check", choice()))
);

byId("save").addEventListener("click", () =>
  act("Checking…", async () => {
    const result = await call("/api/save", choice());
    if (result.saved) byId("key").value = "";
    return result;
  })
);

byId("delete").addEventListener("click", () => {
  const service = chosen();
  if (!window.confirm("Remove the saved " + service.label + " key (" + service.variable + ") from the Windows user environment?")) return;
  act("Removing…", () => call("/api/delete", { api: service.name }));
});

byId("reset").addEventListener("click", () => {
  if (!window.confirm("Go back to the defaults: Anthropic, with " + status.default_strong + " and " + status.default_cheap + "? Saved keys are kept.")) return;
  act("Resetting…", () => call("/api/reset", {}), true);
});

byId("key").addEventListener("keydown", (event) => {
  if (event.key === "Enter") byId("save").click();
});

byId("quit").addEventListener("click", async () => {
  setBusy(true);
  try {
    await call("/api/quit", {});
  } catch (error) {
    // The server may already be gone.
  }
  const done = document.createElement("p");
  done.className = "lead";
  done.textContent = "Closed. This tab can be closed.";
  document.querySelector("main").replaceChildren(done);
});

refresh(false).catch(() => say(unreachable, false));
