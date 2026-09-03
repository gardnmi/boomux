import { describe, expect, test } from "bun:test";
import { EventEmitter } from "node:events";
import { PassThrough } from "node:stream";

import BoomuxOmpExtension, { __internal } from "./boomux.ts";

const HANDLER_KEYS = [
  "session_start",
  "session_switch",
  "agent_start",
  "tool_approval_requested",
  "tool_approval_resolved",
  "tool_execution_start",
  "tool_execution_end",
  "agent_end",
  "agent_settled",
  "session_shutdown",
];

function agent(id, state, evidence) {
  return {
    data: {
      agent: {
        id,
        ended_at_ms: null,
        observation: {
          state,
          authority: "lifecycle_integration",
          evidence,
          confidence: 100,
        },
      },
    },
  };
}

function context(sessionID, extras = {}) {
  return {
    hasUI: Object.hasOwn(extras, "hasUI") ? extras.hasUI : true,
    cwd: extras.cwd,
    isIdle: extras.isIdle,
    sessionManager: {
      getSessionId: () => sessionID,
    },
  };
}

function withBoomuxEnv(env, fn) {
  const previousShell = process.env.BOOMUX_SHELL_ID;
  const previousRun = process.env.BOOMUX_RUN_ID;
  try {
    if (env.BOOMUX_SHELL_ID === undefined) delete process.env.BOOMUX_SHELL_ID;
    else process.env.BOOMUX_SHELL_ID = env.BOOMUX_SHELL_ID;
    if (env.BOOMUX_RUN_ID === undefined) delete process.env.BOOMUX_RUN_ID;
    else process.env.BOOMUX_RUN_ID = env.BOOMUX_RUN_ID;
    return fn();
  } finally {
    if (previousShell === undefined) delete process.env.BOOMUX_SHELL_ID;
    else process.env.BOOMUX_SHELL_ID = previousShell;
    if (previousRun === undefined) delete process.env.BOOMUX_RUN_ID;
    else process.env.BOOMUX_RUN_ID = previousRun;
  }
}

function createClock() {
  let now = 0;
  let nextId = 1;
  const timers = new Map();
  return {
    now: () => now,
    setTimeout(fn, ms) {
      const id = nextId++;
      timers.set(id, { fn, due: now + ms });
      return id;
    },
    clearTimeout(id) {
      timers.delete(id);
    },
    advance(ms) {
      now += ms;
      const due = [...timers.entries()].filter(([, timer]) => timer.due <= now);
      for (const [id, timer] of due) {
        timers.delete(id);
        timer.fn();
      }
    },
  };
}

function installHandlers(lifecycle, options = {}) {
  const handlers = new Map();
  const clock = options.clock ?? createClock();
  __internal.registerLifecycleHandlers(
    { on: (event, handler) => handlers.set(event, handler) },
    lifecycle,
    {
      setTimeout: clock.setTimeout,
      clearTimeout: clock.clearTimeout,
      idleDebounceMs: options.idleDebounceMs ?? 25,
      retryGraceMs: options.retryGraceMs ?? 50,
      log: options.log ?? (() => {}),
    },
  );
  return { handlers, clock };
}

function recordingLifecycle() {
  const calls = [];
  return {
    calls,
    enqueue: async (...values) => {
      calls.push(values);
    },
    enqueueContexts: async () => {},
  };
}

function states(calls) {
  return calls.map((argv) => argv[argv.indexOf("--state") + 1]);
}

describe("OMP lifecycle", () => {
  test("registers no handlers unless both Boomux env vars are set", () => {
    const cases = [
      {},
      { BOOMUX_SHELL_ID: "shell-1" },
      { BOOMUX_RUN_ID: "run-1" },
    ];
    for (const env of cases) {
      const handlers = new Map();
      withBoomuxEnv(env, () => {
        BoomuxOmpExtension({
          on: (event, handler) => handlers.set(event, handler),
        });
      });
      expect([...handlers.keys()]).toEqual([]);
    }
  });

  test("registers the Task 3 lifecycle handler set", () => {
    const handlers = new Map();
    withBoomuxEnv(
      { BOOMUX_SHELL_ID: "shell-1", BOOMUX_RUN_ID: "run-1" },
      () => {
        BoomuxOmpExtension({
          on: (event, handler) => handlers.set(event, handler),
        });
      },
    );
    expect([...handlers.keys()]).toEqual(HANDLER_KEYS);
  });

  test("first CLI call ensures omp with the external session id", async () => {
    const calls = [];
    const lifecycle = __internal.createLifecycle({
      env: { BOOMUX_SHELL_ID: "shell-1", BOOMUX_RUN_ID: "run-1" },
      run: async (argv) => {
        calls.push(argv);
        return argv[2] === "ensure"
          ? agent(
              "agent-1",
              argv[argv.indexOf("--state") + 1],
              argv[argv.indexOf("--evidence") + 1],
            )
          : { data: { ok: true } };
      },
      log: () => {},
    });

    await lifecycle.enqueue("session-1", "idle", "Oh My Pi session idle");

    expect(calls[0].slice(0, 8)).toEqual([
      "boomux",
      "agent",
      "ensure",
      "omp",
      "--integration",
      "omp",
      "--external-session-id",
      "session-1",
    ]);
  });

  test("reports idle working settled and inactive states", async () => {
    const calls = [];
    const lifecycle = __internal.createLifecycle({
      env: { BOOMUX_SHELL_ID: "shell-1", BOOMUX_RUN_ID: "run-1" },
      run: async (argv) => {
        calls.push(argv);
        return argv[2] === "ensure"
          ? agent(
              "agent-1",
              argv[argv.indexOf("--state") + 1],
              argv[argv.indexOf("--evidence") + 1],
            )
          : { data: { ok: true } };
      },
      log: () => {},
    });

    await lifecycle.enqueue("session-1", "idle", "Oh My Pi session idle");
    await lifecycle.enqueue("session-1", "working", "Oh My Pi agent working");
    await lifecycle.enqueue("session-1", "idle", "Oh My Pi agent settled");
    await lifecycle.enqueue("session-1", "inactive", "Oh My Pi session inactive");

    expect(states(calls.slice(1))).toEqual(["working", "idle", "inactive"]);
    expect(states(calls).includes("done")).toBe(false);
    expect(calls.every((argv) => argv.includes("--json"))).toBe(true);
  });

  test("session_start without hasUI does not report", async () => {
    const lifecycle = recordingLifecycle();
    const { handlers } = installHandlers(lifecycle);

    await handlers.get("session_start")(
      { type: "session_start" },
      context("session-1", { hasUI: false }),
    );
    await handlers.get("session_start")(
      { type: "session_start" },
      context("session-1", { hasUI: undefined }),
    );

    expect(lifecycle.calls).toEqual([]);
  });

  test("maps idle working idle inactive through handlers without done", async () => {
    const lifecycle = recordingLifecycle();
    const { handlers } = installHandlers(lifecycle);
    const ctx = context("session-1");

    await handlers.get("session_start")({ type: "session_start" }, ctx);
    await handlers.get("agent_start")({ type: "agent_start" }, ctx);
    handlers.get("agent_end")(
      { type: "agent_end", messages: [{ role: "assistant", stopReason: "stop" }] },
      ctx,
    );
    await handlers.get("agent_settled")({ type: "agent_settled" }, ctx);
    await handlers.get("session_shutdown")({ type: "session_shutdown" }, ctx);

    expect(lifecycle.calls.map((call) => call[1])).toEqual([
      "idle",
      "working",
      "idle",
      "inactive",
    ]);
    expect(lifecycle.calls.at(-1).slice(0, 4)).toEqual([
      "session-1",
      "inactive",
      "Oh My Pi session inactive",
      2,
    ]);
    expect(lifecycle.calls.some((call) => call[1] === "done")).toBe(false);
  });

  test("blocks on tool approval then restores the prior state", async () => {
    const lifecycle = recordingLifecycle();
    const { handlers } = installHandlers(lifecycle);
    const ctx = context("session-1");

    await handlers.get("session_start")({ type: "session_start" }, ctx);
    await handlers.get("agent_start")({ type: "agent_start" }, ctx);
    await handlers.get("tool_approval_requested")(
      { type: "tool_approval_requested", toolName: "bash", reason: "needs approval" },
      ctx,
    );
    await handlers.get("tool_approval_resolved")(
      { type: "tool_approval_resolved" },
      ctx,
    );

    expect(lifecycle.calls.map((call) => [call[1], call[2]])).toEqual([
      ["idle", "Oh My Pi session idle"],
      ["working", "Oh My Pi agent working"],
      ["blocked", "needs approval"],
      ["working", "Oh My Pi agent working"],
    ]);
  });

  test("retryable agent_end stays working then becomes blocked", async () => {
    const lifecycle = recordingLifecycle();
    const { handlers, clock } = installHandlers(lifecycle, {
      idleDebounceMs: 25,
      retryGraceMs: 50,
    });
    const ctx = context("session-1");

    await handlers.get("session_start")({ type: "session_start" }, ctx);
    await handlers.get("agent_start")({ type: "agent_start" }, ctx);
    handlers.get("agent_end")(
      {
        type: "agent_end",
        messages: [
          {
            role: "assistant",
            stopReason: "error",
            errorMessage: "429 rate limit",
          },
        ],
      },
      ctx,
    );

    expect(lifecycle.calls.at(-1).slice(1, 3)).toEqual([
      "working",
      "Oh My Pi retrying",
    ]);

    clock.advance(50);

    expect(lifecycle.calls.at(-1).slice(1, 3)).toEqual([
      "blocked",
      "429 rate limit",
    ]);
    expect(lifecycle.calls.some((call) => call[1] === "done")).toBe(false);
  });

  test("agent_settled prefers idle over the debounce when there is no retry hold", async () => {
    const lifecycle = recordingLifecycle();
    const { handlers, clock } = installHandlers(lifecycle, {
      idleDebounceMs: 25,
      retryGraceMs: 50,
    });
    const ctx = context("session-1");

    await handlers.get("session_start")({ type: "session_start" }, ctx);
    await handlers.get("agent_start")({ type: "agent_start" }, ctx);
    handlers.get("agent_end")(
      { type: "agent_end", messages: [{ role: "assistant", stopReason: "stop" }] },
      ctx,
    );

    expect(lifecycle.calls.map((call) => call[1])).toEqual(["idle", "working"]);

    await handlers.get("agent_settled")({ type: "agent_settled" }, ctx);
    expect(lifecycle.calls.map((call) => call[1])).toEqual([
      "idle",
      "working",
      "idle",
    ]);
    expect(lifecycle.calls.at(-1)[2]).toBe("Oh My Pi agent settled");

    clock.advance(25);
    expect(lifecycle.calls).toHaveLength(3);
  });

  test("agent_settled during retry hold does not cancel the retrying window", async () => {
    const lifecycle = recordingLifecycle();
    const { handlers, clock } = installHandlers(lifecycle, {
      idleDebounceMs: 25,
      retryGraceMs: 50,
    });
    const ctx = context("session-1");

    await handlers.get("session_start")({ type: "session_start" }, ctx);
    await handlers.get("agent_start")({ type: "agent_start" }, ctx);
    handlers.get("agent_end")(
      {
        type: "agent_end",
        messages: [
          {
            role: "assistant",
            stopReason: "error",
            errorMessage: "provider returned error",
          },
        ],
      },
      ctx,
    );
    await handlers.get("agent_settled")({ type: "agent_settled" }, ctx);

    expect(lifecycle.calls.at(-1).slice(1, 3)).toEqual([
      "working",
      "Oh My Pi retrying",
    ]);

    clock.advance(25);
    expect(lifecycle.calls.at(-1).slice(1, 3)).toEqual([
      "working",
      "Oh My Pi retrying",
    ]);

    clock.advance(25);
    expect(lifecycle.calls.at(-1).slice(1, 3)).toEqual([
      "blocked",
      "provider returned error",
    ]);
  });

  test("run_changed disables further reports", async () => {
    const calls = [];
    const lifecycle = __internal.createLifecycle({
      env: { BOOMUX_SHELL_ID: "shell-1", BOOMUX_RUN_ID: "run-1" },
      run: async (argv) => {
        calls.push(argv);
        if (argv[2] === "ensure") {
          return agent(
            "agent-1",
            argv[argv.indexOf("--state") + 1],
            argv[argv.indexOf("--evidence") + 1],
          );
        }
        const error = new Error("run changed");
        error.code = "run_changed";
        throw error;
      },
      log: () => {},
    });

    await lifecycle.enqueue("session-1", "idle", "Oh My Pi session idle");
    await lifecycle.enqueue("session-1", "working", "Oh My Pi agent working");
    await lifecycle.enqueue("session-1", "idle", "Oh My Pi agent settled");

    expect(calls.map((argv) => argv[2])).toEqual(["ensure", "report"]);
  });

  test("fail-open logger does not throw", async () => {
    const logs = [];
    const lifecycle = __internal.createLifecycle({
      env: { BOOMUX_SHELL_ID: "shell-1", BOOMUX_RUN_ID: "run-1" },
      run: async () => {
        throw new Error("cli exploded");
      },
      log: (message) => logs.push(message),
    });

    await lifecycle.enqueue("session-1", "idle", "Oh My Pi session idle");

    expect(logs.some((message) => String(message).includes("[boomux-omp]"))).toBe(
      true,
    );

    const recording = recordingLifecycle();
    recording.enqueue = () => {
      throw new Error("enqueue failed");
    };
    const { handlers } = installHandlers(recording, {
      log: (message) => logs.push(message),
    });
    await handlers.get("session_start")(
      { type: "session_start" },
      context("session-1"),
    );
  });

  test("bounds lifecycle evidence", () => {
    expect(__internal.boundedEvidence(`  ${"x".repeat(200)}  `)).toHaveLength(160);
  });

  test("reports only final assistant errors as blocked", () => {
    const outcomes = __internal.createOutcomeTracker();
    outcomes.record("session-1", [
      { role: "toolResult", isError: true },
      {
        role: "assistant",
        stopReason: "error",
        errorMessage: "provider unavailable",
      },
    ]);
    expect(outcomes.settled("session-1")).toEqual({
      state: "blocked",
      evidence: "Oh My Pi error: provider unavailable",
    });

    outcomes.clear("session-1");
    outcomes.record("session-1", [
      {
        role: "assistant",
        stopReason: "error",
        errorMessage: "recovered",
      },
      { role: "assistant", stopReason: "stop" },
    ]);
    expect(outcomes.settled("session-1")).toEqual({
      state: "idle",
      evidence: "Oh My Pi agent settled",
    });
  });

  test("observes cwd and typed tool paths after lifecycle identity exists", async () => {
    const calls = [];
    const lifecycle = __internal.createLifecycle({
      env: { BOOMUX_SHELL_ID: "shell-1", BOOMUX_RUN_ID: "run-1" },
      run: async (argv) => {
        calls.push(argv);
        return argv[2] === "ensure"
          ? agent("agent-1", "working", "Oh My Pi agent working")
          : { data: { ok: true } };
      },
      log: () => {},
    });
    await lifecycle.enqueue(
      "session-1",
      "working",
      "Oh My Pi agent working",
      1,
      ["/worktrees/boomux"],
    );
    await lifecycle.enqueueContexts("session-1", ["/worktrees/omarchy"]);

    expect(calls.map((argv) => argv[2])).toEqual([
      "ensure",
      "observe-working-context",
      "observe-working-context",
    ]);
    expect(
      __internal.workingContextPaths(
        { cwd: "/worktrees/boomux" },
        {
          toolName: "read",
          input: { path: "/worktrees/omarchy/Panel.qml", command: "ignored" },
        },
      ),
    ).toEqual(["/worktrees/boomux", "/worktrees/omarchy/Panel.qml"]);
  });

  test("ensures a new identity when OMP switches sessions", async () => {
    const calls = [];
    let next = 0;
    const lifecycle = __internal.createLifecycle({
      env: { BOOMUX_SHELL_ID: "shell-1", BOOMUX_RUN_ID: "run-1" },
      run: async (argv) => {
        calls.push(argv);
        next += 1;
        return agent(`agent-${next}`, "idle", "Oh My Pi session idle");
      },
      log: () => {},
    });

    await lifecycle.enqueue("session-1", "idle", "Oh My Pi session idle");
    await lifecycle.enqueue("session-2", "idle", "Oh My Pi session idle");

    expect(calls).toHaveLength(2);
    expect(calls.map((argv) => argv[7])).toEqual(["session-1", "session-2"]);
  });

  test("retries an inactive report without creating another identity", async () => {
    const calls = [];
    let inactiveAttempts = 0;
    const lifecycle = __internal.createLifecycle({
      env: { BOOMUX_SHELL_ID: "shell-1", BOOMUX_RUN_ID: "run-1" },
      run: async (argv) => {
        calls.push(argv);
        if (argv[2] === "ensure") {
          return agent("agent-1", "idle", "Oh My Pi session idle");
        }
        if (argv[argv.indexOf("--state") + 1] === "inactive") {
          inactiveAttempts += 1;
          if (inactiveAttempts === 1) throw new Error("temporary failure");
        }
        return { data: { ok: true } };
      },
      log: () => {},
    });

    await lifecycle.enqueue("session-1", "idle", "Oh My Pi session idle");
    await lifecycle.enqueue(
      "session-1",
      "inactive",
      "Oh My Pi session inactive",
      2,
    );

    expect(inactiveAttempts).toBe(2);
    expect(calls.filter((argv) => argv[2] === "ensure")).toHaveLength(1);
  });

  test("waits for a timed out process to close before settling", async () => {
    const child = new EventEmitter();
    child.stdout = new PassThrough();
    child.stderr = new PassThrough();
    const signals = [];
    child.kill = (signal) => {
      signals.push(signal ?? "SIGTERM");
      return true;
    };
    const runner = __internal.createProcessRunner({
      spawn: () => child,
      timeoutMs: 5,
      killGraceMs: 5,
    });
    let settled = false;
    const outcome = runner(["boomux"]).then(
      () => undefined,
      (error) => error,
    );
    outcome.then(() => {
      settled = true;
    });

    await Bun.sleep(8);
    expect(settled).toBe(false);
    expect(signals).toEqual(["SIGTERM"]);

    child.emit("close", null);
    expect((await outcome).message).toBe("boomux command timed out");
    expect(settled).toBe(true);
  });

  test("settles after escalating a timed out process that never closes", async () => {
    const child = new EventEmitter();
    child.stdout = new PassThrough();
    child.stderr = new PassThrough();
    const signals = [];
    child.kill = (signal) => {
      signals.push(signal ?? "SIGTERM");
      return true;
    };
    const runner = __internal.createProcessRunner({
      spawn: () => child,
      timeoutMs: 5,
      killGraceMs: 5,
    });

    const outcome = await runner(["boomux"]).then(
      () => undefined,
      (error) => error,
    );

    expect(outcome.message).toBe("boomux command timed out");
    expect(signals).toEqual(["SIGTERM", "SIGKILL"]);
    expect(child.stdout.destroyed).toBe(true);
    expect(child.stderr.destroyed).toBe(true);
  });
});
