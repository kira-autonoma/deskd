/**
 * Unix socket client for the deskd bus.
 *
 * Connects to the deskd bus socket, registers as "tui" with wildcard
 * subscription, sends/receives newline-delimited JSON messages,
 * and auto-reconnects on disconnect with exponential backoff.
 */

import { createConnection, type Socket } from "net";
import { EventEmitter } from "events";

export interface BusMessage {
  type: string;
  id?: string;
  source?: string;
  target?: string;
  payload?: unknown;
  [key: string]: unknown;
}

export type ConnectionState = "disconnected" | "connecting" | "connected";

const MIN_RECONNECT_MS = 500;
const MAX_RECONNECT_MS = 10_000;
const BACKOFF_FACTOR = 1.5;

export class BusClient extends EventEmitter {
  private socketPath: string;
  private socket: Socket | null = null;
  private buffer = "";
  private reconnectDelay = MIN_RECONNECT_MS;
  private reconnectTimer: ReturnType<typeof setTimeout> | null = null;
  private closed = false;
  private _state: ConnectionState = "disconnected";
  private _messages: BusMessage[] = [];
  private maxMessages = 2000;

  constructor(socketPath: string) {
    super();
    this.socketPath = socketPath;
  }

  get state(): ConnectionState {
    return this._state;
  }

  /** Recent messages (capped ring buffer). */
  get messages(): readonly BusMessage[] {
    return this._messages;
  }

  connect(): void {
    if (this.closed) return;
    this.setState("connecting");

    const sock = createConnection(this.socketPath, () => {
      this.setState("connected");
      this.reconnectDelay = MIN_RECONNECT_MS;

      // Register as TUI with wildcard subscription
      const registration = JSON.stringify({
        type: "register",
        name: "tui",
        subscriptions: ["*"],
      });
      sock.write(registration + "\n");
    });

    sock.setEncoding("utf8");

    sock.on("data", (chunk: string) => {
      this.buffer += chunk;
      const lines = this.buffer.split("\n");
      // Keep the last incomplete line in the buffer
      this.buffer = lines.pop() ?? "";

      for (const line of lines) {
        const trimmed = line.trim();
        if (!trimmed) continue;
        try {
          const msg: BusMessage = JSON.parse(trimmed);
          this._messages.push(msg);
          if (this._messages.length > this.maxMessages) {
            this._messages.shift();
          }
          this.emit("message", msg);
        } catch {
          // Non-JSON line — ignore
        }
      }
    });

    sock.on("error", () => {
      // Error handled in close event
    });

    sock.on("close", () => {
      this.socket = null;
      if (!this.closed) {
        this.setState("disconnected");
        this.scheduleReconnect();
      }
    });

    this.socket = sock;
  }

  /** Send a JSON message to the bus. */
  send(msg: BusMessage): boolean {
    if (!this.socket || this._state !== "connected") return false;
    try {
      this.socket.write(JSON.stringify(msg) + "\n");
      return true;
    } catch {
      return false;
    }
  }

  /** Send a shutdown signal to deskd. */
  sendShutdown(): void {
    this.send({
      type: "message",
      source: "tui",
      target: "broadcast",
      payload: { action: "shutdown" },
    });
  }

  /** Disconnect and stop reconnecting. */
  disconnect(): void {
    this.closed = true;
    if (this.reconnectTimer) {
      clearTimeout(this.reconnectTimer);
      this.reconnectTimer = null;
    }
    if (this.socket) {
      this.socket.destroy();
      this.socket = null;
    }
    this.setState("disconnected");
  }

  private setState(state: ConnectionState): void {
    if (this._state !== state) {
      this._state = state;
      this.emit("stateChange", state);
    }
  }

  private scheduleReconnect(): void {
    if (this.closed) return;
    this.reconnectTimer = setTimeout(() => {
      this.reconnectTimer = null;
      this.connect();
    }, this.reconnectDelay);
    this.reconnectDelay = Math.min(
      this.reconnectDelay * BACKOFF_FACTOR,
      MAX_RECONNECT_MS
    );
  }
}
