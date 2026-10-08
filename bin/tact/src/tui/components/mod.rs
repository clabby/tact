//! Stateful UI components and their event boundary.

mod actions;
mod app;
mod composer;
mod confirmation;
mod context_diagnostics;
mod dial;
mod effort;
mod file_finder;
mod fit;
mod floating;
mod keybindings;
mod memory;
mod model_selector;
mod node;
mod qr_code;
mod queue;
mod recent_prompt_picker;
mod root;
mod selection;
mod session_picker;
mod sessions;
mod skill_picker;
mod speed;
mod subagent_tree_layout;
mod subagents;
mod theme_selector;
mod transcript;
mod waved_text;

pub(crate) use app::{AppEffect, AppEvent, AppNode};
pub(crate) use node::{ComponentUpdate, RenderRequest};
pub(crate) use root::{
    DraftReset, RecentPromptDraft, RestoredSessionProjection, RootEffect, RootNode, SessionListKind,
};
pub(crate) use transcript::image::initialize as initialize_image_renderer;
