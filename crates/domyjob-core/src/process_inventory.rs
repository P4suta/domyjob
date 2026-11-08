use crate::process_group::ProcessGroup;

pub const MAX_INVENTORY_BYTES: usize = 1_048_576;
const STATE_FLAGS: &[u8] = b"<NLsl+AXEVW";

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum InvalidInventory {
    #[error("the process inventory is empty")]
    Empty,
    #[error("the process inventory exceeds its budget ({bytes} bytes)")]
    TooLarge { bytes: usize },
    #[error("the process inventory has a malformed row at line {line}")]
    Malformed { line: usize },
}

const fn classify_state(state: u8) -> Option<bool> {
    match state {
        b'Z' => Some(true),
        b'R' | b'S' | b'D' | b'I' | b'T' | b't' | b'U' | b'X' | b'W' | b'E' | b'P' | b'K'
        | b'H' | b'?' => Some(false),
        _ => None,
    }
}

#[derive(Debug, Clone, Copy)]
struct Process {
    group: u32,
    zombie: bool,
}

impl Process {
    const fn blocks(self, group: u32) -> bool {
        self.group == group && !self.zombie
    }
}

pub fn group_quiescent(text: &str, group: ProcessGroup) -> Result<bool, InvalidInventory> {
    if text.len() > MAX_INVENTORY_BYTES {
        return Err(InvalidInventory::TooLarge { bytes: text.len() });
    }
    if text.trim().is_empty() {
        return Err(InvalidInventory::Empty);
    }
    let selected = group.get().unsigned_abs();
    let mut quiet = true;
    for (index, line) in text
        .lines()
        .enumerate()
        .filter(|(_, line)| !line.trim().is_empty())
    {
        let invalid = || InvalidInventory::Malformed {
            line: index.saturating_add(1),
        };
        let (pid, remaining) = line
            .trim_start()
            .split_once(char::is_whitespace)
            .ok_or_else(invalid)?;
        let (process_group, state) = remaining
            .trim_start()
            .split_once(char::is_whitespace)
            .ok_or_else(invalid)?;
        let _pid: u32 = pid.parse().map_err(|_invalid| invalid())?;
        let process_group: u32 = process_group.parse().map_err(|_invalid| invalid())?;
        let mut state_bytes = state.trim().bytes();
        let state_code = state_bytes.next().ok_or_else(invalid)?;
        let zombie = classify_state(state_code).ok_or_else(invalid)?;
        if !state_bytes.all(|byte| STATE_FLAGS.contains(&byte)) {
            return Err(invalid());
        }
        quiet &= !Process {
            group: process_group,
            zombie,
        }
        .blocks(selected);
    }
    Ok(quiet)
}

#[cfg(kani)]
mod proofs {
    use super::{Process, classify_state};

    #[kani::proof]
    fn live_group_members_prevent_workspace_removal() {
        let selected: u32 = kani::any();
        let group: u32 = kani::any();
        let state: u8 = kani::any();
        let recognized = matches!(
            state,
            b'R' | b'S'
                | b'D'
                | b'I'
                | b'T'
                | b't'
                | b'Z'
                | b'U'
                | b'X'
                | b'W'
                | b'E'
                | b'P'
                | b'K'
                | b'H'
                | b'?'
        );
        let classified = classify_state(state);
        assert_eq!(classified, recognized.then_some(state == b'Z'));
        if let Some(zombie) = classified {
            let process = Process { group, zombie };
            assert_eq!(process.blocks(selected), group == selected && !zombie);
            kani::cover!(group == selected && !zombie);
            kani::cover!(group == selected && zombie);
            kani::cover!(group != selected && !zombie);
            kani::cover!(group == selected && state == b'H');
            kani::cover!(group == selected && state == b'?');
        }
    }

    #[kani::proof]
    fn rejects_live_group_cleanup() {
        let Some(zombie) = classify_state(b'H') else {
            return;
        };
        kani::assert(
            !Process { group: 1, zombie }.blocks(1),
            "NEGATIVE_QUIESCENCE_CONTROL",
        );
    }
}

#[cfg(test)]
mod tests {
    use alloc::string::String;

    use super::{InvalidInventory, MAX_INVENTORY_BYTES, group_quiescent};
    use crate::process_group::ProcessGroup;

    const SELECTED_GROUP: i32 = 17;

    #[test]
    fn only_live_members_of_the_exact_group_block_cleanup() {
        let group = ProcessGroup::new(SELECTED_GROUP).unwrap();
        assert_eq!(group_quiescent("1 1 S\n42 17 Z+\n", group), Ok(true));
        assert_eq!(group_quiescent("1 1 S\n42 17 S+\n", group), Ok(false));
        assert_eq!(group_quiescent("", group), Err(InvalidInventory::Empty));
        for text in [
            "42",
            "42 17",
            "42 17 \t",
            "42 17 S trailing",
            "42 garbage S",
            "garbage 17 Z",
            "42 17 Zgarbage",
        ] {
            assert_eq!(
                group_quiescent(text, group),
                Err(InvalidInventory::Malformed { line: 1 })
            );
        }
    }

    #[test]
    fn inventory_accepts_its_exact_budget_and_refuses_one_more_byte() {
        let group = ProcessGroup::new(SELECTED_GROUP).unwrap();
        let mut text = String::from("1 1 S\n");
        text.extend(core::iter::repeat_n(' ', MAX_INVENTORY_BYTES - text.len()));
        assert_eq!(group_quiescent(&text, group), Ok(true));
        text.push(' ');
        assert_eq!(
            group_quiescent(&text, group),
            Err(InvalidInventory::TooLarge { bytes: text.len() })
        );
    }

    #[test]
    fn native_halted_and_unknown_task_states_are_conservatively_live() {
        let group = ProcessGroup::new(SELECTED_GROUP).unwrap();
        for state in ["H", "?", "H<", "?EV"] {
            let selected = alloc::format!("1 1 S\n42 {SELECTED_GROUP} {state}\n");
            assert_eq!(group_quiescent(&selected, group), Ok(false));
            let foreign = alloc::format!("1 1 S\n42 1 {state}\n");
            assert_eq!(group_quiescent(&foreign, group), Ok(true));
        }
    }

    #[test]
    fn malformed_inventory_keeps_the_original_line_number_including_blank_rows() {
        const INVALID_LINE: usize = 4;
        let group = ProcessGroup::new(SELECTED_GROUP).unwrap();
        assert_eq!(
            group_quiescent("\n1 1 S\n\n42 17 Q\n", group),
            Err(InvalidInventory::Malformed { line: INVALID_LINE })
        );
    }
}
