const RENEW_INTERVAL_MS = 60_000;
const LOG_INTERVAL_MS = 30_000;

function text(value) {
  return typeof value === "string" && value.length > 0 ? value : undefined;
}

function unwrapSession(response) {
  const value = response?.data ?? response;
  return value?.session ?? value;
}

export function createRootResolver(client, cachedSession = () => undefined) {
  const sessions = new Map();

  function remember(info) {
    const id = text(info?.id);
    if (!id) return undefined;
    const session = { id, parentID: text(info.parentID) };
    sessions.set(id, session);
    return session;
  }

  async function load(id) {
    if (sessions.has(id)) return sessions.get(id);
    const cached = remember(cachedSession(id));
    if (cached) return cached;
    const info = unwrapSession(await client.session.get({ sessionID: id }));
    const loaded = remember(info);
    if (!loaded) throw new Error(`OpenCode session not found: ${id}`);
    return loaded;
  }

  async function root(id) {
    let current = id;
    const seen = new Set();
    while (current && !seen.has(current)) {
      seen.add(current);
      const session = await load(current);
      if (!session.parentID) return session.id;
      current = session.parentID;
    }
    throw new Error(`invalid OpenCode session ancestry for ${id}`);
  }

  return { remember, root };
}

export function ensureClaimArgv(identity, rootID) {
  return [
    "boomux",
    "opencode",
    "claim",
    "ensure",
    "--generation",
    identity.generation,
    "--holder",
    identity.holder,
    "--root-session-id",
    rootID,
    "--shell-id",
    identity.shellID,
    "--run-id",
    identity.runID,
    "--json",
  ];
}

export function releaseClaimArgv(identity, claimID) {
  return [
    "boomux",
    "opencode",
    "claim",
    "release",
    "--generation",
    identity.generation,
    "--holder",
    identity.holder,
    "--claim-id",
    claimID,
    "--json",
  ];
}

function claimResponse(result) {
  const data = result?.data ?? result;
  const claim = data?.claim;
  const claimID = text(claim?.claim_id);
  if (!claimID) {
    throw new Error("boomux claim response has no claim identity");
  }
  return { claimID };
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
    const suffix = suppressed ? ` (${suppressed} similar errors suppressed)` : "";
    suppressed = 0;
    last = time;
    log(`[boomux-opencode-tui] ${error?.message ?? error}${suffix}`);
  };
}

export function createClaimController(options) {
  const env = options.env ?? {};
  const identity = {
    shellID: text(env.BOOMUX_SHELL_ID),
    runID: text(env.BOOMUX_RUN_ID),
    generation: text(env.BOOMUX_OPENCODE_SHARED_GENERATION),
    holder: text(env.BOOMUX_OPENCODE_CLAIM_HOLDER),
  };
  if (Object.values(identity).some((value) => !value)) return undefined;

  const resolver = createRootResolver(
    options.client,
    options.cachedSession ?? (() => undefined),
  );
  const reportError = rateLimitedLogger(options.log ?? console.error, options.now);
  const setIntervalFn = options.setInterval ?? globalThis.setInterval;
  const clearIntervalFn = options.clearInterval ?? globalThis.clearInterval;
  let current;
  let desiredSessionID;
  let disposed = false;
  let disposal;
  let queue = Promise.resolve();

  async function release(claim) {
    await options.run(releaseClaimArgv(identity, claim.claimID));
  }

  async function transition(sessionID) {
    if (disposed) return;
    if (!sessionID) {
      const prior = current;
      current = undefined;
      if (prior) await release(prior);
      return;
    }

    const rootID = await resolver.root(sessionID);
    const prior = current;
    const ensured = claimResponse(
      await options.run(ensureClaimArgv(identity, rootID)),
    );
    current = { rootID, claimID: ensured.claimID };
    if (prior && prior.rootID !== rootID) await release(prior);
  }

  function enqueue(sessionID) {
    const pending = queue.then(() => transition(sessionID));
    queue = pending.catch((error) => {
      reportError(error);
      options.onError?.(error);
    });
    return queue;
  }

  function select(sessionID) {
    desiredSessionID = text(sessionID);
    return enqueue(desiredSessionID);
  }

  const timer = setIntervalFn(() => {
    enqueue(desiredSessionID);
  }, options.renewIntervalMs ?? RENEW_INTERVAL_MS);

  function dispose() {
    if (disposal) return disposal;
    disposed = true;
    clearIntervalFn(timer);
    disposal = queue.then(async () => {
      const prior = current;
      current = undefined;
      if (prior) {
        try {
          await release(prior);
        } catch (error) {
          reportError(error);
        }
      }
    });
    return disposal;
  }

  return {
    dispose,
    resolver,
    select,
    tick: () => enqueue(desiredSessionID),
    snapshot: () => ({ current, desiredSessionID, disposed }),
  };
}
