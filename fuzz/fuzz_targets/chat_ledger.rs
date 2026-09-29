#![no_main]

use domyjob_core::chat::card::{Access, Card, Mode, Skills, Tool};
use domyjob_core::chat::event::{Body, Chain, Event, Intent, Members, Outcome};
use domyjob_core::chat::id::{
    AgentId, AgentName, Conversation, Line, Origin, RoomId, RoomName, Text,
};
use domyjob_core::chat::model::Model;
use domyjob_core::chat::policy::audience;

const MACHINES: usize = 3;

fn origin(machine: usize) -> Origin {
    Origin::from_entropy([u8::try_from(machine).unwrap().wrapping_add(1); 16])
}

fn agent(machine: usize, name: u8) -> AgentId {
    AgentId::new(
        AgentName::try_from(format!("agent{name}")).unwrap(),
        origin(machine),
    )
}

fn room_id() -> RoomId {
    RoomId::new(origin(0), RoomName::try_from("all".to_owned()).unwrap())
}

fn members() -> Vec<AgentId> {
    (0..MACHINES).map(|machine| agent(machine, 0)).collect()
}

fn card(role: u8) -> Box<Card> {
    Box::new(Card {
        display_name: Line::try_from(format!("Agent {role}")).unwrap(),
        role: None,
        description: None,
        skills: Skills::default(),
        project: None,
        status: None,
        tool: Tool::Codex,
        mode: Mode::Managed,
        access: Access::Read,
    })
}

fn message(from: &AgentId, conversation: Conversation, to: &[&AgentId], intent: Intent) -> Body {
    Body::Message {
        conversation,
        from: from.name().clone(),
        text: Text::try_from("text".to_owned()).unwrap(),
        audience: audience(to.iter().copied().chain([from])).unwrap(),
        intent,
        at: 0,
    }
}

fn follow_up(request: &Event, by: &AgentId, outcome: Option<Outcome>) -> Option<Body> {
    let conversation = request.body().conversation()?.clone();
    let audience = request.body().audience()?.clone();
    Some(match outcome {
        None => Body::Message {
            conversation,
            from: by.name().clone(),
            text: Text::try_from("answer".to_owned()).unwrap(),
            audience,
            intent: Intent::Reply {
                request: request.id().clone(),
            },
            at: 0,
        },
        Some(outcome) => Body::Resolved {
            request: request.id().clone(),
            conversation,
            audience,
            agent: by.name().clone(),
            outcome,
        },
    })
}

struct World {
    models: Vec<Model>,
    asks: Vec<(usize, usize, Event)>,
}

impl World {
    fn write(&mut self, machine: usize, body: Body) -> Option<Event> {
        Some(self.models[machine].write(body).expect("a valid local write is admitted"))
    }

    fn try_write(&mut self, machine: usize, body: Body) -> Option<Event> {
        match self.models[machine].write(body) {
            Ok(event) => Some(event),
            Err(domyjob_core::chat::ledger::Failure::Rejected(
                domyjob_core::chat::ledger::Rejection::MissingDependency { .. },
            )) => None,
            Err(other) => panic!("an ending of a known ask is admitted: {other:?}"),
        }
    }

    fn sync(&mut self, first: usize, second: usize, rounds: usize) {
        if first == second {
            return;
        }
        let (low, high) = (first.min(second), first.max(second));
        let (left, right) = self.models.split_at_mut(high);
        let (a, b) = (&mut left[low], &mut right[0]);
        let result = if first < second {
            a.sync_with(b, rounds)
        } else {
            b.sync_with(a, rounds)
        };
        result.expect("a round between known peers is never refused");
    }

    fn step(&mut self, operation: u8, x: usize, y: usize) {
        let (from, to) = (x % MACHINES, y % MACHINES);
        match operation % 8 {
            0 => {
                let body = Body::Profile {
                    agent: agent(from, 1).name().clone(),
                    card: card(u8::try_from(y % 5).unwrap()),
                };
                self.write(from, body);
            }
            1 => {
                let (sender, receiver) = (agent(from, 0), agent(to, 1));
                if sender == receiver {
                    return;
                }
                let conversation = Conversation::direct(&sender, &receiver).unwrap();
                self.write(from, message(&sender, conversation, &[&receiver], Intent::Send {}));
            }
            2 | 3 => {
                let sender = agent(from, 0);
                let (conversation, responder, audience) = if operation % 8 == 2 {
                    let responder = agent(to, 1);
                    let direct = Conversation::direct(&sender, &responder).unwrap();
                    (direct, responder.clone(), vec![responder])
                } else {
                    if from == to {
                        return;
                    }
                    (Conversation::Room(room_id()), agent(to, 0), members())
                };
                let recipients: Vec<&AgentId> = audience.iter().collect();
                let intent = Intent::Ask {
                    responder: responder.clone(),
                    chain: Chain::default(),
                };
                if let Some(event) = self.write(from, message(&sender, conversation, &recipients, intent)) {
                    self.asks.push((from, to, event));
                }
            }
            4..=6 if !self.asks.is_empty() => {
                let (asker, responder_machine, request) = self.asks[x % self.asks.len()].clone();
                let Some(responder) = (match request.body() {
                    Body::Message {
                        intent: Intent::Ask { responder, .. },
                        ..
                    } => Some(responder.clone()),
                    _ => None,
                }) else {
                    return;
                };
                let (machine, by, outcome) = match operation % 8 {
                    4 => (responder_machine, responder, None),
                    5 => (asker, request.author().unwrap(), Some(Outcome::Withdrawn)),
                    _ => (responder_machine, responder, Some(Outcome::Failed)),
                };
                if let Some(body) = follow_up(&request, &by, outcome) {
                    self.try_write(machine, body);
                }
            }
            4..=6 => {}
            _ => self.sync(from, to, 1 + y % 4),
        }
    }
}

libfuzzer_sys::fuzz_target!(|bytes: &[u8]| {
    let mut world = World {
        models: (0..MACHINES).map(|machine| Model::new(origin(machine))).collect(),
        asks: Vec::new(),
    };
    world
        .write(
            0,
            Body::Room {
                name: RoomName::try_from("all".to_owned()).unwrap(),
                topic: None,
                members: Members::try_from(members()).unwrap(),
            },
        )
        .expect("the owner opens its room");
    for chunk in bytes.chunks(3).take(256) {
        let byte = |index: usize| usize::from(chunk.get(index).copied().unwrap_or(0));
        world.step(chunk[0], byte(1), byte(2));
    }
    for _ in 0..MACHINES + 1 {
        for first in 0..MACHINES {
            for second in 0..MACHINES {
                world.sync(first, second, 64);
            }
        }
    }
    for first in 0..MACHINES {
        for second in 0..MACHINES {
            if first == second {
                continue;
            }
            let (low, high) = (first.min(second), first.max(second));
            let (left, right) = world.models.split_at_mut(high);
            let progress = left[low].sync_with(&mut right[0], 64).unwrap();
            assert_eq!(progress.received.rejected, None, "a converged exchange rejects nothing");
            assert_eq!(progress.refused, None, "a converged exchange rejects nothing");
        }
    }
    let reference = &world.models[0];
    let ids = |model: &Model| model.ordered().iter().map(|event| event.id().clone()).collect::<Vec<_>>();
    for model in &world.models[1..] {
        assert_eq!(ids(model), ids(reference), "every machine stores the same sequence");
        assert_eq!(model.profiles(), reference.profiles());
        assert_eq!(model.rooms(), reference.rooms());
    }
    for (_, _, request) in &world.asks {
        let seen: Vec<_> = world
            .models
            .iter()
            .filter(|model| {
                model
                    .ordered()
                    .iter()
                    .any(|event| event.id() == request.id() && event.body() == request.body())
            })
            .map(|model| model.resolutions().get(request.id()).cloned())
            .collect();
        assert!(seen.windows(2).all(|pair| pair[0] == pair[1]), "every audience machine selects the same ending");
        for model in &world.models {
            let open = model.open_asks().iter().any(|(_, id)| id == request.id());
            let resolved = model.resolutions().contains_key(request.id());
            assert!(!(open && resolved), "a resolved ask is never open");
        }
    }
});
