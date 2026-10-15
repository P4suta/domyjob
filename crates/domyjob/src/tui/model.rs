use domyjob_core::chat::id::{AgentId, Conversation, Origin};
use domyjob_core::dashboard::{self, IdentityMatch, Movement, Refresh, Section};

pub(crate) use crate::chat::store::observer::Request;
use crate::chat::store::observer::Snapshot;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum RowId {
    Machine(Origin),
    Agent(AgentId),
    Chat(Conversation),
}

#[derive(Debug, Default)]
pub(super) struct Model {
    pub(super) section: Section,
    pub(super) selected: Option<RowId>,
    pub(super) intended: Option<RowId>,
    pub(super) query: String,
    pub(super) searching: bool,
    pub(super) detail: bool,
    pub(super) help: bool,
    pub(super) picker: Option<usize>,
    pub(super) request: Request,
    pub(super) snapshot: Snapshot,
    pub(super) refresh: Refresh,
    pub(super) error: Option<String>,
    pub(super) scroll: u16,
}

impl Model {
    pub(super) fn rows(&self) -> Vec<(RowId, String)> {
        let rows: Vec<_> = match self.section {
            Section::Machines => self
                .snapshot
                .machines
                .iter()
                .map(|machine| (RowId::Machine(machine.id.clone()), machine.label.clone()))
                .collect(),
            Section::Agents => self
                .snapshot
                .agents
                .iter()
                .map(|agent| {
                    (
                        RowId::Agent(agent.id.clone()),
                        format!("{} · {}", agent.id.name(), agent.card.display_name.as_str()),
                    )
                })
                .collect(),
            Section::Chats => self
                .snapshot
                .chats
                .iter()
                .map(|chat| (RowId::Chat(chat.id.clone()), chat.label.clone()))
                .collect(),
        };
        let query = self.query.to_lowercase();
        rows.into_iter()
            .filter(|(_, label)| label.to_lowercase().contains(&query))
            .collect()
    }

    pub(super) fn reconcile(&mut self) {
        let rows = self.rows();
        if self.intended.is_none() {
            self.intended.clone_from(&self.selected);
        }
        let found = rows
            .iter()
            .position(|(id, _)| self.intended.as_ref() == Some(id));
        let retained = found.and_then(|position| {
            dashboard::retained_selection(IdentityMatch::Exact, position, rows.len())
        });
        let selected =
            dashboard::selection_after_refresh(self.intended.is_some(), retained, rows.len());
        self.selected = selected.and_then(|position| rows.get(position).map(|(id, _)| id.clone()));
        if self.intended.is_none() {
            self.intended.clone_from(&self.selected);
        }
    }

    pub(super) fn navigate(&mut self, movement: Movement) {
        let rows = self.rows();
        let current = rows
            .iter()
            .position(|(id, _)| self.selected.as_ref() == Some(id));
        self.selected = dashboard::navigate(current, rows.len(), movement)
            .and_then(|position| rows.get(position).map(|(id, _)| id.clone()));
        self.intended.clone_from(&self.selected);
        self.scroll = 0;
    }

    pub(super) fn choose_actor(&mut self, position: usize) {
        if let Some(actor) = self.snapshot.local_agents.get(position) {
            self.request.actor = Some(actor.clone());
            self.request.initial_actor = None;
            self.request.conversation = None;
            self.snapshot.chats.clear();
            self.snapshot.messages.clear();
            self.selected = None;
            self.intended = None;
            self.picker = None;
            self.detail = false;
        }
    }

    pub(super) fn accept(
        &mut self,
        generation: u64,
        request: &Request,
        result: Result<Snapshot, String>,
    ) {
        let scope = if *request == self.request {
            IdentityMatch::Exact
        } else {
            IdentityMatch::Different
        };
        if !self.refresh.complete_view(generation, scope) {
            return;
        }
        match result {
            Ok(snapshot) => {
                self.request.actor.clone_from(&snapshot.actor);
                self.request.initial_actor = None;
                self.snapshot = snapshot;
                self.error = None;
                self.reconcile();
            }
            Err(error) => self.error = Some(error),
        }
    }
}

#[cfg(test)]
mod tests;
