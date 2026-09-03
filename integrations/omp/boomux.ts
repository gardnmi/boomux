import { spawn } from "node:child_process";

const MAX_EVIDENCE = 160;
const MAX_OUTPUT = 64 * 1024;
const COMMAND_TIMEOUT_MS = 1_000;
const LOG_INTERVAL_MS = 30_000;
const IDLE_DEBOUNCE_MS = 250;
const RETRY_GRACE_MS = 2500;
const LIFECYCLE_AUTHORITY = "lifecycle_integration";
const LIFECYCLE_CONFIDENCE = 100;
const DEFAULT_EVIDENCE = "Oh My Pi lifecycle event";

const retryableErrorPattern =
  /overloaded|provider.?returned.?error|rate.?limit|too many requests|429|500|502|503|504|service.?unavailable|server.?error|internal.?error|network.?error|connection.?error|connection.?refused|connection.?lost|websocket.?closed|websocket.?error|other side closed|fetch failed|upstream.?connect|reset before headers|socket hang up|ended without|http2 request did not get a response|timed? out|timeout|terminated|retry delay/i;

function text(value) {
  return typeof value === "string" && value.length > 0 ? value : undefined;
}

function absolutePath(value) {
  const path = text(value);
  return path?.startsWith("/") ? path : undefined;
}

function workingContextPaths(ctx, event) {
  const paths = new Set([absolutePath(ctx?.cwd)].filter(Boolean));
  const tool = text(event?.toolName ?? event?.tool)?.toLowerCase();
  const input = event?.input ?? event?.args;
  if (!tool || !input || typeof input !== "object" || Array.isArray(input)) {
    return [...paths];
  }
  let fields = [];
  if (["read", "write", "edit", "multiedit"].includes(tool)) {
    fields = ["filePath", "file_path", "path"];
  } else if (["grep", "find", "ls", "list"].includes(tool)) {
    fields = ["path"];
  } else if (["bash", "shell"].includes(tool)) {
    fields = ["workdir", "cwd"];
  }
  for (const field of fields) {
    const path = absolutePath(input[field]);
    if (path) paths.add(path);
  }
  return [...paths].slice(0, 8);
}

function boundedEvidence(value) {
  const clean = String(value ?? DEFAULT_EVIDENCE)
    .replace(/\s+/g, " ")
    .trim();
  return (clean || DEFAULT_EVIDENCE).slice(0, MAX_EVIDENCE);
}

function agentErrorEvidence(messages) {
  if (!Array.isArray(messages)) return undefined;
  for (let index = messages.length - 1; index >= 0; index -= 1) {
    const message = messages[index];
    if (message?.role !== "assistant") continue;
    if (message.stopReason !== "error") return undefined;
    return boundedEvidence(
      `Oh My Pi error: ${text(message.errorMessage) ?? "assistant request failed"}`,
    );
  }
  return undefined;
}

function createOutcomeTracker() {
  const errors = new Map();
  return {
    clear(sessionID) {
      if (text(sessionID)) errors.delete(sessionID);
    },
    record(sessionID, messages) {
      if (!text(sessionID)) return;
      const evidence = agentErrorEvidence(messages);
      if (evidence) errors.set(sessionID, evidence);
      else errors.delete(sessionID);
    },
    settled(sessionID) {
      const evidence = errors.get(sessionID);
      return evidence
        ? { state: "blocked", evidence }
        : { state: "idle", evidence: "Oh My Pi agent settled" };
    },
  };
}

function lastAssistantMessage(messages) {
  if (!Array.isArray(messages)) return undefined;
  for (let index = messages.length - 1; index >= 0; index -= 1) {
    const message = messages[index];
    if (message?.role === "assistant") return message;
  }
  return undefined;
}

function retryableErrorMessage(event) {
  const assistant = lastAssistantMessage(event?.messages);
  if (assistant?.stopReason !== "error") return undefined;
  const errorMessage = String(assistant.errorMessage ?? "");
  if (!retryableErrorPattern.test(errorMessage)) return undefined;
  return errorMessage || "retryable provider error";
}

function askEvidence(event) {
  const args = event?.args ?? event?.input;
  if (args && typeof args === "object" && !Array.isArray(args)) {
    return text(args.question) ?? text(args.prompt) ?? "Ask";
  }
  return "Ask";
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

function createProcessRunner(options = {}) {
  const spawnProcess = options.spawn ?? spawn;
  const timeoutMs = options.timeoutMs ?? COMMAND_TIMEOUT_MS;
  const maxOutput = options.maxOutput ?? MAX_OUTPUT;
  const killGraceMs = options.killGraceMs ?? 250;

  return (argv) =>
    new Promise((resolve, reject) => {
      const child = spawnProcess(argv[0], argv.slice(1), {
        stdio: ["ignore", "pipe", "pipe"],
        shell: false,
      });
      let stdout = "";
      let stderr = "";
      let size = 0;
      let settled = false;
      let failure;
      let timer;
      let killTimer;
      let abandonTimer;
      const finish = (callback) => {
        if (settled) return;
        settled = true;
        clearTimeout(timer);
        clearTimeout(killTimer);
        clearTimeout(abandonTimer);
        callback();
      };
      const terminate = (error) => {
        if (failure) return;
        failure = error;
        child.kill();
        killTimer = setTimeout(() => {
          child.kill("SIGKILL");
          abandonTimer = setTimeout(() => {
            child.stdout?.destroy();
            child.stderr?.destroy();
            finish(() => reject(failure));
          }, killGraceMs);
        }, killGraceMs);
      };
      const append = (chunk, target) => {
        if (failure) return;
        size += chunk.length;
        if (size > maxOutput) {
          terminate(new Error("boomux output limit exceeded"));
          return;
        }
        if (target === "stdout") stdout += chunk.toString();
        else stderr += chunk.toString();
      };
      child.stdout?.on("data", (chunk) => append(chunk, "stdout"));
      child.stderr?.on("data", (chunk) => append(chunk, "stderr"));
      child.on("error", (error) => finish(() => reject(error)));
      child.on("close", (code) =>
        finish(() => {
          if (failure) {
            reject(failure);
            return;
          }
          try {
            const parsed = parseJSON(stdout.trim() ? stdout : stderr);
            if (code !== 0 || parsed.error) {
              const error = new Error(
                boundedEvidence(
                  parsed.error?.message ?? stderr ?? "boomux command failed",
                ),
              );
              error.code = parsed.error?.code;
              reject(error);
              return;
            }
            resolve(parsed);
          } catch (error) {
            reject(error);
          }
        }),
      );
      timer = setTimeout(
        () => terminate(new Error("boomux command timed out")),
        timeoutMs,
      );
    });
}

function ensureArgv(shellID, runID, sessionID, state, evidence) {
  return [
    "boomux",
    "agent",
    "ensure",
    "omp",
    "--integration",
    "omp",
    "--external-session-id",
    sessionID,
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

function observeWorkingContextArgv(agentID, shellID, runID, path) {
  return [
    "boomux",
    "agent",
    "observe-working-context",
    agentID,
    path,
    "--shell-id",
    shellID,
    "--run-id",
    runID,
    "--json",
  ];
}

function agentFromJSON(result) {
  return result?.data?.agent ?? result?.agent ?? result?.data;
}

function observationMatches(observation, state, evidence) {
  return (
    observation?.state === state &&
    observation?.authority === LIFECYCLE_AUTHORITY &&
    observation?.evidence === evidence &&
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
    log(`[boomux-omp] ${boundedEvidence(error?.message ?? error)}${suffix}`);
  };
}

function createLifecycle({ env, run, log = console.error, now }) {
  const shellID = text(env?.BOOMUX_SHELL_ID);
  const runID = text(env?.BOOMUX_RUN_ID);
  if (!shellID || !runID) return undefined;

  const reportError = rateLimitedLogger(log, now);
  let queue = Promise.resolve();
  let sessionID;
  let agentID;
  let disabled = false;

  async function send(nextSessionID, state, rawEvidence) {
    if (nextSessionID !== sessionID) {
      sessionID = nextSessionID;
      agentID = undefined;
      disabled = false;
    }
    if (disabled) return;

    const evidence = boundedEvidence(rawEvidence);
    try {
      if (!agentID) {
        const result = await run(
          ensureArgv(shellID, runID, sessionID, state, evidence),
        );
        const agent = agentFromJSON(result);
        agentID = text(agent?.id);
        if (!agentID) throw new Error("boomux ensure response has no agent id");
        if (agent?.ended_at_ms != null || agent?.observation?.state === "done") {
          disabled = true;
          return;
        }
        if (observationMatches(agent?.observation, state, evidence)) return;
      }
      await run(reportArgv(agentID, shellID, runID, state, evidence));
    } catch (error) {
      if (error?.code === "run_changed") disabled = true;
      throw error;
    }
  }

  async function sendWithAttempts(nextSessionID, state, evidence, attempts) {
    let lastError;
    for (let attempt = 0; attempt < attempts; attempt += 1) {
      try {
        await send(nextSessionID, state, evidence);
        return;
      } catch (error) {
        lastError = error;
        if (disabled || error?.code === "run_changed") break;
      }
    }
    throw lastError;
  }

  async function observe(paths) {
    if (!agentID) return;
    for (const path of paths) {
      try {
        await run(observeWorkingContextArgv(agentID, shellID, runID, path));
      } catch (error) {
        reportError(error);
      }
    }
  }

  function enqueue(nextSessionID, state, evidence, attempts = 1, paths = []) {
    if (!text(nextSessionID)) return Promise.resolve();
    const pending = queue.then(async () => {
      await sendWithAttempts(nextSessionID, state, evidence, attempts);
      await observe(paths);
    });
    queue = pending.catch(reportError);
    return queue;
  }

  function enqueueContexts(nextSessionID, paths) {
    if (!text(nextSessionID)) return Promise.resolve();
    const pending = queue.then(async () => {
      if (nextSessionID === sessionID) await observe(paths);
    });
    queue = pending.catch(reportError);
    return queue;
  }

  return { enqueue, enqueueContexts };
}

function currentSessionID(ctx) {
  return text(ctx?.sessionManager?.getSessionId?.());
}

function registerLifecycleHandlers(pi, lifecycle, options = {}) {
  const outcomes = createOutcomeTracker();
  const schedule = options.setTimeout ?? setTimeout;
  const unschedule = options.clearTimeout ?? clearTimeout;
  const idleDebounceMs = options.idleDebounceMs ?? IDLE_DEBOUNCE_MS;
  const retryGraceMs = options.retryGraceMs ?? RETRY_GRACE_MS;
  const reportError = rateLimitedLogger(
    options.log ?? console.error,
    options.now,
  );

  let rootSession = false;
  let agentActive = false;
  let retryHoldActive = false;
  let failureBlocked = false;
  let failureEvidence;
  let blockedCount = 0;
  let blockedEvidence;
  let idleEvidence = "Oh My Pi session idle";
  let idleTimer;
  let retryTimer;

  const report = (ctx, state, evidence, attempts = 1) =>
    lifecycle.enqueue(
      currentSessionID(ctx),
      state,
      evidence,
      attempts,
      workingContextPaths(ctx),
    );

  function arm(fn, ms) {
    const handle = schedule(fn, ms);
    handle?.unref?.();
    return handle;
  }

  function clearPendingTimers() {
    if (idleTimer) unschedule(idleTimer);
    if (retryTimer) unschedule(retryTimer);
    idleTimer = undefined;
    retryTimer = undefined;
  }

  function clearFailureState() {
    retryHoldActive = false;
    failureBlocked = false;
    failureEvidence = undefined;
  }

  function resetSessionState() {
    clearPendingTimers();
    clearFailureState();
    agentActive = false;
    blockedCount = 0;
    blockedEvidence = undefined;
    idleEvidence = "Oh My Pi session idle";
  }

  function desired() {
    if (blockedCount > 0) {
      return { state: "blocked", evidence: blockedEvidence };
    }
    if (failureBlocked) {
      return { state: "blocked", evidence: failureEvidence };
    }
    if (agentActive || retryHoldActive) {
      return {
        state: "working",
        evidence: retryHoldActive
          ? "Oh My Pi retrying"
          : "Oh My Pi agent working",
      };
    }
    return { state: "idle", evidence: idleEvidence };
  }

  function publish(ctx, attempts = 1) {
    const next = desired();
    return report(ctx, next.state, next.evidence, attempts);
  }

  function activateRoot(ctx) {
    if (ctx?.hasUI !== true) return false;
    rootSession = true;
    return true;
  }

  function requireRoot(ctx) {
    return rootSession || activateRoot(ctx);
  }

  function holdForRetry(ctx, message) {
    clearPendingTimers();
    retryHoldActive = true;
    failureBlocked = false;
    failureEvidence = boundedEvidence(message);
    publish(ctx);
    retryTimer = arm(() => {
      retryTimer = undefined;
      retryHoldActive = false;
      failureBlocked = true;
      publish(ctx);
    }, retryGraceMs);
  }

  function scheduleIdle(ctx) {
    clearPendingTimers();
    clearFailureState();
    idleTimer = arm(() => {
      idleTimer = undefined;
      const outcome = outcomes.settled(currentSessionID(ctx));
      if (outcome.state === "blocked") {
        failureBlocked = true;
        failureEvidence = outcome.evidence;
      } else {
        idleEvidence = outcome.evidence;
      }
      publish(ctx);
    }, idleDebounceMs);
  }

  function settleNow(ctx) {
    if (retryHoldActive) return;
    clearPendingTimers();
    retryHoldActive = false;
    agentActive = false;
    const outcome = outcomes.settled(currentSessionID(ctx));
    if (outcome.state === "blocked") {
      failureBlocked = true;
      failureEvidence = outcome.evidence;
    } else {
      clearFailureState();
      idleEvidence = outcome.evidence;
    }
    publish(ctx);
  }

  function guard(handler) {
    return (event, ctx) => {
      try {
        return handler(event, ctx);
      } catch (error) {
        reportError(error);
      }
    };
  }

  pi.on(
    "session_start",
    guard((_event, ctx) => {
      if (!activateRoot(ctx)) return;
      outcomes.clear(currentSessionID(ctx));
      clearPendingTimers();
      clearFailureState();
      blockedCount = 0;
      blockedEvidence = undefined;
      idleEvidence = "Oh My Pi session idle";
      agentActive = ctx?.isIdle?.() === false;
      return publish(ctx);
    }),
  );
  pi.on(
    "session_switch",
    guard((_event, ctx) => {
      if (!activateRoot(ctx)) return;
      outcomes.clear(currentSessionID(ctx));
      resetSessionState();
      return publish(ctx);
    }),
  );
  pi.on(
    "agent_start",
    guard((_event, ctx) => {
      if (!requireRoot(ctx)) return;
      outcomes.clear(currentSessionID(ctx));
      clearPendingTimers();
      clearFailureState();
      agentActive = true;
      return publish(ctx);
    }),
  );
  pi.on(
    "tool_approval_requested",
    guard((event, ctx) => {
      if (!requireRoot(ctx)) return;
      blockedCount += 1;
      blockedEvidence =
        event?.reason || `${event?.toolName || "Tool"} approval`;
      return publish(ctx);
    }),
  );
  pi.on(
    "tool_approval_resolved",
    guard((_event, ctx) => {
      if (!requireRoot(ctx)) return;
      blockedCount = Math.max(0, blockedCount - 1);
      if (blockedCount === 0) blockedEvidence = undefined;
      return publish(ctx);
    }),
  );
  pi.on(
    "tool_execution_start",
    guard((event, ctx) => {
      if (event?.toolName !== "ask") return;
      if (!requireRoot(ctx)) return;
      blockedCount += 1;
      blockedEvidence = askEvidence(event);
      return publish(ctx);
    }),
  );
  pi.on(
    "tool_execution_end",
    guard((event, ctx) => {
      if (event?.toolName !== "ask") return;
      if (!requireRoot(ctx)) return;
      blockedCount = Math.max(0, blockedCount - 1);
      if (blockedCount === 0) blockedEvidence = undefined;
      return publish(ctx);
    }),
  );
  pi.on(
    "agent_end",
    guard((event, ctx) => {
      if (!rootSession) return;
      if (!agentActive) return;
      agentActive = false;
      outcomes.record(currentSessionID(ctx), event?.messages);
      const retryable = retryableErrorMessage(event);
      if (retryable) {
        holdForRetry(ctx, retryable);
        return;
      }
      scheduleIdle(ctx);
    }),
  );
  pi.on(
    "agent_settled",
    guard((_event, ctx) => {
      if (!rootSession) return;
      settleNow(ctx);
    }),
  );
  pi.on(
    "session_shutdown",
    guard((_event, ctx) => {
      clearPendingTimers();
      outcomes.clear(currentSessionID(ctx));
      rootSession = false;
      resetSessionState();
      return report(ctx, "inactive", "Oh My Pi session inactive", 2);
    }),
  );
}

export default function BoomuxOmpExtension(pi) {
  const lifecycle = createLifecycle({
    env: globalThis.process?.env ?? {},
    run: createProcessRunner(),
  });
  if (!lifecycle) return;
  registerLifecycleHandlers(pi, lifecycle);
}

export const __internal = Object.freeze({
  boundedEvidence,
  createLifecycle,
  createOutcomeTracker,
  createProcessRunner,
  registerLifecycleHandlers,
  observeWorkingContextArgv,
  workingContextPaths,
  retryableErrorPattern,
  retryableErrorMessage,
  askEvidence,
  IDLE_DEBOUNCE_MS,
  RETRY_GRACE_MS,
});
