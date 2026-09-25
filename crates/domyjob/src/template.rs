use std::collections::BTreeMap;
use std::fmt;

use serde::{Deserialize, Serialize};

use crate::shell;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TemplateError {
    #[error("{text:?}: unclosed placeholder")]
    Unclosed { text: String },
    #[error("{text:?}: {{{name}}} is not available here; available: {available}")]
    Unknown {
        text: String,
        name: String,
        available: String,
    },
    #[error("{text:?}: unknown filter |{filter}")]
    Filter { text: String, filter: String },
    #[error("{text:?}: a list placeholder {{{name}...}} must be a whole argument")]
    Splice { text: String, name: String },
    #[error("{{{name}}} was not bound")]
    Unbound { name: String },
    #[error("{{{name}}} holds a list; use {{{name}...}} as a whole argument")]
    ListInText { name: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Filter {
    Raw,
    Posix,
    PowerShell,
    Json,
    AppleScript,
    Fileset,
}

impl Filter {
    fn parse(name: &str) -> Option<Self> {
        match name {
            "posix" => Some(Self::Posix),
            "powershell" => Some(Self::PowerShell),
            "json" => Some(Self::Json),
            "applescript" => Some(Self::AppleScript),
            "fileset" => Some(Self::Fileset),
            _ => None,
        }
    }

    fn apply(self, value: &str) -> String {
        match self {
            Self::Raw => value.to_owned(),
            Self::Posix => shell::posix_quote(value),
            Self::PowerShell => shell::powershell_quote(value),
            Self::Json => serde_json::Value::String(value.to_owned()).to_string(),
            Self::AppleScript | Self::Fileset => {
                format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Piece {
    Text(String),
    Var { name: String, filter: Filter },
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Word {
    Pieces(Vec<Piece>),
    Splice(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "Vec<String>", into = "Vec<String>")]
pub struct Argv {
    source: Vec<String>,
    words: Vec<Word>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Text {
    source: String,
    pieces: Vec<Piece>,
}

fn parse_pieces(text: &str) -> Result<Vec<Piece>, TemplateError> {
    let mut pieces = Vec::new();
    let mut literal = String::new();
    let mut rest = text;
    while let Some(open) = rest.find(['{', '}']) {
        let (before, tail) = rest.split_at(open);
        literal.push_str(before);
        if let Some(after) = tail.strip_prefix("{{") {
            literal.push('{');
            rest = after;
            continue;
        }
        if let Some(after) = tail.strip_prefix("}}") {
            literal.push('}');
            rest = after;
            continue;
        }
        let Some(inner_and_tail) = tail.strip_prefix('{') else {
            literal.push('}');
            rest = tail.get(1..).unwrap_or("");
            continue;
        };
        let Some(close) = inner_and_tail.find('}') else {
            return Err(TemplateError::Unclosed {
                text: text.to_owned(),
            });
        };
        let (inner, after) = inner_and_tail.split_at(close);
        if !literal.is_empty() {
            pieces.push(Piece::Text(std::mem::take(&mut literal)));
        }
        let (name, filter) = match inner.split_once('|') {
            Some((name, filter_name)) => match Filter::parse(filter_name) {
                Some(filter) => (name, filter),
                None => {
                    return Err(TemplateError::Filter {
                        text: text.to_owned(),
                        filter: filter_name.to_owned(),
                    });
                }
            },
            None => (inner, Filter::Raw),
        };
        pieces.push(Piece::Var {
            name: name.to_owned(),
            filter,
        });
        rest = after.get(1..).unwrap_or("");
    }
    literal.push_str(rest);
    if !literal.is_empty() {
        pieces.push(Piece::Text(literal));
    }
    Ok(pieces)
}

fn parse_word(text: &str) -> Result<Word, TemplateError> {
    let pieces = parse_pieces(text)?;
    for piece in &pieces {
        if let Piece::Var { name, .. } = piece
            && let Some(list) = name.strip_suffix("...")
        {
            return match pieces.as_slice() {
                [
                    Piece::Var {
                        filter: Filter::Raw,
                        ..
                    },
                ] => Ok(Word::Splice(list.to_owned())),
                _ => Err(TemplateError::Splice {
                    text: text.to_owned(),
                    name: list.to_owned(),
                }),
            };
        }
    }
    Ok(Word::Pieces(pieces))
}

impl TryFrom<Vec<String>> for Argv {
    type Error = TemplateError;

    fn try_from(source: Vec<String>) -> Result<Self, TemplateError> {
        let words = source
            .iter()
            .map(|word| parse_word(word))
            .collect::<Result<_, _>>()?;
        Ok(Self { source, words })
    }
}

impl From<Argv> for Vec<String> {
    fn from(value: Argv) -> Self {
        value.source
    }
}

impl TryFrom<String> for Text {
    type Error = TemplateError;

    fn try_from(source: String) -> Result<Self, TemplateError> {
        let pieces = parse_pieces(&source)?;
        Ok(Self { source, pieces })
    }
}

impl From<Text> for String {
    fn from(value: Text) -> Self {
        value.source
    }
}

impl fmt::Display for Argv {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}", self.source)
    }
}

pub trait SafeWord {
    fn safe_word(&self) -> &str;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Arg(String);

impl Arg {
    #[must_use]
    pub fn literal(text: &'static str) -> Self {
        Self(text.to_owned())
    }

    #[must_use]
    pub fn joined(parts: &[&'static str]) -> Self {
        Self(parts.concat())
    }

    #[must_use]
    pub fn path(path: &std::path::Path) -> Self {
        Self(path.display().to_string())
    }

    #[must_use]
    pub fn word(word: &impl SafeWord) -> Self {
        Self(word.safe_word().to_owned())
    }

    #[must_use]
    pub fn number(value: u64) -> Self {
        Self(value.to_string())
    }

    #[must_use]
    pub fn config(text: &crate::config::ConfigText) -> Self {
        Self(text.as_config_str().to_owned())
    }

    #[must_use]
    pub fn user(text: &crate::input::UserText) -> Self {
        Self(text.as_user_str().to_owned())
    }

    #[must_use]
    pub fn rendered(text: Rendered) -> Self {
        Self(text.0)
    }

    #[must_use]
    pub fn powershell_encoded(script: &Self) -> Self {
        let utf16: Vec<u8> = script.0.encode_utf16().flat_map(u16::to_le_bytes).collect();
        Self(crate::remote::base64(&utf16))
    }

    #[must_use]
    pub fn version(version: &crate::dist::Version) -> Self {
        Self(version.to_string())
    }

    #[must_use]
    pub fn posix_command(words: &[Self]) -> Self {
        Self(
            words
                .iter()
                .map(|w| shell::posix_quote(&w.0))
                .collect::<Vec<_>>()
                .join(" "),
        )
    }

    #[must_use]
    pub fn spaced(words: &[Self]) -> Self {
        Self(
            words
                .iter()
                .map(|w| w.0.as_str())
                .collect::<Vec<_>>()
                .join(" "),
        )
    }

    #[must_use]
    pub fn cmd_wrapped(inner: &Self) -> Self {
        Self(format!("cmd /c \"{}\"", inner.0))
    }

    #[must_use]
    pub fn as_arg_str(&self) -> &str {
        &self.0
    }

    #[must_use]
    pub fn into_string(self) -> String {
        self.0
    }

    #[must_use]
    pub fn concat(parts: &[Self]) -> Self {
        Self(parts.iter().map(|p| p.0.as_str()).collect())
    }

    #[must_use]
    pub const fn authorized_job_text(text: String) -> Self {
        Self(text)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rendered(String);

impl Rendered {
    #[must_use]
    pub fn into_string(self) -> String {
        self.0
    }

    #[must_use]
    pub fn suffixed(&self, suffix: &'static str) -> Self {
        Self(format!("{}{suffix}", self.0))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Value {
    One(Arg),
    Many(Vec<Arg>),
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Bindings {
    values: BTreeMap<&'static str, Value>,
}

impl Bindings {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn with(mut self, name: &'static str, value: Arg) -> Self {
        self.values.insert(name, Value::One(value));
        self
    }

    #[must_use]
    pub fn with_list(mut self, name: &'static str, value: Vec<Arg>) -> Self {
        self.values.insert(name, Value::Many(value));
        self
    }

    fn text(&self, name: &str) -> Result<&str, TemplateError> {
        match self.values.get(name) {
            Some(Value::One(arg)) => Ok(&arg.0),
            Some(Value::Many(_)) => Err(TemplateError::ListInText {
                name: name.to_owned(),
            }),
            None => Err(TemplateError::Unbound {
                name: name.to_owned(),
            }),
        }
    }

    fn list(&self, name: &str) -> Result<Vec<String>, TemplateError> {
        match self.values.get(name) {
            Some(Value::Many(list)) => Ok(list.iter().map(|arg| arg.0.clone()).collect()),
            Some(Value::One(arg)) => Ok(vec![arg.0.clone()]),
            None => Err(TemplateError::Unbound {
                name: name.to_owned(),
            }),
        }
    }
}

fn render_pieces(pieces: &[Piece], bindings: &Bindings) -> Result<String, TemplateError> {
    let mut out = String::new();
    for piece in pieces {
        match piece {
            Piece::Text(text) => out.push_str(text),
            Piece::Var { name, filter } => out.push_str(&filter.apply(bindings.text(name)?)),
        }
    }
    Ok(out)
}

fn check_names<'a>(
    text: &str,
    names: impl Iterator<Item = &'a str>,
    allowed: &[&str],
) -> Result<(), TemplateError> {
    for name in names {
        if !allowed.contains(&name) {
            return Err(TemplateError::Unknown {
                text: text.to_owned(),
                name: name.to_owned(),
                available: allowed.join(", "),
            });
        }
    }
    Ok(())
}

fn piece_names(pieces: &[Piece]) -> impl Iterator<Item = &str> {
    pieces.iter().filter_map(|piece| match piece {
        Piece::Var { name, .. } => Some(name.as_str()),
        Piece::Text(_) => None,
    })
}

impl Argv {
    pub fn check(&self, allowed: &[&str]) -> Result<(), TemplateError> {
        for (source, word) in self.source.iter().zip(&self.words) {
            match word {
                Word::Pieces(pieces) => check_names(source, piece_names(pieces), allowed)?,
                Word::Splice(name) => check_names(source, std::iter::once(name.as_str()), allowed)?,
            }
        }
        Ok(())
    }

    pub fn render(&self, bindings: &Bindings) -> Result<Vec<Arg>, TemplateError> {
        let mut out = Vec::new();
        for word in &self.words {
            match word {
                Word::Pieces(pieces) => out.push(Arg(render_pieces(pieces, bindings)?)),
                Word::Splice(name) => out.extend(bindings.list(name)?.into_iter().map(Arg)),
            }
        }
        Ok(out)
    }

    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.words.is_empty()
    }
}

impl Text {
    pub fn check(&self, allowed: &[&str]) -> Result<(), TemplateError> {
        check_names(&self.source, piece_names(&self.pieces), allowed)
    }

    pub fn render(&self, bindings: &Bindings) -> Result<Rendered, TemplateError> {
        render_pieces(&self.pieces, bindings).map(Rendered)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(words: &[&str]) -> Argv {
        Argv::try_from(words.iter().map(|w| (*w).to_owned()).collect::<Vec<_>>()).unwrap()
    }

    #[test]
    fn renders_text_filters_and_splices() {
        let template = argv(&[
            "ssh",
            "-T",
            "{host}",
            "{remote|posix}",
            "{{literal}}",
            "{extra...}",
        ]);
        let bindings = Bindings::new()
            .with("host", Arg::literal("me@box"))
            .with("remote", Arg::literal("it's"))
            .with_list("extra", vec![Arg::literal("a"), Arg::literal("b c")]);
        assert_eq!(
            template
                .render(&bindings)
                .unwrap()
                .iter()
                .map(Arg::as_arg_str)
                .collect::<Vec<_>>(),
            ["ssh", "-T", "me@box", r#""it's""#, "{literal}", "a", "b c"]
        );
        template.check(&["host", "remote", "extra"]).unwrap();
        assert!(matches!(
            template.check(&["host"]),
            Err(TemplateError::Unknown { .. })
        ));
    }

    #[test]
    fn refuses_malformed_templates() {
        assert!(matches!(
            Text::try_from("{open".to_owned()),
            Err(TemplateError::Unclosed { .. })
        ));
        assert!(matches!(
            Text::try_from("{x|shout}".to_owned()),
            Err(TemplateError::Filter { .. })
        ));
        assert!(matches!(
            Argv::try_from(vec!["pre{all...}".to_owned()]),
            Err(TemplateError::Splice { .. })
        ));
        let text = Text::try_from("{message|applescript}".to_owned()).unwrap();
        assert_eq!(
            text.render(&Bindings::new().with("message", Arg::literal("say \"hi\"")))
                .unwrap()
                .as_str(),
            r#""say \"hi\"""#
        );
        assert!(matches!(
            text.render(&Bindings::new()),
            Err(TemplateError::Unbound { .. })
        ));
    }
}
