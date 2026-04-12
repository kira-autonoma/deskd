/**
 * Cost Tracker view (View 6) — per-agent cost breakdown.
 *
 * Features:
 *   - Per-agent table: tasks, cost, input/output tokens, duration
 *   - Totals row at bottom
 *   - Period switching: 1=today, 2=7d, 3=30d, 4=all (Tab cycles)
 *   - Hourly cost sparkline (ASCII bar chars)
 *   - Budget bar with percentage
 */

import { Box, Text, useInput } from "ink";
import { colors } from "../theme.js";
import type { ViewProps } from "./types.js";
import { useCosts, type CostPeriod, type HourlyCost } from "../hooks/useCosts.js";

const PERIODS: CostPeriod[] = ["today", "7d", "30d", "all"];
const PERIOD_LABELS: Record<CostPeriod, string> = {
  today: "Today",
  "7d": "7 days",
  "30d": "30 days",
  all: "All time",
};

const SPARKLINE_CHARS = "▁▂▃▄▅▆▇█";

function formatTokens(n: number): string {
  if (n >= 1_000_000) return `${(n / 1_000_000).toFixed(1)}M`;
  if (n >= 1_000) return `${(n / 1_000).toFixed(1)}K`;
  return n.toString();
}

function formatDuration(ms: number): string {
  if (ms < 1000) return `${ms}ms`;
  if (ms < 60_000) return `${(ms / 1000).toFixed(1)}s`;
  if (ms < 3_600_000) return `${Math.floor(ms / 60_000)}m ${Math.floor((ms % 60_000) / 1000)}s`;
  return `${Math.floor(ms / 3_600_000)}h ${Math.floor((ms % 3_600_000) / 60_000)}m`;
}

function sparkline(hourly: HourlyCost[]): string {
  const max = Math.max(...hourly.map((h) => h.costUsd), 0.001);
  return hourly
    .map((h) => {
      const idx = Math.round((h.costUsd / max) * (SPARKLINE_CHARS.length - 1));
      return SPARKLINE_CHARS[idx] ?? SPARKLINE_CHARS[0];
    })
    .join("");
}

function BudgetBar({ used, total }: { used: number; total: number }) {
  const pct = total > 0 ? Math.min((used / total) * 100, 100) : 0;
  const barWidth = 30;
  const filled = Math.round((pct / 100) * barWidth);
  const empty = barWidth - filled;
  const barColor = pct > 90 ? colors.error : pct > 70 ? colors.warning : colors.success;

  return (
    <Box>
      <Text color={colors.textDim}>Budget: </Text>
      <Text color={barColor}>{"█".repeat(filled)}</Text>
      <Text color={colors.textDim}>{"░".repeat(empty)}</Text>
      <Text color={barColor}>
        {" "}${used.toFixed(2)} / ${total.toFixed(2)} ({pct.toFixed(0)}%)
      </Text>
    </Box>
  );
}

function padRight(s: string, width: number): string {
  return s.length >= width ? s.slice(0, width) : s + " ".repeat(width - s.length);
}

function padLeft(s: string, width: number): string {
  return s.length >= width ? s.slice(0, width) : " ".repeat(width - s.length) + s;
}

export function CostTracker({ bus }: ViewProps) {
  const { agents, totals, hourly, budgetUsd, period, setPeriod } = useCosts(bus);

  useInput((input, key) => {
    // Period switching with number keys (shifted by view keys, use Tab)
    if (key.tab) {
      const idx = PERIODS.indexOf(period);
      setPeriod(PERIODS[(idx + 1) % PERIODS.length]!);
      return;
    }

    // Direct period selection: use brackets or function-style
    if (input === "[") {
      const idx = PERIODS.indexOf(period);
      setPeriod(PERIODS[(idx - 1 + PERIODS.length) % PERIODS.length]!);
      return;
    }
    if (input === "]") {
      const idx = PERIODS.indexOf(period);
      setPeriod(PERIODS[(idx + 1) % PERIODS.length]!);
      return;
    }
  });

  // Column widths
  const COL_AGENT = 16;
  const COL_TASKS = 7;
  const COL_COST = 10;
  const COL_INPUT = 10;
  const COL_OUTPUT = 10;
  const COL_DURATION = 12;

  const headerLine =
    padRight("Agent", COL_AGENT) +
    padLeft("Tasks", COL_TASKS) +
    padLeft("Cost", COL_COST) +
    padLeft("In Tokens", COL_INPUT) +
    padLeft("Out Tokens", COL_OUTPUT) +
    padLeft("Duration", COL_DURATION);

  const separator = "─".repeat(
    COL_AGENT + COL_TASKS + COL_COST + COL_INPUT + COL_OUTPUT + COL_DURATION,
  );

  return (
    <Box flexDirection="column" flexGrow={1}>
      {/* Header */}
      <Box paddingX={1}>
        <Text bold color={colors.primary}>
          Cost Tracker
        </Text>
        <Box marginLeft={2}>
          {PERIODS.map((p) => (
            <Box key={p} marginRight={1}>
              <Text
                color={p === period ? colors.accent : colors.textDim}
                bold={p === period}
              >
                {PERIOD_LABELS[p]}
              </Text>
            </Box>
          ))}
        </Box>
      </Box>

      {/* Budget bar */}
      <Box paddingX={1}>
        <BudgetBar used={totals.costUsd} total={budgetUsd} />
      </Box>

      {/* Sparkline */}
      <Box paddingX={1}>
        <Text color={colors.textDim}>Hourly: </Text>
        <Text color={colors.secondary}>{sparkline(hourly)}</Text>
        <Text color={colors.textDim}>
          {" "}(0h{" ".repeat(20)}23h)
        </Text>
      </Box>

      {/* Table */}
      <Box
        flexDirection="column"
        borderStyle="single"
        borderColor={colors.tabBorder}
        paddingX={1}
        marginTop={1}
        flexGrow={1}
      >
        {/* Table header */}
        <Text bold color={colors.secondary}>
          {headerLine}
        </Text>
        <Text color={colors.tabBorder}>{separator}</Text>

        {/* Agent rows */}
        {agents.length === 0 ? (
          <Text color={colors.textDim}>No cost data for this period</Text>
        ) : (
          agents.map((a) => (
            <Text key={a.agent} wrap="truncate">
              <Text color={colors.text}>{padRight(a.agent, COL_AGENT)}</Text>
              <Text color={colors.textDim}>{padLeft(a.tasks.toString(), COL_TASKS)}</Text>
              <Text color={colors.accent}>{padLeft("$" + a.costUsd.toFixed(2), COL_COST)}</Text>
              <Text color={colors.textDim}>{padLeft(formatTokens(a.inputTokens), COL_INPUT)}</Text>
              <Text color={colors.textDim}>{padLeft(formatTokens(a.outputTokens), COL_OUTPUT)}</Text>
              <Text color={colors.textDim}>{padLeft(formatDuration(a.durationMs), COL_DURATION)}</Text>
            </Text>
          ))
        )}

        {/* Totals row */}
        {agents.length > 0 ? (
          <>
            <Text color={colors.tabBorder}>{separator}</Text>
            <Text bold wrap="truncate">
              <Text color={colors.text}>{padRight("TOTAL", COL_AGENT)}</Text>
              <Text color={colors.text}>{padLeft(totals.tasks.toString(), COL_TASKS)}</Text>
              <Text color={colors.accent}>{padLeft("$" + totals.costUsd.toFixed(2), COL_COST)}</Text>
              <Text color={colors.text}>{padLeft(formatTokens(totals.inputTokens), COL_INPUT)}</Text>
              <Text color={colors.text}>{padLeft(formatTokens(totals.outputTokens), COL_OUTPUT)}</Text>
              <Text color={colors.text}>{padLeft(formatDuration(totals.durationMs), COL_DURATION)}</Text>
            </Text>
          </>
        ) : null}
      </Box>

      {/* Status bar */}
      <Box
        borderStyle="single"
        borderColor={colors.tabBorder}
        borderTop
        borderBottom={false}
        borderLeft={false}
        borderRight={false}
        paddingX={1}
      >
        <Text color={colors.textDim}>
          Tab/[/]:period  1-8:views  ?:help
        </Text>
      </Box>
    </Box>
  );
}
