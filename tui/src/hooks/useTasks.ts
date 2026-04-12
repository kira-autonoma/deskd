/**
 * Hook: task state tracking from bus messages.
 *
 * Listens for bus messages about tasks and maintains a list of recent
 * tasks with their status, assignee, and timestamps.
 */

import { useState, useEffect } from "react";
import type { BusClient, BusMessage } from "../bus.js";

export type TaskStatus = "pending" | "running" | "done" | "failed" | "cancelled";

export interface TaskState {
  id: string;
  title: string;
  status: TaskStatus;
  assignee?: string;
  createdAt: number;
  updatedAt: number;
}

const MAX_TASKS = 100;

function extractTaskInfo(msg: BusMessage): Partial<TaskState> | null {
  const payload = msg.payload as Record<string, unknown> | undefined;
  if (!payload) return null;

  // Task creation
  if (payload.action === "task_create" || msg.type === "task_create") {
    const task = (payload.task ?? payload.text ?? payload.title) as string;
    return {
      id: (payload.task_id as string) || msg.id || `task-${Date.now()}`,
      title: typeof task === "string"
        ? task.length > 80 ? task.slice(0, 77) + "..." : task
        : "untitled",
      status: "pending",
      assignee: (payload.assignee as string) || msg.target?.replace("agent:", ""),
      createdAt: Date.now(),
      updatedAt: Date.now(),
    };
  }

  // Task sent to agent — infer as running
  if (
    msg.type === "message" &&
    msg.target?.startsWith("agent:") &&
    (payload.task || payload.text)
  ) {
    const task = (payload.task ?? payload.text) as string;
    return {
      id: (payload.task_id as string) || msg.id || `task-${Date.now()}`,
      title: typeof task === "string"
        ? task.length > 80 ? task.slice(0, 77) + "..." : task
        : "untitled",
      status: "running",
      assignee: msg.target.slice(6),
      createdAt: Date.now(),
      updatedAt: Date.now(),
    };
  }

  // Task result — completed
  if (payload.result !== undefined && msg.source) {
    const taskId = (payload.task_id as string) || msg.id;
    if (taskId) {
      return {
        id: taskId,
        status: payload.error ? "failed" : "done",
        updatedAt: Date.now(),
      };
    }
  }

  // Explicit status update
  if (payload.action === "task_status" || msg.type === "task_status") {
    return {
      id: (payload.task_id as string) || msg.id || "",
      status: (payload.status as TaskStatus) || "running",
      updatedAt: Date.now(),
    };
  }

  return null;
}

export function useTasks(bus: BusClient): TaskState[] {
  const [tasks, setTasks] = useState<Map<string, TaskState>>(new Map());

  useEffect(() => {
    const onMessage = (msg: BusMessage) => {
      const info = extractTaskInfo(msg);
      if (!info || !info.id) return;

      setTasks((prev) => {
        const next = new Map(prev);
        const existing = next.get(info.id!);

        if (existing) {
          next.set(info.id!, {
            ...existing,
            ...Object.fromEntries(
              Object.entries(info).filter(([, v]) => v !== undefined),
            ),
          } as TaskState);
        } else if (info.title) {
          next.set(info.id!, {
            id: info.id!,
            title: info.title,
            status: info.status ?? "pending",
            assignee: info.assignee,
            createdAt: info.createdAt ?? Date.now(),
            updatedAt: info.updatedAt ?? Date.now(),
          });
        }

        // Cap at MAX_TASKS — remove oldest
        if (next.size > MAX_TASKS) {
          const entries = Array.from(next.entries()).sort(
            (a, b) => a[1].updatedAt - b[1].updatedAt,
          );
          for (let i = 0; i < entries.length - MAX_TASKS; i++) {
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

  return Array.from(tasks.values()).sort(
    (a, b) => b.updatedAt - a.updatedAt,
  );
}
