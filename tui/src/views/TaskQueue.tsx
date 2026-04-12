/**
 * Task Queue view (View 7) — full task list with management.
 *
 * Features:
 *   - Full task list with status icons
 *   - Status filter: Tab cycles all/pending/active/done/failed/dead_letter
 *   - Sort toggle: s cycles created/updated/cost
 *   - a: create task (opens MessageComposer)
 *   - c: cancel selected task (with ConfirmDialog)
 *   - Up/Down for selection, scrolling
 */

import { useState, useMemo } from "react";
import { Box, Text, useInput } from "ink";
import { colors, symbols } from "../theme.js";
import type { ViewProps } from "./types.js";
import { useTasks, type TaskStatus } from "../hooks/useTasks.js";
import { ConfirmDialog } from "../components/ConfirmDialog.js";
import { MessageComposer } from "../components/MessageComposer.js";

type StatusFilter = "all" | TaskStatus | "dead_letter";
type SortField = "created" | "updated" | "cost";

const FILTERS: StatusFilter[] = ["all", "pending", "running", "done", "failed", "cancelled"];
const FILTER_LABELS: Record<StatusFilter, string> = {
  all: "All",
  pending: "Pending",
  running: "Active",
  done: "Done",
  failed: "Failed",
  cancelled: "Dead Letter",
  dead_letter: "Dead Letter",
};

const SORT_FIELDS: SortField[] = ["created", "updated", "cost"];

const statusIcon: Record<TaskStatus, string> = {
  pending: symbols.disconnected,
  running: symbols.connected,
  done: "\u2713",
  failed: "\u2717",
  cancelled: "\u2620",
};

const statusColor: Record<TaskStatus, string> = {
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

const PAGE_SIZE = 20;

export function TaskQueue({ bus }: ViewProps) {
  const tasks = useTasks(bus);
  const [filter, setFilter] = useState<StatusFilter>("all");
  const [sortField, setSortField] = useState<SortField>("updated");
  const [selectedIndex, setSelectedIndex] = useState(0);
  const [scrollOffset, setScrollOffset] = useState(0);
  const [showConfirm, setShowConfirm] = useState(false);
  const [showComposer, setShowComposer] = useState(false);

  const filtered = useMemo(() => {
    let list = tasks;
    if (filter !== "all") {
      const statusMatch = filter === "dead_letter" ? "cancelled" : filter;
      list = list.filter((t) => t.status === statusMatch);
    }

    // Sort
    return [...list].sort((a, b) => {
      switch (sortField) {
        case "created":
          return b.createdAt - a.createdAt;
        case "updated":
          return b.updatedAt - a.updatedAt;
        case "cost":
          return 0; // tasks don't have cost in TaskState; keep order
        default:
          return 0;
      }
    });
  }, [tasks, filter, sortField]);

  const visibleTasks = filtered.slice(scrollOffset, scrollOffset + PAGE_SIZE);
  const selectedTask = filtered[selectedIndex] ?? null;

  useInput((input, key) => {
    if (showConfirm || showComposer) return;

    if (key.tab) {
      const idx = FILTERS.indexOf(filter);
      const next = FILTERS[(idx + 1) % FILTERS.length]!;
      setFilter(next);
      setSelectedIndex(0);
      setScrollOffset(0);
      return;
    }

    if (input === "s") {
      const idx = SORT_FIELDS.indexOf(sortField);
      setSortField(SORT_FIELDS[(idx + 1) % SORT_FIELDS.length]!);
      return;
    }

    if (input === "a") {
      setShowComposer(true);
      return;
    }

    if (input === "c" && selectedTask) {
      setShowConfirm(true);
      return;
    }

    if (key.upArrow) {
      setSelectedIndex((prev) => {
        const next = Math.max(0, prev - 1);
        if (next < scrollOffset) setScrollOffset(next);
        return next;
      });
      return;
    }

    if (key.downArrow) {
      setSelectedIndex((prev) => {
        const next = Math.min(filtered.length - 1, prev + 1);
        if (next >= scrollOffset + PAGE_SIZE) setScrollOffset(next - PAGE_SIZE + 1);
        return next;
      });
      return;
    }
  });

  const handleCancelTask = () => {
    if (!selectedTask) return;
    bus.send({
      type: "message",
      source: "tui",
      target: "broadcast",
      payload: {
        action: "task_status",
        task_id: selectedTask.id,
        status: "cancelled",
      },
    });
    setShowConfirm(false);
  };

  const handleCreateTask = (text: string) => {
    bus.send({
      type: "message",
      source: "tui",
      target: "broadcast",
      payload: {
        action: "task_create",
        task: text,
      },
    });
    setShowComposer(false);
  };

  return (
    <Box flexDirection="column" flexGrow={1}>
      {/* Header with filter tabs */}
      <Box paddingX={1}>
        <Text bold color={colors.primary}>
          Task Queue
        </Text>
        <Box marginLeft={2}>
          {FILTERS.map((f) => (
            <Box key={f} marginRight={1}>
              <Text
                color={f === filter ? colors.accent : colors.textDim}
                bold={f === filter}
              >
                {FILTER_LABELS[f]}
              </Text>
            </Box>
          ))}
        </Box>
        <Box flexGrow={1} />
        <Text color={colors.textDim}>
          sort:{sortField} | {filtered.length} tasks
        </Text>
      </Box>

      {/* Task list */}
      <Box
        flexDirection="column"
        borderStyle="single"
        borderColor={colors.tabBorder}
        paddingX={1}
        flexGrow={1}
      >
        {/* Column header */}
        <Text bold color={colors.secondary}>
          {"  "}Status{"  "}Time{"     "}Assignee{"    "}Title
        </Text>
        <Text color={colors.tabBorder}>
          {"─".repeat(72)}
        </Text>

        {filtered.length === 0 ? (
          <Text color={colors.textDim}>No tasks matching filter</Text>
        ) : (
          visibleTasks.map((t, visIdx) => {
            const globalIdx = scrollOffset + visIdx;
            const isSelected = globalIdx === selectedIndex;
            return (
              <Text key={t.id} wrap="truncate">
                <Text color={isSelected ? colors.accent : colors.textDim}>
                  {isSelected ? symbols.arrow : " "}
                </Text>
                <Text color={statusColor[t.status]}>
                  {statusIcon[t.status]}
                </Text>
                {" "}
                <Text color={isSelected ? colors.text : statusColor[t.status]}>
                  {t.status.padEnd(10)}
                </Text>
                <Text color={colors.textDim}>
                  {formatTimestamp(t.updatedAt)}
                </Text>
                {"  "}
                <Text color={colors.muted}>
                  {(t.assignee ?? "—").padEnd(12)}
                </Text>
                <Text
                  color={isSelected ? colors.textBright : colors.text}
                  bold={isSelected}
                >
                  {t.title}
                </Text>
              </Text>
            );
          })
        )}

        {filtered.length > PAGE_SIZE ? (
          <Text color={colors.textDim}>
            Showing {scrollOffset + 1}-{Math.min(scrollOffset + PAGE_SIZE, filtered.length)} of {filtered.length}
          </Text>
        ) : null}
      </Box>

      {/* Overlays */}
      {showConfirm && selectedTask ? (
        <ConfirmDialog
          message={`Cancel task "${selectedTask.title}"?`}
          onConfirm={handleCancelTask}
          onCancel={() => setShowConfirm(false)}
        />
      ) : null}

      {showComposer ? (
        <MessageComposer
          target="new task"
          onSend={handleCreateTask}
          onCancel={() => setShowComposer(false)}
        />
      ) : null}

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
          Tab:filter  s:sort  a:create  c:cancel  Up/Down:select  1-8:views  ?:help
        </Text>
      </Box>
    </Box>
  );
}
