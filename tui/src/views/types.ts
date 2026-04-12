import type { BusClient, BusMessage } from "../bus.js";

/** Common props passed to all view components. */
export interface ViewProps {
  bus: BusClient;
  messages: BusMessage[];
}
