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
    #[error("{origin} exceeds its {limit}-byte project config budget")]
    TooLarge { origin: String, limit: u64 },
    #[error("no job named {0} in {FILE}")]
    NoSuchJob(JobName),
    #[error("{0} exists but is not a file")]
    NotFile(PathBuf),
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

    pub fn job(&self, name: &JobName) -> Result<&JobDef, ProjectError> {
        self.jobs
            .get(name)
            .ok_or_else(|| ProjectError::NoSuchJob(name.clone()))
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
