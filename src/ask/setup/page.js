"use strict";

// The token from the address `ebira ask-setup` printed; every call to this page's server carries it.
const token = new URLSearchParams(location.search).get("t") || "";
const byId = (id) => document.getElementById(id);
const buttons = ["check-saved", "delete", "toggle", "save", "check", "quit"].map(byId);
const unreachable = "The page's server is gone: run `ebira ask-setup` again.";

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
  for (const button of buttons) button.disabled = busy;
}

function say(text, ok) {
  const box = byId("result");
  box.textContent = text || "";
  box.className = "result" + (ok === true ? " ok" : ok === false ? " bad" : "");
}

function showModels(models) {
  const list = byId("models");
  list.replaceChildren();
  for (const model of models || []) {
    const item = document.createElement("li");
    item.textContent = (model.available ? "Available: " : "Not available to this key: ") + model.id + " (" + model.role + ")";
    if (!model.available) item.className = "no";
    list.append(item);
  }
  list.hidden = list.childElementCount === 0;
}

async function refresh() {
  const status = await call("/api/status");
  if (!status.ok) {
    say(status.message, false);
    return;
  }
  byId("state").textContent = status.saved ? "Saved" : "No key saved yet";
  byId("masked").textContent = status.saved ? status.masked : "";
  byId("where").textContent = "Kept in: " + status.where;
  byId("check-saved").hidden = !status.saved;
  byId("delete").hidden = !(status.saved && status.windows);
  byId("spending").textContent = status.spending || "";
}

async function act(waiting, action) {
  setBusy(true);
  say(waiting);
  showModels(null);
  try {
    const result = await action();
    say(result.message, result.ok);
    showModels(result.defaults);
    await refresh();
  } catch (error) {
    say(unreachable, false);
  } finally {
    setBusy(false);
  }
}

byId("toggle").addEventListener("click", () => {
  const input = byId("key");
  const show = input.type === "password";
  input.type = show ? "text" : "password";
  byId("toggle").textContent = show ? "Hide" : "Show";
  byId("toggle").setAttribute("aria-pressed", String(show));
});

byId("save").addEventListener("click", () =>
  act("Checking…", async () => {
    const result = await call("/api/save", { key: byId("key").value.trim() });
    if (result.ok) byId("key").value = "";
    return result;
  })
);

byId("check").addEventListener("click", () =>
  act("Checking…", () => call("/api/check", { key: byId("key").value.trim() }))
);

byId("check-saved").addEventListener("click", () =>
  act("Checking the saved key…", () => call("/api/check", { key: "" }))
);

byId("delete").addEventListener("click", () => {
  if (!window.confirm("Remove the saved key from the Windows user environment?")) return;
  act("Removing…", () => call("/api/delete", {}));
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

refresh().catch(() => say(unreachable, false));
