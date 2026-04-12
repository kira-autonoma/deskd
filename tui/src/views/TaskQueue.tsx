import { Box, Text } from "ink";
import { colors } from "../theme.js";
import type { ViewProps } from "./types.js";

export function TaskQueue(_props: ViewProps) {
  return (
    <Box flexDirection="column" padding={1}>
      <Text bold color={colors.primary}>
        Task Queue
      </Text>
      <Text color={colors.textDim}>Pending and active tasks</Text>
    </Box>
  );
}
