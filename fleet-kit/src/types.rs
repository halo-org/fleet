use fleet_core::{EventCursor, TurnEnd};

// === Turns ===

/// An event marking the end of a turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TurnEndEvent {
    /// The cursor of the event that ended the turn.
    pub cursor: EventCursor,
    /// The payload of this event.
    pub payload: TurnEnd,
}

// === Events ===

/// The direction to read events.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum EventDirection {
    /// Reads from oldest to newest.
    Forward,
    /// Reads from newest to oldest.
    Backward,
}
