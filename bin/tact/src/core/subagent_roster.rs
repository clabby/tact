//! The subagent tree of one root session as front-ends present it: who exists, who spawned whom,
//! and each agent's lifecycle state. The terminal's Subagents overlay and the web interface both
//! render this model, which is folded from the runtime's [`AgentUpdate`]s.

use nanocodex::Thinking;
use serde::Serialize;
use tact_subagents::{AgentDescriptor, AgentId, AgentStatus, AgentUpdate};

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
pub(crate) struct SubagentRoster {
    /// The process-wide concurrency limit for subagents.
    pub(crate) max_subagents: usize,
    /// Agents in arrival order. A `parent` that is not listed makes the agent a root of the tree.
    pub(crate) agents: Vec<SubagentNode>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(crate) struct SubagentNode {
    pub(crate) id: AgentId,
    /// The agent that spawned this one; `None` for a direct child of the root session.
    pub(crate) parent: Option<AgentId>,
    pub(crate) session_id: String,
    pub(crate) role: String,
    pub(crate) task: String,
    /// The canonical model identifier.
    pub(crate) model: &'static str,
    pub(crate) thinking: Thinking,
    pub(crate) status: AgentStatus,
}

impl SubagentRoster {
    pub(crate) fn new(max_subagents: usize) -> Self {
        Self {
            max_subagents,
            agents: Vec::new(),
        }
    }

    /// Folds one runtime update in and reports whether the roster changed. Events and directed
    /// messages belong to an agent's transcript, not to the roster.
    pub(crate) fn apply(&mut self, update: &AgentUpdate) -> bool {
        match update {
            AgentUpdate::Added(descriptor) => {
                if let Some(node) = self.agent_mut(descriptor.id) {
                    node.describe(descriptor);
                } else {
                    self.agents.push(SubagentNode::new(descriptor));
                }
                true
            }
            AgentUpdate::Status { id, status } => {
                let Some(node) = self.agent_mut(*id) else {
                    return false;
                };
                if node.status == *status {
                    return false;
                }
                node.status = status.clone();
                true
            }
            AgentUpdate::Event { .. } | AgentUpdate::Message(_) => false,
        }
    }

    pub(crate) fn agent(&self, id: AgentId) -> Option<&SubagentNode> {
        self.agents.iter().find(|node| node.id == id)
    }

    fn agent_mut(&mut self, id: AgentId) -> Option<&mut SubagentNode> {
        self.agents.iter_mut().find(|node| node.id == id)
    }

    /// Agents that still own or are stopping work.
    pub(crate) fn active_count(&self) -> usize {
        self.agents
            .iter()
            .filter(|node| node.status.is_active())
            .count()
    }
}

impl SubagentNode {
    /// A newly announced agent is running: the runtime announces an agent as it starts its task.
    fn new(descriptor: &AgentDescriptor) -> Self {
        Self {
            id: descriptor.id,
            parent: descriptor.parent,
            session_id: descriptor.session_id.clone(),
            role: descriptor.role.clone(),
            task: descriptor.task.clone(),
            model: descriptor.model.as_str(),
            thinking: descriptor.thinking,
            status: AgentStatus::Running,
        }
    }

    fn describe(&mut self, descriptor: &AgentDescriptor) {
        let status = std::mem::replace(&mut self.status, AgentStatus::Running);
        *self = Self {
            status,
            ..Self::new(descriptor)
        };
    }
}
