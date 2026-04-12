import { Box, Text } from "ink";
import { colors } from "../theme.js";
import type { ViewProps } from "./types.js";

export function Schedules(_props: ViewProps) {
  return (
    <Box flexDirection="column" padding={1}>
      <Text bold color={colors.primary}>
        Schedules
      </Text>
      <Text color={colors.textDim}>Cron schedules and next fire times</Text>
    </Box>
  );
}
