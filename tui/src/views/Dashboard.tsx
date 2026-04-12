import { Box, Text } from "ink";
import { colors } from "../theme.js";
import type { ViewProps } from "./types.js";

export function Dashboard(_props: ViewProps) {
  return (
    <Box flexDirection="column" padding={1}>
      <Text bold color={colors.primary}>
        Dashboard
      </Text>
      <Text color={colors.textDim}>Agent overview, tasks, and system health</Text>
    </Box>
  );
}
