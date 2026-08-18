import { describe, expect, test } from "bun:test";
import { reconcileAgentCompletions } from "./mobile-model.js";

function agent(state, overrides = {}) {
  return {
    node_id: "local-node",
    agent_id: "agent-1",
    node_local: true,
    run_current: true,
    state,
    ...overrides,
  };
}

function reconcile(agents, prior = {}) {
  return reconcileAgentCompletions({
    agents,
    previousStates: prior.previousStates || new Map(),
    completedAgents: prior.completedAgents || new Set(),
    baselineReady: prior.baselineReady || false,
  });
}

describe("mobile completion attention", () => {
  test("does not fabricate completion from an idle baseline", () => {
    const agents = [agent("idle")];
    const result = reconcile(agents);

    expect(agents[0].just_completed).toBe(false);
    expect(result.completedAgents.size).toBe(0);
  });

  test("retains local working-to-idle completion only while idle", () => {
    const working = reconcile([agent("working")]);
    const idleAgents = [agent("idle")];
    const idle = reconcileAgentCompletions({
      agents: idleAgents,
      previousStates: working.previousStates,
      completedAgents: working.completedAgents,
      baselineReady: true,
    });

    expect(idleAgents[0].just_completed).toBe(true);
    const stillIdle = [agent("idle")];
    const retained = reconcileAgentCompletions({
      agents: stillIdle,
      previousStates: idle.previousStates,
      completedAgents: idle.completedAgents,
      baselineReady: true,
    });
    expect(stillIdle[0].just_completed).toBe(true);

    const resumed = [agent("working")];
    const cleared = reconcileAgentCompletions({
      agents: resumed,
      previousStates: retained.previousStates,
      completedAgents: retained.completedAgents,
      baselineReady: true,
    });
    expect(resumed[0].just_completed).toBe(false);
    expect(cleared.completedAgents.size).toBe(0);
  });

  test("does not derive completion for remote or historical Agents", () => {
    const previousStates = new Map([
      ["remote-node\u0000agent-1", "working"],
      ["local-node\u0000agent-2", "working"],
    ]);
    const agents = [
      agent("idle", { node_id: "remote-node", node_local: false }),
      agent("idle", { agent_id: "agent-2", run_current: false }),
    ];
    const result = reconcileAgentCompletions({
      agents,
      previousStates,
      completedAgents: new Set(),
      baselineReady: true,
    });

    expect(agents.every((candidate) => !candidate.just_completed)).toBe(true);
    expect(result.completedAgents.size).toBe(0);
  });
});
