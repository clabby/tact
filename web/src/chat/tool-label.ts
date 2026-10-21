/** Names shown for tool calls; unlisted tools show their own name. */
const TOOL_LABELS: Record<string, string> = {
  exec_command: "Shell",
  write_stdin: "Stdin",
  apply_patch: "Patch",
  exec: "Code",
  web__run: "Web",
  view_image: "Image",
  update_plan: "Plan",
  wait: "Wait",
  image_gen__imagegen: "Image",
  spawn_agent: "Agent",
  wait_agent: "Wait",
  list_agents: "Agents",
  close_agent: "Close",
  interrupt_agent: "Stop",
  send_agent_message: "Message",
  message_session: "Message",
  current_session: "Session",
  read_session: "Session",
  find_sessions: "Sessions",
  memory: "Memory",
};

/**
 * The name shown for a tool call. Code Mode runs are a "Batch" once they have called tools of their
 * own; tools without a listed name read their identifier as words.
 */
export function toolLabel(name: string, childCount: number) {
  if (name === "exec") return childCount > 0 ? "Batch" : "Code";
  const label = TOOL_LABELS[name];
  if (label) return label;
  const words = name.replace(/_+/g, " ").trim();
  return words.charAt(0).toUpperCase() + words.slice(1);
}
