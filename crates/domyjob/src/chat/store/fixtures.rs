use domyjob_core::chat::card::{Access, Card, Mode, Skills, Tool};
use domyjob_core::chat::event::{Body, Members};
use domyjob_core::chat::handoff::{Metadata, Project, Scope, Target};
use domyjob_core::chat::id::{AgentId, AgentName, Line};
use domyjob_core::chat::policy::Priority;

use super::Store;
use super::views::LocalAgent;

pub(crate) fn assert_removed_after_drop(fixture: impl Sized, path: &std::path::Path) {
    drop(fixture);
    assert_eq!(
        std::fs::symlink_metadata(path).unwrap_err().kind(),
        std::io::ErrorKind::NotFound
    );
}

pub(crate) fn handoff_metadata(
    sender: &AgentId,
    previous: &AgentId,
    continuing: &AgentId,
) -> Metadata {
    Metadata::new(
        sender.clone(),
        Target::Agent {
            agent: previous.clone(),
        },
        (
            Project::try_from("owner/project".to_owned()).unwrap(),
            Scope::try_from("issue-47".to_owned()).unwrap(),
        ),
        Members::try_from(vec![previous.clone(), continuing.clone()]).unwrap(),
    )
    .unwrap()
}

pub(crate) fn store(root: &tempfile::TempDir, name: &str) -> Store {
    Store::open_in(&crate::layout::State::at(&root.path().join(name))).unwrap()
}

pub(crate) fn register(store: &Store, name: &str, mode: Mode) -> AgentId {
    let agent = AgentName::try_from(name.to_owned()).unwrap();
    store
        .write(|tx| {
            tx.configure(
                &agent,
                Some(&LocalAgent {
                    cwd: "/work".to_owned(),
                    session: None,
                }),
            )?;
            tx.author(
                Priority::Ordinary,
                Body::Profile {
                    agent: agent.clone(),
                    card: Box::new(Card {
                        display_name: Line::try_from(name.to_owned()).unwrap(),
                        role: None,
                        description: None,
                        skills: Skills::default(),
                        project: None,
                        status: None,
                        tool: Tool::Codex,
                        mode,
                        access: Access::Read,
                    }),
                },
            )
        })
        .unwrap();
    AgentId::new(agent, store.origin().clone())
}
