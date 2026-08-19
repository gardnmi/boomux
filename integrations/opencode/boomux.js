const MAX_EVIDENCE = 160;
const MAX_OUTPUT = 64 * 1024;
const COMMAND_TIMEOUT_MS = 5_000;
const LOG_INTERVAL_MS = 30_000;
const LIFECYCLE_AUTHORITY = "lifecycle_integration";
const LIFECYCLE_CONFIDENCE = 100;

function text(value) {
  return typeof value === "string" && value.length > 0 ? value : undefined;
}

function boundedEvidence(value) {
  const clean = String(value ?? "OpenCode lifecycle event")
    .replace(/\s+/g, " ")
    .trim();
  return (clean || "OpenCode lifecycle event").slice(0, MAX_EVIDENCE);
}

function sessionID(event) {
  const properties = event?.properties ?? {};
  return (
    text(properties.sessionID) ??
    text(properties.sessionId) ??
    text(properties.info?.id) ??
    text(properties.part?.sessionID) ??
    text(properties.message?.sessionID) ??
    text(event?.sessionID)
  );
}

function statusKind(status) {
  const value = typeof status === "string" ? status : status?.type;
  return typeof value === "string" ? value.toLowerCase() : undefined;
}

function requestID(properties) {
  return (
    text(properties?.requestID) ??
    text(properties?.permissionID) ??
    text(properties?.id)
  );
}

function errorEvidence(error) {
  const name = text(error?.name) ?? text(error?.data?.name);
  const message =
    text(error?.message) ?? text(error?.data?.message) ?? "session error";
  return boundedEvidence(
    name ? `OpenCode error: ${name}: ${message}` : `OpenCode error: ${message}`,
  );
}

function classifyEvent(event) {
  const type = text(event?.type);
  const properties = event?.properties ?? {};
  const id = sessionID(event);
  if (!type || !id) return undefined;

  if (type === "session.created") {
    return {
      kind: "idle",
      sessionID: id,
      evidence: "OpenCode root session created",
    };
  }

  if (type === "session.status") {
    const kind = statusKind(properties.status);
    if (
      [
        "busy",
        "retry",
        "active",
        "pending",
        "running",
        "streaming",
        "working",
      ].includes(kind)
    ) {
      return {
        kind: "working",
        sessionID: id,
        evidence: `OpenCode session ${kind}`,
      };
    }
    if (kind === "idle") {
      return {
        kind: "idle",
        sessionID: id,
        evidence: "OpenCode root session idle",
      };
    }
    return undefined;
  }

  if (type === "session.idle") {
    return {
      kind: "idle",
      sessionID: id,
      evidence: "OpenCode root session idle",
    };
  }
  if (type === "session.error") {
    return {
      kind: "error",
      sessionID: id,
      evidence: errorEvidence(properties.error),
    };
  }
  if (type === "session.deleted") {
    return {
      kind: "deleted",
      sessionID: id,
      evidence: "OpenCode session deleted",
    };
  }
  if (
    ["permission.asked", "permission.updated", "question.asked"].includes(type)
  ) {
    const blocker = requestID(properties);
    if (!blocker) return undefined;
    const label = type.startsWith("question") ? "question" : "permission";
    return {
      kind: "block",
      sessionID: id,
      requestID: `${label}:${blocker}`,
      evidence: `OpenCode awaiting ${label}`,
    };
  }
  if (
    ["permission.replied", "question.replied", "question.rejected"].includes(
      type,
    )
  ) {
    const blocker = requestID(properties);
    if (!blocker) return undefined;
    const label = type.startsWith("question") ? "question" : "permission";
    return {
      kind: "unblock",
      sessionID: id,
      requestID: `${label}:${blocker}`,
      evidence: `OpenCode ${label} resolved`,
    };
  }
  if (
    [
      "chat.message",
      "tool.execute.before",
      "tool.execute.after",
      "session.compacted",
      "session.compacting",
      "experimental.session.compacting",
    ].includes(type)
  ) {
    return { kind: "working", sessionID: id, evidence: `OpenCode ${type}` };
  }
  if (type === "message.part.updated") {
    const partType = text(properties.part?.type)?.toLowerCase();
    if (["tool", "retry", "compaction"].includes(partType)) {
      return {
        kind: "working",
        sessionID: id,
        evidence: `OpenCode ${partType}`,
      };
    }
  }
  return undefined;
}

function createReducerState() {
  return {
    blockers: new Map(),
    errors: new Set(),
    lastEvidence: undefined,
    lastState: undefined,
    terminal: false,
  };
}

function blockerCount(state) {
  let count = 0;
  for (const blockers of state.blockers.values()) count += blockers.size;
  return count;
}

function isBlocked(state) {
  return blockerCount(state) > 0 || state.errors.size > 0;
}

function reduce(state, action, isRootEvent) {
  if (state.terminal) return undefined;
  let next;
  let evidence = action.evidence;

  switch (action.kind) {
    case "block": {
      let blockers = state.blockers.get(action.sessionID);
      if (!blockers) {
        blockers = new Set();
        state.blockers.set(action.sessionID, blockers);
      }
      blockers.add(action.requestID);
      next = "blocked";
      evidence = `${action.evidence} (${blockerCount(state)} pending)`;
      break;
    }
    case "unblock": {
      const blockers = state.blockers.get(action.sessionID);
      blockers?.delete(action.requestID);
      if (blockers?.size === 0) state.blockers.delete(action.sessionID);
      next = isBlocked(state) ? "blocked" : "working";
      break;
    }
    case "error":
      state.errors.add(action.sessionID);
      next = "blocked";
      break;
    case "working":
      if (isRootEvent) state.errors.clear();
      else state.errors.delete(action.sessionID);
      next = isBlocked(state) ? "blocked" : "working";
      break;
    case "idle":
      if (!isRootEvent) return undefined;
      next = isBlocked(state) ? "blocked" : "idle";
      break;
    case "deleted": {
      if (isRootEvent) {
        state.terminal = true;
        next = "done";
        break;
      }
      const removedBlockers = state.blockers.delete(action.sessionID);
      const removedError = state.errors.delete(action.sessionID);
      if (!removedBlockers && !removedError) return undefined;
      next = isBlocked(state) ? "blocked" : "working";
      break;
    }
    default:
      return undefined;
  }

  const bounded = boundedEvidence(evidence);
  if (
    next === state.lastState &&
    (next === "working" || bounded === state.lastEvidence)
  ) {
    return undefined;
  }
  state.lastEvidence = bounded;
  state.lastState = next;
  return { state: next, evidence: bounded };
}

function unwrapSession(response) {
  const value = response?.data ?? response;
  return value?.session ?? value;
}

function createRootResolver(client) {
  const sessions = new Map();

  function remember(info) {
    const id = text(info?.id);
    if (id) sessions.set(id, { id, parentID: text(info.parentID) });
  }

  async function load(id) {
    if (sessions.has(id)) return sessions.get(id);
    const info = unwrapSession(await client.session.get({ path: { id } }));
    if (!text(info?.id)) throw new Error(`OpenCode session not found: ${id}`);
    remember(info);
    return sessions.get(id);
  }

  async function root(id) {
    let current = id;
    const seen = new Set();
    while (current && !seen.has(current)) {
      seen.add(current);
      const info = await load(current);
      if (!info.parentID) return current;
      current = info.parentID;
    }
    throw new Error(`invalid OpenCode session ancestry for ${id}`);
  }

  return { remember, root };
}

function parseJSON(value) {
  if (typeof value !== "string" || value.trim() === "") {
    throw new Error("boomux returned empty JSON output");
  }
  const parsed = JSON.parse(value);
  if (!parsed || typeof parsed !== "object" || Array.isArray(parsed)) {
    throw new Error("boomux returned invalid JSON output");
  }
  return parsed;
}

async function readBounded(stream, limit, onOverflow) {
  if (!stream) return "";
  const reader = stream.getReader();
  const decoder = new TextDecoder();
  let size = 0;
  let result = "";
  while (true) {
    const { done, value } = await reader.read();
    if (done) break;
    size += value.byteLength;
    if (size > limit) {
      onOverflow();
      throw new Error("boomux output limit exceeded");
    }
    result += decoder.decode(value, { stream: true });
  }
  return result + decoder.decode();
}

function createProcessRunner(options = {}) {
  const spawn = options.spawn ?? globalThis.Bun?.spawn;
  const timeoutMs = options.timeoutMs ?? COMMAND_TIMEOUT_MS;
  const maxOutput = options.maxOutput ?? MAX_OUTPUT;
  if (typeof spawn !== "function") {
    return async () => {
      throw new Error("Bun.spawn is unavailable");
    };
  }

  return async (argv) => {
    const child = spawn(argv, {
      stdin: "ignore",
      stdout: "pipe",
      stderr: "pipe",
      shell: false,
    });
    let timedOut = false;
    const kill = () => child.kill?.();
    const timer = setTimeout(() => {
      timedOut = true;
      kill();
    }, timeoutMs);
    try {
      const [stdout, stderr, exitCode] = await Promise.all([
        readBounded(child.stdout, maxOutput, kill),
        readBounded(child.stderr, maxOutput, kill),
        child.exited,
      ]);
      if (timedOut) throw new Error("boomux command timed out");
      const parsed = parseJSON(stdout.trim() ? stdout : stderr);
      if (exitCode !== 0 || parsed.error) {
        const failure = new Error(
          boundedEvidence(
            parsed.error?.message ?? stderr ?? "boomux command failed",
          ),
        );
        failure.code = parsed.error?.code;
        throw failure;
      }
      return parsed;
    } finally {
      clearTimeout(timer);
    }
  };
}

function ensureArgv(shellID, runID, rootID, state, evidence) {
  return [
    "boomux",
    "agent",
    "ensure",
    "opencode",
    "--integration",
    "opencode",
    "--external-session-id",
    rootID,
    "--shell-id",
    shellID,
    "--run-id",
    runID,
    "--state",
    state,
    "--authority",
    "lifecycle-integration",
    "--evidence",
    boundedEvidence(evidence),
    "--confidence",
    "100",
    "--json",
  ];
}

function reportArgv(agentID, shellID, runID, state, evidence) {
  return [
    "boomux",
    "agent",
    "report",
    agentID,
    "--shell-id",
    shellID,
    "--run-id",
    runID,
    "--state",
    state,
    "--authority",
    "lifecycle-integration",
    "--evidence",
    boundedEvidence(evidence),
    "--confidence",
    "100",
    "--json",
  ];
}

function sharedReportArgv(generation, rootID, state, evidence) {
  return [
    "boomux",
    "opencode",
    "claim",
    "report",
    "--generation",
    generation,
    "--root-session-id",
    rootID,
    "--state",
    state,
    "--authority",
    "lifecycle-integration",
    "--evidence",
    boundedEvidence(evidence),
    "--confidence",
    "100",
    "--json",
  ];
}

function agentFromJSON(result) {
  return result?.data?.agent ?? result?.agent ?? result?.data;
}

function observationMatches(observation, derived) {
  return (
    observation?.state === derived.state &&
    observation?.authority === LIFECYCLE_AUTHORITY &&
    observation?.evidence === derived.evidence &&
    observation?.confidence === LIFECYCLE_CONFIDENCE
  );
}

function observationAlreadyWorking(observation, derived) {
  return (
    derived.state === "working" &&
    observation?.state === "working" &&
    observation?.authority === LIFECYCLE_AUTHORITY &&
    observation?.confidence === LIFECYCLE_CONFIDENCE
  );
}

function rateLimitedLogger(log, now = Date.now) {
  let last = -Infinity;
  let suppressed = 0;
  return (error) => {
    const time = now();
    if (time - last < LOG_INTERVAL_MS) {
      suppressed += 1;
      return;
    }
    const suffix = suppressed
      ? ` (${suppressed} similar errors suppressed)`
      : "";
    suppressed = 0;
    last = time;
    log(
      `[boomux-opencode] ${boundedEvidence(error?.message ?? error)}${suffix}`,
    );
  };
}

function createLifecycle({ client, env, run, log = console.error, now }) {
  const environmentShellID = text(env?.BOOMUX_SHELL_ID);
  const environmentRunID = text(env?.BOOMUX_RUN_ID);
  const generation = text(env?.BOOMUX_OPENCODE_SHARED_GENERATION);
  const standalone = Boolean(environmentShellID && environmentRunID);

  const resolver = createRootResolver(client);
  const roots = new Map();
  const reportError = rateLimitedLogger(log, now);
  let queue = Promise.resolve();

  function tracked(rootID) {
    let value = roots.get(rootID);
    if (!value) {
      value = {
        reducer: createReducerState(),
        disabled: false,
      };
      if (standalone) value.agentID = undefined;
      roots.set(rootID, value);
    }
    return value;
  }

  async function send(rootID, derived) {
    const item = tracked(rootID);
    if (item.disabled) return;
    try {
      if (!standalone) {
        await run(
          sharedReportArgv(
            generation,
            rootID,
            derived.state,
            derived.evidence,
          ),
        );
        return;
      }
      if (!item.agentID) {
        const result = await run(
          ensureArgv(
            environmentShellID,
            environmentRunID,
            rootID,
            derived.state,
            derived.evidence,
          ),
        );
        const agent = agentFromJSON(result);
        item.agentID = text(agent?.id);
        if (!item.agentID)
          throw new Error("boomux ensure response has no agent id");
        if (
          agent?.ended_at_ms != null ||
          agent?.observation?.state === "done"
        ) {
          item.disabled = true;
          item.reducer.terminal = true;
          return;
        }
        if (
          observationMatches(agent?.observation, derived) ||
          observationAlreadyWorking(agent?.observation, derived)
        ) {
          return;
        }
      }
      await run(
        reportArgv(
          item.agentID,
          environmentShellID,
          environmentRunID,
          derived.state,
          derived.evidence,
        ),
      );
    } catch (error) {
      if (error?.code === "run_changed") {
        item.agentID = undefined;
        if (standalone) item.disabled = true;
      }
      throw error;
    }
  }

  async function handle(event) {
    const info = event?.properties?.info;
    if (info) resolver.remember(info);
    const action = classifyEvent(event);
    if (!action) return;
    const rootID = await resolver.root(action.sessionID);
    const item = tracked(rootID);
    if (item.disabled) return;
    const previous = {
      blockers: new Map(
        [...item.reducer.blockers].map(([id, blockers]) => [
          id,
          new Set(blockers),
        ]),
      ),
      errors: new Set(item.reducer.errors),
      lastEvidence: item.reducer.lastEvidence,
      lastState: item.reducer.lastState,
      terminal: item.reducer.terminal,
    };
    const derived = reduce(item.reducer, action, action.sessionID === rootID);
    if (!derived) return;
    try {
      await send(rootID, derived);
    } catch (error) {
      if (!item.disabled) {
        item.reducer.blockers = previous.blockers;
        item.reducer.errors = previous.errors;
        item.reducer.lastEvidence = previous.lastEvidence;
        item.reducer.lastState = previous.lastState;
        item.reducer.terminal = previous.terminal;
      }
      throw error;
    }
  }

  function enqueue(event) {
    const pending = queue.then(() => handle(event));
    queue = pending.catch(reportError);
    return queue;
  }

  return { enqueue, resolver, roots };
}

function hookEvent(type, input) {
  return { type, properties: { ...input, sessionID: input?.sessionID } };
}

export async function BoomuxOpenCodePlugin({ client }) {
  const env = globalThis.process?.env ?? {};
  const standalone = text(env.BOOMUX_SHELL_ID) && text(env.BOOMUX_RUN_ID);
  const shared = text(env.BOOMUX_OPENCODE_SHARED_GENERATION);
  if (!standalone && !shared) return {};
  const lifecycle = createLifecycle({
    client,
    env,
    run: createProcessRunner(),
  });
  return {
    event: ({ event }) => lifecycle.enqueue(event),
    "chat.message": (input) =>
      lifecycle.enqueue(hookEvent("chat.message", input)),
    "tool.execute.before": (input) =>
      lifecycle.enqueue(hookEvent("tool.execute.before", input)),
    "tool.execute.after": (input) =>
      lifecycle.enqueue(hookEvent("tool.execute.after", input)),
    "experimental.session.compacting": (input) =>
      lifecycle.enqueue(hookEvent("experimental.session.compacting", input)),
  };
}

// OpenCode treats every ESM export as a plugin. Keep test seams on the sole
// plugin function so this asset remains directly auto-loadable.
BoomuxOpenCodePlugin.__internal = Object.freeze({
  COMMAND_TIMEOUT_MS,
  classifyEvent,
  createLifecycle,
  createProcessRunner,
  createReducerState,
  reduce,
  sharedReportArgv,
});
