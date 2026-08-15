import { describe, expect, test } from "bun:test";
import { BoomuxOpenCodePlugin } from "./boomux.js";

const {
  COMMAND_TIMEOUT_MS,
  classifyEvent,
  createLifecycle,
  createProcessRunner,
  createReducerState,
  reduce,
} = BoomuxOpenCodePlugin.__internal;

const env = {
  BOOMUX_SHELL_ID: "shell;not-a-command",
  BOOMUX_RUN_ID: "run $(false)",
};

function event(type, properties) {
  return { type, properties };
}

function successfulEnsure(
  id = "agent-1",
  state = "working",
  evidence = "OpenCode session busy",
) {
  return {
    data: {
      agent: {
        id,
        observation: {
          state,
          authority: "lifecycle_integration",
          evidence,
          confidence: 100,
        },
        ended_at_ms: null,
      },
    },
  };
}

describe("event mapping and reducer", () => {
  test("maps structural status, chat, tool, compaction, waits, and errors", () => {
    expect(
      classifyEvent(event("session.created", { info: { id: "root" } })),
    ).toEqual({
      kind: "idle",
      sessionID: "root",
      evidence: "OpenCode root session created",
    });
    expect(
      classifyEvent(
        event("session.status", { sessionID: "s", status: { type: "retry" } }),
      ).kind,
    ).toBe("working");
    expect(classifyEvent(event("chat.message", { sessionID: "s" })).kind).toBe(
      "working",
    );
    expect(
      classifyEvent(event("tool.execute.before", { sessionID: "s" })).kind,
    ).toBe("working");
    expect(
      classifyEvent(
        event("message.part.updated", {
          part: { sessionID: "s", type: "compaction" },
        }),
      ).kind,
    ).toBe("working");
    expect(
      classifyEvent(event("permission.updated", { sessionID: "s", id: "p" }))
        .kind,
    ).toBe("block");
    expect(
      classifyEvent(event("question.asked", { sessionID: "s", id: "q" })).kind,
    ).toBe("block");
    expect(
      classifyEvent(
        event("session.error", { sessionID: "s", error: { message: "bad" } }),
      ).kind,
    ).toBe("error");
  });

  test("tracks multiple blockers and latches errors", () => {
    const state = createReducerState();
    expect(
      reduce(
        state,
        {
          kind: "block",
          sessionID: "root",
          requestID: "p:1",
          evidence: "wait",
        },
        true,
      ).state,
    ).toBe("blocked");
    expect(
      reduce(
        state,
        {
          kind: "block",
          sessionID: "root",
          requestID: "p:2",
          evidence: "wait",
        },
        true,
      ).evidence,
    ).toBe("wait (2 pending)");
    expect(
      reduce(
        state,
        {
          kind: "unblock",
          sessionID: "root",
          requestID: "p:1",
          evidence: "one",
        },
        true,
      ).state,
    ).toBe("blocked");
    expect(
      reduce(
        state,
        {
          kind: "unblock",
          sessionID: "root",
          requestID: "p:2",
          evidence: "two",
        },
        true,
      ).state,
    ).toBe("working");
    expect(
      reduce(
        state,
        { kind: "error", sessionID: "root", evidence: "error" },
        true,
      ).state,
    ).toBe("blocked");
    expect(
      reduce(state, { kind: "idle", sessionID: "root", evidence: "idle" }, true)
        .state,
    ).toBe("blocked");
    expect(
      reduce(
        state,
        { kind: "working", sessionID: "root", evidence: "retry" },
        true,
      ).state,
    ).toBe("working");
  });

  test("suppresses evidence-only updates while already working", () => {
    const state = createReducerState();
    const chat = {
      kind: "working",
      sessionID: "root",
      evidence: "OpenCode chat.message",
    };
    const tool = {
      kind: "working",
      sessionID: "root",
      evidence: "OpenCode tool.execute.before",
    };

    expect(reduce(state, chat, true).evidence).toBe("OpenCode chat.message");
    expect(reduce(state, tool, true)).toBeUndefined();
    expect(reduce(state, tool, true)).toBeUndefined();
  });
});

describe("root aggregation", () => {
  test("maps OpenCode 1.18.15 fields consumed for child permissions", async () => {
    const calls = [];
    const lifecycle = createLifecycle({
      client: { session: { get: async () => {} } },
      env,
      run: async (argv) => {
        calls.push(argv);
        return calls.length === 1 ? successfulEnsure() : { data: {} };
      },
      log: () => {},
    });
    await lifecycle.enqueue(event("session.created", { info: { id: "root" } }));
    await lifecycle.enqueue(
      event("session.created", { info: { id: "child", parentID: "root" } }),
    );
    expect(calls[0]).toContain("root");
    calls.length = 0;
    await lifecycle.enqueue(
      event("session.status", {
        sessionID: "child",
        status: { type: "busy" },
      }),
    );
    await lifecycle.enqueue(
      event("permission.asked", {
        id: "permission-1",
        sessionID: "child",
        permission: "bash",
      }),
    );
    await lifecycle.enqueue(
      event("permission.replied", {
        requestID: "permission-1",
        sessionID: "child",
        reply: "reject",
      }),
    );
    await lifecycle.enqueue(event("session.idle", { sessionID: "child" }));
    await lifecycle.enqueue(event("session.idle", { sessionID: "root" }));

    expect(calls.map((argv) => argv[argv.indexOf("--state") + 1])).toEqual([
      "working",
      "blocked",
      "working",
      "idle",
    ]);
    expect(calls.every((argv) => !argv.includes("child"))).toBe(true);
  });

  test("resolves unknown nested ancestry through the client", async () => {
    const calls = [];
    const sessions = {
      child: { id: "child", parentID: "middle" },
      middle: { id: "middle", parentID: "root" },
      root: { id: "root" },
    };
    const client = {
      session: { get: async ({ path }) => ({ data: sessions[path.id] }) },
    };
    const lifecycle = createLifecycle({
      client,
      env,
      run: async (argv) => {
        calls.push(argv);
        return successfulEnsure();
      },
      log: () => {},
    });

    await lifecycle.enqueue(
      event("session.status", { sessionID: "child", status: { type: "busy" } }),
    );

    expect(calls).toHaveLength(1);
    expect(calls[0]).toContain("root");
    expect(calls[0]).not.toContain("child");
  });

  test("child deletion never reports done while root deletion does", async () => {
    const calls = [];
    const client = {
      session: {
        get: async () => {
          throw new Error("unexpected get");
        },
      },
    };
    const lifecycle = createLifecycle({
      client,
      env,
      run: async (argv) => {
        calls.push(argv);
        return successfulEnsure();
      },
      log: () => {},
    });

    await lifecycle.enqueue(event("session.created", { info: { id: "root" } }));
    await lifecycle.enqueue(
      event("session.created", { info: { id: "child", parentID: "root" } }),
    );
    calls.length = 0;
    await lifecycle.enqueue(
      event("session.deleted", { info: { id: "child", parentID: "root" } }),
    );
    expect(calls).toHaveLength(0);
    await lifecycle.enqueue(event("session.deleted", { info: { id: "root" } }));
    expect(calls[0]).toContain("done");
  });

  test("child deletion clears its sole permission blocker", async () => {
    const calls = [];
    const lifecycle = createLifecycle({
      client: { session: { get: async () => {} } },
      env,
      run: async (argv) => {
        calls.push(argv);
        return calls.length === 1
          ? successfulEnsure(
              "agent",
              "blocked",
              "OpenCode awaiting permission (1 pending)",
            )
          : { data: {} };
      },
      log: () => {},
    });
    await lifecycle.enqueue(event("session.created", { info: { id: "root" } }));
    await lifecycle.enqueue(
      event("session.created", { info: { id: "child", parentID: "root" } }),
    );
    calls.length = 0;

    await lifecycle.enqueue(
      event("permission.asked", { sessionID: "child", id: "permission-1" }),
    );
    await lifecycle.enqueue(
      event("session.deleted", { info: { id: "child", parentID: "root" } }),
    );

    expect(calls).toHaveLength(2);
    expect(calls[1].slice(0, 4)).toEqual([
      "boomux",
      "agent",
      "report",
      "agent",
    ]);
    expect(calls[1][calls[1].indexOf("--state") + 1]).toBe("working");
    expect(calls[1]).not.toContain("done");
  });

  test("child deletion preserves another child's blocker", async () => {
    const calls = [];
    const lifecycle = createLifecycle({
      client: { session: { get: async () => {} } },
      env,
      run: async (argv) => {
        calls.push(argv);
        return calls.length === 1
          ? successfulEnsure(
              "agent",
              "blocked",
              "OpenCode awaiting permission (1 pending)",
            )
          : { data: {} };
      },
      log: () => {},
    });
    await lifecycle.enqueue(event("session.created", { info: { id: "root" } }));
    calls.length = 0;
    for (const id of ["first", "second"]) {
      await lifecycle.enqueue(
        event("session.created", { info: { id, parentID: "root" } }),
      );
      await lifecycle.enqueue(
        event("permission.asked", { sessionID: id, id: `permission-${id}` }),
      );
    }

    await lifecycle.enqueue(
      event("session.deleted", { info: { id: "first", parentID: "root" } }),
    );
    expect(calls).toHaveLength(3);
    expect(calls[2][calls[2].indexOf("--state") + 1]).toBe("blocked");
    await lifecycle.enqueue(
      event("session.deleted", { info: { id: "second", parentID: "root" } }),
    );
    expect(calls).toHaveLength(4);
    expect(calls[3][calls[3].indexOf("--state") + 1]).toBe("working");
  });

  test("root work clears a latched child error", async () => {
    const calls = [];
    const lifecycle = createLifecycle({
      client: { session: { get: async () => {} } },
      env,
      run: async (argv) => {
        calls.push(argv);
        return calls.length === 1
          ? successfulEnsure("agent", "blocked", "OpenCode error: child failed")
          : { data: {} };
      },
      log: () => {},
    });
    await lifecycle.enqueue(event("session.created", { info: { id: "root" } }));
    await lifecycle.enqueue(
      event("session.created", { info: { id: "child", parentID: "root" } }),
    );
    calls.length = 0;

    await lifecycle.enqueue(
      event("session.error", {
        sessionID: "child",
        error: { message: "child failed" },
      }),
    );
    await lifecycle.enqueue(
      event("session.status", { sessionID: "root", status: "busy" }),
    );
    expect(calls).toHaveLength(2);
    expect(calls[1][calls[1].indexOf("--state") + 1]).toBe("working");
    await lifecycle.enqueue(
      event("session.deleted", { info: { id: "child", parentID: "root" } }),
    );

    expect(calls).toHaveLength(2);
  });

  test("child deletion clears its latched error", async () => {
    const calls = [];
    const lifecycle = createLifecycle({
      client: { session: { get: async () => {} } },
      env,
      run: async (argv) => {
        calls.push(argv);
        return successfulEnsure();
      },
      log: () => {},
    });
    await lifecycle.enqueue(event("session.created", { info: { id: "root" } }));
    await lifecycle.enqueue(
      event("session.created", { info: { id: "child", parentID: "root" } }),
    );
    calls.length = 0;

    await lifecycle.enqueue(
      event("session.error", {
        sessionID: "child",
        error: { message: "child failed" },
      }),
    );
    await lifecycle.enqueue(
      event("session.deleted", { info: { id: "child", parentID: "root" } }),
    );

    expect(calls).toHaveLength(2);
    expect(calls[1][calls[1].indexOf("--state") + 1]).toBe("working");
  });

  test("root work clears errors without clearing an outstanding prompt", async () => {
    const calls = [];
    const lifecycle = createLifecycle({
      client: { session: { get: async () => {} } },
      env,
      run: async (argv) => {
        calls.push(argv);
        return successfulEnsure();
      },
      log: () => {},
    });
    await lifecycle.enqueue(event("session.created", { info: { id: "root" } }));
    await lifecycle.enqueue(
      event("session.created", { info: { id: "child", parentID: "root" } }),
    );
    calls.length = 0;

    await lifecycle.enqueue(
      event("session.error", {
        sessionID: "child",
        error: { message: "child failed" },
      }),
    );
    await lifecycle.enqueue(
      event("permission.asked", {
        sessionID: "child",
        id: "permission-1",
      }),
    );
    await lifecycle.enqueue(
      event("session.status", { sessionID: "root", status: "busy" }),
    );

    expect(calls).toHaveLength(3);
    expect(calls[2][calls[2].indexOf("--state") + 1]).toBe("blocked");
    await lifecycle.enqueue(
      event("permission.replied", {
        sessionID: "child",
        requestID: "permission-1",
      }),
    );
    expect(calls).toHaveLength(4);
    expect(calls[3][calls[3].indexOf("--state") + 1]).toBe("working");
  });
});

describe("Boomux commands", () => {
  test("allows enough time for local lifecycle reporting under load", () => {
    expect(COMMAND_TIMEOUT_MS).toBeGreaterThanOrEqual(5_000);
  });

  test("uses exact argv and never invokes a shell", async () => {
    let received;
    const encoder = new TextEncoder();
    const stream = (value) =>
      new ReadableStream({
        start(controller) {
          controller.enqueue(encoder.encode(value));
          controller.close();
        },
      });
    const runner = createProcessRunner({
      spawn: (argv, options) => {
        received = { argv, options };
        return {
          stdout: stream('{"data":{"agent":{"id":"a"}}}'),
          stderr: stream(""),
          exited: Promise.resolve(0),
          kill() {},
        };
      },
    });
    const argv = [
      "boomux",
      "agent",
      "ensure",
      env.BOOMUX_SHELL_ID,
      env.BOOMUX_RUN_ID,
    ];
    await runner(argv);
    expect(received.argv).toEqual(argv);
    expect(received.options.shell).toBe(false);
  });

  test("parses structured stderr failures", async () => {
    const encoder = new TextEncoder();
    const stream = (value) =>
      new ReadableStream({
        start(controller) {
          controller.enqueue(encoder.encode(value));
          controller.close();
        },
      });
    const runner = createProcessRunner({
      spawn: () => ({
        stdout: stream(""),
        stderr: stream(
          '{"error":{"code":"run_changed","message":"different run"}}',
        ),
        exited: Promise.resolve(1),
        kill() {},
      }),
    });
    let failure;
    try {
      await runner(["boomux"]);
    } catch (error) {
      failure = error;
    }
    expect(failure?.code).toBe("run_changed");
  });

  test("builds ensure and report argv with preserved shell and run", async () => {
    const calls = [];
    const client = {
      session: { get: async ({ path }) => ({ data: { id: path.id } }) },
    };
    const lifecycle = createLifecycle({
      client,
      env,
      run: async (argv) => {
        calls.push(argv);
        return calls.length === 1 ? successfulEnsure("a1") : { data: {} };
      },
      log: () => {},
    });
    await lifecycle.enqueue(
      event("session.status", { sessionID: "root", status: "busy" }),
    );
    expect(calls).toHaveLength(1);
    await lifecycle.enqueue(event("session.idle", { sessionID: "root" }));
    expect(calls).toHaveLength(2);
    expect(calls[0]).toEqual([
      "boomux",
      "agent",
      "ensure",
      "opencode",
      "--integration",
      "opencode",
      "--external-session-id",
      "root",
      "--shell-id",
      env.BOOMUX_SHELL_ID,
      "--run-id",
      env.BOOMUX_RUN_ID,
      "--state",
      "working",
      "--authority",
      "lifecycle-integration",
      "--evidence",
      "OpenCode session busy",
      "--confidence",
      "100",
      "--json",
    ]);
    expect(calls[1].slice(0, 5)).toEqual([
      "boomux",
      "agent",
      "report",
      "a1",
      "--shell-id",
    ]);
    expect(calls[1]).toContain(env.BOOMUX_RUN_ID);
  });

  test("same-state Ensure with different working evidence does not report", async () => {
    const calls = [];
    const lifecycle = createLifecycle({
      client: {
        session: { get: async ({ path }) => ({ data: { id: path.id } }) },
      },
      env,
      run: async (argv) => {
        calls.push(argv);
        return calls.length === 1
          ? successfulEnsure("existing", "working", "stale evidence")
          : { data: {} };
      },
      log: () => {},
    });

    await lifecycle.enqueue(
      event("session.status", { sessionID: "root", status: "busy" }),
    );

    expect(calls).toHaveLength(1);
  });

  test("coalesces a burst of working activity into one lifecycle command", async () => {
    const calls = [];
    const lifecycle = createLifecycle({
      client: { session: { get: async ({ path }) => ({ data: { id: path.id } }) } },
      env,
      run: async (argv) => {
        calls.push(argv);
        return successfulEnsure("agent", "working");
      },
      log: () => {},
    });

    await lifecycle.enqueue(
      event("session.status", { sessionID: "root", status: "busy" }),
    );
    for (let index = 0; index < 100; index += 1) {
      await lifecycle.enqueue(
        event(index % 2 ? "tool.execute.before" : "tool.execute.after", {
          sessionID: "root",
        }),
      );
    }

    expect(calls).toHaveLength(1);
  });

  test("reattached working agent reports the first idle event", async () => {
    const calls = [];
    const lifecycle = createLifecycle({
      client: {
        session: { get: async ({ path }) => ({ data: { id: path.id } }) },
      },
      env,
      run: async (argv) => {
        calls.push(argv);
        return calls.length === 1
          ? successfulEnsure("existing", "working")
          : { data: {} };
      },
      log: () => {},
    });

    await lifecycle.enqueue(event("session.idle", { sessionID: "root" }));

    expect(calls).toHaveLength(2);
    expect(calls[0].slice(0, 4)).toEqual([
      "boomux",
      "agent",
      "ensure",
      "opencode",
    ]);
    expect(calls[0][calls[0].indexOf("--state") + 1]).toBe("idle");
    expect(calls[1].slice(0, 4)).toEqual([
      "boomux",
      "agent",
      "report",
      "existing",
    ]);
    expect(calls[1][calls[1].indexOf("--state") + 1]).toBe("idle");
  });

  test("reattached idle agent reports root deletion as done", async () => {
    const calls = [];
    const lifecycle = createLifecycle({
      client: {
        session: { get: async ({ path }) => ({ data: { id: path.id } }) },
      },
      env,
      run: async (argv) => {
        calls.push(argv);
        return calls.length === 1
          ? successfulEnsure("existing", "idle")
          : { data: {} };
      },
      log: () => {},
    });

    await lifecycle.enqueue(event("session.deleted", { info: { id: "root" } }));

    expect(calls).toHaveLength(2);
    expect(calls[0][calls[0].indexOf("--state") + 1]).toBe("done");
    expect(calls[1].slice(0, 4)).toEqual([
      "boomux",
      "agent",
      "report",
      "existing",
    ]);
    expect(calls[1][calls[1].indexOf("--state") + 1]).toBe("done");
  });

  test("does not reopen a completed reused agent", async () => {
    const calls = [];
    const lifecycle = createLifecycle({
      client: {
        session: { get: async ({ path }) => ({ data: { id: path.id } }) },
      },
      env,
      run: async (argv) => {
        calls.push(argv);
        return {
          data: {
            agent: {
              id: "done",
              ended_at_ms: 1,
              observation: { state: "done" },
            },
          },
        };
      },
      log: () => {},
    });
    await lifecycle.enqueue(
      event("session.status", { sessionID: "root", status: "busy" }),
    );
    await lifecycle.enqueue(event("session.idle", { sessionID: "root" }));
    expect(calls).toHaveLength(1);
  });

  test("run_changed permanently disables that tracked root", async () => {
    const calls = [];
    const lifecycle = createLifecycle({
      client: {
        session: { get: async ({ path }) => ({ data: { id: path.id } }) },
      },
      env,
      run: async (argv) => {
        calls.push(argv);
        const error = new Error("different run");
        error.code = "run_changed";
        throw error;
      },
      log: () => {},
    });
    await lifecycle.enqueue(
      event("session.status", { sessionID: "root", status: "busy" }),
    );
    await lifecycle.enqueue(event("session.idle", { sessionID: "root" }));
    expect(calls).toHaveLength(1);
  });
});

describe("queue and activation", () => {
  test("serializes handling and fails open after runner failure", async () => {
    const order = [];
    let count = 0;
    const lifecycle = createLifecycle({
      client: {
        session: { get: async ({ path }) => ({ data: { id: path.id } }) },
      },
      env,
      run: async () => {
        const current = ++count;
        order.push(`start${current}`);
        await Bun.sleep(10);
        order.push(`end${current}`);
        if (current === 1) throw new Error("offline");
        return successfulEnsure();
      },
      log: () => {},
    });
    const first = lifecycle.enqueue(
      event("session.status", { sessionID: "root", status: "busy" }),
    );
    const second = lifecycle.enqueue(
      event("session.status", { sessionID: "root", status: "busy" }),
    );
    await Promise.all([first, second]);
    expect(order).toEqual(["start1", "end1", "start2", "end2"]);
  });

  test("missing environment is a no-op", async () => {
    expect(
      createLifecycle({ client: {}, env: {}, run: async () => {} }),
    ).toBeUndefined();
    const oldShell = process.env.BOOMUX_SHELL_ID;
    const oldRun = process.env.BOOMUX_RUN_ID;
    delete process.env.BOOMUX_SHELL_ID;
    delete process.env.BOOMUX_RUN_ID;
    try {
      expect(await BoomuxOpenCodePlugin({ client: {} })).toEqual({});
    } finally {
      if (oldShell === undefined) delete process.env.BOOMUX_SHELL_ID;
      else process.env.BOOMUX_SHELL_ID = oldShell;
      if (oldRun === undefined) delete process.env.BOOMUX_RUN_ID;
      else process.env.BOOMUX_RUN_ID = oldRun;
    }
  });
});
