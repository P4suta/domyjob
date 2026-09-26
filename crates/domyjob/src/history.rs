use std::collections::BTreeMap;

use crate::clock::{Elapsed, Timestamp};
use crate::domain::MachineName;
use crate::protocol::{Job, State};

const RECENT: usize = 12;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Series {
    pub machine: MachineName,
    pub label: String,
    pub recent: Vec<State>,
    pub runs: usize,
    pub succeeded: usize,
    pub typical: Option<Elapsed>,
    pub last: Timestamp,
    last_sequence: u64,
}

#[must_use]
pub fn series(jobs: &[(MachineName, Job)]) -> Vec<Series> {
    let mut grouped: BTreeMap<(MachineName, String), Vec<&Job>> = BTreeMap::new();
    for (machine, job) in jobs.iter().filter(|(_, job)| job.is_settled()) {
        grouped
            .entry((machine.clone(), crate::ui::label(job)))
            .or_default()
            .push(job);
    }
    let mut all: Vec<Series> = grouped
        .into_iter()
        .filter_map(|((machine, label), mut runs)| {
            runs.sort_by_key(|job| job.spec.sequence);
            let newest = *runs.last()?;
            let mut took: Vec<Elapsed> = runs
                .iter()
                .filter(|job| matches!(job.state(), State::Succeeded | State::Failed))
                .filter_map(|job| job.took())
                .collect();
            took.sort_by_key(|span| span.millis());
            let typical = took.get(took.len() / 2).copied();
            Some(Series {
                machine,
                label,
                recent: runs
                    .iter()
                    .rev()
                    .take(RECENT)
                    .rev()
                    .map(|job| job.state())
                    .collect(),
                runs: runs.len(),
                succeeded: runs.iter().filter(|job| job.succeeded()).count(),
                typical,
                last: newest.spec.submitted_at,
                last_sequence: newest.spec.sequence,
            })
        })
        .collect();
    all.sort_by(|a, b| {
        a.machine
            .cmp(&b.machine)
            .then(b.last_sequence.cmp(&a.last_sequence))
    });
    all
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{Outcome, Phase};

    fn job((id, name): (&str, &str), sequence: u64, outcome: Outcome, took: i64) -> Job {
        let mut job = crate::view::tests::sample();
        job.spec.id = id.parse().unwrap();
        job.spec.name = Some(name.parse().unwrap());
        job.spec.sequence = sequence;
        job.phase = Phase::Finished {
            started_at: Some(Timestamp::at_millis(0)),
            finished_at: Timestamp::at_millis(took),
            outcome,
        };
        job
    }

    #[test]
    fn runs_group_by_machine_and_name_newest_first_with_a_typical_duration() {
        let linux: MachineName = "linux".parse().unwrap();
        let win: MachineName = "win".parse().unwrap();
        let jobs = vec![
            (
                linux.clone(),
                job(("0AAAAAAAAAAAAAAA", "tests"), 1, Outcome::Succeeded, 10_000),
            ),
            (
                linux.clone(),
                job(
                    ("0BBBBBBBBBBBBBBB", "tests"),
                    2,
                    Outcome::Failed { exit_code: 1 },
                    30_000,
                ),
            ),
            (
                linux.clone(),
                job(("0CCCCCCCCCCCCCCC", "tests"), 3, Outcome::Succeeded, 20_000),
            ),
            (
                win,
                job(("0DDDDDDDDDDDDDDD", "build"), 4, Outcome::Succeeded, 5_000),
            ),
            (linux, crate::view::tests::sample()),
        ];
        let found = series(&jobs);
        assert_eq!(found.len(), 2);
        let (tests, build) = (found.first().unwrap(), found.last().unwrap());
        assert_eq!(
            (build.machine.as_str(), build.label.as_str()),
            ("win", "build")
        );
        assert_eq!(
            tests.recent,
            [State::Succeeded, State::Failed, State::Succeeded]
        );
        assert_eq!((tests.runs, tests.succeeded), (3, 2));
        assert_eq!(tests.typical.map(Elapsed::millis), Some(20_000));
    }
}
