/**
 * Color scheme and styling constants for the deskd TUI.
 *
 * Designed for 256-color terminals. Uses a muted, professional palette
 * inspired by terminal productivity tools.
 */

export const colors = {
  // Primary palette
  primary: "#5B9BD5",
  secondary: "#70AD47",
  accent: "#FFC000",
  error: "#FF5555",
  warning: "#FFB86C",
  success: "#50FA7B",
  muted: "#6272A4",

  // Text
  text: "#F8F8F2",
  textDim: "#6272A4",
  textBright: "#FFFFFF",

  // Backgrounds
  bg: "",
  bgHighlight: "#44475A",
  bgSelected: "#3B4252",

  // Status indicators
  statusConnected: "#50FA7B",
  statusDisconnected: "#FF5555",
  statusIdle: "#6272A4",
  statusBusy: "#FFB86C",

  // Tab bar
  tabActive: "#5B9BD5",
  tabInactive: "#6272A4",
  tabBorder: "#44475A",
} as const;

export const viewLabels: Record<number, string> = {
  1: "Dashboard",
  2: "Bus Stream",
  3: "Agent Detail",
  4: "Task Detail",
  5: "Workflows",
  6: "Cost Tracker",
  7: "Task Queue",
  8: "Schedules",
};

export const symbols = {
  connected: "\u25CF",   // ●
  disconnected: "\u25CB", // ○
  separator: "\u2502",    // │
  horizontal: "\u2500",   // ─
  arrow: "\u25B6",        // ▶
  bullet: "\u2022",       // •
} as const;
