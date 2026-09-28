use alloc::string::String;
use alloc::vec::Vec;
use core::fmt;

use serde::{Deserialize, Serialize};

use super::id::{AgentName, Invalid, Line, Paragraph};

/// The AI command-line client that runs an agent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Tool {
    Claude,
    Codex,
    Opencode,
}

impl Tool {
    pub const ALL: [Self; 3] = [Self::Claude, Self::Codex, Self::Opencode];

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
            Self::Opencode => "opencode",
        }
    }
}

/// Whether domyjob runs the agent's turns or an interactive session answers by itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    Managed,
    Interactive,
}

/// What an agent may change while it works.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Access {
    Read,
    Write,
}

fn valid_tag(text: &str) -> bool {
    let plain = |byte: u8| byte.is_ascii_lowercase() || byte.is_ascii_digit();
    (1..=32).contains(&text.len())
        && text.bytes().next().is_some_and(plain)
        && text
            .bytes()
            .all(|byte| plain(byte) || matches!(byte, b'+' | b'#' | b'.' | b'-'))
}

validated_string!(
    /// A skill tag: 1 to 32 lowercase letters, digits, `+`, `#`, `.`, or `-`, starting with a letter or digit.
    Tag,
    Invalid,
    |text| if valid_tag(text) {
        Ok(())
    } else {
        Err(Invalid("skill tag"))
    }
);

/// At most 16 sorted, unique skill tags.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "Vec<Tag>", into = "Vec<Tag>")]
pub struct Skills(Vec<Tag>);

impl TryFrom<Vec<Tag>> for Skills {
    type Error = Invalid;

    fn try_from(tags: Vec<Tag>) -> Result<Self, Self::Error> {
        if tags.len() > 16
            || tags
                .windows(2)
                .any(|pair| matches!(pair, [first, second] if first >= second))
        {
            return Err(Invalid("skills"));
        }
        Ok(Self(tags))
    }
}

impl From<Skills> for Vec<Tag> {
    fn from(value: Skills) -> Self {
        value.0
    }
}

impl Skills {
    /// Sort and deduplicate tags before validating their count.
    pub fn collect(tags: impl IntoIterator<Item = Tag>) -> Result<Self, Invalid> {
        let mut tags: Vec<Tag> = tags.into_iter().collect();
        tags.sort();
        tags.dedup();
        Self::try_from(tags)
    }

    #[must_use]
    pub fn tags(&self) -> &[Tag] {
        &self.0
    }

    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

/// An agent's public profile, shown in the directory of every machine that knows it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Card {
    pub display_name: Line<64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<Line<64>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<Paragraph<1024>>,
    #[serde(default, skip_serializing_if = "Skills::is_empty")]
    pub skills: Skills,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<Line<128>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<Line<128>>,
    pub tool: Tool,
    pub mode: Mode,
    pub access: Access,
}

/// How well a card matches a directory query; smaller ranks first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Relevance {
    Skill,
    Name,
    Role,
    Text,
}

impl Card {
    /// Rank a case-insensitive query against the card, or `None` when nothing matches.
    #[must_use]
    pub fn relevance(&self, name: &AgentName, query: &str) -> Option<Relevance> {
        let query = query.trim().to_lowercase();
        if query.is_empty() {
            return Some(Relevance::Text);
        }
        let contains = |text: &str| text.to_lowercase().contains(&query);
        if self.skills.tags().iter().any(|tag| tag.as_str() == query) {
            Some(Relevance::Skill)
        } else if contains(name.as_str()) || contains(self.display_name.as_str()) {
            Some(Relevance::Name)
        } else if self
            .role
            .as_ref()
            .is_some_and(|role| contains(role.as_str()))
        {
            Some(Relevance::Role)
        } else if self
            .description
            .as_ref()
            .map(Paragraph::as_str)
            .into_iter()
            .chain(self.project.as_ref().map(Line::as_str))
            .chain(self.status.as_ref().map(Line::as_str))
            .chain(self.skills.tags().iter().map(Tag::as_str))
            .any(contains)
        {
            Some(Relevance::Text)
        } else {
            None
        }
    }
}

/// The operating system family a machine reports about itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Os {
    Macos,
    Linux,
    Windows,
    Other,
}

/// How a machine names itself, so peers with different SSH aliases display it consistently.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MachineCard {
    pub label: Line<64>,
    pub os: Os,
}

#[cfg(test)]
mod tests {
    use alloc::borrow::ToOwned;
    use alloc::vec;

    use super::{Relevance, Skills, Tag};
    use crate::chat::fixtures::{card, tags};
    use crate::chat::id::AgentName;

    #[test]
    fn skills_are_normalized_and_bounded() {
        assert_eq!(
            Skills::collect(tags(&["rust", "c++", "rust"]))
                .unwrap()
                .tags(),
            tags(&["c++", "rust"])
        );
        Skills::try_from(tags(&["rust", "c++"])).unwrap_err();
        Tag::try_from("Rust".to_owned()).unwrap_err();
        Skills::collect((0..17).map(|index| Tag::try_from(alloc::format!("t{index}")).unwrap()))
            .unwrap_err();
    }

    #[test]
    fn directory_relevance_prefers_exact_skills_then_names_then_roles() {
        let reviewer = card("Code Reviewer", "reviewer", &["rust", "security"]);
        let name = AgentName::try_from("alice".to_owned()).unwrap();
        assert_eq!(reviewer.relevance(&name, "Rust"), Some(Relevance::Skill));
        assert_eq!(reviewer.relevance(&name, "ALICE"), Some(Relevance::Name));
        assert_eq!(reviewer.relevance(&name, "code"), Some(Relevance::Name));
        assert_eq!(reviewer.relevance(&name, "review"), Some(Relevance::Name));
        assert_eq!(reviewer.relevance(&name, "changes"), Some(Relevance::Text));
        assert_eq!(reviewer.relevance(&name, "curity"), Some(Relevance::Text));
        assert_eq!(reviewer.relevance(&name, "kubernetes"), None);
        assert_eq!(
            vec![Relevance::Text, Relevance::Skill].into_iter().min(),
            Some(Relevance::Skill)
        );
    }
}
