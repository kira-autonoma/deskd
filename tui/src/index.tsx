#!/usr/bin/env node
/**
 * Entry point for deskd TUI.
 *
 * Usage:
 *   npx tsx tui/src/index.tsx [--socket /path/to/bus.sock]
 *   bun run tui/src/index.tsx [--socket /path/to/bus.sock]
 */

import { render } from "ink";
import { App } from "./app.js";
import { BusClient } from "./bus.js";
import { join } from "path";
import { homedir } from "os";

function parseArgs(argv: string[]): { socketPath: string } {
  const args = argv.slice(2);
  let socketPath = "";

  for (let i = 0; i < args.length; i++) {
    if (args[i] === "--socket" && i + 1 < args.length) {
      socketPath = args[i + 1]!;
      i++;
    }
  }

  if (!socketPath) {
    // Try DESKD_BUS_SOCKET env var, then default
    socketPath =
      process.env["DESKD_BUS_SOCKET"] ||
      join(homedir(), ".deskd", "bus.sock");
  }

  return { socketPath };
}

const { socketPath } = parseArgs(process.argv);

const bus = new BusClient(socketPath);
bus.connect();

const { waitUntilExit } = render(<App bus={bus} />, {
  exitOnCtrlC: true,
});

waitUntilExit().then(() => {
  bus.disconnect();
  process.exit(0);
});
