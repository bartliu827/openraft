use std::error::Error;
use std::ops::Deref;

use tokio::time::Instant;

use crate::engine::LogIdList;
use crate::entry::RaftEntry;
use crate::equal;
use crate::error::ForwardToLeader;
use crate::less_equal;
use crate::node::Node;
use crate::raft_types::RaftLogId;
use crate::utime::UTime;
use crate::validate::Validate;
use crate::LogId;
use crate::LogIdOptionExt;
use crate::MembershipState;
use crate::NodeId;
use crate::ServerState;
use crate::SnapshotMeta;
use crate::Vote;

mod log_state_reader;
mod vote_state_reader;

#[cfg(test)]
mod tests {
    mod forward_to_leader_test;
    mod log_state_reader_test;
    mod validate_test;
}

pub(crate) use log_state_reader::LogStateReader;
pub(crate) use vote_state_reader::VoteStateReader;

/// A struct used to represent the raft state which a Raft node needs.
#[derive(Clone, Debug)]
#[derive(Default)]
#[derive(PartialEq, Eq)]
pub struct RaftState<NID, N>
where
    NID: NodeId,
    N: Node,
{
    /// The vote state of this node.
    pub(crate) vote: UTime<Vote<NID>>,

    /// The LogId of the last log committed(AKA applied) to the state machine.
    ///
    /// - Committed means: a log that is replicated to a quorum of the cluster and it is of the term
    ///   of the leader.
    ///
    /// - A quorum could be a uniform quorum or joint quorum.
    pub committed: Option<LogId<NID>>,

    pub(crate) purged_next: u64,

    /// All log ids this node has.
    pub log_ids: LogIdList<NID>,

    /// The latest cluster membership configuration found, in log or in state machine.
    pub membership_state: MembershipState<NID, N>,

    /// The metadata of the last snapshot.
    pub snapshot_meta: SnapshotMeta<NID, N>,

    // --
    // -- volatile fields: they are not persisted.
    // --
    pub server_state: ServerState,

    /// The log id upto which the next time it purges.
    ///
    /// If a log is in use by a replication task, the purge is postponed and is stored in this
    /// field.
    pub(crate) purge_upto: Option<LogId<NID>>,
}

impl<NID, N> LogStateReader<NID> for RaftState<NID, N>
where
    NID: NodeId,
    N: Node,
{
    fn get_log_id(&self, index: u64) -> Option<LogId<NID>> {
        self.log_ids.get(index)
    }

    fn last_log_id(&self) -> Option<&LogId<NID>> {
        self.log_ids.last()
    }

    fn committed(&self) -> Option<&LogId<NID>> {
        self.committed.as_ref()
    }

    fn snapshot_last_log_id(&self) -> Option<&LogId<NID>> {
        self.snapshot_meta.last_log_id.as_ref()
    }

    fn purge_upto(&self) -> Option<&LogId<NID>> {
        self.purge_upto.as_ref()
    }

    fn last_purged_log_id(&self) -> Option<&LogId<NID>> {
        if self.purged_next == 0 {
            return None;
        }
        self.log_ids.first()
    }
}

impl<NID, N> VoteStateReader<NID> for RaftState<NID, N>
where
    NID: NodeId,
    N: Node,
{
    fn vote_ref(&self) -> &Vote<NID> {
        self.vote.deref()
    }
}

impl<NID, N> Validate for RaftState<NID, N>
where
    NID: NodeId,
    N: Node,
{
    fn validate(&self) -> Result<(), Box<dyn Error>> {
        if self.purged_next == 0 {
            less_equal!(self.log_ids.first().index(), Some(0));
        } else {
            equal!(self.purged_next, self.log_ids.first().next_index());
        }

        less_equal!(self.last_purged_log_id(), self.purge_upto());
        if self.snapshot_last_log_id().is_none() {
            // There is no snapshot, it is possible the application does not store snapshot, and
            // just restarted. it is just ok.
            // In such a case, we assert the monotonic relation without  snapshot-last-log-id
            less_equal!(self.purge_upto(), self.committed());
        } else {
            less_equal!(self.purge_upto(), self.snapshot_last_log_id());
        }
        less_equal!(self.snapshot_last_log_id(), self.committed());
        less_equal!(self.committed(), self.last_log_id());

        self.membership_state.validate()?;

        Ok(())
    }
}

impl<NID, N> RaftState<NID, N>
where
    NID: NodeId,
    N: Node,
{
    /// Get a reference to the current vote.
    pub fn vote_ref(&self) -> &Vote<NID> {
        self.vote.deref()
    }

    /// Return the last updated time of the vote.
    pub fn vote_last_modified(&self) -> Option<Instant> {
        self.vote.utime()
    }

    /// Append a list of `log_id`.
    ///
    /// The log ids in the input has to be continuous.
    pub(crate) fn extend_log_ids_from_same_leader<'a, LID: RaftLogId<NID> + 'a>(&mut self, new_log_ids: &[LID]) {
        self.log_ids.extend_from_same_leader(new_log_ids)
    }

    pub(crate) fn extend_log_ids<'a, LID: RaftLogId<NID> + 'a>(&mut self, new_log_id: &[LID]) {
        self.log_ids.extend(new_log_id)
    }

    /// Update field `committed` if the input is greater.
    /// If updated, it returns the previous value in a `Some()`.
    #[tracing::instrument(level = "debug", skip_all)]
    pub(crate) fn update_committed(&mut self, committed: &Option<LogId<NID>>) -> Option<Option<LogId<NID>>> {
        if committed.as_ref() > self.committed() {
            let prev = self.committed().copied();

            self.committed = *committed;
            self.membership_state.commit(committed);

            Some(prev)
        } else {
            None
        }
    }

    /// Find the first entry in the input that does not exist on local raft-log,
    /// by comparing the log id.
    pub(crate) fn first_conflicting_index<Ent>(&self, entries: &[Ent]) -> usize
    where Ent: RaftLogId<NID> {
        let l = entries.len();

        for (i, ent) in entries.iter().enumerate() {
            let log_id = ent.get_log_id();

            if !self.has_log_id(log_id) {
                tracing::debug!(
                    at = display(i),
                    entry_log_id = display(log_id),
                    "found nonexistent log id"
                );
                return i;
            }
        }

        tracing::debug!("not found nonexistent");
        l
    }

    #[tracing::instrument(level = "debug", skip_all)]
    pub(crate) fn purge_log(&mut self, upto: &LogId<NID>) {
        self.purged_next = upto.index + 1;
        self.log_ids.purge(upto);
    }

    /// Determine the current server state by state.
    #[tracing::instrument(level = "debug", skip_all)]
    pub(crate) fn calc_server_state(&self, id: &NID) -> ServerState {
        tracing::debug!(
            is_member = display(self.is_voter(id)),
            is_leader = display(self.is_leader(id)),
            is_leading = display(self.is_leading(id)),
            "states"
        );
        if self.is_voter(id) {
            if self.is_leader(id) {
                ServerState::Leader
            } else if self.is_leading(id) {
                ServerState::Candidate
            } else {
                ServerState::Follower
            }
        } else {
            ServerState::Learner
        }
    }

    pub(crate) fn is_voter(&self, id: &NID) -> bool {
        self.membership_state.is_voter(id)
    }

    /// The node is candidate(leadership is not granted by a quorum) or leader(leadership is granted
    /// by a quorum)
    pub(crate) fn is_leading(&self, id: &NID) -> bool {
        self.vote.leader_id().voted_for().as_ref() == Some(id)
    }

    pub(crate) fn is_leader(&self, id: &NID) -> bool {
        self.vote.leader_id().voted_for().as_ref() == Some(id) && self.vote.is_committed()
    }

    pub(crate) fn assign_log_ids<'a, Ent: RaftEntry<NID, N> + 'a>(
        &mut self,
        entries: impl Iterator<Item = &'a mut Ent>,
    ) {
        let mut log_id = LogId::new(
            self.vote_ref().committed_leader_id().unwrap(),
            self.last_log_id().next_index(),
        );
        for entry in entries {
            entry.set_log_id(&log_id);
            tracing::debug!("assign log id: {}", log_id);
            log_id.index += 1;
        }
    }

    /// Build a ForwardToLeader error that contains the leader id and node it knows.
    pub(crate) fn forward_to_leader(&self) -> ForwardToLeader<NID, N> {
        let vote = self.vote_ref();

        if vote.is_committed() {
            // Safe unwrap(): vote that is committed has to already have voted for some node.
            let id = vote.leader_id().voted_for().unwrap();

            // leader may not step down after being removed from `voters`.
            // It does not have to be a voter, being in membership is just enough
            let node = self.membership_state.effective().get_node(&id);
            if let Some(n) = node {
                return ForwardToLeader::new(id, n.clone());
            } else {
                tracing::debug!("id={} is not in membership, when getting leader id", id);
            }
        };

        ForwardToLeader::empty()
    }
}