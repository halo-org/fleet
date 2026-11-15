use redb::TableDefinition;

pub const SCOPES: TableDefinition<u128, &[u8]> = TableDefinition::new("agent_scopes");
pub const REQUESTS: TableDefinition<&str, &[u8]> = TableDefinition::new("agent_scope_requests");
