use std::collections::BTreeMap;

use domyjob_core::chat::card::MachineCard;
use domyjob_core::chat::id::{AgentId, AgentName, Conversation, Origin, RoomId, RoomName};

use super::store::StoreError;

#[derive(Debug, Default)]
pub(crate) struct Book {
    pub(crate) local: Option<Origin>,
    pub(crate) peers: BTreeMap<String, Origin>,
    pub(crate) machines: BTreeMap<Origin, MachineCard>,
    pub(crate) agents: Vec<AgentId>,
    pub(crate) rooms: Vec<RoomId>,
}

fn unique<T: Clone + std::fmt::Display>(found: &[T], text: &str) -> Result<T, StoreError> {
    match found {
        [only] => Ok(only.clone()),
        [] => Err(StoreError::Unknown(text.to_owned())),
        [_, _, ..] => Err(StoreError::Ambiguous(format!(
            "{text} ({})",
            found
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(", ")
        ))),
    }
}

impl Book {
    fn machines_named(&self, text: &str) -> Vec<Origin> {
        let mut found: Vec<Origin> = Vec::new();
        let mut add = |origin: &Origin| {
            if !found.contains(origin) {
                found.push(origin.clone());
            }
        };
        if text == "local"
            && let Some(local) = &self.local
        {
            add(local);
        }
        if let Some(origin) = self.peers.get(text) {
            add(origin);
        }
        for (origin, card) in &self.machines {
            if card.label.as_str().eq_ignore_ascii_case(text) || origin.as_str() == text {
                add(origin);
            }
        }
        if let Ok(origin) = Origin::try_from(text.to_owned()) {
            add(&origin);
        }
        found
    }

    pub(crate) fn agent(&self, text: &str) -> Result<AgentId, StoreError> {
        let found: Vec<AgentId> = match text.rsplit_once('@') {
            Some((name, machine)) => {
                let origins = self.machines_named(machine);
                self.agents
                    .iter()
                    .filter(|agent| {
                        agent.name().as_str() == name && origins.contains(agent.origin())
                    })
                    .cloned()
                    .collect()
            }
            None => self
                .agents
                .iter()
                .filter(|agent| agent.name().as_str() == text)
                .cloned()
                .collect(),
        };
        unique(&found, text)
    }

    pub(crate) fn room(&self, text: &str) -> Result<RoomId, StoreError> {
        if let Ok(Conversation::Room(room)) = Conversation::try_from(text.to_owned()) {
            return unique(
                &self
                    .rooms
                    .iter()
                    .filter(|known| **known == room)
                    .cloned()
                    .collect::<Vec<_>>(),
                text,
            );
        }
        let (name, origins) = match text.rsplit_once('@') {
            Some((name, machine)) => (name, Some(self.machines_named(machine))),
            None => (text, None),
        };
        let found: Vec<RoomId> = self
            .rooms
            .iter()
            .filter(|room| room.name().as_str() == name)
            .filter(|room| {
                origins
                    .as_ref()
                    .is_none_or(|origins| origins.contains(room.origin()))
            })
            .cloned()
            .collect();
        unique(&found, text)
    }

    pub(crate) fn conversation(
        &self,
        me: &AgentId,
        target: &str,
    ) -> Result<Conversation, StoreError> {
        if let Ok(conversation) = Conversation::try_from(target.to_owned()) {
            return Ok(conversation);
        }
        match (self.agent(target), self.room(target)) {
            (Ok(agent), Err(_)) => Ok(Conversation::direct(me, &agent)?),
            (Err(_), Ok(room)) => Ok(Conversation::Room(room)),
            (Ok(agent), Ok(room)) => Err(StoreError::Ambiguous(format!(
                "{target} (agent {agent}, room {})",
                Conversation::Room(room)
            ))),
            (Err(agent), Err(StoreError::Unknown(_))) => Err(agent),
            (Err(_), Err(room)) => Err(room),
        }
    }

    #[must_use]
    pub(crate) fn machine_label(&self, origin: &Origin) -> String {
        if self.local.as_ref() == Some(origin) {
            return "local".to_owned();
        }
        if let Some((alias, _)) = self.peers.iter().find(|(_, known)| *known == origin) {
            return alias.clone();
        }
        self.machines.get(origin).map_or_else(
            || origin.as_str().chars().take(8).collect(),
            |card| card.label.as_str().to_owned(),
        )
    }

    #[must_use]
    pub(crate) fn agent_label(&self, agent: &AgentId) -> String {
        format!("{}@{}", agent.name(), self.machine_label(agent.origin()))
    }
}

pub(crate) fn handle<T: TryFrom<String, Error = domyjob_core::chat::id::Invalid>>(
    text: &str,
) -> Result<T, StoreError> {
    Ok(T::try_from(text.to_owned())?)
}

pub(crate) fn local_agent(book: &Book, text: &str) -> Result<AgentId, StoreError> {
    let local = book
        .local
        .clone()
        .ok_or(StoreError::Corrupt("the local machine is unknown"))?;
    let name = text
        .strip_suffix("@local")
        .or_else(|| text.strip_suffix(&format!("@{local}")))
        .unwrap_or(text);
    Ok(AgentId::new(handle::<AgentName>(name)?, local))
}

pub(crate) fn local_room(book: &Book, text: &str) -> Result<RoomId, StoreError> {
    let local = book
        .local
        .clone()
        .ok_or(StoreError::Corrupt("the local machine is unknown"))?;
    Ok(RoomId::new(local, handle::<RoomName>(text)?))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use domyjob_core::chat::card::{MachineCard, Os};
    use domyjob_core::chat::id::{AgentId, Conversation, Line, Origin, RoomId, RoomName};

    use super::{Book, local_agent, local_room};
    use crate::chat::store::StoreError;

    fn origin(digit: char) -> Origin {
        Origin::try_from(std::iter::repeat_n(digit, 32).collect::<String>()).unwrap()
    }

    fn agent(name: &str, digit: char) -> AgentId {
        AgentId::try_from(format!("{name}@{}", origin(digit))).unwrap()
    }

    fn book() -> Book {
        Book {
            local: Some(origin('a')),
            peers: BTreeMap::from([("linux".to_owned(), origin('b'))]),
            machines: BTreeMap::from([(
                origin('c'),
                MachineCard {
                    label: Line::try_from("laptop".to_owned()).unwrap(),
                    os: Os::Windows,
                },
            )]),
            agents: vec![
                agent("reviewer", 'a'),
                agent("reviewer", 'b'),
                agent("builder", 'c'),
            ],
            rooms: vec![RoomId::new(
                origin('a'),
                RoomName::try_from("release".to_owned()).unwrap(),
            )],
        }
    }

    #[test]
    fn agents_resolve_by_unique_name_or_any_machine_spelling() {
        let book = book();
        assert_eq!(book.agent("builder").unwrap(), agent("builder", 'c'));
        assert_eq!(book.agent("builder@laptop").unwrap(), agent("builder", 'c'));
        assert_eq!(
            book.agent("reviewer@linux").unwrap(),
            agent("reviewer", 'b')
        );
        assert_eq!(
            book.agent("reviewer@local").unwrap(),
            agent("reviewer", 'a')
        );
        assert!(matches!(
            book.agent("reviewer"),
            Err(StoreError::Ambiguous(_))
        ));
        assert!(matches!(book.agent("nobody"), Err(StoreError::Unknown(_))));
    }

    #[test]
    fn targets_distinguish_rooms_from_direct_conversations() {
        let book = book();
        let me = agent("reviewer", 'a');
        assert!(matches!(
            book.conversation(&me, "release").unwrap(),
            Conversation::Room(_)
        ));
        assert_eq!(
            book.conversation(&me, "builder").unwrap(),
            Conversation::direct(&me, &agent("builder", 'c')).unwrap()
        );
        assert_eq!(book.machine_label(&origin('b')), "linux");
        assert_eq!(book.machine_label(&origin('c')), "laptop");
    }

    #[test]
    fn machine_spellings_resolve_and_labels_prefer_local_then_aliases() {
        let mut book = book();
        book.machines.insert(
            origin('b'),
            MachineCard {
                label: Line::try_from("LINUX".to_owned()).unwrap(),
                os: Os::Linux,
            },
        );
        assert_eq!(
            book.agent("reviewer@linux").unwrap(),
            agent("reviewer", 'b')
        );
        assert_eq!(
            book.agent("reviewer@LINUX").unwrap(),
            agent("reviewer", 'b')
        );
        assert_eq!(
            book.agent(&format!("reviewer@{}", origin('b'))).unwrap(),
            agent("reviewer", 'b')
        );
        assert!(matches!(
            book.agent("builder@missing-machine"),
            Err(StoreError::Unknown(_))
        ));
        assert!(matches!(
            book.agent(&format!("builder@{}", origin('b'))),
            Err(StoreError::Unknown(_))
        ));
        assert_eq!(book.machine_label(&origin('a')), "local");
        assert_eq!(book.machine_label(&origin('b')), "linux");
        assert_eq!(book.machine_label(&origin('d')), "dddddddd");
        assert_eq!(book.agent_label(&agent("reviewer", 'b')), "reviewer@linux");
    }

    #[test]
    fn rooms_require_a_unique_known_address() {
        let mut book = book();
        let local = book.rooms.first().expect("known local room").clone();
        let peer = RoomId::new(
            origin('b'),
            RoomName::try_from("release".to_owned()).unwrap(),
        );
        book.rooms.push(peer.clone());
        assert!(matches!(
            book.room("release"),
            Err(StoreError::Ambiguous(_))
        ));
        assert_eq!(book.room("release@local").unwrap(), local);
        assert_eq!(book.room("release@linux").unwrap(), peer);
        assert_eq!(
            book.room(&Conversation::Room(local.clone()).to_string())
                .unwrap(),
            local
        );
        let unknown = Conversation::Room(RoomId::new(
            origin('c'),
            RoomName::try_from("release".to_owned()).unwrap(),
        ));
        assert!(matches!(
            book.room(&unknown.to_string()),
            Err(StoreError::Unknown(_))
        ));
        assert!(matches!(
            book.room("missing@local"),
            Err(StoreError::Unknown(_))
        ));
    }

    #[test]
    fn conversation_resolution_preserves_ambiguity_and_self_address_errors() {
        let mut book = book();
        let me = agent("reviewer", 'a');
        book.agents.push(agent("release", 'c'));
        assert!(matches!(
            book.conversation(&me, "release"),
            Err(StoreError::Ambiguous(_))
        ));
        assert!(matches!(
            book.conversation(&me, "reviewer@local"),
            Err(StoreError::Invalid(_))
        ));
        assert!(matches!(
            book.conversation(&me, "reviewer"),
            Err(StoreError::Ambiguous(_))
        ));
        let explicit = Conversation::direct(&me, &agent("other", 'd')).unwrap();
        assert_eq!(
            book.conversation(&me, &explicit.to_string()).unwrap(),
            explicit
        );
    }

    #[test]
    fn local_handles_require_identity_and_preserve_invalid_name_errors() {
        let book = book();
        assert_eq!(
            local_agent(&book, "worker@local").unwrap(),
            agent("worker", 'a')
        );
        assert_eq!(
            local_agent(&book, &format!("worker@{}", origin('a'))).unwrap(),
            agent("worker", 'a')
        );
        assert!(matches!(
            local_agent(&book, "bad name"),
            Err(StoreError::Invalid(_))
        ));
        assert!(matches!(
            local_room(&book, "bad name"),
            Err(StoreError::Invalid(_))
        ));
        let room = RoomId::new(
            origin('a'),
            RoomName::try_from("release".to_owned()).unwrap(),
        );
        assert_eq!(local_room(&book, "release").unwrap(), room);
        let missing = Book::default();
        assert!(matches!(
            local_agent(&missing, "worker"),
            Err(StoreError::Corrupt("the local machine is unknown"))
        ));
        assert!(matches!(
            local_room(&missing, "release"),
            Err(StoreError::Corrupt("the local machine is unknown"))
        ));
    }
}
