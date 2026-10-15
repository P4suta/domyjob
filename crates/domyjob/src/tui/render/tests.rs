use std::num::NonZeroU64;

use crate::tui::frame::{Cell, Grid as Buffer};
use domyjob_core::chat::card::{Access, Card, MachineCard, Mode, Os, Skills, Tool};
use domyjob_core::chat::event::Stamp;
use domyjob_core::chat::id::{AgentId, AgentName, Conversation, EventId, Line, Origin};
use domyjob_core::chat::notification::{Activity, ActivityState, Delivery, MILLIS_PER_SECOND};
use domyjob_core::dashboard::{self, Section};

use super::{CYAN, MINT, Model, NAVY, RowId, draw};
use crate::chat::store::{Link, LinkState, observer};

const OBSERVED: u64 = 1000;
const LOCAL_ORIGIN: u8 = 1;
const SYNCED_ORIGIN: u8 = 2;
const FAILED_ORIGIN: u8 = 3;

fn agent(byte: u8, name: &str, state: ActivityState) -> observer::Agent {
    observer::Agent {
        id: AgentId::new(
            AgentName::try_from(name.to_owned()).unwrap(),
            Origin::from_entropy([byte; 16]),
        ),
        activity: Some(Activity {
            state,
            at: Stamp::from_unix_millis(OBSERVED),
            status: Some(Line::try_from("Reviewing invariants".to_owned()).unwrap()),
        }),
        card: Card {
            display_name: Line::try_from(format!("{name} 界")).unwrap(),
            role: Some(Line::try_from("Verifier".to_owned()).unwrap()),
            description: None,
            skills: Skills::default(),
            project: Some(Line::try_from(format!("project-{byte}")).unwrap()),
            status: None,
            tool: Tool::Codex,
            mode: Mode::Interactive,
            access: Access::Read,
        },
    }
}

fn machine(byte: u8, label: &str, state: LinkState) -> observer::Machine {
    observer::Machine {
        id: Origin::from_entropy([byte; 16]),
        label: label.to_owned(),
        card: MachineCard {
            label: Line::try_from(label.to_owned()).unwrap(),
            os: Os::Linux,
        },
        local: byte == 1,
        link: Some(Link {
            state,
            detail: None,
            at: Stamp::from_unix_millis(OBSERVED),
        }),
    }
}

fn populated() -> Model {
    let first = agent(LOCAL_ORIGIN, "alice", ActivityState::Working);
    let second = agent(SYNCED_ORIGIN, "bob", ActivityState::Waiting);
    let actor = first.id.clone();
    let conversation = Conversation::direct(&first.id, &second.id).unwrap();
    let machines = vec![
        machine(LOCAL_ORIGIN, "local", LinkState::Synced),
        machine(SYNCED_ORIGIN, "linux", LinkState::Synced),
        machine(FAILED_ORIGIN, "windows", LinkState::Failed),
    ];
    Model {
        selected: machines
            .first()
            .map(|machine| RowId::Machine(machine.id.clone())),
        snapshot: observer::Snapshot {
            observed: Some(OBSERVED),
            actor: Some(actor.clone()),
            machines,
            local_agents: vec![actor.clone()],
            agents: vec![first, second],
            chats: vec![observer::Chat {
                id: conversation,
                label: "Verification room".to_owned(),
            }],
            ..observer::Snapshot::default()
        },
        request: super::super::model::Request {
            actor: Some(actor),
            ..Default::default()
        },
        ..Model::default()
    }
}

fn screen(model: &Model, size: (u16, u16), now: u64) -> Buffer {
    screen_at(model, size, Some(now))
}

fn screen_at(model: &Model, size: (u16, u16), now: Option<u64>) -> Buffer {
    let mut frame = Buffer::new(size.0, size.1).unwrap();
    draw(&mut frame, model, now);
    frame
}

fn text(buffer: &Buffer) -> String {
    buffer
        .content()
        .chunks(usize::from(buffer.area().width).max(1))
        .map(|row| row.iter().map(Cell::symbol).collect::<String>())
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn machine_lists_show_connection_states_and_selection_in_the_actual_frame() {
    let buffer = screen(&populated(), (128, 36), OBSERVED);
    let shown = text(&buffer);
    for expected in [
        "DOMYJOB",
        "Viewing alice",
        "3 machines",
        "2 agents",
        "1 chats",
        "Synced",
        "Offline",
        "Local ledger",
    ] {
        assert!(shown.contains(expected), "missing {expected}:\n{shown}");
    }
    assert!(
        buffer
            .content()
            .iter()
            .any(|cell| cell.symbol() == "›" && cell.style.foreground == Some(CYAN))
    );
    assert!(buffer.content().iter().any(|cell| cell.style.bold));
    assert!(
        buffer
            .content()
            .iter()
            .any(|cell| cell.style.foreground == Some(MINT))
    );
    assert!(
        buffer
            .content()
            .iter()
            .any(|cell| cell.style.background == Some(NAVY))
    );
}

#[test]
fn prior_sync_success_requires_a_current_reliable_observation() {
    let model = populated();
    for (now, expected) in [
        (
            Some(OBSERVED + dashboard::STALE_MILLIS),
            "Sync observation expired",
        ),
        (Some(OBSERVED - 1), "Sync freshness unknown"),
        (None, "Sync freshness unknown"),
    ] {
        let shown = text(&screen_at(&model, (128, 36), now));
        assert!(shown.contains(expected), "missing {expected}:\n{shown}");
        assert!(
            !shown.contains("Synced"),
            "expired sync looks current:\n{shown}"
        );
    }
    assert!(text(&screen(&model, (128, 36), OBSERVED)).contains("Synced"));
}

#[test]
fn every_responsive_frame_keeps_navigation_freshness_and_viewing_identity_visible() {
    let model = populated();
    for size in [(128, 36), (80, 24), (48, 12)] {
        let buffer = screen(&model, size, OBSERVED);
        assert_eq!(
            buffer.content().len(),
            usize::from(size.0) * usize::from(size.1)
        );
        let shown = text(&buffer);
        for expected in [
            "DOMYJOB",
            "Viewing alice",
            "Machines",
            "Agents",
            "Chats",
            "fresh",
            "q exit",
        ] {
            assert!(
                shown.contains(expected),
                "{size:?} missing {expected}:\n{shown}"
            );
        }
    }
}

#[test]
fn unchanged_observations_keep_frames_stable_across_ticks_and_background_refreshes() {
    for section in [Section::Machines, Section::Agents, Section::Chats] {
        for size in [(128, 36), (48, 12)] {
            for detail in [false, true] {
                let mut model = populated();
                model.section = section;
                model.detail = detail;
                model.selected = None;
                model.reconcile();
                let expected = screen(&model, size, OBSERVED);
                for elapsed in [0, MILLIS_PER_SECOND - 1, MILLIS_PER_SECOND] {
                    assert_eq!(screen(&model, size, OBSERVED + elapsed), expected);
                }
                let now = OBSERVED + MILLIS_PER_SECOND;
                let generation = model.refresh.request().unwrap();
                assert_eq!(screen(&model, size, now), expected);
                let mut next = populated().snapshot;
                next.observed = Some(now);
                model.accept(generation, &model.request.clone(), Ok(next));
                assert_eq!(screen(&model, size, now), expected);

                model.snapshot.observed = None;
                let unknown = screen_at(&model, size, None);
                model.refresh.request().unwrap();
                assert_eq!(screen_at(&model, size, None), unknown);
            }
        }
    }
}

#[test]
fn errors_stale_observations_and_clock_rollback_have_distinct_labels() {
    let mut model = populated();
    assert!(
        text(&screen(
            &model,
            (128, 36),
            OBSERVED + dashboard::STALE_MILLIS
        ))
        .contains("stale")
    );
    assert!(text(&screen(&model, (128, 36), OBSERVED - 1)).contains("unknown"));
    model.refresh.request().unwrap();
    assert!(!text(&screen(&model, (128, 36), OBSERVED)).contains("Refreshing"));
    model.error = Some("connection\u{1b}[31m refused\u{202e}".to_owned());
    let failed = text(&screen(&model, (128, 36), OBSERVED));
    assert!(failed.contains("Refresh error"));
    assert!(failed.contains("prior data retained"));
    assert!(!failed.contains('\u{1b}'));
    assert!(!failed.contains('\u{202e}'));
}

#[test]
fn an_unavailable_native_clock_never_labels_prior_activity_as_fresh() {
    let mut model = populated();
    model.section = Section::Agents;
    model.selected = model
        .snapshot
        .agents
        .first()
        .map(|agent| RowId::Agent(agent.id.clone()));
    let shown = text(&screen_at(&model, (128, 36), None));
    assert!(shown.contains("unknown"));
    assert!(shown.contains("Activity unknown"));
    assert!(!shown.contains("fresh ·"));
    assert!(!shown.contains("0s ago"));
}

#[test]
fn an_exact_agent_selection_shows_its_own_activity_and_identity() {
    let first = agent(1, "assistant", ActivityState::Working);
    let second = agent(2, "assistant", ActivityState::Waiting);
    let identity = second.id.clone();
    let mut model = Model {
        section: Section::Agents,
        selected: Some(RowId::Agent(identity.clone())),
        snapshot: observer::Snapshot {
            agents: vec![first, second],
            ..Default::default()
        },
        ..Model::default()
    };
    let shown = text(&screen(&model, (128, 36), OBSERVED));
    assert!(shown.contains(&identity.to_string()));
    assert!(shown.contains("project-2"));
    assert!(!shown.contains("project-1"));
    assert!(shown.contains("Waiting"));
    assert!(shown.contains("Reviewing invariants"));
    assert!(shown.contains("Current task Reviewing invariants"));
    assert!(text(&screen(&model, (128, 36), OBSERVED + super::STALLED_MILLIS)).contains("Stalled"));
    model.snapshot.agents.last_mut().unwrap().activity = None;
    assert!(text(&screen(&model, (128, 36), OBSERVED)).contains("Activity unknown"));
}

#[test]
fn utf8_and_untrusted_controls_are_rendered_as_literal_safe_text() {
    let mut model = populated();
    model.snapshot.machines.first_mut().unwrap().label =
        "界 e\u{301}\u{1b}[2J\r\t\u{2066}".to_owned();
    model.query = "界\u{1b}[31m".to_owned();
    model.searching = true;
    for size in [(128, 36), (80, 24), (48, 12)] {
        let shown = text(&screen(&model, size, OBSERVED));
        assert!(shown.contains('界'));
        assert!(!shown.contains('\u{1b}'));
        assert!(!shown.contains('\r'));
        assert!(!shown.contains('\t'));
        assert!(!shown.contains('\u{2066}'));
    }
    model.query.clear();
    assert!(text(&screen(&model, (128, 36), OBSERVED)).contains("e\u{301}"));
}

#[test]
fn conversation_details_show_each_recipients_own_delivery_progress() {
    let mut model = populated();
    let conversation = model.snapshot.chats.first().unwrap().id.clone();
    model.section = Section::Chats;
    model.selected = Some(RowId::Chat(conversation.clone()));
    model.request.conversation = Some(conversation);
    model.snapshot.messages.push(observer::Message {
        id: EventId::new(Origin::from_entropy([1; 16]), NonZeroU64::new(1).unwrap()),
        from: "alice".to_owned(),
        text: "Checked invariants\nUTF-8 界".to_owned(),
        at: Stamp::from_unix_millis(OBSERVED),
        recipients: [
            Delivery::Pending,
            Delivery::Delivered,
            Delivery::Read,
            Delivery::Acknowledged,
        ]
        .into_iter()
        .enumerate()
        .map(|(index, delivery)| {
            (
                agent(
                    u8::try_from(index + 2).unwrap(),
                    "reviewer",
                    ActivityState::Waiting,
                )
                .id,
                delivery,
            )
        })
        .collect(),
    });
    let shown = text(&screen(&model, (128, 36), OBSERVED));
    for expected in [
        "Checked invariants",
        "Pending",
        "Delivered",
        "Read",
        "Acknowledged",
        "reviewer",
    ] {
        assert!(shown.contains(expected), "missing {expected}:\n{shown}");
    }
    for (recipient, _) in &model.snapshot.messages.first().unwrap().recipients {
        assert!(shown.contains(&recipient.to_string()));
    }
}

#[test]
fn responsive_empty_views_render_without_panics_at_all_supported_sizes() {
    for (width, height) in [(128, 36), (80, 24), (48, 12), (20, 5), (1, 1)] {
        let frame = screen(&Model::default(), (width, height), 0);
        let text: String = frame.content().iter().map(Cell::symbol).collect();
        assert!(text.contains("domyjob") || text.contains("DOMYJOB") || width == 1);
    }
}

#[test]
fn minimal_frames_have_a_stable_resize_prompt_and_never_render_private_detail() {
    let model = populated();
    let buffer = screen(&model, (20, 5), OBSERVED);
    let rows: Vec<_> = text(&buffer)
        .lines()
        .map(str::trim_end)
        .map(str::to_owned)
        .collect();
    assert_eq!(rows, ["domyjob · dashboard", "Resize · q exit", "", "", ""]);
    assert!(!text(&buffer).contains("alice"));
    for size in [(1, 1), (0, 0)] {
        let tiny_buffer = screen(&model, size, OBSERVED);
        assert_eq!(
            tiny_buffer.content().len(),
            usize::from(size.0) * usize::from(size.1)
        );
    }
}

#[test]
fn the_complete_compact_empty_frame_keeps_its_information_hierarchy() {
    const WIDTH: usize = 48;
    const INNER: usize = WIDTH - 2;
    const TITLE_WIDTH: usize = 23;
    let expected = [
        " DOMYJOB  0 machines · 0 agents · chats: choose agent".to_owned(),
        " Viewing public directory · unknown · no reliable observation".to_owned(),
        "─".repeat(WIDTH),
        " Machines  ·  Agents  ·  Chats ".to_owned(),
        format!(
            "╭ Machines · 0 observed {}╮",
            "─".repeat(INNER - TITLE_WIDTH)
        ),
        format!("│{:<INNER$}│", "No observations yet."),
        format!("│{:<INNER$}│", "Existing local state appears here."),
        format!("│{:<INNER$}│", "r refreshes the view."),
        format!("│{:<INNER$}│", ""),
        format!("╰{}╯", "─".repeat(INNER)),
        " q exit · ? help · Tab sections · Enter detail".to_owned(),
        " unknown · no reliable observation · passive local view".to_owned(),
    ]
    .map(|line| {
        let clipped: String = line.chars().take(WIDTH).collect();
        format!("{clipped:<WIDTH$}")
    })
    .join("\n");
    let buffer = screen(&Model::default(), (u16::try_from(WIDTH).unwrap(), 12), 0);
    assert_eq!(text(&buffer), expected);
    assert!(
        buffer
            .content()
            .iter()
            .any(|cell| cell.style.foreground == Some(CYAN)
                && cell.style.background == Some(super::SELECTED))
    );
    assert!(
        buffer
            .content()
            .iter()
            .any(|cell| cell.symbol() == "╭" && cell.style.foreground == Some(super::BORDER))
    );
}

#[test]
fn terminals_without_room_for_one_observed_row_show_the_exit_and_resize_prompt() {
    let model = populated();
    for size in [(28, 8), (31, 12), (48, 9)] {
        let shown = text(&screen(&model, size, OBSERVED));
        assert!(shown.contains("Resize"), "{size:?}:\n{shown}");
        assert!(shown.contains("q exit"), "{size:?}:\n{shown}");
    }
}

#[test]
fn empty_search_and_identity_restricted_views_explain_the_available_next_action() {
    let mut model = Model::default();
    assert!(text(&screen(&model, (128, 36), 0)).contains("No observations yet"));
    model.query = "unmatched".to_owned();
    let searched = text(&screen(&model, (128, 36), 0));
    assert!(searched.contains("No rows match this search"));
    assert!(searched.contains("Esc clears the search"));
    model.query.clear();
    model.section = Section::Chats;
    let public = text(&screen(&model, (128, 36), 0));
    assert!(public.contains("Choose an agent with a"));
    assert!(public.contains("chats: choose agent"));
    assert!(!public.contains("0 chats"));
    assert!(public.contains("read status unchanged"));
    model.request.actor = populated().snapshot.actor;
    let empty_chats = text(&screen(&model, (128, 36), 0));
    for expected in [
        "No conversations to show for this agent",
        "domyjob messages",
        "a chooses another agent",
    ] {
        assert!(empty_chats.contains(expected));
    }
    model.query = "unmatched".to_owned();
    assert!(text(&screen(&model, (128, 36), 0)).contains("No rows match this search"));
}

#[test]
fn narrow_detail_and_scroll_reveal_the_selected_agents_information() {
    let mut model = populated();
    let selected = model.snapshot.agents.last().unwrap().id.clone();
    model.section = Section::Agents;
    model.selected = Some(RowId::Agent(selected));
    model.detail = true;
    assert!(text(&screen(&model, (48, 12), OBSERVED)).contains("Waiting"));
    model.scroll = 5;
    let scrolled = text(&screen(&model, (48, 12), OBSERVED));
    assert!(scrolled.contains("project-2"));
    assert!(scrolled.contains("Reviewing invariants"));
}

#[test]
fn help_and_identity_picker_render_controls_without_changing_the_viewing_identity() {
    let mut model = populated();
    model.help = true;
    let help = text(&screen(&model, (128, 36), OBSERVED));
    for control in [
        "Keyboard guide",
        "PgUp/PgDn",
        "Search",
        "Choose agent for Chats",
        "status unchanged",
    ] {
        assert!(help.contains(control), "missing {control}:\n{help}");
    }
    model.help = false;
    model.request.actor = None;
    model.picker = Some(0);
    let identity = model.snapshot.local_agents.first().unwrap().to_string();
    let picker = text(&screen(&model, (128, 36), OBSERVED));
    assert!(picker.contains("Choose agent"));
    assert!(picker.contains(&identity));
    assert!(model.request.actor.is_none());
    let narrow_picker = text(&screen(&model, (48, 12), OBSERVED));
    assert!(narrow_picker.contains("Enter select"));
    assert!(narrow_picker.contains("Esc back"));
    model.snapshot.local_agents.clear();
    let empty_picker = text(&screen(&model, (80, 24), OBSERVED));
    assert!(empty_picker.contains("No local agents registered"));
    assert!(empty_picker.contains("Esc closes this picker"));
}

#[test]
fn refresh_errors_and_unknown_freshness_are_visible_without_color() {
    let model = Model {
        error: Some("connection\u{1b}[31m refused".to_owned()),
        ..Model::default()
    };
    let frame = screen(&model, (128, 36), 0);
    let text: String = frame.content().iter().map(Cell::symbol).collect();
    assert!(text.contains("Refresh error"));
    assert!(text.contains("unknown"));
    assert!(!text.contains('\u{1b}'));
}
