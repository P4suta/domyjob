use alloc::string::String;
use alloc::vec::Vec;
use core::fmt;

use serde::{Deserialize, Serialize};

use super::id::{AgentName, Invalid, Line, Paragraph};

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

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    Managed,
    Interactive,
}

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

validated_string!(Tag, Invalid, |text| if valid_tag(text) {
    Ok(())
} else {
    Err(Invalid("skill tag"))
});

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

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Relevance {
    Skill,
    Name,
    Role,
    Text,
}

impl Card {
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Os {
    Macos,
    Linux,
    Windows,
    Other,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MachineCard {
    pub label: Line<64>,
    pub os: Os,
}

#[cfg(test)]
mod tests {
    use alloc::borrow::ToOwned;
    use alloc::format;
    use alloc::string::String;
    use alloc::vec;
    use alloc::vec::Vec;

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
    fn tags_and_skills_hold_their_exact_limits() {
        Tag::try_from("a".repeat(32)).unwrap();
        Tag::try_from("a".repeat(33)).unwrap_err();
        Tag::try_from(String::new()).unwrap_err();
        Tag::try_from("-rust".to_owned()).unwrap_err();
        Tag::try_from("Rust".to_owned()).unwrap_err();
        Tag::try_from("c++".to_owned()).unwrap();
        let names: Vec<String> = (0..17).map(|n| format!("t{n:02}")).collect();
        let all: Vec<&str> = names.iter().map(String::as_str).collect();
        let sixteen: Vec<&str> = all.iter().take(16).copied().collect();
        let skills = Skills::collect(tags(&sixteen)).unwrap();
        assert_eq!(Vec::<Tag>::from(skills), tags(&sixteen));
        Skills::collect(tags(&all)).unwrap_err();
        Skills::try_from(tags(&["b", "a"])).unwrap_err();
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
        assert_eq!(reviewer.relevance(&name, "  "), Some(Relevance::Text));
        let guard = card("Alice", "security guard", &["rust"]);
        assert_eq!(guard.relevance(&name, "guard"), Some(Relevance::Role));
        assert_eq!(
            vec![Relevance::Text, Relevance::Skill].into_iter().min(),
            Some(Relevance::Skill)
        );
    }
}
