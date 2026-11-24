//! Stalled agents and their ancestors' stalled descendant counts, maintained with all feature sets.

use crate::{
    StoreError, data,
    records::{self, AgentRecord, Records},
};
use redb::Table;
use std::collections::BTreeSet;

/// The agents changed by resuming.
#[derive(Default)]
pub struct Resumed {
    /// The resumed agents and whether they have an interrupt intent.
    pub agents: Vec<(u128, bool)>,
    /// The resumed agents and their ancestors.
    pub changed: BTreeSet<u128>,
}

/// Stalls the agent and returns the ancestors whose counts changed.
pub fn stall(
    records: &mut Records<Table<'_, &'static [u8], &'static [u8]>>,
    agent: &mut AgentRecord,
    reason: data::StallReason,
) -> Result<Vec<u128>, StoreError> {
    if agent.stalled.replace(reason).is_some() {
        return Ok(Vec::new());
    }
    update_ancestors(records, agent, |count| count.checked_add(1))
}

/// Records a mutation that clears the agent's stall and returns the ancestors whose counts
/// changed.
///
/// The agent's revision must already have advanced for the mutation.
pub fn clear(
    records: &mut Records<Table<'_, &'static [u8], &'static [u8]>>,
    agent: &mut AgentRecord,
) -> Result<Vec<u128>, StoreError> {
    agent.stall_token = agent.revision;
    if agent.stalled.take().is_none() {
        return Ok(Vec::new());
    }
    update_ancestors(records, agent, |count| count.checked_sub(1))
}

/// Counts the stalled descendants of a new agent's existing children and returns the ancestors
/// whose counts changed.
///
/// Children can exist first when they name a parent that does not exist yet.
pub fn count_children(
    records: &mut Records<Table<'_, &'static [u8], &'static [u8]>>,
    agent: &mut AgentRecord,
) -> Result<Vec<u128>, StoreError> {
    let children =
        crate::agents::children(records, agent.agent.id)?.collect::<Result<Vec<_>, _>>()?;
    for child in children {
        let child = records.agent(child)?;
        agent.stalled_descendants = agent
            .stalled_descendants
            .checked_add(u64::from(child.stalled.is_some()))
            .and_then(|count| count.checked_add(child.stalled_descendants))
            .ok_or(StoreError::UnsupportedFormat)?;
    }
    let stalled = agent.stalled_descendants;
    if stalled == 0 {
        return Ok(Vec::new());
    }
    update_ancestors(records, agent, |count| count.checked_add(stalled))
}

/// Resumes a stalled agent and, with `subtree`, its stalled descendants.
///
/// Only children that are stalled or have stalled descendants are visited.
pub fn resume_agents(
    records: &mut Records<Table<'_, &'static [u8], &'static [u8]>>,
    id: u128,
    subtree: bool,
) -> Result<Resumed, StoreError> {
    let mut resumed = Resumed::default();
    let mut pending = vec![id];
    let mut visited = BTreeSet::new();
    while let Some(id) = pending.pop() {
        if !visited.insert(id) {
            continue;
        }
        let mut agent = records.agent(id)?;
        if subtree && agent.stalled_descendants != 0 {
            for child in crate::agents::children(records, id)?.collect::<Result<Vec<_>, _>>()? {
                let child_record = records.agent(child)?;
                if child_record.stalled.is_some() || child_record.stalled_descendants != 0 {
                    pending.push(child);
                }
            }
        }
        if agent.stalled.is_none() {
            continue;
        }
        let activity = agent.activity();
        agent.revision = agent
            .revision
            .checked_add(1)
            .expect("revisions do not overflow");
        resumed.changed.extend(clear(records, &mut agent)?);
        records.update_activity(&mut agent, &activity)?;
        records.save_agent(&agent)?;
        crate::agents::sync_agent(records, &agent, crate::agents::now_millis())?;
        resumed.changed.insert(id);
        resumed.agents.push((id, agent.interrupt_intent));
    }
    Ok(resumed)
}

fn update_ancestors(
    records: &mut Records<Table<'_, &'static [u8], &'static [u8]>>,
    agent: &AgentRecord,
    update: impl Fn(u64) -> Option<u64>,
) -> Result<Vec<u128>, StoreError> {
    let now = crate::agents::now_millis();
    let mut ancestors = Vec::new();
    let mut parent = crate::metadata::agent_parent(&agent.agent.extensions).map(|(id, _)| id);
    while let Some(id) = parent {
        if id == agent.agent.id || ancestors.contains(&id) {
            return Err(StoreError::UnsupportedFormat);
        }
        let Some(mut ancestor) =
            records.get::<AgentRecord>(&records::key(records::AGENT, id, &[]))?
        else {
            break;
        };
        let activity = ancestor.activity();
        ancestor.revision = ancestor
            .revision
            .checked_add(1)
            .expect("revisions do not overflow");
        ancestor.stalled_descendants =
            update(ancestor.stalled_descendants).ok_or(StoreError::UnsupportedFormat)?;
        records.update_activity(&mut ancestor, &activity)?;
        records.save_agent(&ancestor)?;
        crate::agents::sync_agent(records, &ancestor, now)?;
        ancestors.push(id);
        parent = crate::metadata::agent_parent(&ancestor.agent.extensions).map(|(id, _)| id);
    }
    Ok(ancestors)
}
