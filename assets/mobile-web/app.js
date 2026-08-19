"use strict";

const POLL_INTERVAL_MS = 2_000;
const state = {
  snapshot: null,
  filter: "attention",
  deferredInstallPrompt: null,
  pollTimer: null,
  snapshotRequest: null,
  detailRequest: null,
  online: true,
};

const elements = {
  dashboardView: document.querySelector("#dashboard-view"),
  detailView: document.querySelector("#detail-view"),
  dashboardState: document.querySelector("#dashboard-state"),
  detailState: document.querySelector("#detail-state"),
  detailContent: document.querySelector("#detail-content"),
  agentList: document.querySelector("#agent-list"),
  resultCount: document.querySelector("#result-count"),
  viewerCopy: document.querySelector("#viewer-copy"),
  lastUpdated: document.querySelector("#last-updated"),
  connectionStatus: document.querySelector("#connection-status"),
  connectionLabel: document.querySelector("#connection-label"),
  installButton: document.querySelector("#install-button"),
  refreshDetail: document.querySelector("#refresh-detail"),
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

function formatTime(timestamp) {
  if (!Number.isFinite(timestamp)) return "Unknown";
  return new Intl.DateTimeFormat(undefined, {
    month: "short",
    day: "numeric",
    hour: "numeric",
    minute: "2-digit",
    second: "2-digit",
  }).format(new Date(timestamp));
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

function detailHash(agent) {
  return `#/agents/${encodeURIComponent(agent.node_id)}/${encodeURIComponent(agent.agent_id)}`;
}

function routeDetail() {
  const match = location.hash.match(/^#\/agents\/([^/]+)\/([^/]+)$/);
  if (!match) return null;
  try {
    return { nodeId: decodeURIComponent(match[1]), agentId: decodeURIComponent(match[2]) };
  } catch {
    return null;
  }
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
    ? `Read-only fleet view for ${snapshot.viewer}.`
    : "Read-only visibility across the fleet.";
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
  const link = el("a", "agent-card-link");
  link.href = detailHash(agent);

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
  link.append(rail, body, el("span", "card-arrow", "→"));
  item.append(link);
  return item;
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

function renderDetail(payload) {
  const agent = payload.agent;
  document.title = `${agentDisplayName(agent)} · Boomux Agents`;
  document.querySelector("#detail-title").textContent = agentDisplayName(agent);
  document.querySelector("#detail-integration").textContent = agent.integration || "Agent";
  document.querySelector("#detail-location").textContent = `${agent.workspace_name || "Unnamed workspace"} / ${agent.shell_name || "Unnamed shell"}`;
  document.querySelector("#detail-revision").textContent = `Observation r${agent.observation_revision ?? "?"}`;

  const glyph = document.querySelector("#detail-glyph");
  glyph.dataset.tone = toneFor(agent);
  const badge = document.querySelector("#detail-badge");
  badge.textContent = formatState(agent.state);
  badge.dataset.tone = toneFor(agent);

  const facts = document.querySelector("#detail-facts");
  facts.replaceChildren(
    fact("Node", agent.node_alias || agent.node_id, agent.node_stale ? "Stale" : agent.node_health || "Current"),
    fact("Started", formatTime(agent.started_at_ms), relativeTime(agent.started_at_ms)),
    fact("Run ID", agent.run_id || "Unavailable", agent.run_current ? "Current run" : "Historical run"),
  );

  const timeline = document.querySelector("#timeline");
  const events = Array.isArray(payload.timeline) ? [...payload.timeline] : [];
  if (agent.just_completed && !agent.attention) {
    events.push({
      kind: "completion",
      at_ms: agent.observed_at_ms,
      title: "Turn completed",
      body: "This local completion was observed from the working-to-idle transition and is ready for review.",
      tone: "success",
    });
  }
  events.sort((left, right) => (left.at_ms || 0) - (right.at_ms || 0));
  timeline.replaceChildren(...events.map(renderTimelineEvent));
  if (!events.length) {
    const empty = el("li", "timeline-empty", "No timeline events have been observed for this run.");
    timeline.append(empty);
  }

  const nativeWeb = payload.native_web;
  const nativeLink = document.querySelector("#native-handoff-link");
  const nativeNotice = document.querySelector("#native-handoff-notice");
  if (nativeLink && nativeNotice) {
    nativeNotice.textContent = payload.native_web_notice || "Native web handoff is unavailable.";
    const nativeUrl = nativeWeb?.url || derivedNativeUrl(nativeWeb);
    nativeLink.hidden = !nativeUrl;
    nativeLink.removeAttribute("href");
    nativeLink.textContent = nativeWeb?.label || "Open native interface";
    if (nativeUrl) nativeLink.href = nativeUrl;
  }

  const terminal = document.querySelector("#terminal-output");
  const terminalSection = document.querySelector("#terminal-section");
  const notice = document.querySelector("#terminal-notice");
  terminal.textContent = payload.terminal_output || "No rendered terminal output is available.";
  terminal.classList.toggle("is-empty", !payload.terminal_output);
  terminalSection.dataset.available = String(Boolean(payload.terminal_available));
  notice.textContent = payload.notice || (payload.terminal_available
    ? "The terminal is available, but no output has been rendered yet."
    : "Terminal output is not available for this agent.");

  elements.detailState.hidden = true;
  elements.detailContent.hidden = false;
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

function clearNativeHandoff() {
  const nativeLink = document.querySelector("#native-handoff-link");
  if (!nativeLink) return;
  nativeLink.hidden = true;
  nativeLink.removeAttribute("href");
}

function fact(label, value, note) {
  const node = el("div", "fact");
  node.append(el("small", "", label), el("strong", "", value), el("span", "", note));
  return node;
}

function renderTimelineEvent(event) {
  const item = el("li", "timeline-event");
  item.dataset.tone = event.tone || "neutral";
  const marker = el("span", "timeline-marker");
  marker.setAttribute("aria-hidden", "true");
  const content = el("div", "timeline-content");
  const header = el("div", "timeline-header");
  header.append(el("strong", "", event.title || formatState(event.kind)), el("time", "", formatTime(event.at_ms)));
  const body = el("p", "", event.body || "No additional detail.");
  content.append(header, body, el("small", "timeline-kind", formatState(event.kind)));
  item.append(marker, content);
  return item;
}

async function refreshDetail({ announceLoading = false } = {}) {
  const route = routeDetail();
  if (!route) return;
  state.detailRequest?.abort();
  const controller = new AbortController();
  state.detailRequest = controller;
  if (announceLoading || elements.detailContent.hidden) {
    elements.detailContent.hidden = true;
    showState(elements.detailState, "Loading agent detail…", "Fetching the latest observation.");
  }
  try {
    const url = `/api/agents/${encodeURIComponent(route.nodeId)}/${encodeURIComponent(route.agentId)}`;
    const payload = await fetchJson(url, controller.signal);
    if (routeDetail()?.nodeId === route.nodeId && routeDetail()?.agentId === route.agentId) renderDetail(payload);
  } catch (error) {
    if (error.name === "AbortError") return;
    clearNativeHandoff();
    if (elements.detailContent.hidden) {
      showState(elements.detailState, "Agent detail unavailable", "The run may have ended or the node cannot be reached. Return to the fleet or try again.", "error");
    }
  } finally {
    if (state.detailRequest === controller) state.detailRequest = null;
  }
}

function applyRoute() {
  const detail = routeDetail();
  elements.dashboardView.hidden = Boolean(detail);
  elements.detailView.hidden = !detail;
  if (detail) {
    refreshDetail({ announceLoading: true });
  } else {
    state.detailRequest?.abort();
    document.title = "Boomux Agents";
    renderSnapshot();
  }
  window.scrollTo({ top: 0, behavior: "instant" });
}

async function poll() {
  if (document.visibilityState !== "visible") return;
  const requests = [refreshSnapshot()];
  if (routeDetail()) requests.push(refreshDetail());
  await Promise.all(requests);
}

function restartPolling() {
  clearInterval(state.pollTimer);
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

elements.refreshDetail.addEventListener("click", () => refreshDetail({ announceLoading: true }));
window.addEventListener("hashchange", applyRoute);
window.addEventListener("focus", restartPolling);
document.addEventListener("visibilitychange", () => {
  if (document.visibilityState === "visible") restartPolling();
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

applyRoute();
restartPolling();
