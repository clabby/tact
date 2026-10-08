// Renders an update_plan call as its checklist.

export type PlanStatus = "pending" | "in_progress" | "completed";

export type Plan = { explanation: string | null; steps: Array<{ step: string; status: PlanStatus }> };

const STATUSES: readonly PlanStatus[] = ["pending", "in_progress", "completed"];

/** The plan an update_plan call carries, or null when its arguments do not hold one. */
export function parsePlan(args: unknown): Plan | null {
  if (typeof args !== "object" || args === null) return null;
  const { plan, explanation } = args as { plan?: unknown; explanation?: unknown };
  if (!Array.isArray(plan)) return null;
  const steps = plan.flatMap((item): Plan["steps"] => {
    if (typeof item !== "object" || item === null) return [];
    const { step, status } = item as { step?: unknown; status?: unknown };
    if (typeof step !== "string") return [];
    return [{ step, status: STATUSES.find((known) => known === status) ?? "pending" }];
  });
  return { explanation: typeof explanation === "string" && explanation ? explanation : null, steps };
}

/** Builds the checklist: the optional explanation above one row per step, marked by its status. */
export function presentPlan(plan: Plan) {
  const root = document.createElement("div");
  root.className = "plan";
  if (plan.explanation) {
    const note = document.createElement("p");
    note.className = "plan-note";
    note.textContent = plan.explanation;
    root.append(note);
  }
  const list = document.createElement("ol");
  list.className = "plan-steps";
  for (const { step, status } of plan.steps) {
    const item = document.createElement("li");
    item.className = "plan-step";
    item.dataset.status = status;
    if (status === "in_progress") item.setAttribute("aria-current", "step");
    const mark = document.createElement("span");
    mark.className = "plan-mark";
    mark.setAttribute("aria-label", status.replace("_", " "));
    const text = document.createElement("span");
    text.className = "plan-text";
    text.textContent = step;
    item.append(mark, text);
    list.append(item);
  }
  root.append(list);
  return root;
}

