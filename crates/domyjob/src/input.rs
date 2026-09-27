#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserText(String);

impl UserText {
    #[must_use]
    pub(crate) fn from_cli(text: crate::cli::CliText) -> Self {
        Self(text.into_string())
    }

    #[must_use]
    pub fn as_user_str(&self) -> &str {
        &self.0
    }
}

impl UserText {
    #[must_use]
    pub(crate) fn from_agent(text: crate::mcp::AgentText) -> Self {
        Self(text.into_string())
    }

    #[must_use]
    pub(crate) fn from_project_job_the_user_invoked(text: crate::cli::ProjectJobText) -> Self {
        Self(text.into_string())
    }

    #[cfg(test)]
    #[must_use]
    pub(crate) const fn for_test(text: String) -> Self {
        Self(text)
    }

    #[must_use]
    pub fn split_once(&self, separator: char) -> (Self, Option<Self>) {
        match self.0.split_once(separator) {
            Some((head, tail)) => (Self(head.to_owned()), Some(Self(tail.to_owned()))),
            None => (self.clone(), None),
        }
    }

    #[must_use]
    pub fn joined(words: &[Self]) -> Self {
        Self(
            words
                .iter()
                .map(|w| w.0.as_str())
                .collect::<Vec<_>>()
                .join(" "),
        )
    }
}
