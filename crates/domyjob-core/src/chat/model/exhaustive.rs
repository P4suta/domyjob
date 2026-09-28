//! Every interleaving of a small scenario, checked state by state.
//!
//! Three machines share a room.
//! The asker asks the responder there, the responder starts, answers, or fails, the asker withdraws,
//! and any machine runs one exchange round with any other, in every order up to [`DEPTH`] steps.
//! Every reachable state must be safe: each machine ends the ask with the earliest ending it stores,
//! a resolved ask is never open, and honest peers never refuse each other's events.
//! From every reachable state, exchanging until nothing moves must converge:
//! the same events everywhere, the same ending, and no rejection left.
//! Most synchronization faults need only a few machines and steps to appear,
//! so this bounded search stands in for a proof over small scopes.

use alloc::collections::{BTreeSet, VecDeque};
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::fmt::Write as _;

use super::Model;
use crate::chat::event::{Body, Chain, Event, Intent, Members, Outcome};
use crate::chat::exchange::Outbox as _;
use crate::chat::fixtures::{agent, origin, text};
use crate::chat::id::{AgentId, Conversation, RoomId, RoomName};
use crate::chat::ledger::{Ledger as _, Rejection};
use crate::chat::policy::audience;

const MACHINES: [char; 3] = ['a', 'b', 'c'];

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Step {
    Ask,
    Start,
    Answer,
    Fail,
    Withdraw,
    Exchange(usize, usize),
}

#[derive(Debug, Clone)]
struct World {
    /// Whether the ask is direct rather than in the room.
    direct: bool,
    models: [Model; 3],
    ask: Option<Event>,
    /// The steps that each happen at most once.
    taken: BTreeSet<Step>,
}

fn asker() -> AgentId {
    agent("asker", 'a')
}

fn responder() -> AgentId {
    agent("responder", 'b')
}

fn room() -> RoomId {
    RoomId::new(
        origin('a'),
        RoomName::try_from(String::from("all")).unwrap(),
    )
}

fn members() -> Vec<AgentId> {
    alloc::vec![asker(), responder(), agent("watcher", 'c')]
}

/// A follow-up of the ask by `by`: an answer, or an ending with `outcome`.
fn follow_up(request: &Event, by: &AgentId, outcome: Option<Outcome>) -> Result<Body, String> {
    let (conversation, audience) = request
        .body()
        .thread()
        .ok_or_else(|| String::from("an ask is a message"))?;
    Ok(match outcome {
        None => Body::Message {
            conversation: conversation.clone(),
            from: by.name().clone(),
            text: text("answer"),
            audience: audience.clone(),
            intent: Intent::Reply {
                request: request.id().clone(),
            },
            at: 0,
        },
        Some(outcome) => Body::Resolved {
            request: request.id().clone(),
            conversation: conversation.clone(),
            audience: audience.clone(),
            agent: by.name().clone(),
            outcome,
        },
    })
}

/// Whether `event` is an ending of `ask`: the responder's answer, or a resolution.
fn ends(event: &Event, ask: &Event) -> bool {
    match event.body() {
        Body::Message {
            from,
            intent: Intent::Reply { request },
            ..
        } => {
            request == ask.id()
                && from == responder().name()
                && event.origin() == responder().origin()
        }
        Body::Resolved { request, .. } => request == ask.id(),
        Body::Profile { .. }
        | Body::Left { .. }
        | Body::Machine { .. }
        | Body::Room { .. }
        | Body::RoomClosed { .. }
        | Body::Message { .. }
        | Body::TurnStarted { .. }
        | Body::Omitted {} => false,
    }
}

impl World {
    fn new(direct: bool) -> Self {
        let mut models = MACHINES.map(|digit| Model::new(origin(digit)));
        if let Some(owner) = models.first_mut() {
            owner
                .write(Body::Room {
                    name: RoomName::try_from(String::from("all")).unwrap(),
                    topic: None,
                    members: Members::try_from(members()).unwrap(),
                })
                .unwrap();
        }
        Self {
            direct,
            models,
            ask: None,
            taken: BTreeSet::new(),
        }
    }

    /// Whether the responder's machine has stored the ask.
    fn responder_knows(&self) -> bool {
        self.ask.as_ref().is_some_and(|ask| {
            self.models
                .get(1)
                .is_some_and(|model| model.ordered().iter().any(|event| event.id() == ask.id()))
        })
    }

    fn enabled(&self) -> Vec<Step> {
        let mut steps = Vec::new();
        if self.ask.is_none() {
            steps.push(Step::Ask);
        }
        if self.responder_knows() {
            steps.extend([Step::Start, Step::Answer, Step::Fail]);
        }
        if self.ask.is_some() {
            steps.push(Step::Withdraw);
        }
        steps.retain(|step| !self.taken.contains(step));
        for from in 0..MACHINES.len() {
            for to in 0..MACHINES.len() {
                if from != to {
                    steps.push(Step::Exchange(from, to));
                }
            }
        }
        steps
    }

    fn write(&mut self, machine: usize, body: Body) -> Result<Event, String> {
        self.models
            .get_mut(machine)
            .ok_or_else(|| String::from("no such machine"))?
            .write(body)
            .map_err(|failure| format!("a valid local write was refused: {failure:?}"))
    }

    /// One exchange round from `from` to `to`; only a missing dependency may stop events.
    fn exchange(&mut self, from: usize, to: usize) -> Result<bool, String> {
        let (low, high) = (from.min(to), from.max(to));
        let (left, right) = self.models.split_at_mut(high);
        let (Some(first), Some(second)) = (left.get_mut(low), right.first_mut()) else {
            return Err(String::from("no such pair of machines"));
        };
        let progress = if from < to {
            first.sync_with(second, 1)
        } else {
            second.sync_with(first, 1)
        }
        .map_err(|rejection| format!("an exchange between honest peers failed: {rejection:?}"))?;
        for rejection in [&progress.received.rejected, &progress.refused]
            .into_iter()
            .flatten()
        {
            if !matches!(rejection, Rejection::MissingDependency { .. }) {
                return Err(format!("honest peers refused an event: {rejection:?}"));
            }
        }
        Ok(progress.received.appended > 0 || progress.more)
    }

    fn apply(&mut self, step: Step) -> Result<(), String> {
        if !matches!(step, Step::Exchange(..)) {
            self.taken.insert(step);
        }
        let ask = self.ask.clone();
        match (step, ask) {
            (Step::Ask, _) => {
                let (conversation, audience) = if self.direct {
                    (
                        Conversation::direct(&asker(), &responder())
                            .map_err(|_invalid| String::from("direct conversation"))?,
                        audience([&asker(), &responder()]),
                    )
                } else {
                    (Conversation::Room(room()), audience(members().iter()))
                };
                let body = Body::Message {
                    conversation,
                    from: asker().name().clone(),
                    text: text("question"),
                    audience: audience.map_err(|_invalid| String::from("audience"))?,
                    intent: Intent::Ask {
                        responder: responder(),
                        chain: Chain::default(),
                    },
                    at: 0,
                };
                self.ask = Some(self.write(0, body)?);
            }
            (Step::Start, Some(ask)) => {
                let (conversation, audience) = ask
                    .body()
                    .thread()
                    .ok_or_else(|| String::from("an ask is a message"))?;
                let body = Body::TurnStarted {
                    request: ask.id().clone(),
                    conversation: conversation.clone(),
                    audience: audience.clone(),
                    agent: responder().name().clone(),
                };
                self.write(1, body)?;
            }
            (Step::Answer, Some(ask)) => {
                self.write(1, follow_up(&ask, &responder(), None)?)?;
            }
            (Step::Fail, Some(ask)) => {
                self.write(1, follow_up(&ask, &responder(), Some(Outcome::Failed))?)?;
            }
            (Step::Withdraw, Some(ask)) => {
                self.write(0, follow_up(&ask, &asker(), Some(Outcome::Withdrawn))?)?;
            }
            (Step::Exchange(from, to), _) => {
                self.exchange(from, to)?;
            }
            (Step::Start | Step::Answer | Step::Fail | Step::Withdraw, None) => {
                return Err(String::from("a follow-up was enabled before its ask"));
            }
        }
        Ok(())
    }

    /// Everything that distinguishes this state from another.
    fn key(&self) -> String {
        let mut key = String::new();
        for model in &self.models {
            for event in model.ordered() {
                let _written = write!(
                    key,
                    "{}{}",
                    event.id(),
                    matches!(event.body(), Body::Omitted {})
                );
            }
            for peer in MACHINES {
                let _written = write!(
                    key,
                    "|{}:{}",
                    model.ack(&origin(peer)).unwrap(),
                    model.cursor(&origin(peer)).unwrap().seen
                );
            }
            key.push('#');
        }
        let _written = write!(key, "{:?}", self.taken);
        key
    }

    /// Each machine ends the ask with the earliest ending it stores, and a resolved ask is never open.
    ///
    /// Machines may disagree until they exchange, because each sees only the endings it stores.
    fn check_safety(&self) -> Result<(), String> {
        let Some(ask) = &self.ask else {
            return Ok(());
        };
        for (machine, model) in self.models.iter().enumerate() {
            let earliest = model
                .ordered()
                .into_iter()
                .find(|event| ends(event, ask))
                .map(|event| event.id().clone());
            let resolution = model.resolutions().get(ask.id());
            if resolution.map(|ending| ending.event.clone()) != earliest {
                return Err(format!(
                    "machine {machine} ends the ask with {resolution:?}, not its earliest ending {earliest:?}"
                ));
            }
            if resolution.is_some() && model.open_asks().iter().any(|(_, id)| id == ask.id()) {
                return Err(format!("machine {machine} keeps a resolved ask open"));
            }
        }
        Ok(())
    }

    /// Exchange between every pair until nothing moves, then require one shared history.
    fn check_convergence(&self) -> Result<(), String> {
        let mut settled = self.clone();
        let mut rounds = 0_usize;
        loop {
            let mut moved = false;
            for from in 0..MACHINES.len() {
                for to in 0..MACHINES.len() {
                    if from != to {
                        moved |= settled.exchange(from, to)?;
                    }
                }
            }
            if !moved {
                break;
            }
            rounds = rounds.saturating_add(1);
            if rounds > 16 {
                return Err(String::from("exchanges never settled"));
            }
        }
        settled.check_safety()?;
        let histories: BTreeSet<Vec<_>> = settled
            .models
            .iter()
            .map(|model| {
                model
                    .ordered()
                    .iter()
                    .map(|event| event.id().clone())
                    .collect()
            })
            .collect();
        if histories.len() > 1 {
            return Err(String::from("settled machines store different events"));
        }
        if let Some(ask) = &settled.ask {
            let audience = ask
                .body()
                .thread()
                .map(|(_, audience)| audience.clone())
                .ok_or_else(|| String::from("an ask is a message"))?;
            // A machine outside the audience stores only placeholders and knows no ending.
            let endings: BTreeSet<_> = settled
                .models
                .iter()
                .filter(|model| audience.includes(model.local()))
                .map(|model| {
                    model
                        .resolutions()
                        .get(ask.id())
                        .map(|ending| ending.event.clone())
                })
                .collect();
            if endings.len() > 1 {
                return Err(format!(
                    "settled machines end the ask differently: {endings:?}"
                ));
            }
        }
        Ok(())
    }
}

/// Visit every state reachable from the start, checking each; returns how many there are.
fn explore(direct: bool) -> usize {
    let mut seen = BTreeSet::new();
    let mut queue = VecDeque::from([(World::new(direct), Vec::new())]);
    while let Some((world, path)) = queue.pop_front() {
        if !seen.insert(world.key()) {
            continue;
        }
        assert!(
            seen.len() < 100_000,
            "the scenario has finitely many states"
        );
        let checked = world
            .check_safety()
            .and_then(|()| world.check_convergence());
        assert!(checked.is_ok(), "{} after {path:?}", checked.unwrap_err());
        for step in world.enabled() {
            let mut next = world.clone();
            let applied = next.apply(step);
            let mut longer = path.clone();
            longer.push(step);
            assert!(applied.is_ok(), "{} after {longer:?}", applied.unwrap_err());
            queue.push_back((next, longer));
        }
    }
    seen.len()
}

#[test]
fn every_interleaving_of_a_room_ask_is_safe_and_converges() {
    assert!(explore(false) > 1000, "the search visits the interleavings");
}

#[test]
fn every_interleaving_of_a_direct_ask_is_safe_and_converges() {
    assert!(explore(true) > 1000, "the search visits the interleavings");
}
