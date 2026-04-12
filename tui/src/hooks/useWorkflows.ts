/**
 * Hook: workflow / state machine tracking from bus messages.
 *
 * Listens for bus messages about graph executions and workflow state
 * machines, maintaining a list of active instances.
 */

import { useState, useEffect } from "react";
import type { BusClient, BusMessage } from "../bus.js";

export interface WorkflowState {
  id: string;
  name: string;
  currentState: string;
  costUsd: number;
  startedAt: number;
  updatedAt: number;
}

const MAX_WORKFLOWS = 50;

function extractWorkflowInfo(msg: BusMessage): Partial<WorkflowState> | null {
  const payload = msg.payload as Record<string, unknown> | undefined;
  if (!payload) return null;

  // Graph/workflow start
  if (
    msg.type === "graph_start" ||
    payload.action === "graph_start" ||
    payload.action === "workflow_start"
  ) {
    return {
      id: (payload.graph_id as string) || (payload.workflow_id as string) || msg.id || `wf-${Date.now()}`,
      name: (payload.name as string) || (payload.file as string) || "unnamed",
      currentState: "started",
      costUsd: 0,
      startedAt: Date.now(),
      updatedAt: Date.now(),
    };
  }

  // State transition
  if (
    msg.type === "graph_step" ||
    payload.action === "graph_step" ||
    payload.action === "workflow_transition"
  ) {
    const id = (payload.graph_id as string) || (payload.workflow_id as string) || msg.id;
    if (id) {
      return {
        id,
        currentState: (payload.state as string) || (payload.node as string) || "unknown",
        costUsd: typeof payload.cost === "number" ? payload.cost : undefined,
        updatedAt: Date.now(),
      };
    }
  }

  // Graph/workflow completion
  if (
    msg.type === "graph_complete" ||
    payload.action === "graph_complete" ||
    payload.action === "workflow_complete"
  ) {
    const id = (payload.graph_id as string) || (payload.workflow_id as string) || msg.id;
    if (id) {
      return {
        id,
        currentState: payload.error ? "failed" : "completed",
        costUsd: typeof payload.cost === "number" ? payload.cost : undefined,
        updatedAt: Date.now(),
      };
    }
  }

  return null;
}

export function useWorkflows(bus: BusClient): WorkflowState[] {
  const [workflows, setWorkflows] = useState<Map<string, WorkflowState>>(new Map());

  useEffect(() => {
    const onMessage = (msg: BusMessage) => {
      const info = extractWorkflowInfo(msg);
      if (!info || !info.id) return;

      setWorkflows((prev) => {
        const next = new Map(prev);
        const existing = next.get(info.id!);

        if (existing) {
          next.set(info.id!, {
            ...existing,
            ...Object.fromEntries(
              Object.entries(info).filter(([, v]) => v !== undefined),
            ),
          } as WorkflowState);
        } else if (info.name) {
          next.set(info.id!, {
            id: info.id!,
            name: info.name,
            currentState: info.currentState ?? "unknown",
            costUsd: info.costUsd ?? 0,
            startedAt: info.startedAt ?? Date.now(),
            updatedAt: info.updatedAt ?? Date.now(),
          });
        }

        // Cap
        if (next.size > MAX_WORKFLOWS) {
          const entries = Array.from(next.entries()).sort(
            (a, b) => a[1].updatedAt - b[1].updatedAt,
          );
          for (let i = 0; i < entries.length - MAX_WORKFLOWS; i++) {
            next.delete(entries[i]![0]);
          }
        }

        return next;
      });
    };

    bus.on("message", onMessage);
    return () => {
      bus.off("message", onMessage);
    };
  }, [bus]);

  return Array.from(workflows.values()).sort(
    (a, b) => b.updatedAt - a.updatedAt,
  );
}
