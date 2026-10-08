//! A presentation model of one root session's subagent tree.
//!
//! Front-ends fold the runtime's [`AgentUpdate`] stream into a [`SubagentRoster`] to learn which
//! agents exist, who spawned whom, and each agent's lifecycle state. Model events and directed
//! messages belong to an agent's transcript and leave the roster unchanged.

use super::model::{AgentDescriptor, AgentId, AgentStatus, AgentUpdate};
use nanocodex::{HarnessModel as Model, Thinking};
use serde::{Serialize, Serializer};

/// The agents of one root session's task tree and the concurrency limit they share.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
pub struct SubagentRoster {
    /// The process-wide concurrency limit for subagents.
    pub max_subagents: usize,
    /// Agents in arrival order. A `parent` that is not listed makes the agent a root of the tree.
    pub agents: Vec<SubagentNode>,
}

/// One agent in a [`SubagentRoster`].
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct SubagentNode {
    /// The agent's identifier within its root session.
    pub id: AgentId,
    /// The agent that spawned this one; `None` for a direct child of the root session.
    pub parent: Option<AgentId>,
    /// The child's own Nanocodex session.
    pub session_id: String,
    /// The short role assigned at spawn.
    pub role: String,
    /// The current task, replaced when a delegate message is delivered.
    pub task: String,
    /// The child's model, serialized as its canonical identifier.
    #[serde(serialize_with = "serialize_model")]
    pub model: Model,
    /// The child's assigned reasoning effort.
    pub thinking: Thinking,
    /// The most recently reported lifecycle state.
    pub status: AgentStatus,
}

impl SubagentRoster {
    /// Creates an empty roster for a runtime with the given concurrency limit.
    pub fn new(max_subagents: usize) -> Self {
        Self {
            max_subagents,
            agents: Vec::new(),
        }
    }

    /// Folds one runtime update in and reports whether the roster changed. Events and directed
    /// messages belong to an agent's transcript, not to the roster.
    pub fn apply(&mut self, update: &AgentUpdate) -> bool {
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

    /// Returns the agent with `id`, if it has been announced.
    pub fn agent(&self, id: AgentId) -> Option<&SubagentNode> {
        self.agents.iter().find(|node| node.id == id)
    }

    fn agent_mut(&mut self, id: AgentId) -> Option<&mut SubagentNode> {
        self.agents.iter_mut().find(|node| node.id == id)
    }

    /// Agents that still own or are stopping work.
    pub fn active_count(&self) -> usize {
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
            model: descriptor.model,
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

fn serialize_model<S: Serializer>(model: &Model, serializer: S) -> Result<S::Ok, S::Error> {
    serializer.serialize_str(model.as_str())
}

#[cfg(test)]
mod tests {
    use super::SubagentRoster;
    use crate::{AgentDescriptor, AgentId, AgentStatus, AgentUpdate};
    use nanocodex::{HarnessModel as Model, Model as CodexModel, Thinking};
    use serde_json::json;

    fn added(id: u64, parent: Option<u64>, task: &str) -> AgentUpdate {
        AgentUpdate::Added(AgentDescriptor {
            id: AgentId::new(id),
            session_id: format!("child-{id}"),
            model: Model::Codex(CodexModel::Sol),
            thinking: Thinking::High,
            role: "reviewer".to_owned(),
            task: task.to_owned(),
            parent: parent.map(AgentId::new),
        })
    }

    #[test]
    fn folds_additions_and_status_changes_and_ignores_transcript_updates() {
        let mut roster = SubagentRoster::new(4);
        assert!(roster.apply(&added(1, None, "audit")));
        assert!(roster.apply(&added(2, Some(1), "trace")));
        assert_eq!(roster.active_count(), 2);

        let completed = AgentUpdate::Status {
            id: AgentId::new(2),
            status: AgentStatus::Completed {
                output: json!({ "ok": true }),
            },
        };
        assert!(roster.apply(&completed));
        assert!(
            !roster.apply(&completed),
            "an unchanged status is not a change"
        );
        assert!(!roster.apply(&AgentUpdate::Status {
            id: AgentId::new(9),
            status: AgentStatus::Running,
        }));
        assert_eq!(roster.active_count(), 1);

        assert!(roster.apply(&added(2, Some(1), "trace again")));
        let child = roster.agent(AgentId::new(2)).unwrap();
        assert_eq!(child.task, "trace again");
        assert!(
            matches!(child.status, AgentStatus::Completed { .. }),
            "a new task keeps the agent's last known status"
        );
        assert_eq!(roster.agents.len(), 2);
    }

    #[test]
    fn serializes_the_wire_shape() {
        let mut roster = SubagentRoster::new(3);
        roster.apply(&added(1, None, "audit"));
        assert_eq!(
            serde_json::to_value(&roster).unwrap(),
            json!({
                "max_subagents": 3,
                "agents": [{
                    "id": 1,
                    "parent": null,
                    "session_id": "child-1",
                    "role": "reviewer",
                    "task": "audit",
                    "model": Model::Codex(CodexModel::Sol).as_str(),
                    "thinking": "high",
                    "status": { "state": "running" },
                }],
            })
        );
    }
}
