"use strict";

function agentKey(agent) {
  return `${agent.node_id}\u0000${agent.agent_id}`;
}

export function reconcileAgentCompletions({
  agents,
  previousStates,
  completedAgents,
  baselineReady,
}) {
  const nextStates = new Map();
  const nextCompleted = new Set();
  for (const agent of agents) {
    const key = agentKey(agent);
    nextStates.set(key, agent.state);
    if (completedAgents.has(key) && agent.node_local && agent.run_current && agent.state === "idle") {
      nextCompleted.add(key);
    }
    if (
      baselineReady
      && agent.node_local
      && agent.run_current
      && previousStates.get(key) === "working"
      && agent.state === "idle"
    ) {
      nextCompleted.add(key);
    }
    agent.just_completed = nextCompleted.has(key);
  }
  return {
    previousStates: nextStates,
    completedAgents: nextCompleted,
  };
}
