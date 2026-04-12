import { Box, Text } from "ink";
import { colors } from "../theme.js";
import type { ViewProps } from "./types.js";

export function BusStream({ messages }: ViewProps) {
  const recent = messages.slice(-20);

  return (
    <Box flexDirection="column" padding={1}>
      <Text bold color={colors.primary}>
        Bus Stream
      </Text>
      <Text color={colors.textDim}>Raw bus messages (last {recent.length})</Text>
      <Box flexDirection="column" marginTop={1}>
        {recent.map((msg, i) => (
          <Text key={i} color={colors.text} wrap="truncate">
            {JSON.stringify(msg)}
          </Text>
        ))}
        {recent.length === 0 && (
          <Text color={colors.textDim}>No messages yet</Text>
        )}
      </Box>
    </Box>
  );
}
