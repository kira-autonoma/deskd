import { Box, Text } from "ink";
import { colors } from "../theme.js";
import type { ViewProps } from "./types.js";

export function CostTracker(_props: ViewProps) {
  return (
    <Box flexDirection="column" padding={1}>
      <Text bold color={colors.primary}>
        Cost Tracker
      </Text>
      <Text color={colors.textDim}>Token usage and cost breakdown</Text>
    </Box>
  );
}
