import { Box, Text } from "ink";
import { colors } from "../theme.js";
import type { ViewProps } from "./types.js";

export function AgentDetail(_props: ViewProps) {
  return (
    <Box flexDirection="column" padding={1}>
      <Text bold color={colors.primary}>
        Agent Detail
      </Text>
      <Text color={colors.textDim}>Agent status, tasks, and logs</Text>
    </Box>
  );
}
