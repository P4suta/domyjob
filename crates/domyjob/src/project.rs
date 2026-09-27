use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::config::{Config, ConfigError, MAX_SELECTOR_TERMS, Machine};
use crate::domain::{EnvName, JobName, MachineName, RelPath};
use crate::protocol::Workspace;

pub const FILE: &str = "domyjob.toml";

#[derive(Debug, thiserror::Error)]
pub enum ProjectError {
    #[error(transparent)]
    Io(#[from] crate::failure::IoFailure),
    #[error("{origin}: {source}")]
    Parse {
        origin: String,
        source: Box<toml::de::Error>,
    },
    #[error("{origin} exceeds its {limit}-byte project config budget")]
    TooLarge { origin: String, limit: u64 },
    #[error("no job named {0} in {FILE}")]
    NoSuchJob(JobName),
    #[error("{0} exists but is not a file")]
    NotFile(PathBuf),
    #[error(transparent)]
    Config(#[from] ConfigError),
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct File {
    jobs: Option<BTreeMap<JobName, RepositoryRequest>>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RepositoryRequest {
    on: ProjectSelector,
    run: Vec<String>,
    runner: Option<String>,
    workspace: Option<Workspace>,
    dir: Option<RelPath>,
    env: Option<BTreeMap<EnvName, String>>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(try_from = "String")]
pub struct ProjectSelector(RequestedTargets);

#[derive(Debug, Clone)]
enum RequestedTargets {
    All,
    Names(Vec<MachineName>),
}

impl TryFrom<String> for ProjectSelector {
    type Error = ConfigError;

    fn try_from(text: String) -> Result<Self, Self::Error> {
        if text == "@all" {
            return Ok(Self(RequestedTargets::All));
        }
        let names = text
            .split(',')
            .map(str::trim)
            .map(str::parse::<MachineName>)
            .take(MAX_SELECTOR_TERMS.saturating_add(1))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|_invalid| ConfigError::ProjectSelector(text.clone()))?;
        if names.len() > MAX_SELECTOR_TERMS {
            return Err(ConfigError::SelectorTooComplex {
                limit: MAX_SELECTOR_TERMS,
            });
        }
        if names.is_empty()
            || names.iter().any(|name| name.as_str().starts_with("ssh:"))
            || names.iter().any(|name| name.as_str().starts_with('@'))
        {
            return Err(ConfigError::ProjectSelector(text));
        }
        Ok(Self(RequestedTargets::Names(names)))
    }
}

impl std::str::FromStr for ProjectSelector {
    type Err = ConfigError;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        Self::try_from(text.to_owned())
    }
}

#[derive(Debug, Clone)]
pub struct ApprovedProjectTargets(Vec<Machine>);

impl ApprovedProjectTargets {
    #[must_use]
    pub(crate) fn into_machines(self) -> Vec<Machine> {
        self.0
    }
}

#[derive(Debug, Clone)]
pub struct ApprovedProjectWord(String);

impl ApprovedProjectWord {
    #[must_use]
    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone)]
pub struct ApprovedProjectJob {
    parts: ProjectOrderParts,
}

#[derive(Debug, Clone)]
pub(crate) struct ProjectOrderParts {
    pub root: PathBuf,
    pub targets: ApprovedProjectTargets,
    pub words: Vec<ApprovedProjectWord>,
    pub runner: Option<String>,
    pub workspace: Workspace,
    pub dir: Option<RelPath>,
    pub env: BTreeMap<EnvName, String>,
}

impl ApprovedProjectJob {
    #[must_use]
    pub(crate) fn into_parts(self) -> ProjectOrderParts {
        self.parts
    }
}

#[derive(Debug, Clone, Default)]
pub struct Project {
    jobs: BTreeMap<JobName, RepositoryRequest>,
}

impl Project {
    pub fn parse(text: &str, origin: &str) -> Result<Self, ProjectError> {
        if crate::domain::len_u64(text.len()) > crate::bounded::CONFIG_TEXT {
            return Err(ProjectError::TooLarge {
                origin: origin.to_owned(),
                limit: crate::bounded::CONFIG_TEXT,
            });
        }
        let file: File = crate::ingress::toml(text).map_err(|source| ProjectError::Parse {
            origin: origin.to_owned(),
            source: Box::new(source),
        })?;
        Ok(Self {
            jobs: file.jobs.unwrap_or_default(),
        })
    }

    pub fn load(root: &Path) -> Result<Option<Self>, ProjectError> {
        let path = root.join(FILE);
        match crate::bounded::text_file(&path, crate::bounded::CONFIG_TEXT) {
            Ok(text) => Self::parse(&text, &path.display().to_string()).map(Some),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(source) => Err(crate::failure::io("reading", &path)(source).into()),
        }
    }

    pub fn job(&self, name: &JobName) -> Result<&RepositoryRequest, ProjectError> {
        self.jobs
            .get(name)
            .ok_or_else(|| ProjectError::NoSuchJob(name.clone()))
    }
}

impl RepositoryRequest {
    pub fn approve(
        &self,
        config: &Config,
        root: &Path,
        override_on: Option<&ProjectSelector>,
    ) -> Result<ApprovedProjectJob, ProjectError> {
        let (root, policy) = config.project_policy(root)?;
        let selector = override_on.unwrap_or(&self.on);
        let names = match &selector.0 {
            RequestedTargets::All => policy.machines().iter().cloned().collect(),
            RequestedTargets::Names(names) => names.clone(),
        };
        let mut machines = Vec::new();
        for machine_name in names {
            if !policy.machines().contains(&machine_name) {
                return Err(ConfigError::ProjectMachineDenied {
                    root: policy.root().to_path_buf(),
                    machine: machine_name,
                }
                .into());
            }
            if !machines
                .iter()
                .any(|known: &Machine| known.name == machine_name)
            {
                machines.push(config.machine(&machine_name)?);
            }
        }
        Ok(ApprovedProjectJob {
            parts: ProjectOrderParts {
                root,
                targets: ApprovedProjectTargets(machines),
                words: self.run.iter().cloned().map(ApprovedProjectWord).collect(),
                runner: self.runner.clone(),
                workspace: self.workspace.unwrap_or(Workspace::Warm),
                dir: self.dir.clone(),
                env: self.env.clone().unwrap_or_default(),
            },
        })
    }
}

pub fn find_root(start: &Path) -> Result<Option<PathBuf>, ProjectError> {
    for dir in start.ancestors() {
        let path = dir.join(FILE);
        crate::faults::at("project::stat", &path).map_err(crate::failure::io("checking", &path))?;
        match std::fs::metadata(&path) {
            Ok(meta) if meta.is_file() => return Ok(Some(dir.to_path_buf())),
            Ok(_) => return Err(ProjectError::NotFile(path)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(source) => return Err(crate::failure::io("checking", &path)(source).into()),
        }
    }
    Ok(None)
}

impl crate::ingress::Ingress for File {}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"
[jobs.test]
on = "@all"
run = ["cargo", "test"]

[jobs.bench]
on = "gpu"
run = ["just bench"]
workspace = "fresh"
"#;

    #[test]
    fn projects_declare_jobs_and_nothing_else() {
        let project = Project::parse(SAMPLE, "sample").unwrap();
        let test: JobName = "test".parse().unwrap();
        assert_eq!(project.job(&test).unwrap().run, ["cargo", "test"]);
        let with_trigger = "[triggers.x]\nevent = \"push\"\n";
        assert!(matches!(
            Project::parse(with_trigger, "x"),
            Err(ProjectError::Parse { .. })
        ));
        let with_notify = "notify = [\"ntfy:https://example.com\"]\n";
        assert!(matches!(
            Project::parse(with_notify, "x"),
            Err(ProjectError::Parse { .. })
        ));
        let escaping = "[jobs.test]\non = 'local'\nrun = ['pwd']\ndir = '../outside'\n";
        assert!(matches!(
            Project::parse(escaping, "x"),
            Err(ProjectError::Parse { .. })
        ));
        let invalid_env =
            "[jobs.test]\non = 'local'\nrun = ['pwd']\n[jobs.test.env]\nDOMYJOB_SECRET = 'x'\n";
        assert!(matches!(
            Project::parse(invalid_env, "x"),
            Err(ProjectError::Parse { .. })
        ));
    }

    #[test]
    fn repository_requests_need_a_local_root_and_machine_grant() {
        let root = tempfile::tempdir().unwrap();
        let other = tempfile::tempdir().unwrap();
        let project = Project::parse(SAMPLE, "sample").unwrap();
        let test: JobName = "test".parse().unwrap();
        let bench: JobName = "bench".parse().unwrap();
        let no_policy = Config::builtin().unwrap();
        assert!(matches!(
            project
                .job(&test)
                .unwrap()
                .approve(&no_policy, root.path(), None),
            Err(ProjectError::Config(ConfigError::ProjectNotApproved(_)))
        ));

        let local = format!(
            "[machines.win]\n[[project_jobs]]\nroot = '{}'\nmachines = ['local', 'win']\n",
            root.path().display()
        );
        let config = Config::layered(&local, "local config").unwrap();
        assert!(matches!(
            project
                .job(&test)
                .unwrap()
                .approve(&config, other.path(), None),
            Err(ProjectError::Config(ConfigError::ProjectNotApproved(_)))
        ));
        assert!(matches!(
            project
                .job(&bench)
                .unwrap()
                .approve(&config, root.path(), None),
            Err(ProjectError::Config(
                ConfigError::ProjectMachineDenied { .. }
            ))
        ));

        let all = project
            .job(&test)
            .unwrap()
            .approve(&config, root.path(), None)
            .unwrap()
            .into_parts();
        assert_eq!(all.root, std::fs::canonicalize(root.path()).unwrap());
        let names: Vec<_> = all
            .targets
            .into_machines()
            .into_iter()
            .map(|machine| machine.name)
            .collect();
        assert_eq!(names, ["local".parse().unwrap(), "win".parse().unwrap()]);
        assert_eq!(all.words.first().unwrap().as_str(), "cargo");

        let win: ProjectSelector = "win".parse().unwrap();
        let approved = project
            .job(&test)
            .unwrap()
            .approve(&config, root.path(), Some(&win))
            .unwrap()
            .into_parts();
        assert_eq!(
            approved.targets.into_machines().first().unwrap().name,
            "win".parse().unwrap()
        );
        let gpu: ProjectSelector = "gpu".parse().unwrap();
        assert!(matches!(
            project
                .job(&test)
                .unwrap()
                .approve(&config, root.path(), Some(&gpu)),
            Err(ProjectError::Config(
                ConfigError::ProjectMachineDenied { .. }
            ))
        ));
        "ssh:unlisted".parse::<ProjectSelector>().unwrap_err();
        "@unknown".parse::<ProjectSelector>().unwrap_err();
        "win,,local".parse::<ProjectSelector>().unwrap_err();
        let too_many = std::iter::repeat_n("win", MAX_SELECTOR_TERMS + 1)
            .collect::<Vec<_>>()
            .join(",");
        assert!(matches!(
            too_many.parse::<ProjectSelector>(),
            Err(ConfigError::SelectorTooComplex { .. })
        ));
    }

    #[test]
    fn a_project_marker_that_cannot_be_checked_does_not_select_an_ancestor() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("root");
        crate::state_file::write_bytes(&root.join(FILE), SAMPLE.as_bytes()).unwrap();
        let child = root.join("child");
        crate::state_file::private_dir(&child).unwrap();
        assert_eq!(find_root(&child).unwrap(), Some(root));
        let marker = child.join(FILE);
        let tag = marker.display().to_string();
        {
            let _faults = crate::faults::inject(&[("project::stat", &tag)]);
            assert!(matches!(find_root(&child), Err(ProjectError::Io(_))));
        }
        crate::state_file::private_dir(&marker).unwrap();
        assert!(matches!(find_root(&child), Err(ProjectError::NotFile(_))));
    }

    #[test]
    fn oversized_project_config_is_rejected_before_parsing() {
        let huge = " ".repeat(4_194_305);
        assert!(matches!(
            Project::parse(&huge, "oversized"),
            Err(ProjectError::TooLarge { .. })
        ));
        let dir = tempfile::tempdir().unwrap();
        crate::user_files::write(&dir.path().join(FILE), huge.as_bytes()).unwrap();
        assert!(matches!(
            Project::load(dir.path()),
            Err(ProjectError::Io(_))
        ));
    }
}
