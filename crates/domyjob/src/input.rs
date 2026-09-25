#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserText(String);

impl UserText {
    #[must_use]
    pub const fn from_cli(text: String) -> Self {
        Self(text)
    }

    #[must_use]
    pub fn as_user_str(&self) -> &str {
        &self.0
    }
}

impl UserText {
    #[must_use]
    pub const fn from_agent(text: String) -> Self {
        Self(text)
    }

    #[must_use]
    pub const fn from_project_job_the_user_invoked(text: String) -> Self {
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
