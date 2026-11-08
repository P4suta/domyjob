use super::group_quiescent_using;
use crate::process::{MAX_TOOL_OUTPUT, Tool, ToolError};

const SELECTED_GROUP: i32 = 17;

#[test]
fn absent_and_nonterminal_wait_results_cannot_authorize_process_group_cleanup() {
    use rustix::process::{Pid, Signal, WaitId, WaitIdOptions, kill_process, waitid};

    let mut child = crate::process::command("/bin/sh");
    child.args(["-c", "kill -STOP $$; exit 7"]);
    let mut child = child.spawn().unwrap();
    let pid = Pid::from_raw(i32::try_from(child.id()).unwrap()).unwrap();
    let stopped = waitid(
        WaitId::Pid(pid),
        WaitIdOptions::STOPPED | WaitIdOptions::NOWAIT,
    )
    .unwrap()
    .unwrap();
    assert!(stopped.stopped());
    kill_process(pid, Signal::CONT).unwrap();
    let exited = waitid(
        WaitId::Pid(pid),
        WaitIdOptions::EXITED | WaitIdOptions::NOWAIT,
    )
    .unwrap()
    .unwrap();
    assert_eq!(exited.exit_status(), Some(7));
    let mut observations = [
        Ok(None),
        Ok(Some(stopped)),
        Err(rustix::io::Errno::INTR),
        Ok(Some(exited)),
    ]
    .into_iter();
    super::await_leader_using(|| observations.next().unwrap()).unwrap();
    assert_eq!(child.wait().unwrap().code(), Some(7));
    assert_eq!(observations.count(), 0);
}

#[test]
fn a_live_group_beyond_the_capture_budget_cannot_be_declared_quiet() {
    let arguments = [
        "-c".to_owned(),
        format!("yes '1 1 S  ' | head -c {MAX_TOOL_OUTPUT}; printf '42 {SELECTED_GROUP} S\\n'"),
    ];
    let error = group_quiescent_using(SELECTED_GROUP, Tool::new("sh", &arguments)).unwrap_err();
    let cause = error
        .get_ref()
        .and_then(|cause| cause.downcast_ref::<ToolError>());
    assert!(
        matches!(cause, Some(ToolError::Run { source, .. }) if source.kind() == std::io::ErrorKind::InvalidData),
        "the complete inventory must refuse its omitted suffix: {error}"
    );
}

#[test]
fn a_rejected_inventory_keeps_the_exact_offending_row_and_stderr() {
    const BAD_ROW: &str = "42 17 Q";
    const INSPECTION_CAUSE: &str = "native inspection fixture failed";
    let arguments = [
        "-c".to_owned(),
        format!("printf '1 1 S\\n{BAD_ROW}\\n'; printf '{INSPECTION_CAUSE}' >&2"),
    ];
    let error = group_quiescent_using(SELECTED_GROUP, Tool::new("sh", &arguments)).unwrap_err();
    let detail = error.to_string();
    assert!(detail.contains(BAD_ROW), "{error}");
    assert!(detail.contains(INSPECTION_CAUSE), "{error}");
}

#[test]
fn successful_empty_native_output_keeps_its_stderr_refusal() {
    const INSPECTION_CAUSE: &str = "Failure calling sysctl";
    let arguments = [
        "-c".to_owned(),
        format!("printf '{INSPECTION_CAUSE}' >&2; exit 0"),
    ];
    let error = group_quiescent_using(SELECTED_GROUP, Tool::new("sh", &arguments)).unwrap_err();
    assert!(error.to_string().contains(INSPECTION_CAUSE), "{error}");
}

#[test]
fn unreadable_inventory_never_confirms_quiescence_or_removes_a_workspace() {
    const WORKSPACE_BYTES: &[u8] = b"preserved workspace";
    for output in ["printf '42 17 Q\\n'", "printf 'Failure calling sysctl' >&2"] {
        let root = tempfile::tempdir().unwrap();
        let state = root.path().join("state");
        let workspace = state.join("workspace/payload");
        let marker = state.join("quiesced");
        crate::state_io::write_bytes(&workspace, WORKSPACE_BYTES).unwrap();
        let arguments = ["-c".to_owned(), output.to_owned()];
        let error = super::super::confirm_quiescence_using(
            || {
                let quiet = group_quiescent_using(SELECTED_GROUP, Tool::new("sh", &arguments))
                    .map_err(crate::process::ProcessError::Wait)?;
                if quiet {
                    Ok(())
                } else {
                    Err(crate::process::ProcessError::Wait(std::io::Error::other(
                        "the process group is still live",
                    )))
                }
            },
            Some(&marker),
        )
        .unwrap_err();
        assert!(
            matches!(&error, crate::process::ProcessError::Wait(source) if source.kind() == std::io::ErrorKind::InvalidData),
            "{error}"
        );
        assert_eq!(crate::state_io::read_bytes(&marker).unwrap(), None);
        assert_eq!(
            crate::state_io::read_bytes(&workspace).unwrap().as_deref(),
            Some(WORKSPACE_BYTES)
        );
    }
}

#[test]
fn inventory_refusal_diagnostics_keep_finite_row_and_stderr_prefixes() {
    const PREFIX_BYTES: usize = 1024;
    const ROW_SUFFIX: &str = "unrecorded row suffix";
    const STDERR_SUFFIX: &str = "unrecorded stderr suffix";
    let arguments = [
        "-c".to_owned(),
        format!(
            "printf '%s{ROW_SUFFIX}\\n' '{prefix}'; printf '%s{STDERR_SUFFIX}' '{prefix}' >&2",
            prefix = "q".repeat(PREFIX_BYTES),
        ),
    ];
    let error = group_quiescent_using(SELECTED_GROUP, Tool::new("sh", &arguments)).unwrap_err();
    let detail = error.to_string();
    assert!(detail.contains("later diagnostic bytes omitted"));
    assert!(!detail.contains(ROW_SUFFIX));
    assert!(!detail.contains(STDERR_SUFFIX));
    assert!(detail.len() < PREFIX_BYTES);
}
