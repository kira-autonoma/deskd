/**
 * Hook: agent state tracking from bus messages.
 *
 * Listens for bus messages about agents and maintains an up-to-date
 * map of agent states including status, model, current task, and cost.
 */

import { useState, useEffect } from "react";
import type { BusClient, BusMessage } from "../bus.js";

export interface AgentState {
  name: string;
  status: "online" | "busy" | "idle" | "offline";
  model?: string;
  currentTask?: string;
  costUsd: number;
  lastSeen: number;
}

function extractAgentInfo(msg: BusMessage): Partial<AgentState> | null {
  const payload = msg.payload as Record<string, unknown> | undefined;
  if (!payload) return null;

  // Agent registration / heartbeat
  if (msg.type === "register" || msg.type === "heartbeat") {
    return {
      name: (msg.source ?? msg.name as string) || undefined,
      status: "online",
      model: payload.model as string | undefined,
      lastSeen: Date.now(),
    };
  }

  // Agent status update
  if (msg.type === "status" || payload.action === "status") {
    return {
      name: msg.source || undefined,
      status: (payload.status as AgentState["status"]) || "online",
      model: payload.model as string | undefined,
      currentTask: payload.task as string | undefined,
      costUsd: typeof payload.cost === "number" ? payload.cost : undefined,
      lastSeen: Date.now(),
    };
  }

  // Task assignment — mark agent as busy
  if (msg.type === "message" && msg.target?.startsWith("agent:")) {
    const agentName = msg.target.slice(6);
    const task =
      typeof payload.task === "string"
        ? payload.task
        : typeof payload.text === "string"
          ? payload.text
          : undefined;
    if (task) {
      return {
        name: agentName,
        status: "busy",
        currentTask:
          task.length > 60 ? task.slice(0, 57) + "..." : task,
        lastSeen: Date.now(),
      };
    }
  }

  // Task result — mark agent back to idle
  if (
    msg.type === "message" &&
    msg.source &&
    payload.result !== undefined
  ) {
    return {
      name: msg.source,
      status: "idle",
      currentTask: undefined,
      lastSeen: Date.now(),
    };
  }

  // Cost update
  if (payload.cost_usd !== undefined && msg.source) {
    return {
      name: msg.source,
      costUsd: payload.cost_usd as number,
      lastSeen: Date.now(),
    };
  }

  return null;
}

export function useAgents(bus: BusClient): AgentState[] {
  const [agents, setAgents] = useState<Map<string, AgentState>>(new Map());

  useEffect(() => {
    const onMessage = (msg: BusMessage) => {
      const info = extractAgentInfo(msg);
      if (!info || !info.name) return;

      setAgents((prev) => {
        const next = new Map(prev);
        const existing = next.get(info.name!) ?? {
          name: info.name!,
          status: "offline" as const,
          costUsd: 0,
          lastSeen: 0,
        };
        next.set(info.name!, {
          ...existing,
          ...Object.fromEntries(
            Object.entries(info).filter(([, v]) => v !== undefined),
          ),
        } as AgentState);
        return next;
      });
    };

    bus.on("message", onMessage);

    // Request client list
    bus.send({ type: "list" });

    return () => {
      bus.off("message", onMessage);
    };
  }, [bus]);

  return Array.from(agents.values()).sort((a, b) =>
    a.name.localeCompare(b.name),
  );
}
