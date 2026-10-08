//! The contract shared by stateful UI components.
//!
//! A component owns its state, reacts to events in [`Component::update`], and draws itself in
//! [`Component::render`]. Updates leave work outside the component, such as
//! starting turns or touching the clipboard, to their parent: they return effects for it to
//! interpret, and a render request telling the event loop how soon the screen must be redrawn.
//! Parents translate child effects into their own and merge child render requests, so the event
//! loop only sees the effects of the outermost component.

use crate::app::theme::Theme;
use ratatui::{Frame, layout::Rect};

/// How urgently a state change needs to reach the screen. Variants are ordered by urgency, so
/// combining requests keeps the maximum.
#[derive(Clone, Copy, Debug, Default, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) enum RenderRequest {
    /// Nothing visible changed.
    #[default]
    None,
    /// Output arrived that can wait for the next frame slot, so bursts of streamed content are
    /// drawn at the frame rate rather than once per event.
    Streaming,
    /// The change answers user input and is drawn without waiting for the frame interval.
    Immediate,
}

/// The result of one [`Component::update`]: effects in the order they must run, and the render
/// request the change needs.
pub(crate) struct ComponentUpdate<E> {
    pub(crate) effects: Vec<E>,
    pub(crate) render: RenderRequest,
}

impl<E> ComponentUpdate<E> {
    pub(crate) fn none() -> Self {
        Self {
            effects: Vec::new(),
            render: RenderRequest::None,
        }
    }

    pub(crate) fn render(render: RenderRequest) -> Self {
        Self {
            effects: Vec::new(),
            render,
        }
    }

    /// Appends another update's effects after this one's and keeps the more urgent render request.
    pub(crate) fn merge(&mut self, other: Self) {
        self.effects.extend(other.effects);
        self.render = self.render.max(other.render);
    }
}

pub(crate) trait Component {
    type Event;
    type Effect;

    fn update(&mut self, event: Self::Event) -> ComponentUpdate<Self::Effect>;

    fn render(&mut self, frame: &mut Frame<'_>, area: Rect, theme: &Theme);
}
