use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::domain::JobName;
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
    #[error("no job named {0} in {FILE}")]
    NoSuchJob(JobName),
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct File {
    jobs: Option<BTreeMap<JobName, JobDef>>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JobDef {
    pub on: String,
    pub run: Vec<String>,
    pub runner: Option<String>,
    pub workspace: Option<Workspace>,
    pub dir: Option<String>,
    pub env: Option<BTreeMap<String, String>>,
}

#[derive(Debug, Clone, Default)]
pub struct Project {
    pub jobs: BTreeMap<JobName, JobDef>,
}

#[must_use]
pub fn glob(pattern: &str, text: &str) -> bool {
    match pattern.split_once('*') {
        None => pattern == text,
        Some((head, tail)) => text.strip_prefix(head).is_some_and(|rest| {
            (0..=rest.len()).any(|cut| rest.get(cut..).is_some_and(|end| glob(tail, end)))
        }),
    }
}

impl Project {
    pub fn parse(text: &str, origin: &str) -> Result<Self, ProjectError> {
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
        match std::fs::read_to_string(&path) {
            Ok(text) => Self::parse(&text, &path.display().to_string()).map(Some),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(source) => Err(crate::failure::io("reading", &path)(source).into()),
        }
    }

    pub fn job(&self, name: &JobName) -> Result<&JobDef, ProjectError> {
        self.jobs
            .get(name)
            .ok_or_else(|| ProjectError::NoSuchJob(name.clone()))
    }
}

pub fn find_root(start: &Path) -> Option<PathBuf> {
    start
        .ancestors()
        .find(|dir| std::fs::metadata(dir.join(FILE)).is_ok_and(|m| m.is_file()))
        .map(Path::to_path_buf)
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
    }

    #[test]
    fn globs() {
        assert!(glob("refs/heads/*", "refs/heads/main"));
        assert!(glob("*", ""));
        assert!(glob("a*b*c", "aXbYc"));
        assert!(!glob("a*b", "ac"));
    }
}
