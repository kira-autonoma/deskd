/**
 * BusStream view (View 2) — full-screen live message tail.
 *
 * Features:
 *   - Timestamp, source -> target, payload display
 *   - Follow mode (auto-scroll, toggle with 'f')
 *   - Filter syntax: source:X, target:X, type:event, freetext
 *   - Compact mode toggle ('c') — 1 line vs multi-line per message
 *   - Ring buffer of 2000 messages via useBus hook
 */

import { useState, useMemo } from "react";
import { Box, Text, useInput } from "ink";
import { colors, symbols } from "../theme.js";
import type { ViewProps } from "./types.js";
import {
  useBus,
  parseFilter,
  matchesFilter,
} from "../hooks/useBus.js";
import type { BusMessage } from "../bus.js";

function formatTime(ts: number): string {
  const d = new Date(ts);
  const h = d.getHours().toString().padStart(2, "0");
  const m = d.getMinutes().toString().padStart(2, "0");
  const s = d.getSeconds().toString().padStart(2, "0");
  const ms = d.getMilliseconds().toString().padStart(3, "0");
  return `${h}:${m}:${s}.${ms}`;
}

function formatPayload(payload: unknown, compact: boolean): string {
  if (payload === undefined || payload === null) return "";
  if (typeof payload === "string") return payload;
  try {
    if (compact) {
      const s = JSON.stringify(payload);
      return s.length > 80 ? s.slice(0, 77) + "..." : s;
    }
    return JSON.stringify(payload, null, 2);
  } catch {
    return String(payload);
  }
}

interface MessageWithTimestamp {
  msg: BusMessage;
  receivedAt: number;
}

function CompactMessage({ entry }: { entry: MessageWithTimestamp }) {
  const { msg, receivedAt } = entry;
  const src = msg.source ?? "?";
  const tgt = msg.target ?? "*";
  const payloadStr = formatPayload(msg.payload, true);

  return (
    <Text wrap="truncate">
      <Text color={colors.textDim}>{formatTime(receivedAt)}</Text>
      {" "}
      <Text color={colors.secondary}>{src}</Text>
      <Text color={colors.textDim}> {symbols.arrow} </Text>
      <Text color={colors.primary}>{tgt}</Text>
      {" "}
      <Text color={colors.muted}>[{msg.type}]</Text>
      {payloadStr ? (
        <Text color={colors.text}> {payloadStr}</Text>
      ) : null}
    </Text>
  );
}

function ExpandedMessage({ entry }: { entry: MessageWithTimestamp }) {
  const { msg, receivedAt } = entry;
  const src = msg.source ?? "?";
  const tgt = msg.target ?? "*";
  const payloadStr = formatPayload(msg.payload, false);

  return (
    <Box flexDirection="column" marginBottom={1}>
      <Text>
        <Text color={colors.textDim}>{formatTime(receivedAt)}</Text>
        {" "}
        <Text color={colors.secondary} bold>{src}</Text>
        <Text color={colors.textDim}> {symbols.arrow} </Text>
        <Text color={colors.primary} bold>{tgt}</Text>
        {" "}
        <Text color={colors.muted}>[{msg.type}]</Text>
        {msg.id ? <Text color={colors.textDim}> id:{msg.id}</Text> : null}
      </Text>
      {payloadStr ? (
        <Box paddingLeft={2}>
          <Text color={colors.text}>{payloadStr}</Text>
        </Box>
      ) : null}
    </Box>
  );
}

export function BusStream({ bus }: ViewProps) {
  const [follow, setFollow] = useState(true);
  const [compact, setCompact] = useState(true);
  const [filterText, setFilterText] = useState("");
  const [editing, setEditing] = useState(false);
  const [scrollOffset, setScrollOffset] = useState(0);

  const { messages } = useBus(bus);

  // Attach timestamps (approximate — use current time for display)
  const timestamped: MessageWithTimestamp[] = useMemo(
    () => messages.map((msg) => ({ msg, receivedAt: Date.now() })),
    [messages],
  );

  // Apply filter
  const filter = useMemo(() => parseFilter(filterText), [filterText]);
  const filtered = useMemo(() => {
    if (!filterText.trim()) return timestamped;
    return timestamped.filter((e) => matchesFilter(e.msg, filter));
  }, [timestamped, filterText, filter]);

  // Visible messages
  const maxVisible = compact ? 30 : 12;
  const visible = useMemo(() => {
    if (follow) {
      return filtered.slice(-maxVisible);
    }
    const start = Math.max(0, filtered.length - maxVisible - scrollOffset);
    return filtered.slice(start, start + maxVisible);
  }, [filtered, follow, scrollOffset, maxVisible]);

  useInput((input, key) => {
    if (editing) {
      if (key.return || key.escape) {
        setEditing(false);
        return;
      }
      if (key.backspace || key.delete) {
        setFilterText((prev) => prev.slice(0, -1));
        return;
      }
      if (input && !key.ctrl && !key.meta) {
        setFilterText((prev) => prev + input);
        return;
      }
      return;
    }

    if (input === "f") {
      setFollow((prev) => !prev);
      setScrollOffset(0);
      return;
    }
    if (input === "c") {
      setCompact((prev) => !prev);
      return;
    }
    if (input === "/") {
      setEditing(true);
      return;
    }
    if (key.escape) {
      setFilterText("");
      return;
    }
    if (key.upArrow && !follow) {
      setScrollOffset((prev) => Math.min(prev + 1, filtered.length - maxVisible));
      return;
    }
    if (key.downArrow && !follow) {
      setScrollOffset((prev) => Math.max(prev - 1, 0));
      return;
    }
  });

  return (
    <Box flexDirection="column" flexGrow={1}>
      {/* Header bar */}
      <Box paddingX={1}>
        <Text color={colors.primary} bold>Bus Stream</Text>
        <Text color={colors.textDim}>
          {"  "}|{"  "}
          {filtered.length} msgs
          {"  "}|{"  "}
        </Text>
        <Text color={follow ? colors.success : colors.textDim}>
          {follow ? "FOLLOW" : "PAUSED"}
        </Text>
        <Text color={colors.textDim}>
          {"  "}|{"  "}
          {compact ? "compact" : "expanded"}
        </Text>
        {filterText ? (
          <Text color={colors.accent}>{"  "}filter: {filterText}</Text>
        ) : null}
      </Box>

      {/* Separator */}
      <Box>
        <Text color={colors.tabBorder}>
          {symbols.horizontal.repeat(80)}
        </Text>
      </Box>

      {/* Message list */}
      <Box flexDirection="column" flexGrow={1} paddingX={1}>
        {visible.length === 0 ? (
          <Text color={colors.textDim}>
            {filterText
              ? "No messages match filter"
              : "Waiting for bus messages..."}
          </Text>
        ) : (
          visible.map((entry, i) =>
            compact ? (
              <CompactMessage key={i} entry={entry} />
            ) : (
              <ExpandedMessage key={i} entry={entry} />
            ),
          )
        )}
      </Box>

      {/* Filter / status bar */}
      <Box
        borderStyle="single"
        borderColor={colors.tabBorder}
        borderTop
        borderBottom={false}
        borderLeft={false}
        borderRight={false}
        paddingX={1}
      >
        {editing ? (
          <Text>
            <Text color={colors.accent}>filter: </Text>
            <Text color={colors.text}>{filterText}</Text>
            <Text color={colors.accent}>_</Text>
          </Text>
        ) : (
          <Text color={colors.textDim}>
            /:filter  f:follow  c:compact  Esc:clear  {symbols.arrow}/{symbols.arrow}:scroll
          </Text>
        )}
      </Box>
    </Box>
  );
}
