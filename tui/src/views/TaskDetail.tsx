import { Box, Text } from "ink";
import { colors } from "../theme.js";
import type { ViewProps } from "./types.js";

export function TaskDetail(_props: ViewProps) {
  return (
    <Box flexDirection="column" padding={1}>
      <Text bold color={colors.primary}>
        Task Detail
      </Text>
      <Text color={colors.textDim}>Task progress, output, and history</Text>
    </Box>
  );
}
