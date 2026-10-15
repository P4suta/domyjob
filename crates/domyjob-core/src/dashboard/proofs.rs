use super::*;

const MOVEMENT_VARIANTS: u8 = 4;
const FIRST_MOVEMENT_CODE: u8 = 2;
const TERMINAL_STAGES: u8 = 4;
const SCREEN_STAGE_CODE: u8 = 2;

fn movement() -> Movement {
    match kani::any::<u8>() % MOVEMENT_VARIANTS {
        0 => Movement::Previous,
        1 => Movement::Next,
        FIRST_MOVEMENT_CODE => Movement::First,
        _ => Movement::Last,
    }
}

#[kani::proof]
fn navigation_and_selection_are_bounded() {
    let count: usize = kani::any();
    let position: usize = kani::any();
    let selected = if kani::any() { Some(position) } else { None };
    let result = navigate(selected, count, movement());
    assert_eq!(result.is_none(), count == 0);
    if let Some(index) = result {
        assert!(index < count);
    }
    let identity = if kani::any() {
        IdentityMatch::Exact
    } else {
        IdentityMatch::Different
    };
    let retained = retained_selection(identity, position, count);
    assert_eq!(
        retained.is_some(),
        identity == IdentityMatch::Exact && position < count
    );
    let had_selection: bool = kani::any();
    let refreshed = selection_after_refresh(had_selection, retained, count);
    if had_selection && retained.is_none() {
        assert!(refreshed.is_none());
    }
    if let Some(position) = refreshed {
        assert!(position < count);
    }
    if !had_selection && retained.is_none() {
        assert_eq!(refreshed.is_some(), count > 0);
    }
    let registered: bool = kani::any();
    assert_eq!(
        viewing_identity(identity, registered),
        identity == IdentityMatch::Exact && registered
    );
    kani::cover!(count == 0 && result.is_none());
    kani::cover!(count == usize::MAX && result.is_some());
    kani::cover!(retained.is_some());
    kani::cover!(identity == IdentityMatch::Different && retained.is_none());
    kani::cover!(viewing_identity(identity, registered));
    kani::cover!(
        registered
            && identity == IdentityMatch::Different
            && !viewing_identity(identity, registered)
    );
}

#[kani::proof]
fn query_admission_preserves_utf8_and_control_boundaries() {
    let used: usize = kani::any();
    let character: char = kani::any();
    let admitted = query_admits(used, character);
    assert_eq!(
        admitted,
        matches!(display_char(character), DisplayChar::Literal(_))
            && used
                .checked_add(character.len_utf8())
                .is_some_and(|total| total <= MAX_QUERY_BYTES)
    );
    if let DisplayChar::Literal(literal) = display_char(character) {
        assert!(!literal.is_control());
        assert!(
            !matches!(literal, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}' | '\u{200e}' | '\u{200f}')
        );
    }
    kani::cover!(admitted && character == 'a');
    kani::cover!(admitted && character == '界');
    kani::cover!(!admitted && character == '\u{1b}');
    kani::cover!(!admitted && used == usize::MAX);
}

#[kani::proof]
fn freshness_never_hides_clock_rollback() {
    let observed: Option<u64> = kani::any();
    let now: Option<u64> = kani::any();
    let limit: u64 = kani::any();
    let result = freshness(observed, now, limit);
    if let Freshness::Fresh(age) | Freshness::Stale(age) = result {
        assert!(
            observed
                .zip(now)
                .is_some_and(|(at, current)| at <= current && current - at == age)
        );
    }
    match result {
        Freshness::Unknown => assert!(
            observed.is_none()
                || now.is_none()
                || observed.zip(now).is_some_and(|(at, current)| current < at)
        ),
        Freshness::Fresh(age) => assert!(age < limit),
        Freshness::Stale(age) => assert!(age >= limit),
    }
    kani::cover!(observed.is_none());
    kani::cover!(observed.zip(now).is_some_and(|(at, current)| current < at));
    kani::cover!(observed.is_some() && now.is_none());
    kani::cover!(matches!(result, Freshness::Fresh(_)));
    kani::cover!(matches!(result, Freshness::Stale(_)));
}

#[kani::proof]
fn refresh_accepts_only_the_pending_generation() {
    let accepted: u64 = kani::any();
    let pending: Option<u64> = kani::any();
    let mut state = Refresh { accepted, pending };
    let before = state;
    let generation: u64 = kani::any();
    let admitted = state.complete(generation);
    assert_eq!(
        admitted,
        pending == Some(generation) && generation > accepted
    );
    if admitted {
        assert_eq!(state.accepted, generation);
        assert!(state.pending.is_none());
    } else {
        assert_eq!(state, before);
    }
    let mut request = before;
    let issued = request.request();
    assert_eq!(issued.is_some(), pending.is_none() && accepted < u64::MAX);
    if let Some(issued) = issued {
        assert!(issued > accepted);
        assert_eq!(request.pending, Some(issued));
    }
    kani::cover!(admitted);
    kani::cover!(!admitted && pending.is_some());
    kani::cover!(issued.is_some());
    kani::cover!(pending.is_none() && accepted == u64::MAX && issued.is_none());
    let mut view = before;
    let scope = if kani::any() {
        IdentityMatch::Exact
    } else {
        IdentityMatch::Different
    };
    let visible = view.complete_view(generation, scope);
    assert_eq!(visible, admitted && scope == IdentityMatch::Exact);
    assert_eq!(view, state);
    kani::cover!(visible);
    kani::cover!(admitted && scope == IdentityMatch::Different && !visible);
}

#[kani::proof]
fn viewport_policy_is_total_for_every_terminal_size() {
    let width: u16 = kani::any();
    let height: u16 = kani::any();
    let result = viewport(width, height);
    assert_eq!(
        result == Viewport::Minimal,
        width < MIN_VIEW_WIDTH || height < MIN_VIEW_HEIGHT
    );
    match result {
        Viewport::Minimal => {}
        Viewport::Compact => assert!(
            width >= MIN_VIEW_WIDTH && width < SPLIT_VIEW_WIDTH && height >= MIN_VIEW_HEIGHT
        ),
        Viewport::Split => assert!(
            width >= SPLIT_VIEW_WIDTH && width < SIDEBAR_VIEW_WIDTH && height >= MIN_VIEW_HEIGHT
        ),
        Viewport::Sidebar => assert!(width >= SIDEBAR_VIEW_WIDTH && height >= MIN_VIEW_HEIGHT),
    }
    kani::cover!(result == Viewport::Minimal);
    kani::cover!(result == Viewport::Compact);
    kani::cover!(result == Viewport::Split);
    kani::cover!(result == Viewport::Sidebar);
}

#[kani::proof]
fn every_attempted_terminal_mode_has_owned_cleanup() {
    let stage = match kani::any::<u8>() % TERMINAL_STAGES {
        0 => TerminalStage::Untouched,
        1 => TerminalStage::RawAttempted,
        SCREEN_STAGE_CODE => TerminalStage::ScreenAttempted,
        _ => TerminalStage::CursorAttempted,
    };
    assert_eq!(stage.owns(Restore::Raw), stage != TerminalStage::Untouched);
    assert_eq!(
        stage.owns(Restore::Screen),
        matches!(
            stage,
            TerminalStage::ScreenAttempted | TerminalStage::CursorAttempted
        )
    );
    assert_eq!(
        stage.owns(Restore::Cursor),
        stage == TerminalStage::CursorAttempted
    );
    kani::cover!(stage == TerminalStage::Untouched);
    kani::cover!(stage == TerminalStage::RawAttempted);
    kani::cover!(stage == TerminalStage::ScreenAttempted);
    kani::cover!(stage == TerminalStage::CursorAttempted);
}

#[kani::proof]
fn rejects_foreign_selection_retention() {
    kani::assert(
        retained_selection(IdentityMatch::Different, 0, 1).is_some(),
        "NEGATIVE_DASHBOARD_SELECTION_CONTROL",
    );
}

#[kani::proof]
fn rejects_a_control_character_in_search() {
    kani::assert(
        query_admits(0, '\u{1b}'),
        "NEGATIVE_DASHBOARD_QUERY_CONTROL",
    );
}

#[kani::proof]
fn rejects_rollback_reported_as_fresh() {
    kani::assert(
        matches!(freshness(Some(2), Some(1), 10), Freshness::Fresh(_)),
        "NEGATIVE_DASHBOARD_CLOCK_CONTROL",
    );
}

#[kani::proof]
fn rejects_an_unrequested_refresh_completion() {
    let mut refresh = Refresh::default();
    kani::assert(refresh.complete(1), "NEGATIVE_DASHBOARD_REFRESH_CONTROL");
}

#[kani::proof]
fn rejects_skipping_partial_screen_cleanup() {
    kani::assert(
        !TerminalStage::ScreenAttempted.owns(Restore::Screen),
        "NEGATIVE_DASHBOARD_CLEANUP_CONTROL",
    );
}

#[kani::proof]
fn rejects_sidebar_in_a_narrow_terminal() {
    kani::assert(
        viewport(48, 12) == Viewport::Sidebar,
        "NEGATIVE_DASHBOARD_VIEWPORT_CONTROL",
    );
}
