import { spawn } from "node:child_process";

const MAX_EVIDENCE = 160;
const MAX_OUTPUT = 64 * 1024;
const COMMAND_TIMEOUT_MS = 1_000;
const LOG_INTERVAL_MS = 30_000;
const LIFECYCLE_AUTHORITY = "lifecycle_integration";
const LIFECYCLE_CONFIDENCE = 100;

function text(value) {
  return typeof value === "string" && value.length > 0 ? value : undefined;
}

function boundedEvidence(value) {
  const clean = String(value ?? "Pi lifecycle event")
    .replace(/\s+/g, " ")
    .trim();
  return (clean || "Pi lifecycle event").slice(0, MAX_EVIDENCE);
}

function agentErrorEvidence(messages) {
  if (!Array.isArray(messages)) return undefined;
  for (let index = messages.length - 1; index >= 0; index -= 1) {
    const message = messages[index];
    if (message?.role !== "assistant") continue;
    if (message.stopReason !== "error") return undefined;
    return boundedEvidence(
      `Pi error: ${text(message.errorMessage) ?? "assistant request failed"}`,
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
        : { state: "idle", evidence: "Pi agent settled" };
    },
  };
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
    "pi",
    "--integration",
    "pi",
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
    log(`[boomux-pi] ${boundedEvidence(error?.message ?? error)}${suffix}`);
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

  function enqueue(nextSessionID, state, evidence, attempts = 1) {
    if (!text(nextSessionID)) return Promise.resolve();
    const pending = queue.then(() =>
      sendWithAttempts(nextSessionID, state, evidence, attempts),
    );
    queue = pending.catch(reportError);
    return queue;
  }

  return { enqueue };
}

function currentSessionID(ctx) {
  return text(ctx?.sessionManager?.getSessionId?.());
}

function registerLifecycleHandlers(pi, lifecycle) {
  const outcomes = createOutcomeTracker();

  const report = (ctx, state, evidence) =>
    lifecycle.enqueue(currentSessionID(ctx), state, evidence);

  pi.on("session_start", (_event, ctx) => {
    outcomes.clear(currentSessionID(ctx));
    return report(ctx, "idle", "Pi session idle");
  });
  pi.on("agent_start", (_event, ctx) => {
    outcomes.clear(currentSessionID(ctx));
    return report(ctx, "working", "Pi agent working");
  });
  pi.on("agent_end", (event, ctx) => {
    outcomes.record(currentSessionID(ctx), event?.messages);
  });
  pi.on("agent_settled", (_event, ctx) => {
    const outcome = outcomes.settled(currentSessionID(ctx));
    return report(ctx, outcome.state, outcome.evidence);
  });
  pi.on("session_shutdown", (_event, ctx) => {
    outcomes.clear(currentSessionID(ctx));
    return lifecycle.enqueue(
      currentSessionID(ctx),
      "inactive",
      "Pi session inactive",
      2,
    );
  });
}

export default function BoomuxPiExtension(pi) {
  const lifecycle = createLifecycle({
    env: globalThis.process?.env ?? {},
    run: createProcessRunner(),
  });
  if (!lifecycle) return;
  registerLifecycleHandlers(pi, lifecycle);
}

export const __internal = Object.freeze({
  agentErrorEvidence,
  boundedEvidence,
  createLifecycle,
  createOutcomeTracker,
  createProcessRunner,
  ensureArgv,
  reportArgv,
  registerLifecycleHandlers,
});
