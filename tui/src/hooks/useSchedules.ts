/**
 * Hook: schedule and reminder tracking from bus messages.
 *
 * Listens for schedule_list, schedule_add, schedule_remove,
 * reminder_add, and reminder_fire messages.
 */

import { useState, useEffect, useCallback } from "react";
import type { BusClient, BusMessage } from "../bus.js";

export interface Schedule {
  id: string;
  cron: string;
  action: string;
  target: string;
  nextFire?: string;
  createdAt: number;
}

export interface Reminder {
  id: string;
  fireAt: string;
  target: string;
  message: string;
  createdAt: number;
}

function extractScheduleData(
  msg: BusMessage,
): { schedules?: Schedule[]; schedule?: Schedule; removeId?: string; reminder?: Reminder; reminders?: Reminder[] } | null {
  const payload = msg.payload as Record<string, unknown> | undefined;
  if (!payload) return null;

  // Bulk schedule list response
  if (msg.type === "schedule_list" || payload.action === "schedule_list") {
    const items = payload.schedules as Array<Record<string, unknown>> | undefined;
    if (Array.isArray(items)) {
      const schedules: Schedule[] = items.map((s) => ({
        id: (s.id as string) || `sched-${Date.now()}`,
        cron: (s.cron as string) || "",
        action: (s.action as string) || "",
        target: (s.target as string) || "",
        nextFire: s.next_fire as string | undefined,
        createdAt: typeof s.created_at === "number" ? s.created_at : Date.now(),
      }));
      const reminders: Reminder[] = (
        (payload.reminders as Array<Record<string, unknown>>) ?? []
      ).map((r) => ({
        id: (r.id as string) || `rem-${Date.now()}`,
        fireAt: (r.fire_at as string) || "",
        target: (r.target as string) || "",
        message: (r.message as string) || "",
        createdAt: typeof r.created_at === "number" ? r.created_at : Date.now(),
      }));
      return { schedules, reminders };
    }
  }

  // Single schedule added
  if (msg.type === "schedule_added" || payload.action === "schedule_added") {
    return {
      schedule: {
        id: (payload.id as string) || `sched-${Date.now()}`,
        cron: (payload.cron as string) || "",
        action: (payload.action_type as string) || (payload.sched_action as string) || "",
        target: (payload.target as string) || "",
        nextFire: payload.next_fire as string | undefined,
        createdAt: Date.now(),
      },
    };
  }

  // Schedule removed
  if (msg.type === "schedule_removed" || payload.action === "schedule_removed") {
    return { removeId: payload.id as string };
  }

  // Reminder added
  if (msg.type === "reminder_added" || payload.action === "reminder_added") {
    return {
      reminder: {
        id: (payload.id as string) || `rem-${Date.now()}`,
        fireAt: (payload.fire_at as string) || "",
        target: (payload.target as string) || "",
        message: (payload.message as string) || "",
        createdAt: Date.now(),
      },
    };
  }

  // Reminder fired — remove it
  if (msg.type === "reminder_fired" || payload.action === "reminder_fired") {
    return { removeId: payload.id as string };
  }

  return null;
}

export interface UseSchedulesResult {
  schedules: Schedule[];
  reminders: Reminder[];
  addSchedule: (cron: string, action: string, target: string) => void;
  removeSchedule: (id: string) => void;
  addReminder: (delay: string, target: string, message: string) => void;
}

export function useSchedules(bus: BusClient): UseSchedulesResult {
  const [schedules, setSchedules] = useState<Schedule[]>([]);
  const [reminders, setReminders] = useState<Reminder[]>([]);

  useEffect(() => {
    const onMessage = (msg: BusMessage) => {
      const data = extractScheduleData(msg);
      if (!data) return;

      if (data.schedules) {
        setSchedules(data.schedules);
      }
      if (data.reminders) {
        setReminders(data.reminders);
      }
      if (data.schedule) {
        setSchedules((prev) => [...prev, data.schedule!]);
      }
      if (data.reminder) {
        setReminders((prev) => [...prev, data.reminder!]);
      }
      if (data.removeId) {
        const rid = data.removeId;
        setSchedules((prev) => prev.filter((s) => s.id !== rid));
        setReminders((prev) => prev.filter((r) => r.id !== rid));
      }
    };

    bus.on("message", onMessage);

    // Request current schedules
    bus.send({
      type: "message",
      source: "tui",
      target: "broadcast",
      payload: { action: "get_schedules" },
    });

    return () => {
      bus.off("message", onMessage);
    };
  }, [bus]);

  const addSchedule = useCallback(
    (cron: string, action: string, target: string) => {
      bus.send({
        type: "message",
        source: "tui",
        target: "broadcast",
        payload: {
          action: "schedule_add",
          cron,
          sched_action: action,
          target,
        },
      });
    },
    [bus],
  );

  const removeSchedule = useCallback(
    (id: string) => {
      bus.send({
        type: "message",
        source: "tui",
        target: "broadcast",
        payload: {
          action: "schedule_remove",
          id,
        },
      });
    },
    [bus],
  );

  const addReminder = useCallback(
    (delay: string, target: string, message: string) => {
      bus.send({
        type: "message",
        source: "tui",
        target: "broadcast",
        payload: {
          action: "reminder_add",
          fire_at: delay,
          target,
          message,
        },
      });
    },
    [bus],
  );

  return { schedules, reminders, addSchedule, removeSchedule, addReminder };
}
