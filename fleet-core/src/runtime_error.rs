use crate::AgentId;
use core::fmt;

#[derive(Debug)]
pub struct RuntimeError<E> {
    agent_id: Option<AgentId>,
    source: E,
}

impl<E> RuntimeError<E> {
    /// Creates a runtime error with an optional agent ID.
    pub const fn new(agent_id: Option<AgentId>, source: E) -> Self {
        Self { agent_id, source }
    }
}

impl<E> fmt::Display for RuntimeError<E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.agent_id {
            Some(agent_id) => write!(f, "runtime error for agent {agent_id}"),
            None => f.write_str("runtime error"),
        }
    }
}

impl<E> core::error::Error for RuntimeError<E>
where
    E: core::error::Error + 'static,
{
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        Some(&self.source)
    }
}
