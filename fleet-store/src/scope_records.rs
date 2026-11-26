use redb::TableDefinition;

pub const SCOPES: TableDefinition<u128, &[u8]> = TableDefinition::new("agent_scopes");
pub const REQUESTS: TableDefinition<&str, &[u8]> = TableDefinition::new("agent_scope_requests");

/// Returns the key ranges of receipts for requests made by an agent's tool calls.
///
/// Receipt keys are JSON arrays that start with the agent ID, or with `"tool_calls"` and the ID.
pub fn receipts(agent: u128) -> [(String, String); 2] {
    [format!("[{agent}"), format!("[\"tool_calls\",{agent}")]
        .map(|prefix| (format!("{prefix},"), format!("{prefix}-")))
}
