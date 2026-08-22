"use strict";

import { createTerminal } from "./terminal.js";

const POLL_INTERVAL_MS = 2_000;
const state = {
  snapshot: null,
  filter: "all",
  deferredInstallPrompt: null,
  pollTimer: null,
  snapshotRequest: null,
  online: true,
  activeTerminal: null,
};

const elements = {
  dashboardState: document.querySelector("#dashboard-state"),
  agentList: document.querySelector("#agent-list"),
  resultCount: document.querySelector("#result-count"),
  viewerCopy: document.querySelector("#viewer-copy"),
  lastUpdated: document.querySelector("#last-updated"),
  connectionStatus: document.querySelector("#connection-status"),
  connectionLabel: document.querySelector("#connection-label"),
  installButton: document.querySelector("#install-button"),
  counts: {
    attention: document.querySelector("#count-attention"),
    active: document.querySelector("#count-active"),
    agents: document.querySelector("#count-agents"),
    stale: document.querySelector("#count-stale"),
  },
};

function el(tag, className, text) {
  const node = document.createElement(tag);
  if (className) node.className = className;
  if (text !== undefined && text !== null) node.textContent = text;
  return node;
}

function formatState(value) {
  return String(value || "unknown").replaceAll("_", " ");
}

function relativeTime(timestamp) {
  if (!Number.isFinite(timestamp)) return "time unknown";
  const deltaSeconds = Math.round((timestamp - Date.now()) / 1_000);
  const absolute = Math.abs(deltaSeconds);
  const formatter = new Intl.RelativeTimeFormat(undefined, { numeric: "auto" });
  if (absolute < 60) return formatter.format(deltaSeconds, "second");
  if (absolute < 3_600) return formatter.format(Math.round(deltaSeconds / 60), "minute");
  if (absolute < 86_400) return formatter.format(Math.round(deltaSeconds / 3_600), "hour");
  return formatter.format(Math.round(deltaSeconds / 86_400), "day");
}

function isActive(agent) {
  return agent.node_current && agent.run_current && (agent.state === "working" || agent.state === "blocked");
}

function agentDisplayName(agent) {
  if (agent.shell_name && agent.shell_name !== "removed Shell") return agent.shell_name;
  return agent.name || agent.agent_id;
}

function hasAlert(agent) {
  return Boolean(agent.attention || agent.just_completed);
}

function toneFor(agent) {
  if (hasAlert(agent)) return "attention";
  if (agent.node_stale) return "stale";
  return isActive(agent) ? "active" : "quiet";
}

function usefulAgentOrder(a, b) {
  const rank = (agent) => {
    if (hasAlert(agent)) return 0;
    if (isActive(agent) && !agent.node_stale) return 1;
    if (isActive(agent)) return 2;
    if (agent.node_stale) return 4;
    return 3;
  };
  return rank(a) - rank(b) || (b.observed_at_ms || 0) - (a.observed_at_ms || 0);
}

function showState(container, title, message, kind = "empty") {
  container.replaceChildren();
  const symbol = el("span", `state-symbol state-symbol-${kind}`, kind === "error" ? "!" : "·");
  symbol.setAttribute("aria-hidden", "true");
  container.append(symbol, el("strong", "", title));
  if (message) container.append(el("span", "", message));
  container.hidden = false;
}

function updateConnection(status, label) {
  elements.connectionStatus.dataset.status = status;
  elements.connectionLabel.textContent = label;
}

function renderSnapshot() {
  const { snapshot } = state;
  if (!snapshot) return;
  const counts = snapshot.counts || {};
  const agents = Array.isArray(snapshot.agents) ? [...snapshot.agents] : [];
  elements.counts.attention.textContent = agents.filter(hasAlert).length;
  elements.counts.active.textContent = counts.active ?? 0;
  elements.counts.agents.textContent = counts.agents ?? snapshot.agents?.length ?? 0;
  elements.counts.stale.textContent = counts.stale_nodes ?? 0;
  elements.viewerCopy.textContent = snapshot.viewer
    ? `Current work and attention for ${snapshot.viewer}.`
    : "Current work and attention across your Boomux nodes.";
  elements.lastUpdated.textContent = `Snapshot ${relativeTime(snapshot.generated_at_ms)}`;

  const filtered = agents.filter((agent) => {
    if (state.filter === "attention") return hasAlert(agent);
    if (state.filter === "active") return isActive(agent);
    return true;
  }).sort(usefulAgentOrder);

  elements.agentList.replaceChildren(...filtered.map(renderAgentCard));
  elements.agentList.setAttribute("aria-busy", "false");
  elements.resultCount.textContent = `${filtered.length} ${filtered.length === 1 ? "agent" : "agents"}`;

  if (filtered.length) {
    elements.dashboardState.hidden = true;
  } else {
    const messages = {
      attention: ["The queue is clear", "No agents currently need attention."],
      active: ["No agents in motion", "Active agents will appear here as they start."],
      all: ["No agents observed", "The fleet has not reported any agent runs yet."],
    };
    showState(elements.dashboardState, ...messages[state.filter]);
  }
}

function renderAgentCard(agent) {
  const item = el("li", `agent-card tone-${toneFor(agent)}`);
  const content = el("article", "agent-card-content");

  const rail = el("span", "card-rail");
  rail.setAttribute("aria-hidden", "true");
  const body = el("div", "card-body");
  const top = el("div", "card-top");
  const identity = el("div", "card-identity");
  identity.append(el("h2", "", agentDisplayName(agent)));
  identity.append(el("p", "", `${agent.workspace_name || "Unnamed workspace"} / ${agent.shell_name || "Unnamed shell"}`));
  const badge = el("span", "state-badge", formatState(agent.state));
  badge.dataset.tone = toneFor(agent);
  top.append(identity, badge);

  const meta = el("div", "card-meta");
  meta.append(metaItem("Node", agent.node_alias || agent.node_id));
  meta.append(metaItem("Integration", agent.integration || "unknown"));
  meta.append(metaItem("Observed", relativeTime(agent.observed_at_ms)));

  body.append(top);
  if (agent.attention) {
    const callout = el("div", "attention-callout");
    const blocked = agent.attention.reason === "blocked";
    callout.append(
      el("strong", "", blocked ? "Needs attention" : "Completed"),
      el("span", "", blocked ? "Review requested" : "Ready for review"),
    );
    body.append(callout);
  } else if (agent.just_completed) {
    const callout = el("div", "attention-callout attention-callout-completed");
    callout.append(el("strong", "", "Finished"), el("span", "", "Turn completed and ready for review"));
    body.append(callout);
  } else if (agent.node_stale) {
    body.append(el("p", "stale-callout", "Node data is stale; this status may be out of date."));
  }
  body.append(meta);
  const nativeUrl = agent.native_web?.url || derivedNativeUrl(agent.native_web);
  const webTerminal = canOpenWebTerminal(agent);
  const dismissible = agent.node_local && hasAlert(agent);
  if (nativeUrl || webTerminal || dismissible) {
    const actions = el("div", "card-actions");
    if (nativeUrl) {
      const nativeLink = el("a", "card-native-link", agent.native_web.label || "Open in OpenCode");
      nativeLink.href = nativeUrl;
      nativeLink.target = "_blank";
      nativeLink.rel = "noreferrer";
      nativeLink.referrerPolicy = "no-referrer";
      actions.append(nativeLink);
    }
    if (webTerminal) {
      const terminalButton = el("button", "card-native-link", "Open in Web Terminal");
      terminalButton.type = "button";
      terminalButton.addEventListener("click", () => openWebTerminal(agent));
      actions.append(terminalButton);
    }
    if (dismissible) {
      const dismissButton = el("button", "card-dismiss-button", "Dismiss");
      dismissButton.type = "button";
      dismissButton.addEventListener("click", () => dismissAttention(agent, dismissButton));
      actions.append(dismissButton);
    }
    body.append(actions);
  }
  content.append(rail, body);
  item.append(content);
  return item;
}

function canOpenWebTerminal(agent) {
  return agent.node_local && agent.node_current && agent.run_current;
}

async function openWebTerminal(agent) {
  state.activeTerminal?.close();
  const dialog = el("dialog", "terminal-dialog");
  dialog.setAttribute("aria-label", `Web terminal for ${agentDisplayName(agent)}`);
  const shell = el("section", "terminal-shell");
  const notice = el("p", "terminal-notice", "This browser is joining the Agent's current terminal without disconnecting the desktop.");
  notice.setAttribute("aria-live", "polite");
  const viewport = el("div", "terminal-viewport");
  shell.append(notice, viewport);
  dialog.append(shell);
  document.body.append(dialog);
  dialog.showModal();
  const historyId = crypto.randomUUID();

  let socket = null;
  let terminal = null;
  let closed = false;
  const sendJson = (message) => {
    if (socket?.readyState === WebSocket.OPEN) socket.send(JSON.stringify(message));
  };
  const close = () => {
    if (closed) return;
    closed = true;
    const ownsHistory = history.state?.boomuxTerminal === historyId;
    window.removeEventListener("popstate", close);
    sendJson({ type: "detach" });
    socket?.close(1000, "terminal closed");
    terminal?.dispose();
    if (dialog.open) dialog.close();
    dialog.remove();
    if (state.activeTerminal?.dialog === dialog) state.activeTerminal = null;
    if (document.visibilityState === "visible") restartPolling();
    if (ownsHistory) history.back();
  };
  state.activeTerminal = { dialog, terminal, close };
  state.snapshotRequest?.abort();
  clearInterval(state.pollTimer);
  state.pollTimer = null;
  window.addEventListener("popstate", close);
  dialog.addEventListener("cancel", (event) => {
    event.preventDefault();
    close();
  });
  viewport.addEventListener("focusin", () => sendJson({ type: "focus" }));
  viewport.addEventListener("pointerdown", () => terminal?.focus());

  try {
    terminal = await createTerminal(viewport, {
      input: (bytes) => {
        if (socket?.readyState === WebSocket.OPEN) socket.send(bytes);
      },
      resize: (dimensions) => sendJson({ type: "resize", ...dimensions }),
    });
    if (closed) {
      terminal.dispose();
      return;
    }
    state.activeTerminal.terminal = terminal;
    history.pushState({ ...history.state, boomuxTerminal: historyId }, "", "#web-terminal");
    const authorization = await fetch("/api/terminal/authorize", {
      method: "POST",
      cache: "no-store",
      headers: { Accept: "application/json", "Content-Type": "application/json" },
      body: JSON.stringify({
        node_id: agent.node_id,
        agent_id: agent.agent_id,
        shell_id: agent.shell_id,
        run_id: agent.run_id,
        ...terminal.dimensions(),
      }),
    });
    if (!authorization.ok) throw new Error(`Authorization failed (${authorization.status})`);
    const grant = await authorization.json();
    if (closed) return;
    const url = new URL(grant.path, window.location.href);
    url.protocol = window.location.protocol === "https:" ? "wss:" : "ws:";
    socket = new WebSocket(url, [grant.protocol, `boomux.token.${grant.token}`]);
    socket.binaryType = "arraybuffer";
    socket.addEventListener("open", () => {
      terminal.focus();
    });
    socket.addEventListener("message", (event) => {
      if (typeof event.data !== "string") {
        terminal.write(new Uint8Array(event.data));
        return;
      }
      const message = JSON.parse(event.data);
      if (message.type === "attached") {
        terminal.resize(message);
        notice.textContent = message.warning || "This browser has terminal control. Closing it leaves the Agent running.";
        if (message.warning) terminal.write(`\r\n${message.warning}\r\n`);
      } else if (message.type === "resize") {
        terminal.resize(message);
      } else if (message.type === "reconnecting") {
        notice.textContent = "Reconnecting to the same Agent terminal.";
      } else if (message.type === "closed" || message.type === "error") {
        notice.textContent = message.message || "The terminal attachment ended.";
        terminal.write(`\r\n${notice.textContent}\r\n`);
      }
    });
    socket.addEventListener("close", () => {
      if (!closed) {
        notice.textContent = "Terminal control ended. The Agent may still be running.";
        terminal.write(`\r\n${notice.textContent}\r\n`);
      }
    });
    socket.addEventListener("error", () => {
      notice.textContent = "Could not connect to the Agent terminal.";
    });
  } catch (error) {
    if (!terminal) {
      close();
      return;
    }
    notice.textContent = error.message || "Could not open the Agent terminal.";
    terminal.write(`\r\n${notice.textContent}\r\n`);
  }
}

function metaItem(label, value) {
  const node = el("span", "meta-item");
  node.append(el("small", "", label), el("strong", "", value));
  return node;
}

async function fetchJson(url, signal) {
  const response = await fetch(url, {
    signal,
    cache: "no-store",
    headers: { Accept: "application/json" },
  });
  if (!response.ok) throw new Error(`Request failed (${response.status})`);
  return response.json();
}

async function dismissAttention(agent, button) {
  button.disabled = true;
  button.textContent = "Dismissing...";
  try {
    const response = await fetch("/api/attention/dismiss", {
      method: "POST",
      cache: "no-store",
      headers: {
        Accept: "application/json",
        "Content-Type": "application/json",
      },
      body: JSON.stringify({
        node_id: agent.node_id,
        agent_id: agent.agent_id,
        observation_revision: agent.attention?.observation_revision ?? agent.observation_revision,
      }),
    });
    if (!response.ok) throw new Error(`Request failed (${response.status})`);
    await refreshSnapshot();
  } catch (_) {
    button.disabled = false;
    button.textContent = "Retry dismiss";
  }
}

async function refreshSnapshot() {
  state.snapshotRequest?.abort();
  const controller = new AbortController();
  state.snapshotRequest = controller;
  try {
    const snapshot = await fetchJson("/api/snapshot", controller.signal);
    state.snapshot = snapshot;
    state.online = snapshot.daemon_connected;
    updateConnection(
      snapshot.daemon_connected ? "online" : "offline",
      snapshot.daemon_connected ? "Live" : "Daemon offline",
    );
    renderSnapshot();
  } catch (error) {
    if (error.name === "AbortError") return;
    state.online = false;
    updateConnection("offline", "Disconnected");
    if (!state.snapshot) {
      showState(elements.dashboardState, "Snapshot unavailable", "The dashboard could not reach the Boomux API. It will retry automatically.", "error");
      elements.agentList.setAttribute("aria-busy", "false");
    }
  } finally {
    if (state.snapshotRequest === controller) state.snapshotRequest = null;
  }
}

function derivedNativeUrl(nativeWeb) {
  if (!nativeWeb?.port || !nativeWeb?.path) return null;
  const url = new URL(window.location.href);
  url.hash = "";
  url.search = "";
  url.port = String(nativeWeb.port);
  url.pathname = nativeWeb.path;
  return url.href;
}

async function poll() {
  if (document.visibilityState !== "visible" || state.activeTerminal) return;
  await refreshSnapshot();
}

function restartPolling() {
  clearInterval(state.pollTimer);
  if (state.activeTerminal) {
    state.pollTimer = null;
    return;
  }
  poll();
  state.pollTimer = setInterval(poll, POLL_INTERVAL_MS);
}

document.querySelectorAll(".filter-button").forEach((button) => {
  button.addEventListener("click", () => {
    state.filter = button.dataset.filter;
    document.querySelectorAll(".filter-button").forEach((candidate) => {
      const selected = candidate === button;
      candidate.classList.toggle("is-selected", selected);
      candidate.setAttribute("aria-pressed", String(selected));
    });
    renderSnapshot();
  });
});

window.addEventListener("focus", restartPolling);
document.addEventListener("visibilitychange", () => {
  if (document.visibilityState === "visible") {
    restartPolling();
  } else {
    state.activeTerminal?.close();
  }
});

window.addEventListener("beforeinstallprompt", (event) => {
  event.preventDefault();
  state.deferredInstallPrompt = event;
  elements.installButton.hidden = false;
});

elements.installButton.addEventListener("click", async () => {
  if (!state.deferredInstallPrompt) return;
  state.deferredInstallPrompt.prompt();
  await state.deferredInstallPrompt.userChoice;
  state.deferredInstallPrompt = null;
  elements.installButton.hidden = true;
});

window.addEventListener("appinstalled", () => {
  state.deferredInstallPrompt = null;
  elements.installButton.hidden = true;
});

if ("serviceWorker" in navigator) {
  window.addEventListener("load", async () => {
    const registration = await navigator.serviceWorker.register("service-worker.js");
    registration.update();
  });
}

restartPolling();
