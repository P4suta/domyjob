#![no_main]

use domyjob_core::chat::card::{Access, Card, Mode, Skills, Tool};
use domyjob_core::chat::event::{Body, Chain, Event, Intent, Members, Outcome, Stamp};
use domyjob_core::chat::id::{
    AgentId, AgentName, Conversation, Line, Origin, RoomId, RoomName, Text,
};
use domyjob_core::chat::model::Model;
use domyjob_core::chat::notification::{Activity, ActivityState, Delivery, Notice};
use domyjob_core::chat::policy::audience;

const MACHINES: usize = 3;
const OPERATIONS: u8 = 12;
const OP_DIRECT_ASK: u8 = 2;
const OP_ROOM_ASK: u8 = 3;
const OP_REPLY: u8 = 4;
const OP_WITHDRAW: u8 = 5;
const OP_FAIL: u8 = 6;
const OP_DELIVERED: u8 = 8;
const OP_READ: u8 = 9;
const OP_ACKNOWLEDGED: u8 = 10;
const OP_ACTIVITY: u8 = 11;
const ACTIVITY_VARIANTS: usize = 2;
const CONVERGENCE_PAIR_SIZE: usize = 2;
const ROLE_VARIANTS: usize = 5;
const MAX_PARTIAL_ROUNDS: usize = 4;
const INPUT_STEP_BYTES: usize = 3;
const INPUT_Y_INDEX: usize = 2;
const MAX_STEPS: usize = 256;
const CONVERGENCE_ROUNDS: usize = 64;

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
        at: Stamp::from_unix_millis(0),
        notice: Some(Notice {
            recipients: Members::try_from(
                to.iter()
                    .filter(|agent| ***agent != *from)
                    .map(|agent| (*agent).clone())
                    .collect::<Vec<_>>(),
            )
            .unwrap(),
            urgent: false,
            subject: None,
            question: None,
        }),
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
            at: Stamp::from_unix_millis(0),
            notice: None,
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
        Some(
            self.models[machine]
                .write(body)
                .expect("a valid local write is admitted"),
        )
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
        match operation % OPERATIONS {
            0 => {
                let body = Body::Profile {
                    agent: agent(from, 1).name().clone(),
                    card: card(u8::try_from(y % ROLE_VARIANTS).unwrap()),
                };
                self.write(from, body);
            }
            1 => {
                let (sender, receiver) = (agent(from, 0), agent(to, 1));
                if sender == receiver {
                    return;
                }
                let conversation = Conversation::direct(&sender, &receiver).unwrap();
                self.write(
                    from,
                    message(&sender, conversation, &[&receiver], Intent::Send {}),
                );
            }
            OP_DIRECT_ASK | OP_ROOM_ASK => {
                let sender = agent(from, 0);
                let (conversation, responder, audience) = if operation % OPERATIONS == OP_DIRECT_ASK
                {
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
                if let Some(event) =
                    self.write(from, message(&sender, conversation, &recipients, intent))
                {
                    self.asks.push((from, to, event));
                }
            }
            OP_REPLY..=OP_FAIL if !self.asks.is_empty() => {
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
                let (machine, by, outcome) = match operation % OPERATIONS {
                    OP_REPLY => (responder_machine, responder, None),
                    OP_WITHDRAW => (asker, request.author().unwrap(), Some(Outcome::Withdrawn)),
                    _ => (responder_machine, responder, Some(Outcome::Failed)),
                };
                if let Some(body) = follow_up(&request, &by, outcome) {
                    self.try_write(machine, body);
                }
            }
            OP_REPLY..=OP_FAIL => {}
            OP_DELIVERED..=OP_ACKNOWLEDGED if !self.asks.is_empty() => {
                let (_, machine, request) = self.asks[x % self.asks.len()].clone();
                let Body::Message {
                    intent: Intent::Ask { responder, .. },
                    conversation,
                    audience,
                    ..
                } = request.body()
                else {
                    return;
                };
                let delivery = match operation % OPERATIONS {
                    OP_DELIVERED => Delivery::Delivered,
                    OP_READ => Delivery::Read,
                    _ => Delivery::Acknowledged,
                };
                self.try_write(
                    machine,
                    Body::Receipt {
                        request: request.id().clone(),
                        conversation: conversation.clone(),
                        audience: audience.clone(),
                        agent: responder.name().clone(),
                        delivery,
                    },
                );
            }
            OP_DELIVERED..=OP_ACKNOWLEDGED => {}
            OP_ACTIVITY => {
                self.write(
                    from,
                    Body::Activity {
                        agent: agent(from, 0).name().clone(),
                        activity: Activity {
                            state: if y % ACTIVITY_VARIANTS == 0 {
                                ActivityState::Working
                            } else {
                                ActivityState::Waiting
                            },
                            at: Stamp::from_unix_millis(u64::try_from(x).unwrap()),
                            status: None,
                        },
                    },
                );
            }
            _ => self.sync(from, to, 1 + y % MAX_PARTIAL_ROUNDS),
        }
    }
}

libfuzzer_sys::fuzz_target!(|bytes: &[u8]| {
    let mut world = World {
        models: (0..MACHINES)
            .map(|machine| Model::new(origin(machine)))
            .collect(),
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
    for chunk in bytes.chunks(INPUT_STEP_BYTES).take(MAX_STEPS) {
        let byte = |index: usize| usize::from(chunk.get(index).copied().unwrap_or(0));
        world.step(chunk[0], byte(1), byte(INPUT_Y_INDEX));
    }
    for _ in 0..MACHINES + 1 {
        for first in 0..MACHINES {
            for second in 0..MACHINES {
                world.sync(first, second, CONVERGENCE_ROUNDS);
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
            let progress = left[low]
                .sync_with(&mut right[0], CONVERGENCE_ROUNDS)
                .unwrap();
            assert_eq!(
                progress.received.rejected, None,
                "a converged exchange rejects nothing"
            );
            assert_eq!(
                progress.refused, None,
                "a converged exchange rejects nothing"
            );
        }
    }
    let reference = &world.models[0];
    let ids = |model: &Model| {
        model
            .ordered()
            .iter()
            .map(|event| event.id().clone())
            .collect::<Vec<_>>()
    };
    for model in &world.models[1..] {
        assert_eq!(
            ids(model),
            ids(reference),
            "every machine stores the same sequence"
        );
        assert_eq!(model.profiles(), reference.profiles());
        assert_eq!(model.rooms(), reference.rooms());
        assert_eq!(model.activities(), reference.activities());
    }
    for (_, _, request) in &world.asks {
        if let Body::Message {
            intent: Intent::Ask { responder, .. },
            ..
        } = request.body()
        {
            let receipts: Vec<_> = world
                .models
                .iter()
                .filter(|model| {
                    model
                        .ordered()
                        .iter()
                        .any(|event| event.id() == request.id() && event.body() == request.body())
                })
                .map(|model| {
                    model
                        .deliveries()
                        .get(&(request.id().clone(), responder.clone()))
                        .copied()
                        .unwrap_or_default()
                })
                .collect();
            assert!(
                receipts
                    .windows(CONVERGENCE_PAIR_SIZE)
                    .all(|pair| pair[0] == pair[1]),
                "every audience machine projects the same receipt state"
            );
        }
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
        assert!(
            seen.windows(CONVERGENCE_PAIR_SIZE)
                .all(|pair| pair[0] == pair[1]),
            "every audience machine selects the same ending"
        );
        for model in &world.models {
            let open = model.open_asks().iter().any(|(_, id)| id == request.id());
            let resolved = model.resolutions().contains_key(request.id());
            assert!(!(open && resolved), "a resolved ask is never open");
        }
    }
});
