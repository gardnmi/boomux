import { describe, expect, test } from "bun:test";
import { EventEmitter } from "node:events";
import { PassThrough } from "node:stream";

import BoomuxPiExtension, { __internal } from "./boomux.js";

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

function context(sessionID) {
  return {
    sessionManager: {
      getSessionId: () => sessionID,
    },
  };
}

describe("Pi lifecycle", () => {
  test("registers exact lifecycle hooks", () => {
    const handlers = new Map();
    const previous = process.env.BOOMUX_SHELL_ID;
    const previousRun = process.env.BOOMUX_RUN_ID;
    try {
      delete process.env.BOOMUX_SHELL_ID;
      delete process.env.BOOMUX_RUN_ID;
      BoomuxPiExtension({ on: (event, handler) => handlers.set(event, handler) });
      expect([...handlers.keys()]).toEqual([]);

      process.env.BOOMUX_SHELL_ID = "shell-1";
      process.env.BOOMUX_RUN_ID = "run-1";
      BoomuxPiExtension({ on: (event, handler) => handlers.set(event, handler) });
    } finally {
      if (previous === undefined) delete process.env.BOOMUX_SHELL_ID;
      else process.env.BOOMUX_SHELL_ID = previous;
      if (previousRun === undefined) delete process.env.BOOMUX_RUN_ID;
      else process.env.BOOMUX_RUN_ID = previousRun;
    }
    expect([...handlers.keys()]).toEqual([
      "session_start",
      "agent_start",
      "agent_settled",
      "session_shutdown",
    ]);
  });

  test("reports idle working settled and inactive states", async () => {
    const calls = [];
    const lifecycle = __internal.createLifecycle({
      env: { BOOMUX_SHELL_ID: "shell-1", BOOMUX_RUN_ID: "run-1" },
      run: async (argv) => {
        calls.push(argv);
        return argv[2] === "ensure"
          ? agent("agent-1", argv[argv.indexOf("--state") + 1], argv[argv.indexOf("--evidence") + 1])
          : { data: { ok: true } };
      },
      log: () => {},
    });

    await lifecycle.enqueue("session-1", "idle", "Pi session idle");
    await lifecycle.enqueue("session-1", "working", "Pi agent working");
    await lifecycle.enqueue("session-1", "idle", "Pi agent settled");
    await lifecycle.enqueue("session-1", "inactive", "Pi session inactive");

    expect(calls[0].slice(0, 8)).toEqual([
      "boomux",
      "agent",
      "ensure",
      "pi",
      "--integration",
      "pi",
      "--external-session-id",
      "session-1",
    ]);
    expect(calls.slice(1).map((argv) => argv[argv.indexOf("--state") + 1])).toEqual([
      "working",
      "idle",
      "inactive",
    ]);
    expect(calls.every((argv) => argv.includes("--json"))).toBe(true);
  });

  test("ensures a new identity when Pi switches sessions", async () => {
    const calls = [];
    let next = 0;
    const lifecycle = __internal.createLifecycle({
      env: { BOOMUX_SHELL_ID: "shell-1", BOOMUX_RUN_ID: "run-1" },
      run: async (argv) => {
        calls.push(argv);
        next += 1;
        return agent(`agent-${next}`, "idle", "Pi session idle");
      },
      log: () => {},
    });

    await lifecycle.enqueue("session-1", "idle", "Pi session idle");
    await lifecycle.enqueue("session-2", "idle", "Pi session idle");

    expect(calls).toHaveLength(2);
    expect(calls.map((argv) => argv[7])).toEqual(["session-1", "session-2"]);
  });

  test("bounds lifecycle evidence", () => {
    expect(__internal.boundedEvidence(`  ${"x".repeat(200)}  `)).toHaveLength(160);
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
