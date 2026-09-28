//! Rules for events written on this machine.
//!
//! Admission accepts any well-formed history so every machine converges; these rules only refuse to author events that would be surprising locally.

use alloc::collections::BTreeSet;

use super::event::{Chain, MAX_CHAIN};
use super::id::{AgentId, Audience, Invalid, Origin};
use super::ledger::{Resolution, Room};

/// Why this machine refuses to author an event.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Refusal {
    #[error("an agent cannot ask itself")]
    SelfAsk,
    #[error("asking {responder} would deadlock: it is already waiting on this conversation")]
    Cycle { responder: AgentId },
    #[error("nested asks are limited to {MAX_CHAIN} waiting agents")]
    TooDeep,
    #[error("{agent} is not a member of this room")]
    NotMember { agent: AgentId },
    #[error("this machine already ended that ask")]
    AlreadyResolved,
    #[error("the local chat history is full; run `domyjob chat clean`")]
    Full,
    #[error(transparent)]
    Invalid(#[from] Invalid),
}

/// The chain for an ask sent from inside the turn answering an ask of `waiting`.
pub fn delegated(turn: &Chain, waiting: AgentId) -> Result<Chain, Refusal> {
    if turn.agents().len() >= MAX_CHAIN {
        return Err(Refusal::TooDeep);
    }
    Ok(turn.extended(waiting)?)
}

/// Refuse asks that would wait on the sender or on an agent already waiting on the sender.
pub fn check_ask(sender: &AgentId, responder: &AgentId, chain: &Chain) -> Result<(), Refusal> {
    if sender == responder {
        return Err(Refusal::SelfAsk);
    }
    if chain.contains(responder) || chain.contains(sender) {
        return Err(Refusal::Cycle {
            responder: responder.clone(),
        });
    }
    Ok(())
}

/// Require that an agent belongs to the room it posts in or is asked in.
pub fn check_member(room: &Room, agent: &AgentId) -> Result<(), Refusal> {
    if room.members.contains(agent) {
        Ok(())
    } else {
        Err(Refusal::NotMember {
            agent: agent.clone(),
        })
    }
}

/// Refuse a second ending of the same ask from this machine; other machines' endings may race.
pub fn check_resolution(current: Option<&Resolution>, local: &Origin) -> Result<(), Refusal> {
    match current {
        Some(winner) if winner.event.origin() == local => Err(Refusal::AlreadyResolved),
        Some(_) | None => Ok(()),
    }
}

/// The machines of a set of agents.
pub fn audience<'a>(agents: impl IntoIterator<Item = &'a AgentId>) -> Result<Audience, Invalid> {
    Audience::try_from(
        agents
            .into_iter()
            .map(|agent| agent.origin().clone())
            .collect::<BTreeSet<Origin>>(),
    )
}

/// Stored chat history, counted by events and encoded bytes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Usage {
    pub events: u64,
    pub bytes: u64,
}

/// Whether a local write is ordinary or ends an ask that someone waits on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Priority {
    Ordinary,
    Ending,
}

/// Ordinary local writes stop here so answers, failures, and withdrawals still fit.
pub const ORDINARY_LIMIT: Usage = Usage {
    events: 200_000,
    bytes: 512 * 1024 * 1024,
};
/// Endings of asks may use the reserve up to this limit.
pub const ENDING_LIMIT: Usage = Usage {
    events: 220_000,
    bytes: 576 * 1024 * 1024,
};
/// Events received from peers are always stored below this disk-protection limit.
pub const RECEIVED_LIMIT: Usage = Usage {
    events: 400_000,
    bytes: 1024 * 1024 * 1024,
};

const fn below(usage: Usage, limit: Usage) -> bool {
    usage.events < limit.events && usage.bytes < limit.bytes
}

pub const fn check_capacity(usage: Usage, priority: Priority) -> Result<(), Refusal> {
    let limit = match priority {
        Priority::Ordinary => ORDINARY_LIMIT,
        Priority::Ending => ENDING_LIMIT,
    };
    if below(usage, limit) {
        Ok(())
    } else {
        Err(Refusal::Full)
    }
}

#[must_use]
pub const fn fits_received(usage: Usage) -> bool {
    below(usage, RECEIVED_LIMIT)
}

#[cfg(test)]
mod tests {
    use alloc::vec;

    use super::{
        ENDING_LIMIT, ORDINARY_LIMIT, Priority, Refusal, Usage, check_ask, check_capacity,
        check_member, check_resolution, delegated,
    };
    use crate::chat::event::{Chain, MAX_CHAIN, Members};
    use crate::chat::fixtures::{agent, id, origin};
    use crate::chat::ledger::{Ending, Resolution, Room};

    #[test]
    fn members_post_in_their_room_and_a_machine_ends_an_ask_once() {
        let (alice, bob) = (agent("alice", 'a'), agent("bob", 'b'));
        let room = Room {
            topic: None,
            members: Members::try_from(vec![alice.clone()]).unwrap(),
        };
        check_member(&room, &alice).unwrap();
        assert!(matches!(
            check_member(&room, &bob),
            Err(Refusal::NotMember { agent }) if agent == bob
        ));
        let ending = |digit| Resolution {
            event: id(digit, 3),
            clock: 3,
            ending: Ending::Answered,
            asker: alice.clone(),
            responder: bob.clone(),
        };
        check_resolution(None, &origin('a')).unwrap();
        assert!(matches!(
            check_resolution(Some(&ending('a')), &origin('a')),
            Err(Refusal::AlreadyResolved)
        ));
        check_resolution(Some(&ending('b')), &origin('a')).unwrap();
    }

    #[test]
    fn a_delegated_chain_never_holds_an_agent_twice() {
        let alice = agent("alice", 'a');
        let chain = Chain::try_from(vec![alice.clone()]).unwrap();
        assert!(matches!(delegated(&chain, alice), Err(Refusal::Invalid(_))));
    }

    #[test]
    fn asks_cannot_close_a_waiting_cycle_or_exceed_the_depth() {
        let (alice, bob, carol) = (agent("alice", 'a'), agent("bob", 'b'), agent("carol", 'c'));
        assert_eq!(
            check_ask(&alice, &alice, &Chain::default()),
            Err(Refusal::SelfAsk)
        );
        let chain = delegated(&Chain::default(), bob.clone()).unwrap();
        assert!(matches!(
            check_ask(&alice, &bob, &chain),
            Err(Refusal::Cycle { .. })
        ));
        check_ask(&alice, &carol, &chain).unwrap();
        let full = Chain::try_from(
            (0..MAX_CHAIN)
                .map(|index| agent(&alloc::format!("a{index}"), 'd'))
                .collect::<vec::Vec<_>>(),
        )
        .unwrap();
        assert_eq!(delegated(&full, carol), Err(Refusal::TooDeep));
    }

    #[test]
    fn endings_keep_a_reserve_after_ordinary_writes_stop() {
        let at = |limit: Usage| Usage {
            events: limit.events,
            bytes: 0,
        };
        assert_eq!(
            check_capacity(at(ORDINARY_LIMIT), Priority::Ordinary),
            Err(Refusal::Full)
        );
        check_capacity(at(ORDINARY_LIMIT), Priority::Ending).unwrap();
        assert_eq!(
            check_capacity(at(ENDING_LIMIT), Priority::Ending),
            Err(Refusal::Full)
        );
    }
}
