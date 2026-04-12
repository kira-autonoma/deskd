/**
 * Hook: raw bus message ring buffer.
 *
 * Subscribes to all bus messages and maintains a capped ring buffer
 * of the most recent 2000 messages. Provides filtering utilities.
 */

import { useState, useEffect, useCallback, useRef } from "react";
import type { BusClient, BusMessage } from "../bus.js";

const RING_BUFFER_CAP = 2000;

export interface BusFilter {
  source?: string;
  target?: string;
  type?: string;
  freetext?: string;
}

/** Parse filter string like "source:tui target:agent:dev some text" */
export function parseFilter(input: string): BusFilter {
  const filter: BusFilter = {};
  const freeWords: string[] = [];

  const tokens = input.trim().split(/\s+/);
  for (const token of tokens) {
    if (token.startsWith("source:")) {
      filter.source = token.slice(7);
    } else if (token.startsWith("target:")) {
      filter.target = token.slice(7);
    } else if (token.startsWith("type:")) {
      filter.type = token.slice(5);
    } else {
      freeWords.push(token);
    }
  }

  if (freeWords.length > 0) {
    filter.freetext = freeWords.join(" ");
  }

  return filter;
}

/** Check if a message matches a filter. */
export function matchesFilter(msg: BusMessage, filter: BusFilter): boolean {
  if (filter.source && msg.source !== filter.source) return false;
  if (filter.target && msg.target !== filter.target) return false;
  if (filter.type && msg.type !== filter.type) return false;
  if (filter.freetext) {
    const text = JSON.stringify(msg).toLowerCase();
    if (!text.includes(filter.freetext.toLowerCase())) return false;
  }
  return true;
}

export interface UseBusResult {
  messages: BusMessage[];
  filtered: (filter: BusFilter) => BusMessage[];
  tail: (count: number) => BusMessage[];
}

export function useBus(bus: BusClient): UseBusResult {
  const [messages, setMessages] = useState<BusMessage[]>([]);
  const messagesRef = useRef<BusMessage[]>([]);

  useEffect(() => {
    // Seed with existing messages from bus client
    const existing = [...bus.messages];
    messagesRef.current = existing;
    setMessages(existing);

    const onMessage = (msg: BusMessage) => {
      const next = [...messagesRef.current, msg];
      if (next.length > RING_BUFFER_CAP) {
        next.splice(0, next.length - RING_BUFFER_CAP);
      }
      messagesRef.current = next;
      setMessages(next);
    };

    bus.on("message", onMessage);
    return () => {
      bus.off("message", onMessage);
    };
  }, [bus]);

  const filtered = useCallback(
    (filter: BusFilter): BusMessage[] => {
      return messages.filter((m) => matchesFilter(m, filter));
    },
    [messages],
  );

  const tail = useCallback(
    (count: number): BusMessage[] => {
      return messages.slice(-count);
    },
    [messages],
  );

  return { messages, filtered, tail };
}
