import { describe, expect, test } from "bun:test";
import {
  createClaimController,
  createRootResolver,
  ensureClaimArgv,
  releaseClaimArgv,
} from "./boomux-tui-core.js";

const identity = {
  shellID: "shell;literal",
  runID: "run $(literal)",
  generation: "generation literal",
  holder: "holder;literal",
};

const env = {
  BOOMUX_SHELL_ID: identity.shellID,
  BOOMUX_RUN_ID: identity.runID,
  BOOMUX_OPENCODE_SHARED_GENERATION: identity.generation,
  BOOMUX_OPENCODE_CLAIM_HOLDER: identity.holder,
};

function response(root, claim = `claim-${root}`) {
  return {
    data: {
      claim: { claim_id: claim, root_session_id: root },
      agent: { id: `agent-${root}` },
    },
  };
}

function harness(options = {}) {
  const calls = [];
  const sessions = options.sessions ?? {};
  const controller = createClaimController({
    client: {
      session: {
        get: async ({ sessionID }) => ({
          data: sessions[sessionID] ?? { id: sessionID },
        }),
      },
    },
    cachedSession: options.cachedSession,
    env: options.env ?? env,
    log: () => {},
    onError: options.onError,
    run:
      options.run ??
      (async (argv) => {
        calls.push(argv);
        const root = argv[argv.indexOf("--root-session-id") + 1];
        return root ? response(root) : { data: { released: true } };
      }),
    setInterval: options.setInterval ?? (() => 1),
    clearInterval: options.clearInterval ?? (() => {}),
  });
  return { calls, controller };
}

describe("canonical Session roots", () => {
  test("uses cached state and loads unknown ancestors through the client", async () => {
    const gets = [];
    const resolver = createRootResolver(
      {
        session: {
          get: async ({ sessionID }) => {
            gets.push(sessionID);
            return { data: { id: "root" } };
          },
        },
      },
      (id) => (id === "child" ? { id: "child", parentID: "root" } : undefined),
    );

    expect(await resolver.root("child")).toBe("root");
    expect(gets).toEqual(["root"]);
  });

  test("rejects ancestry cycles", async () => {
    const resolver = createRootResolver({
      session: {
        get: async ({ sessionID }) => ({
          data: {
            id: sessionID,
            parentID: sessionID === "a" ? "b" : "a",
          },
        }),
      },
    });
    expect(resolver.root("a")).rejects.toThrow("invalid OpenCode session ancestry");
  });
});

describe("TUI claim selection", () => {
  test("ensures the initial selected canonical root", async () => {
    const { calls, controller } = harness({
      sessions: { child: { id: "child", parentID: "root" }, root: { id: "root" } },
    });
    await controller.select("child");
    expect(calls).toEqual([ensureClaimArgv(identity, "root")]);
    expect(controller.snapshot().current.rootID).toBe("root");
    await controller.dispose();
  });

  test("renews the same root every tick without releasing it", async () => {
    const { calls, controller } = harness();
    await controller.select("root");
    await controller.tick();
    expect(calls).toEqual([
      ensureClaimArgv(identity, "root"),
      ensureClaimArgv(identity, "root"),
    ]);
    await controller.dispose();
  });

  test("ensures a switched root before stale-safe prior release", async () => {
    const { calls, controller } = harness();
    await controller.select("first");
    await controller.select("second");
    expect(calls).toEqual([
      ensureClaimArgv(identity, "first"),
      ensureClaimArgv(identity, "second"),
      releaseClaimArgv(identity, "claim-first"),
    ]);
    expect(controller.snapshot().current.rootID).toBe("second");
    await controller.dispose();
  });

  test("home and disposal release once and disposal is idempotent", async () => {
    const { calls, controller } = harness();
    await controller.select("first");
    await controller.select(undefined);
    expect(calls.at(-1)).toEqual(releaseClaimArgv(identity, "claim-first"));
    await controller.select("second");
    await Promise.all([controller.dispose(), controller.dispose()]);
    expect(
      calls.filter((argv) => argv[3] === "release"),
    ).toEqual([
      releaseClaimArgv(identity, "claim-first"),
      releaseClaimArgv(identity, "claim-second"),
    ]);
  });

  test("a failed switch preserves the prior claim and retries", async () => {
    const calls = [];
    const errors = [];
    let fail = true;
    const { controller } = harness({
      run: async (argv) => {
        calls.push(argv);
        const root = argv[argv.indexOf("--root-session-id") + 1];
        if (root === "second" && fail) {
          fail = false;
          const error = new Error("claim busy");
          error.code = "busy";
          throw error;
        }
        return root ? response(root) : { data: { released: true } };
      },
      onError: (error) => errors.push(error.code),
    });
    await controller.select("first");
    await controller.select("second");
    expect(controller.snapshot().current.rootID).toBe("first");
    expect(calls.some((argv) => argv[3] === "release")).toBe(false);
    expect(errors).toEqual(["busy"]);
    await controller.tick();
    expect(controller.snapshot().current.rootID).toBe("second");
    expect(calls.at(-1)).toEqual(releaseClaimArgv(identity, "claim-first"));
    await controller.dispose();
  });

  test("a handoff-missing claim is reacquired on renewal", async () => {
    const calls = [];
    let ensureCount = 0;
    const { controller } = harness({
      run: async (argv) => {
        calls.push(argv);
        const root = argv[argv.indexOf("--root-session-id") + 1];
        if (!root) return { data: { released: true } };
        ensureCount += 1;
        if (ensureCount === 2) {
          const error = new Error("claim missing after daemon handoff");
          error.code = "not_found";
          throw error;
        }
        return response(root, `claim-${ensureCount}`);
      },
    });
    await controller.select("root");
    await controller.tick();
    expect(controller.snapshot().current.claimID).toBe("claim-1");
    await controller.tick();
    expect(controller.snapshot().current.claimID).toBe("claim-3");
    expect(calls.filter((argv) => argv[3] === "ensure")).toHaveLength(3);
    await controller.dispose();
  });
});

describe("TUI activation and argv", () => {
  test("keeps every identity value in its own argv boundary", () => {
    expect(ensureClaimArgv(identity, "root;literal")).toEqual([
      "boomux",
      "opencode",
      "claim",
      "ensure",
      "--generation",
      identity.generation,
      "--holder",
      identity.holder,
      "--root-session-id",
      "root;literal",
      "--shell-id",
      identity.shellID,
      "--run-id",
      identity.runID,
      "--json",
    ]);
    expect(releaseClaimArgv(identity, "claim;literal")).toEqual([
      "boomux",
      "opencode",
      "claim",
      "release",
      "--generation",
      identity.generation,
      "--holder",
      identity.holder,
      "--claim-id",
      "claim;literal",
      "--json",
    ]);
  });

  test("is inert when any required environment identity is absent", () => {
    for (const name of Object.keys(env)) {
      const incomplete = { ...env };
      delete incomplete[name];
      expect(harness({ env: incomplete }).controller).toBeUndefined();
    }
  });
});
