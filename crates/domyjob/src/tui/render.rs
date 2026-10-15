use super::frame::{Color, Flow, Grid, Line, ListStyle, PanelStyle, Span, Style, Wrap};
use domyjob_core::chat::notification::{
    ActivityState, Delivery, MILLIS_PER_SECOND, STALLED_MILLIS,
};
use domyjob_core::dashboard::geometry::{Rect, share};
use domyjob_core::dashboard::{self, Freshness, Section, Viewport};

use super::model::{Model, RowId};
use crate::chat::store::{LinkState, observer};

const NAVY: Color = Color::Rgb(9, 16, 30);
const PANEL: Color = Color::Rgb(15, 25, 43);
const SELECTED: Color = Color::Rgb(26, 64, 78);
const CYAN: Color = Color::Rgb(104, 211, 255);
const MINT: Color = Color::Rgb(116, 231, 183);
const INK: Color = Color::Rgb(219, 228, 241);
const MUTED: Color = Color::Rgb(141, 159, 180);
const BORDER: Color = Color::Rgb(46, 68, 93);
const AMBER: Color = Color::Rgb(247, 192, 106);
const SIDEBAR_COLUMNS: u16 = 16;
const HEADER_ROWS: u16 = 3;
const FOOTER_ROWS: u16 = 2;
const TAB_ROWS: u16 = 1;
const LIST_SHARE: u8 = 38;
const MODAL_HEIGHT: u8 = 80;
const MODAL_WIDTH: u8 = 90;

fn panel(frame: &mut Grid, area: Rect, title: &str) -> Rect {
    frame.panel(
        area,
        title,
        PanelStyle {
            normal: Style::default().bg(PANEL).fg(INK),
            border: Style::default().bg(PANEL).fg(BORDER),
        },
    )
}

fn emphasis(color: Color) -> Style {
    Style::default().fg(color).bold()
}

fn shown(text: &str) -> String {
    dashboard::display(text).to_string()
}

fn age(observed: Option<u64>, now: Option<u64>, limit: u64) -> String {
    match dashboard::freshness(observed, now, limit) {
        Freshness::Unknown => "unknown · no reliable observation".to_owned(),
        Freshness::Fresh(_) => "fresh".to_owned(),
        Freshness::Stale(millis) => format!("stale · {}s ago", millis / MILLIS_PER_SECOND),
    }
}

pub(super) fn draw(frame: &mut Grid, model: &Model, now: Option<u64>) {
    let area = frame.area();
    let viewport = dashboard::viewport(area.width, area.height);
    frame.fill(area, Style::default().bg(NAVY).fg(INK));
    if viewport == Viewport::Minimal {
        frame.paragraph(
            area,
            &lines("domyjob · dashboard\nResize · q exit"),
            Flow {
                style: emphasis(CYAN).bg(NAVY),
                scroll: 0,
                wrap: Wrap::Clip,
            },
        );
        return;
    }
    let (header, remaining) = area.rows(HEADER_ROWS);
    let (body, footer) = remaining.rows(remaining.height.saturating_sub(FOOTER_ROWS));
    header_view(frame, (header, viewport), model, now);
    body_view(frame, body, (model, now), viewport);
    footer_view(frame, (footer, viewport), model, now);
    if model.help {
        help(frame, area);
    }
    if let Some(position) = model.picker {
        picker(frame, area, model, position);
    }
}

fn header_view(
    frame: &mut Grid,
    (area, viewport): (Rect, Viewport),
    model: &Model,
    now: Option<u64>,
) {
    let mut brand = vec![Span::styled(" DOMYJOB ", emphasis(CYAN))];
    if viewport == Viewport::Sidebar {
        brand.push(Span::styled(" HUMAN DASHBOARD  ", emphasis(INK)));
    }
    let chats = model.request.actor.as_ref().map_or_else(
        || "chats: choose agent".to_owned(),
        |_| format!("{} chats", model.snapshot.chats.len()),
    );
    brand.push(Span::styled(
        format!(
            " {} machines · {} agents · {chats}",
            model.snapshot.machines.len(),
            model.snapshot.agents.len(),
        ),
        Style::default().fg(MINT),
    ));
    let viewing = model.request.actor.as_ref().map_or_else(
        || "Viewing public directory".to_owned(),
        |actor| format!("Viewing {}", actor.name()),
    );
    frame.paragraph(
        area,
        &[
            Line::spans(brand),
            Line::styled(
                format!(
                    " {viewing} · {}",
                    age(model.snapshot.observed, now, dashboard::STALE_MILLIS),
                ),
                Style::default().fg(MUTED),
            ),
        ],
        Flow {
            style: Style::default().bg(NAVY).fg(INK),
            scroll: 0,
            wrap: Wrap::Clip,
        },
    );
    let y = area.y.saturating_add(area.height).saturating_sub(1);
    frame.line(
        Rect::new(area.x, y, area.width, 1),
        &Line::styled(
            "─".repeat(usize::from(area.width)),
            Style::default().fg(BORDER),
        ),
        Style::default().bg(NAVY),
    );
}

fn footer_view(
    frame: &mut Grid,
    (area, viewport): (Rect, Viewport),
    model: &Model,
    now: Option<u64>,
) {
    let hint = if viewport == Viewport::Sidebar {
        " Tab sections  ↑↓/jk rows  Enter detail  / search  a choose agent  r refresh  ? help  q exit"
    } else {
        " q exit · ? help · Tab sections · Enter detail"
    };
    let status = if let Some(error) = model.error.as_deref() {
        format!(" Refresh error · {} · prior data retained", shown(error))
    } else {
        format!(
            " {} · passive local view",
            age(model.snapshot.observed, now, dashboard::STALE_MILLIS)
        )
    };
    frame.paragraph(
        area,
        &[
            Line::styled(hint, Style::default().fg(MUTED)),
            Line::styled(
                status,
                Style::default().fg(if model.error.is_some() { AMBER } else { MINT }),
            ),
        ],
        Flow {
            style: Style::default().bg(NAVY).fg(INK),
            scroll: 0,
            wrap: Wrap::Clip,
        },
    );
}

const fn section_name(section: Section) -> &'static str {
    match section {
        Section::Machines => "Machines",
        Section::Agents => "Agents",
        Section::Chats => "Chats",
    }
}

const fn section_counts(model: &Model) -> [(Section, usize); 3] {
    [
        (Section::Machines, model.snapshot.machines.len()),
        (Section::Agents, model.snapshot.agents.len()),
        (Section::Chats, model.snapshot.chats.len()),
    ]
}

fn navigation(frame: &mut Grid, area: Rect, model: &Model) {
    let selected = section_counts(model)
        .iter()
        .position(|(section, _)| *section == model.section);
    let items = section_counts(model)
        .into_iter()
        .map(|(section, count)| {
            vec![
                Line::from(section_name(section)),
                Line::styled(format!("{count} observed"), Style::default().fg(MUTED)),
                Line::from(""),
            ]
        })
        .collect::<Vec<_>>();
    let area = panel(frame, area, " Explore ");
    frame.list(
        area,
        &items,
        ListStyle {
            selected,
            normal: Style::default().fg(INK).bg(PANEL),
            highlight: emphasis(CYAN).bg(SELECTED),
        },
    );
}

fn tabs(frame: &mut Grid, area: Rect, model: &Model) {
    let counts = section_counts(model);
    let selected = counts
        .iter()
        .position(|(section, _)| *section == model.section)
        .unwrap_or_default();
    let mut spans = Vec::new();
    for (index, (section, _)) in counts.into_iter().enumerate() {
        if index > 0 {
            spans.push(Span::styled(" · ", Style::default().fg(MUTED)));
        }
        let style = if index == selected {
            emphasis(CYAN).bg(SELECTED)
        } else {
            Style::default().fg(MUTED)
        };
        spans.push(Span::styled(format!(" {} ", section_name(section)), style));
    }
    frame.line(area, &Line::spans(spans), Style::default().bg(NAVY));
}

fn body_view(
    frame: &mut Grid,
    area: Rect,
    (model, now): (&Model, Option<u64>),
    viewport: Viewport,
) {
    let content = if viewport == Viewport::Sidebar {
        let (sidebar, content) = area.columns(SIDEBAR_COLUMNS);
        navigation(frame, sidebar, model);
        content
    } else {
        let (navigation, content) = area.rows(TAB_ROWS);
        tabs(frame, navigation, model);
        content
    };
    match viewport {
        Viewport::Sidebar | Viewport::Split => {
            let (list, detail) =
                content.columns(share(content.width, LIST_SHARE).unwrap_or_default());
            rows(frame, list, model, now);
            detail_view(frame, detail, model, now);
        }
        Viewport::Compact if model.detail => detail_view(frame, content, model, now),
        Viewport::Compact => rows(frame, content, model, now),
        Viewport::Minimal => {}
    }
}

fn rows(frame: &mut Grid, area: Rect, model: &Model, now: Option<u64>) {
    let rows = model.rows();
    let mode = if model.searching || !model.query.is_empty() {
        format!("/{}", shown(&model.query))
    } else {
        format!("{} observed", rows.len())
    };
    let title = format!(" {} · {mode} ", section_name(model.section));
    if rows.is_empty() {
        let area = panel(frame, area, &title);
        frame.paragraph(
            area,
            &lines(empty(model)),
            Flow {
                style: Style::default().fg(MUTED).bg(PANEL),
                scroll: 0,
                wrap: Wrap::Fold,
            },
        );
        return;
    }
    let selected = rows
        .iter()
        .position(|(id, _)| model.selected.as_ref() == Some(id));
    let items = rows
        .iter()
        .map(|(id, label)| vec![Line::from(shown(label)), row_status(model, id, now)])
        .collect::<Vec<_>>();
    let area = panel(frame, area, &title);
    frame.list(
        area,
        &items,
        ListStyle {
            selected,
            normal: Style::default().fg(INK).bg(PANEL),
            highlight: emphasis(CYAN).bg(SELECTED),
        },
    );
}

fn empty(model: &Model) -> &'static str {
    if model.section == Section::Chats && model.request.actor.is_none() {
        "Choose an agent with a to view chats.\nBrowsing keeps read status unchanged."
    } else if !model.query.is_empty() {
        "No rows match this search.\nEsc clears the search."
    } else if model.section == Section::Chats {
        "No conversations to show for this agent.\nOnly domyjob messages appear here.\na chooses another agent."
    } else {
        "No observations yet.\nExisting local state appears here.\nr refreshes the view."
    }
}

fn row_status(model: &Model, id: &RowId, now: Option<u64>) -> Line {
    let status = match id {
        RowId::Machine(id) => model
            .snapshot
            .machines
            .iter()
            .find(|machine| &machine.id == id)
            .map(|machine| {
                let (state, color) = machine_status(machine, now);
                (format!("{state} · {:?}", machine.card.os), color)
            }),
        RowId::Agent(id) => model
            .snapshot
            .agents
            .iter()
            .find(|agent| &agent.id == id)
            .map(|agent| {
                let (state, color) = agent_status(agent, now);
                (format!("{} · {state}", agent.card.tool.as_str()), color)
            }),
        RowId::Chat(_) => Some(("Conversation · Enter history".to_owned(), MUTED)),
    };
    status.map_or_else(
        || Line::from("Observation unavailable"),
        |(text, color)| Line::styled(text, Style::default().fg(color)),
    )
}

fn machine_status(machine: &observer::Machine, now: Option<u64>) -> (&'static str, Color) {
    if machine.local {
        return ("Local ledger", MINT);
    }
    match machine.link.as_ref().map(|link| link.state) {
        Some(LinkState::Synced) => match dashboard::freshness(
            machine.link.as_ref().map(|link| link.at.millis()),
            now,
            dashboard::STALE_MILLIS,
        ) {
            Freshness::Fresh(_) => ("Synced", MINT),
            Freshness::Stale(_) => ("Sync observation expired", AMBER),
            Freshness::Unknown => ("Sync freshness unknown", MUTED),
        },
        Some(LinkState::Deferred) => ("Deferred", AMBER),
        Some(LinkState::Failed) => ("Offline", AMBER),
        None => ("Connection unknown", MUTED),
    }
}

fn agent_status(agent: &observer::Agent, now: Option<u64>) -> (&'static str, Color) {
    match dashboard::freshness(
        agent.activity.as_ref().map(|activity| activity.at.millis()),
        now,
        STALLED_MILLIS,
    ) {
        Freshness::Unknown => ("Activity unknown", MUTED),
        Freshness::Stale(_) => ("Stalled / activity expired", AMBER),
        Freshness::Fresh(_) => match agent.activity.as_ref().map(|activity| activity.state) {
            Some(ActivityState::Working) => ("Working", MINT),
            Some(ActivityState::Waiting) => ("Waiting", CYAN),
            Some(ActivityState::Stalled) => ("Stalled", AMBER),
            None => ("Activity unknown", MUTED),
        },
    }
}

fn detail_view(frame: &mut Grid, area: Rect, model: &Model, now: Option<u64>) {
    let lines = match model.selected.as_ref() {
        Some(RowId::Machine(id)) => model
            .snapshot
            .machines
            .iter()
            .find(|machine| &machine.id == id)
            .map(|machine| machine_detail(machine, now)),
        Some(RowId::Agent(id)) => model
            .snapshot
            .agents
            .iter()
            .find(|agent| &agent.id == id)
            .map(|agent| agent_detail(agent, now)),
        Some(RowId::Chat(id)) => {
            let mut lines = vec![
                Line::styled("Full conversation identity", Style::default().fg(MUTED)),
                Line::from(id.to_string()),
                Line::from(""),
            ];
            if model.request.conversation.as_ref() == Some(id) {
                lines.extend(message_detail(
                    &model.snapshot.messages,
                    model.snapshot.history_scan,
                ));
            } else {
                lines.extend([
                    Line::from("Press Enter to inspect this conversation."),
                    Line::from("Viewing keeps read and activity status unchanged."),
                ]);
            }
            Some(lines)
        }
        None => None,
    }
    .unwrap_or_else(|| vec![Line::from("Select a row to inspect its observations.")]);
    let title = if model.section == Section::Chats {
        " Conversation history "
    } else {
        " Detail "
    };
    let area = panel(frame, area, title);
    frame.paragraph(
        area,
        &lines,
        Flow {
            style: Style::default().fg(INK).bg(PANEL),
            scroll: model.scroll,
            wrap: Wrap::Fold,
        },
    );
}

fn field(label: &str, value: &str) -> Line {
    Line::from(format!("{label:<12} {}", shown(value)))
}

fn machine_detail(machine: &observer::Machine, now: Option<u64>) -> Vec<Line> {
    let (state, color) = machine_status(machine, now);
    let mut lines = vec![
        Line::styled(shown(&machine.label), emphasis(CYAN)),
        Line::styled(format!("Connection  {state}"), emphasis(color)),
        field("System", &format!("{:?}", machine.card.os)),
        field(
            "Last seen",
            &age(
                machine.link.as_ref().map(|link| link.at.millis()),
                now,
                dashboard::STALE_MILLIS,
            ),
        ),
        Line::from(""),
        Line::styled("Full identity", Style::default().fg(MUTED)),
        Line::from(machine.id.to_string()),
    ];
    if let Some(detail) = machine
        .link
        .as_ref()
        .and_then(|link| link.detail.as_deref())
    {
        lines.push(Line::from(""));
        lines.push(field("Sync detail", detail));
    }
    lines.push(Line::from(""));
    lines.push(Line::styled(
        "Connection and agent activity are independent.",
        Style::default().fg(MUTED),
    ));
    lines
}

fn agent_detail(agent: &observer::Agent, now: Option<u64>) -> Vec<Line> {
    let (state, color) = agent_status(agent, now);
    let mut lines = vec![
        Line::styled(shown(agent.card.display_name.as_str()), emphasis(CYAN)),
        Line::styled(format!("Activity    {state}"), emphasis(color)),
        field(
            "Last seen",
            &age(
                agent.activity.as_ref().map(|activity| activity.at.millis()),
                now,
                STALLED_MILLIS,
            ),
        ),
        field("Tool", agent.card.tool.as_str()),
        field(
            "Mode",
            &format!("{:?} · {:?} access", agent.card.mode, agent.card.access),
        ),
    ];
    if let Some(role) = &agent.card.role {
        lines.push(field("Role", role.as_str()));
    }
    if let Some(project) = &agent.card.project {
        lines.push(field("Project", project.as_str()));
    }
    let status = agent
        .activity
        .as_ref()
        .and_then(|activity| activity.status.as_ref())
        .map(domyjob_core::chat::id::Line::as_str)
        .or_else(|| {
            agent
                .card
                .status
                .as_ref()
                .map(domyjob_core::chat::id::Line::as_str)
        });
    if let Some(status) = status {
        lines.push(field("Current task", status));
    }
    if let Some(description) = &agent.card.description {
        lines.push(Line::from(""));
        lines.extend(
            description
                .as_str()
                .split('\n')
                .map(|line| Line::from(shown(line))),
        );
    }
    lines.push(Line::from(""));
    lines.push(Line::styled("Full identity", Style::default().fg(MUTED)));
    lines.push(Line::from(agent.id.to_string()));
    lines
}

const fn delivery_label(delivery: Delivery) -> &'static str {
    match delivery {
        Delivery::Pending => "Pending",
        Delivery::Delivered => "Delivered",
        Delivery::Read => "Read",
        Delivery::Acknowledged => "Acknowledged",
    }
}

fn message_detail(messages: &[observer::Message], scan: observer::HistoryScan) -> Vec<Line> {
    let mut lines = Vec::new();
    if scan == observer::HistoryScan::Limited {
        lines.push(Line::styled(
            "History scan limit reached; older messages may be omitted.",
            emphasis(AMBER),
        ));
        lines.push(Line::from(""));
    }
    for message in messages {
        lines.push(Line::styled(
            format!(
                "{} · observed {}",
                shown(&message.from),
                message.at.millis(),
            ),
            emphasis(CYAN),
        ));
        lines.extend(message.text.split('\n').map(|line| Line::from(shown(line))));
        if !message.recipients.is_empty() {
            lines.push(Line::styled(
                "Recipient delivery",
                Style::default().fg(MUTED),
            ));
        }
        for (recipient, delivery) in &message.recipients {
            lines.push(Line::styled(
                format!("{recipient} · {}", delivery_label(*delivery)),
                Style::default().fg(MINT),
            ));
            lines.push(Line::styled(
                "  pending → delivered → read → acknowledged",
                Style::default().fg(MUTED),
            ));
        }
        lines.push(Line::styled(
            message.id.to_string(),
            Style::default().fg(MUTED),
        ));
        lines.push(Line::from(""));
    }
    if messages.is_empty() {
        lines.push(Line::from(match scan {
            observer::HistoryScan::WithinBudget => "No messages to display.",
            observer::HistoryScan::Limited => "No messages in the scanned range.",
        }));
    }
    lines
}

fn modal(area: Rect) -> Rect {
    area.centered(MODAL_WIDTH, MODAL_HEIGHT).unwrap_or_default()
}

fn lines(text: &str) -> Vec<Line> {
    text.split('\n').map(Line::from).collect()
}

fn help(frame: &mut Grid, area: Rect) {
    let area = modal(area);
    let area = panel(frame, area, " Keyboard guide · any key closes ");
    frame.paragraph(area, &lines(
        "Tab           Machines / Agents / Chats\n↑↓ or j/k     Move selection\nEnter         Open detail\nPgUp/PgDn     Scroll detail\n/             Search · Enter confirms\nEsc           Clear search / back\na             Choose agent for Chats\nr             Refresh local observations\n?             Open help · any key closes\nq             Exit\n\nViewing keeps read and activity status unchanged.\nIn Chats, choose a local agent and press Enter.\nSelect a conversation and press Enter to read it."
    ), Flow { style: Style::default().fg(INK).bg(PANEL), scroll: 0, wrap: Wrap::Fold });
}

fn picker(frame: &mut Grid, area: Rect, model: &Model, position: usize) {
    let area = modal(area);
    if model.snapshot.local_agents.is_empty() {
        let area = panel(frame, area, " Choose agent for chats ");
        frame.paragraph(
            area,
            &lines("No local agents registered.\nOnly domyjob messages appear here.\nEsc closes this picker."),
            Flow {
                style: Style::default().fg(INK).bg(PANEL),
                scroll: 0,
                wrap: Wrap::Fold,
            },
        );
        return;
    }
    let items = model
        .snapshot
        .local_agents
        .iter()
        .map(|agent| {
            vec![
                Line::from(agent.name().to_string()),
                Line::styled(agent.to_string(), Style::default().fg(MUTED)),
            ]
        })
        .collect::<Vec<_>>();
    let area = panel(frame, area, " Choose agent · Enter select · Esc back ");
    frame.list(
        area,
        &items,
        ListStyle {
            selected: Some(position),
            normal: Style::default().fg(INK).bg(PANEL),
            highlight: emphasis(CYAN).bg(SELECTED),
        },
    );
}

#[cfg(test)]
mod tests;
