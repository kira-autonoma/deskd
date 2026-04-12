import { Box, Text } from "ink";
import { colors } from "../theme.js";
import type { ViewProps } from "./types.js";

export function WorkflowView(_props: ViewProps) {
  return (
    <Box flexDirection="column" padding={1}>
      <Text bold color={colors.primary}>
        Workflows
      </Text>
      <Text color={colors.textDim}>State machine instances and transitions</Text>
    </Box>
  );
}
