/**
 * Schedules view (View 8) — cron schedules and reminders.
 *
 * Features:
 *   - Top section: cron schedules table (expression, action, target, next fire)
 *   - Bottom section: reminders
 *   - a: add schedule (text input prompts)
 *   - d: delete selected (with ConfirmDialog)
 *   - r: add reminder (text input prompts)
 *   - Tab: switch focus between schedules/reminders
 *   - Up/Down for selection
 */

import { useState } from "react";
import { Box, Text, useInput } from "ink";
import { colors, symbols } from "../theme.js";
import type { ViewProps } from "./types.js";
import { useSchedules } from "../hooks/useSchedules.js";
import { ConfirmDialog } from "../components/ConfirmDialog.js";

type InputMode = "none" | "schedule_cron" | "schedule_action" | "schedule_target" | "reminder_delay" | "reminder_target" | "reminder_message";

function padRight(s: string, width: number): string {
  return s.length >= width ? s.slice(0, width) : s + " ".repeat(width - s.length);
}

export function Schedules({ bus }: ViewProps) {
  const { schedules, reminders, addSchedule, removeSchedule, addReminder } = useSchedules(bus);
  const [focusSection, setFocusSection] = useState<"schedules" | "reminders">("schedules");
  const [schedSelIdx, setSchedSelIdx] = useState(0);
  const [remSelIdx, setRemSelIdx] = useState(0);
  const [showConfirm, setShowConfirm] = useState(false);

  // Multi-step input state
  const [inputMode, setInputMode] = useState<InputMode>("none");
  const [inputText, setInputText] = useState("");
  const [inputBuffer, setInputBuffer] = useState<Record<string, string>>({});

  const selectedScheduleId = schedules[schedSelIdx]?.id;
  const selectedReminderId = reminders[remSelIdx]?.id;

  useInput((input, key) => {
    // Handle text input modes
    if (inputMode !== "none") {
      if (key.escape) {
        setInputMode("none");
        setInputText("");
        setInputBuffer({});
        return;
      }
      if (key.return) {
        handleInputSubmit();
        return;
      }
      if (key.backspace || key.delete) {
        setInputText((prev) => prev.slice(0, -1));
        return;
      }
      if (input && !key.ctrl && !key.meta) {
        setInputText((prev) => prev + input);
      }
      return;
    }

    if (showConfirm) return;

    if (key.tab) {
      setFocusSection((prev) => (prev === "schedules" ? "reminders" : "schedules"));
      return;
    }

    if (key.upArrow) {
      if (focusSection === "schedules") {
        setSchedSelIdx((prev) => Math.max(0, prev - 1));
      } else {
        setRemSelIdx((prev) => Math.max(0, prev - 1));
      }
      return;
    }

    if (key.downArrow) {
      if (focusSection === "schedules") {
        setSchedSelIdx((prev) => Math.min(schedules.length - 1, Math.max(0, prev + 1)));
      } else {
        setRemSelIdx((prev) => Math.min(reminders.length - 1, Math.max(0, prev + 1)));
      }
      return;
    }

    if (input === "a") {
      setInputMode("schedule_cron");
      setInputText("");
      setInputBuffer({});
      return;
    }

    if (input === "r") {
      setInputMode("reminder_delay");
      setInputText("");
      setInputBuffer({});
      return;
    }

    if (input === "d") {
      setShowConfirm(true);
      return;
    }
  });

  const handleInputSubmit = () => {
    const text = inputText.trim();
    if (!text) return;

    switch (inputMode) {
      case "schedule_cron":
        setInputBuffer({ cron: text });
        setInputText("");
        setInputMode("schedule_action");
        break;
      case "schedule_action":
        setInputBuffer((prev) => ({ ...prev, action: text }));
        setInputText("");
        setInputMode("schedule_target");
        break;
      case "schedule_target":
        addSchedule(inputBuffer.cron ?? "", inputBuffer.action ?? "", text);
        setInputMode("none");
        setInputText("");
        setInputBuffer({});
        break;
      case "reminder_delay":
        setInputBuffer({ delay: text });
        setInputText("");
        setInputMode("reminder_target");
        break;
      case "reminder_target":
        setInputBuffer((prev) => ({ ...prev, target: text }));
        setInputText("");
        setInputMode("reminder_message");
        break;
      case "reminder_message":
        addReminder(inputBuffer.delay ?? "", inputBuffer.target ?? "", text);
        setInputMode("none");
        setInputText("");
        setInputBuffer({});
        break;
      default:
        break;
    }
  };

  const handleDelete = () => {
    if (focusSection === "schedules" && selectedScheduleId) {
      removeSchedule(selectedScheduleId);
    } else if (focusSection === "reminders" && selectedReminderId) {
      removeSchedule(selectedReminderId); // same bus msg removes both
    }
    setShowConfirm(false);
  };

  const inputPrompt = (): string => {
    switch (inputMode) {
      case "schedule_cron":
        return "Enter cron expression (e.g. '0 9 * * *'):";
      case "schedule_action":
        return "Enter action (e.g. 'send_message'):";
      case "schedule_target":
        return "Enter target (e.g. 'agent:dev'):";
      case "reminder_delay":
        return "Enter delay/time (e.g. '30m', '2h', '2024-01-01 09:00'):";
      case "reminder_target":
        return "Enter target (e.g. 'agent:kira'):";
      case "reminder_message":
        return "Enter message:";
      default:
        return "";
    }
  };

  const deleteLabel = focusSection === "schedules" ? "schedule" : "reminder";
  const deleteName =
    focusSection === "schedules"
      ? schedules[schedSelIdx]?.cron ?? ""
      : reminders[remSelIdx]?.message ?? "";

  return (
    <Box flexDirection="column" flexGrow={1}>
      {/* Header */}
      <Box paddingX={1}>
        <Text bold color={colors.primary}>
          Schedules
        </Text>
        <Box marginLeft={2}>
          <Text
            color={focusSection === "schedules" ? colors.accent : colors.textDim}
            bold={focusSection === "schedules"}
          >
            Cron Schedules ({schedules.length})
          </Text>
          <Text color={colors.textDim}> | </Text>
          <Text
            color={focusSection === "reminders" ? colors.accent : colors.textDim}
            bold={focusSection === "reminders"}
          >
            Reminders ({reminders.length})
          </Text>
        </Box>
      </Box>

      {/* Schedules table */}
      <Box
        flexDirection="column"
        borderStyle="single"
        borderColor={focusSection === "schedules" ? colors.primary : colors.tabBorder}
        paddingX={1}
        flexGrow={1}
      >
        <Text bold color={colors.secondary}>
          Cron Schedules
        </Text>
        <Text bold color={colors.textDim}>
          {padRight("Expression", 18)}
          {padRight("Action", 18)}
          {padRight("Target", 18)}
          {"Next Fire"}
        </Text>
        <Text color={colors.tabBorder}>{"─".repeat(72)}</Text>

        {schedules.length === 0 ? (
          <Text color={colors.textDim}>No schedules configured</Text>
        ) : (
          schedules.map((s, i) => {
            const isSelected = focusSection === "schedules" && i === schedSelIdx;
            return (
              <Text key={s.id} wrap="truncate">
                <Text color={isSelected ? colors.accent : colors.textDim}>
                  {isSelected ? symbols.arrow : " "}
                </Text>
                <Text color={isSelected ? colors.textBright : colors.text}>
                  {padRight(s.cron, 18)}
                </Text>
                <Text color={colors.secondary}>
                  {padRight(s.action, 18)}
                </Text>
                <Text color={colors.muted}>
                  {padRight(s.target, 18)}
                </Text>
                <Text color={colors.accent}>
                  {s.nextFire ?? "—"}
                </Text>
              </Text>
            );
          })
        )}
      </Box>

      {/* Reminders table */}
      <Box
        flexDirection="column"
        borderStyle="single"
        borderColor={focusSection === "reminders" ? colors.primary : colors.tabBorder}
        paddingX={1}
        flexGrow={1}
      >
        <Text bold color={colors.secondary}>
          Reminders
        </Text>
        <Text bold color={colors.textDim}>
          {padRight("Fire At", 22)}
          {padRight("Target", 18)}
          {"Message"}
        </Text>
        <Text color={colors.tabBorder}>{"─".repeat(72)}</Text>

        {reminders.length === 0 ? (
          <Text color={colors.textDim}>No reminders set</Text>
        ) : (
          reminders.map((r, i) => {
            const isSelected = focusSection === "reminders" && i === remSelIdx;
            return (
              <Text key={r.id} wrap="truncate">
                <Text color={isSelected ? colors.accent : colors.textDim}>
                  {isSelected ? symbols.arrow : " "}
                </Text>
                <Text color={isSelected ? colors.textBright : colors.accent}>
                  {padRight(r.fireAt, 22)}
                </Text>
                <Text color={colors.muted}>
                  {padRight(r.target, 18)}
                </Text>
                <Text color={isSelected ? colors.textBright : colors.text}>
                  {r.message}
                </Text>
              </Text>
            );
          })
        )}
      </Box>

      {/* Input overlay */}
      {inputMode !== "none" ? (
        <Box
          flexDirection="column"
          borderStyle="single"
          borderColor={colors.primary}
          paddingX={2}
          paddingY={1}
        >
          <Text bold color={colors.primary}>
            {inputPrompt()}
          </Text>
          <Box>
            <Text color={colors.accent}>&gt; </Text>
            <Text color={colors.text}>{inputText}</Text>
            <Text color={colors.accent}>_</Text>
          </Box>
          <Text color={colors.textDim}>
            Enter: submit | Esc: cancel
          </Text>
        </Box>
      ) : null}

      {/* Confirm overlay */}
      {showConfirm ? (
        <ConfirmDialog
          message={`Delete ${deleteLabel} "${deleteName}"?`}
          onConfirm={handleDelete}
          onCancel={() => setShowConfirm(false)}
        />
      ) : null}

      {/* Status bar */}
      <Box
        borderStyle="single"
        borderColor={colors.tabBorder}
        borderTop
        borderBottom={false}
        borderLeft={false}
        borderRight={false}
        paddingX={1}
      >
        <Text color={colors.textDim}>
          Tab:section  a:add schedule  r:add reminder  d:delete  Up/Down:select  1-8:views  ?:help
        </Text>
      </Box>
    </Box>
  );
}
