/**
 * Hook: cost tracking aggregation from bus messages.
 *
 * Listens for usage_stats, cost updates, and task completions to build
 * per-agent cost breakdown. Supports period filtering (today/7d/30d/all).
 */

import { useState, useEffect, useMemo, useCallback } from "react";
import type { BusClient, BusMessage } from "../bus.js";

export type CostPeriod = "today" | "7d" | "30d" | "all";

export interface AgentCost {
  agent: string;
  tasks: number;
  costUsd: number;
  inputTokens: number;
  outputTokens: number;
  durationMs: number;
}

export interface HourlyCost {
  hour: number; // 0-23
  costUsd: number;
}

export interface CostTotals {
  tasks: number;
  costUsd: number;
  inputTokens: number;
  outputTokens: number;
  durationMs: number;
}

interface CostEntry {
  agent: string;
  costUsd: number;
  inputTokens: number;
  outputTokens: number;
  durationMs: number;
  timestamp: number;
}

const MAX_ENTRIES = 5000;

function periodStart(period: CostPeriod): number {
  if (period === "all") return 0;
  const now = new Date();
  switch (period) {
    case "today": {
      const start = new Date(now);
      start.setHours(0, 0, 0, 0);
      return start.getTime();
    }
    case "7d":
      return now.getTime() - 7 * 24 * 60 * 60 * 1000;
    case "30d":
      return now.getTime() - 30 * 24 * 60 * 60 * 1000;
  }
}

function extractCostEntry(msg: BusMessage): CostEntry | null {
  const payload = msg.payload as Record<string, unknown> | undefined;
  if (!payload) return null;

  // usage_stats response
  if (msg.type === "usage_stats" || payload.action === "usage_stats") {
    const agent = (payload.agent as string) || msg.source || "unknown";
    return {
      agent,
      costUsd: typeof payload.cost_usd === "number" ? payload.cost_usd : 0,
      inputTokens: typeof payload.input_tokens === "number" ? payload.input_tokens : 0,
      outputTokens: typeof payload.output_tokens === "number" ? payload.output_tokens : 0,
      durationMs: typeof payload.duration_ms === "number" ? payload.duration_ms : 0,
      timestamp: typeof payload.timestamp === "number" ? payload.timestamp : Date.now(),
    };
  }

  // Cost update on task completion
  if (payload.cost_usd !== undefined && msg.source) {
    return {
      agent: msg.source,
      costUsd: typeof payload.cost_usd === "number" ? payload.cost_usd : 0,
      inputTokens: typeof payload.input_tokens === "number" ? payload.input_tokens : 0,
      outputTokens: typeof payload.output_tokens === "number" ? payload.output_tokens : 0,
      durationMs: typeof payload.duration_ms === "number" ? payload.duration_ms : 0,
      timestamp: Date.now(),
    };
  }

  // Task result with cost info
  if (payload.result !== undefined && msg.source && typeof payload.cost === "number") {
    return {
      agent: msg.source,
      costUsd: payload.cost as number,
      inputTokens: typeof payload.input_tokens === "number" ? payload.input_tokens : 0,
      outputTokens: typeof payload.output_tokens === "number" ? payload.output_tokens : 0,
      durationMs: typeof payload.duration_ms === "number" ? payload.duration_ms : 0,
      timestamp: Date.now(),
    };
  }

  return null;
}

export interface UseCostsResult {
  agents: AgentCost[];
  totals: CostTotals;
  hourly: HourlyCost[];
  budgetUsd: number;
  period: CostPeriod;
  setPeriod: (p: CostPeriod) => void;
}

export function useCosts(bus: BusClient): UseCostsResult {
  const [entries, setEntries] = useState<CostEntry[]>([]);
  const [period, setPeriod] = useState<CostPeriod>("today");
  const [budgetUsd] = useState(50); // default budget

  useEffect(() => {
    const onMessage = (msg: BusMessage) => {
      const entry = extractCostEntry(msg);
      if (!entry) return;

      setEntries((prev) => {
        const next = [...prev, entry];
        return next.length > MAX_ENTRIES ? next.slice(-MAX_ENTRIES) : next;
      });
    };

    bus.on("message", onMessage);

    // Request usage stats from bus
    bus.send({
      type: "message",
      source: "tui",
      target: "broadcast",
      payload: { action: "get_usage_stats" },
    });

    return () => {
      bus.off("message", onMessage);
    };
  }, [bus]);

  const filtered = useMemo(() => {
    const start = periodStart(period);
    return entries.filter((e) => e.timestamp >= start);
  }, [entries, period]);

  const agents = useMemo((): AgentCost[] => {
    const map = new Map<string, AgentCost>();
    for (const e of filtered) {
      const existing = map.get(e.agent);
      if (existing) {
        existing.tasks += 1;
        existing.costUsd += e.costUsd;
        existing.inputTokens += e.inputTokens;
        existing.outputTokens += e.outputTokens;
        existing.durationMs += e.durationMs;
      } else {
        map.set(e.agent, {
          agent: e.agent,
          tasks: 1,
          costUsd: e.costUsd,
          inputTokens: e.inputTokens,
          outputTokens: e.outputTokens,
          durationMs: e.durationMs,
        });
      }
    }
    return Array.from(map.values()).sort((a, b) => b.costUsd - a.costUsd);
  }, [filtered]);

  const totals = useMemo((): CostTotals => {
    return agents.reduce(
      (acc, a) => ({
        tasks: acc.tasks + a.tasks,
        costUsd: acc.costUsd + a.costUsd,
        inputTokens: acc.inputTokens + a.inputTokens,
        outputTokens: acc.outputTokens + a.outputTokens,
        durationMs: acc.durationMs + a.durationMs,
      }),
      { tasks: 0, costUsd: 0, inputTokens: 0, outputTokens: 0, durationMs: 0 },
    );
  }, [agents]);

  const hourly = useMemo((): HourlyCost[] => {
    const hours = new Array<number>(24).fill(0);
    for (const e of filtered) {
      const h = new Date(e.timestamp).getHours();
      hours[h] += e.costUsd;
    }
    return hours.map((costUsd, hour) => ({ hour, costUsd }));
  }, [filtered]);

  const setPeriodCb = useCallback((p: CostPeriod) => {
    setPeriod(p);
  }, []);

  return { agents, totals, hourly, budgetUsd, period, setPeriod: setPeriodCb };
}
