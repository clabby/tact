import type { Subagent } from "../core/wire";

export const NODE_WIDTH = 184;
export const NODE_HEIGHT = 68;
const GAP_X = 16;
const GAP_Y = 44;
const PADDING = 24;

export type GraphNode = { agent: Subagent; x: number; y: number };
export type GraphEdge = { from: number; to: number };
export type GraphLayout = { nodes: GraphNode[]; edges: GraphEdge[]; width: number; height: number };

/** Whether an agent is still working, as opposed to settled or stopped (the terminal's rule). */
export function isActive(agent: Subagent) {
  return ["pending", "running", "closing"].includes(agent.status.state);
}

/**
 * Lays the roster out as a top-down tree: each leaf takes the next column, a parent is centred
 * over its children, and depth sets the row. An agent whose parent is missing from the roster is a
 * root, and members of a parent cycle are broken into roots so every agent is placed once.
 * Coordinates are the top-left corner of each node, in pixels.
 */
export function layoutAgents(agents: readonly Subagent[]): GraphLayout {
  const byId = new Map(agents.map((agent) => [agent.id, agent]));
  const children = new Map<number, Subagent[]>();
  const roots: Subagent[] = [];
  for (const agent of [...agents].sort((a, b) => a.id - b.id)) {
    const parent = agent.parent !== null && agent.parent !== agent.id ? byId.get(agent.parent) : undefined;
    if (parent) children.set(parent.id, [...children.get(parent.id) ?? [], agent]);
    else roots.push(agent);
  }

  const placed = new Map<number, { column: number; depth: number }>();
  const edges: GraphEdge[] = [];
  let nextColumn = 0;
  const place = (agent: Subagent, depth: number): number => {
    const below = (children.get(agent.id) ?? []).filter((child) => !placed.has(child.id));
    placed.set(agent.id, { column: -1, depth });
    const columns = below.map((child) => {
      edges.push({ from: agent.id, to: child.id });
      return place(child, depth + 1);
    });
    const column = columns.length ? (columns[0]! + columns[columns.length - 1]!) / 2 : nextColumn++;
    placed.set(agent.id, { column, depth });
    return column;
  };
  for (const root of roots) place(root, 0);
  // A cycle has no root: start from its lowest id so each member is still shown.
  for (const agent of [...agents].sort((a, b) => a.id - b.id)) {
    if (!placed.has(agent.id)) place(agent, 0);
  }

  const nodes = agents.map((agent): GraphNode => {
    const { column, depth } = placed.get(agent.id)!;
    return {
      agent,
      x: PADDING + column * (NODE_WIDTH + GAP_X),
      y: PADDING + depth * (NODE_HEIGHT + GAP_Y),
    };
  });
  const columns = Math.max(0, ...nodes.map((node) => (node.x - PADDING) / (NODE_WIDTH + GAP_X) + 1));
  const depths = Math.max(0, ...nodes.map((node) => (node.y - PADDING) / (NODE_HEIGHT + GAP_Y) + 1));
  return {
    nodes,
    edges,
    width: nodes.length ? PADDING * 2 + columns * (NODE_WIDTH + GAP_X) - GAP_X : 0,
    height: nodes.length ? PADDING * 2 + depths * (NODE_HEIGHT + GAP_Y) - GAP_Y : 0,
  };
}
