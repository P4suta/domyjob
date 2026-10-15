use super::{Model, Request, RowId};
use crate::chat::store::observer::{Agent, Snapshot};
use domyjob_core::chat::card::{Access, Card, Mode, Skills, Tool};
use domyjob_core::chat::id::{AgentId, AgentName, Line, Origin};
use domyjob_core::dashboard::Section;

pub(crate) fn agent(origin: [u8; 16]) -> Agent {
    let name = AgentName::try_from("assistant".to_owned()).unwrap();
    Agent {
        id: AgentId::new(name, Origin::from_entropy(origin)),
        activity: None,
        card: Card {
            display_name: Line::try_from("Assistant 界".to_owned()).unwrap(),
            role: None,
            description: None,
            skills: Skills::default(),
            project: None,
            status: None,
            tool: Tool::Codex,
            mode: Mode::Interactive,
            access: Access::Read,
        },
    }
}

#[test]
fn same_visible_name_cannot_retain_a_foreign_full_identity() {
    let first = agent([1; 16]);
    let foreign = agent([2; 16]);
    let selected = RowId::Agent(first.id);
    let mut model = Model {
        section: Section::Agents,
        selected: Some(selected.clone()),
        snapshot: Snapshot {
            agents: vec![foreign],
            ..Snapshot::default()
        },
        ..Model::default()
    };
    model.reconcile();
    assert_ne!(model.selected, Some(selected));
    assert!(model.selected.is_none());
    model.reconcile();
    assert!(model.selected.is_none());
}

#[test]
fn refresh_retains_exact_identity_across_reordering() {
    let selected = agent([1; 16]);
    let identity = selected.id.clone();
    let mut model = Model {
        section: Section::Agents,
        selected: Some(RowId::Agent(identity.clone())),
        ..Model::default()
    };
    let generation = model.refresh.request().unwrap();
    model.accept(
        generation,
        &Request::default(),
        Ok(Snapshot {
            agents: vec![agent([2; 16]), selected],
            ..Snapshot::default()
        }),
    );
    assert_eq!(model.selected, Some(RowId::Agent(identity)));
}

#[test]
fn a_late_completion_cannot_replace_a_newer_viewing_identity() {
    let old = Request::default();
    let mut model = Model::default();
    let generation = model.refresh.request().unwrap();
    model.request.actor = Some(agent([1; 16]).id);
    model.accept(
        generation,
        &old,
        Ok(Snapshot {
            observed: Some(7),
            ..Snapshot::default()
        }),
    );
    assert!(model.snapshot.observed.is_none());
    assert!(model.refresh.pending().is_none());
    assert!(model.refresh.request().is_some());
}

#[test]
fn changing_the_viewing_actor_clears_limited_history_before_pending_and_failed_refreshes() {
    use crate::chat::store::observer::HistoryScan;

    let previous = agent([1; 16]).id;
    let next = AgentId::new(
        AgentName::try_from("next".to_owned()).unwrap(),
        previous.origin().clone(),
    );
    let old = Request {
        actor: Some(previous),
        ..Request::default()
    };
    let mut model = Model {
        request: old.clone(),
        snapshot: Snapshot {
            actor: old.actor.clone(),
            local_agents: vec![next.clone()],
            history_scan: HistoryScan::Limited,
            ..Snapshot::default()
        },
        ..Model::default()
    };
    let old_generation = model.refresh.request().unwrap();
    model.choose_actor(0);
    assert_eq!(model.request.actor.as_ref(), Some(&next));
    assert_eq!(model.snapshot.history_scan, HistoryScan::WithinBudget);
    assert!(model.snapshot.messages.is_empty());
    assert!(model.snapshot.chats.is_empty());
    assert_eq!(model.refresh.pending(), Some(old_generation));
    model.accept(
        old_generation,
        &old,
        Ok(Snapshot {
            history_scan: HistoryScan::Limited,
            ..Snapshot::default()
        }),
    );
    assert_eq!(model.snapshot.history_scan, HistoryScan::WithinBudget);
    let generation = model.refresh.request().unwrap();
    model.accept(
        generation,
        &model.request.clone(),
        Err("new actor unavailable".to_owned()),
    );
    assert_eq!(model.error.as_deref(), Some("new actor unavailable"));
    assert_eq!(model.snapshot.history_scan, HistoryScan::WithinBudget);
}
