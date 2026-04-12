/**
 * Dashboard view (View 1) — 2x2 grid layout.
 *
 * Panels: Agents, Tasks, Workflows, Bus Tail.
 * Status bar at bottom with daily aggregates.
 * Tab cycles focus between panels, Enter for drill-down.
 */

import { useState, useMemo } from "react";
import { Box, Text, useInput } from "ink";
import { colors, symbols } from "../theme.js";
import type { ViewProps } from "./types.js";
import { useAgents, type AgentState } from "../hooks/useAgents.js";
import { useTasks, type TaskState, type TaskStatus } from "../hooks/useTasks.js";
import { useWorkflows, type WorkflowState } from "../hooks/useWorkflows.js";
import { useBus } from "../hooks/useBus.js";
import type { BusMessage } from "../bus.js";

const PANEL_COUNT = 4;
const BUS_TAIL_COUNT = 15;

const statusBadge: Record<AgentState["status"], string> = {
  online: symbols.connected,
  busy: symbols.connected,
  idle: symbols.disconnected,
  offline: symbols.disconnected,
};

const statusColor: Record<AgentState["status"], string> = {
  online: colors.statusConnected,
  busy: colors.statusBusy,
  idle: colors.statusIdle,
  offline: colors.statusDisconnected,
};

const taskIcon: Record<TaskStatus, string> = {
  pending: symbols.disconnected,
  running: symbols.connected,
  done: "\u2713",
  failed: "\u2717",
  cancelled: "\u2620",
};

const taskColor: Record<TaskStatus, string> = {
  pending: colors.textDim,
  running: colors.statusBusy,
  done: colors.success,
  failed: colors.error,
  cancelled: colors.error,
};

function formatTimestamp(ts: number): string {
  const d = new Date(ts);
  const h = d.getHours().toString().padStart(2, "0");
  const m = d.getMinutes().toString().padStart(2, "0");
  const s = d.getSeconds().toString().padStart(2, "0");
  return `${h}:${m}:${s}`;
}

function formatBusMessage(msg: BusMessage): string {
  const src = msg.source ?? "?";
  const tgt = msg.target ?? "*";
  const payload = msg.payload as Record<string, unknown> | undefined;
  let summary = msg.type;
  if (payload) {
    const action = payload.action as string | undefined;
    const task = payload.task as string | undefined;
    const text = payload.text as string | undefined;
    const detail = action || task || text;
    if (detail) {
      const short = typeof detail === "string" && detail.length > 30
        ? detail.slice(0, 27) + "..."
        : detail;
      summary = `${msg.type}: ${short}`;
    }
  }
  return `${src} ${symbols.arrow} ${tgt} ${summary}`;
}

function PanelHeader({ title, focused }: { title: string; focused: boolean }) {
  return (
    <Text bold color={focused ? colors.primary : colors.textDim}>
      {focused ? symbols.arrow + " " : "  "}
      {title}
    </Text>
  );
}

function AgentsPanel({
  agents,
  focused,
}: {
  agents: AgentState[];
  focused: boolean;
}) {
  return (
    <Box
      flexDirection="column"
      borderStyle="single"
      borderColor={focused ? colors.primary : colors.tabBorder}
      paddingX={1}
      flexGrow={1}
      flexBasis="50%"
    >
      <PanelHeader title="Agents" focused={focused} />
      {agents.length === 0 ? (
        <Text color={colors.textDim}>No agents connected</Text>
      ) : (
        agents.slice(0, 8).map((a) => (
          <Text key={a.name} wrap="truncate">
            <Text color={statusColor[a.status]}>
              {statusBadge[a.status]}
            </Text>
            {" "}
            <Text color={colors.text} bold>
              {a.name}
            </Text>
            {a.model ? (
              <Text color={colors.textDim}> [{a.model}]</Text>
            ) : null}
            {a.currentTask ? (
              <Text color={colors.muted}> {a.currentTask}</Text>
            ) : null}
            {a.costUsd > 0 ? (
              <Text color={colors.accent}> ${a.costUsd.toFixed(2)}</Text>
            ) : null}
          </Text>
        ))
      )}
    </Box>
  );
}

function TasksPanel({
  tasks,
  focused,
}: {
  tasks: TaskState[];
  focused: boolean;
}) {
  return (
    <Box
      flexDirection="column"
      borderStyle="single"
      borderColor={focused ? colors.primary : colors.tabBorder}
      paddingX={1}
      flexGrow={1}
      flexBasis="50%"
    >
      <PanelHeader title="Tasks" focused={focused} />
      {tasks.length === 0 ? (
        <Text color={colors.textDim}>No tasks yet</Text>
      ) : (
        tasks.slice(0, 8).map((t) => (
          <Text key={t.id} wrap="truncate">
            <Text color={taskColor[t.status]}>{taskIcon[t.status]}</Text>
            {" "}
            <Text color={colors.text}>{t.title}</Text>
            {t.assignee ? (
              <Text color={colors.textDim}> @{t.assignee}</Text>
            ) : null}
          </Text>
        ))
      )}
    </Box>
  );
}

function WorkflowsPanel({
  workflows,
  focused,
}: {
  workflows: WorkflowState[];
  focused: boolean;
}) {
  return (
    <Box
      flexDirection="column"
      borderStyle="single"
      borderColor={focused ? colors.primary : colors.tabBorder}
      paddingX={1}
      flexGrow={1}
      flexBasis="50%"
    >
      <PanelHeader title="Workflows" focused={focused} />
      {workflows.length === 0 ? (
        <Text color={colors.textDim}>No active workflows</Text>
      ) : (
        workflows.slice(0, 8).map((w) => (
          <Text key={w.id} wrap="truncate">
            <Text color={colors.secondary} bold>
              {w.name}
            </Text>
            <Text color={colors.textDim}> [{w.currentState}]</Text>
            {w.costUsd > 0 ? (
              <Text color={colors.accent}> ${w.costUsd.toFixed(2)}</Text>
            ) : null}
          </Text>
        ))
      )}
    </Box>
  );
}

function BusTailPanel({
  messages,
  focused,
}: {
  messages: BusMessage[];
  focused: boolean;
}) {
  const recent = messages.slice(-BUS_TAIL_COUNT);

  return (
    <Box
      flexDirection="column"
      borderStyle="single"
      borderColor={focused ? colors.primary : colors.tabBorder}
      paddingX={1}
      flexGrow={1}
      flexBasis="50%"
    >
      <PanelHeader title="Bus Tail" focused={focused} />
      {recent.length === 0 ? (
        <Text color={colors.textDim}>No messages</Text>
      ) : (
        recent.map((msg, i) => (
          <Text key={i} color={colors.text} wrap="truncate">
            <Text color={colors.textDim}>
              {formatTimestamp(Date.now())}
            </Text>
            {" "}
            {formatBusMessage(msg)}
          </Text>
        ))
      )}
    </Box>
  );
}

function StatusBar({
  tasks,
  agents,
}: {
  tasks: TaskState[];
  agents: AgentState[];
}) {
  const today = useMemo(() => {
    const start = new Date();
    start.setHours(0, 0, 0, 0);
    return start.getTime();
  }, []);

  const dailyTasks = tasks.filter((t) => t.createdAt >= today).length;
  const doneTasks = tasks.filter(
    (t) => t.status === "done" && t.updatedAt >= today,
  ).length;
  const totalCost = agents.reduce((sum, a) => sum + a.costUsd, 0);
  const busyCount = agents.filter((a) => a.status === "busy").length;

  return (
    <Box paddingX={1}>
      <Text color={colors.textDim}>
        Tasks today: <Text color={colors.text}>{dailyTasks}</Text>
        {" "}({doneTasks} done)
        {"  "}|{"  "}
        Agents: <Text color={colors.text}>{agents.length}</Text>
        {" "}({busyCount} busy)
        {"  "}|{"  "}
        Cost: <Text color={colors.accent}>${totalCost.toFixed(2)}</Text>
        {"  "}|{"  "}
        <Text color={colors.textDim}>Tab:focus  Enter:detail  1-8:views  ?:help</Text>
      </Text>
    </Box>
  );
}

export function Dashboard({ bus }: ViewProps) {
  const [focusedPanel, setFocusedPanel] = useState(0);
  const agents = useAgents(bus);
  const tasks = useTasks(bus);
  const workflows = useWorkflows(bus);
  const { messages } = useBus(bus);

  useInput((input, key) => {
    if (key.tab) {
      setFocusedPanel((prev) => (prev + 1) % PANEL_COUNT);
      return;
    }
    // Enter — placeholder for drill-down navigation
    if (key.return) {
      // Future: emit navigation event based on focused panel
      return;
    }
    // Suppress unused variable warning
    void input;
  });

  return (
    <Box flexDirection="column" flexGrow={1}>
      {/* Top row: Agents + Tasks */}
      <Box flexGrow={1}>
        <AgentsPanel agents={agents} focused={focusedPanel === 0} />
        <TasksPanel tasks={tasks} focused={focusedPanel === 1} />
      </Box>

      {/* Bottom row: Workflows + Bus Tail */}
      <Box flexGrow={1}>
        <WorkflowsPanel workflows={workflows} focused={focusedPanel === 2} />
        <BusTailPanel messages={messages} focused={focusedPanel === 3} />
      </Box>

      {/* Status bar */}
      <Box borderStyle="single" borderColor={colors.tabBorder} borderTop borderBottom={false} borderLeft={false} borderRight={false}>
        <StatusBar tasks={tasks} agents={agents} />
      </Box>
    </Box>
  );
}
