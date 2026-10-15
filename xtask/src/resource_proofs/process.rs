use super::exit_policy::{self, Phase};

pub(super) fn check_exit(status: std::process::ExitStatus, phase: Phase) -> Result<(), String> {
    if exit_policy::admitted(phase, status.code()) {
        Ok(())
    } else {
        Err(format!(
            "Kani {phase:?} requires its exact normal exit code: {status}"
        ))
    }
}

pub(crate) fn require_native_linux() -> Result<(), String> {
    if std::env::consts::OS == "linux" {
        Ok(())
    } else {
        Err("Parent native-default calibration requires native Linux".to_owned())
    }
}

#[cfg(test)]
mod tests {
    #[cfg(unix)]
    #[test]
    fn actual_unix_status_projection_refuses_signals_and_other_normal_codes() {
        use super::Phase;
        use std::os::unix::process::ExitStatusExt;

        const STATUS_SHIFT: u32 = 8;
        const UNRELATED_EXIT: i32 = 2;
        const TERMINATION_SIGNAL: i32 = 15;
        let cases = [
            (0, true, false),
            (1 << STATUS_SHIFT, false, true),
            (UNRELATED_EXIT << STATUS_SHIFT, false, false),
            (TERMINATION_SIGNAL, false, false),
        ];
        let mut observed = Vec::new();
        let mut expected = Vec::new();
        for (raw, positive, negative) in cases {
            let status = std::process::ExitStatus::from_raw(raw);
            observed.push((
                status.code(),
                super::check_exit(status, Phase::Positive).is_ok(),
                super::check_exit(status, Phase::NamedNegative).is_ok(),
            ));
            expected.push((status.code(), positive, negative));
        }
        assert_eq!(observed, expected);
    }

    #[cfg(not(target_os = "linux"))]
    #[test]
    fn parent_calibration_refuses_non_linux_before_invoking_a_verifier() {
        let root = tempfile::tempdir().unwrap();
        assert!(
            crate::verification::dispatch(root.path(), &["parent-identity"])
                .unwrap_err()
                .contains("requires native Linux")
        );
    }
}
