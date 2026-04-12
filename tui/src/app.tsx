/**
 * Root App component — view switching, global navigation, status bar.
 *
 * Keys:
 *   1-8  Switch views
 *   q    Quit TUI (deskd keeps running)
 *   Q    Send shutdown to deskd, then quit
 *   ?    Toggle help overlay
 */

import { useState, useEffect, useCallback } from "react";
import { Box, Text, useApp, useInput } from "ink";
import type { BusClient, ConnectionState, BusMessage } from "./bus.js";
import { colors, viewLabels, symbols } from "./theme.js";

// Views
import { Dashboard } from "./views/Dashboard.js";
import { BusStream } from "./views/BusStream.js";
import { AgentDetail } from "./views/AgentDetail.js";
import { TaskDetail } from "./views/TaskDetail.js";
import { WorkflowView } from "./views/WorkflowView.js";
import { CostTracker } from "./views/CostTracker.js";
import { TaskQueue } from "./views/TaskQueue.js";
import { Schedules } from "./views/Schedules.js";

interface AppProps {
  bus: BusClient;
}

const views = [
  Dashboard,
  BusStream,
  AgentDetail,
  TaskDetail,
  WorkflowView,
  CostTracker,
  TaskQueue,
  Schedules,
] as const;

export function App({ bus }: AppProps) {
  const { exit } = useApp();
  const [activeView, setActiveView] = useState(1);
  const [connectionState, setConnectionState] =
    useState<ConnectionState>("disconnected");
  const [showHelp, setShowHelp] = useState(false);
  const [debugMessages, setDebugMessages] = useState<BusMessage[]>([]);

  useEffect(() => {
    const onStateChange = (state: ConnectionState) => {
      setConnectionState(state);
    };
    const onMessage = (msg: BusMessage) => {
      setDebugMessages((prev) => {
        const next = [...prev, msg];
        return next.length > 50 ? next.slice(-50) : next;
      });
    };

    bus.on("stateChange", onStateChange);
    bus.on("message", onMessage);
    setConnectionState(bus.state);

    return () => {
      bus.off("stateChange", onStateChange);
      bus.off("message", onMessage);
    };
  }, [bus]);

  const handleQuit = useCallback(() => {
    bus.disconnect();
    exit();
  }, [bus, exit]);

  const handleForceQuit = useCallback(() => {
    bus.sendShutdown();
    setTimeout(() => {
      bus.disconnect();
      exit();
    }, 200);
  }, [bus, exit]);

  useInput((input, key) => {
    // View switching: 1-8
    const num = parseInt(input, 10);
    if (num >= 1 && num <= 8) {
      setActiveView(num);
      setShowHelp(false);
      return;
    }

    if (input === "q" && !key.shift) {
      handleQuit();
      return;
    }

    if (input === "Q" || (input === "q" && key.shift)) {
      handleForceQuit();
      return;
    }

    if (input === "?") {
      setShowHelp((prev) => !prev);
      return;
    }
  });

  const ViewComponent = views[activeView - 1];

  return (
    <Box flexDirection="column" width="100%" height="100%">
      {/* Tab bar */}
      <Box>
        {Object.entries(viewLabels).map(([num, label]) => {
          const n = parseInt(num, 10);
          const isActive = n === activeView;
          return (
            <Box key={num} marginRight={1}>
              <Text
                color={isActive ? colors.tabActive : colors.tabInactive}
                bold={isActive}
              >
                {num}:{label}
              </Text>
            </Box>
          );
        })}
        <Box flexGrow={1} />
        {/* Connection indicator */}
        <Text
          color={
            connectionState === "connected"
              ? colors.statusConnected
              : connectionState === "connecting"
                ? colors.statusBusy
                : colors.statusDisconnected
          }
        >
          {connectionState === "connected"
            ? symbols.connected
            : symbols.disconnected}{" "}
          {connectionState}
        </Text>
      </Box>

      {/* Separator */}
      <Box>
        <Text color={colors.tabBorder}>
          {colors.tabBorder
            ? symbols.horizontal.repeat(80)
            : symbols.horizontal.repeat(80)}
        </Text>
      </Box>

      {/* Help overlay or active view */}
      {showHelp ? (
        <HelpOverlay />
      ) : (
        <Box flexGrow={1} flexDirection="column">
          <ViewComponent bus={bus} messages={debugMessages} />
        </Box>
      )}
    </Box>
  );
}

function HelpOverlay() {
  return (
    <Box flexDirection="column" padding={1}>
      <Text bold color={colors.primary}>
        Keyboard Shortcuts
      </Text>
      <Text> </Text>
      <Text>
        <Text bold>1-8</Text> Switch views
      </Text>
      <Text>
        <Text bold>q</Text> {"  "}Quit TUI (deskd keeps running)
      </Text>
      <Text>
        <Text bold>Q</Text> {"  "}Send shutdown to deskd + quit
      </Text>
      <Text>
        <Text bold>?</Text> {"  "}Toggle this help
      </Text>
      <Text> </Text>
      <Text color={colors.textDim}>Press any key to dismiss</Text>
    </Box>
  );
}
