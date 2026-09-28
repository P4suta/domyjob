//! Comment-preserving edits of JSON with comments, the format of `opencode.json` and `opencode.jsonc`.
//!
//! An edit rewrites only the text of the member it changes, so comments, formatting, and other members stay as they were.
//! Each document is validated before it is edited, and the scanner relies on that validation for the document's structure.

use alloc::format;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use core::ops::Range;

use serde::de::IgnoredAny;

use crate::ingress;

/// The whitespace that JSON allows between tokens.
const SPACE: [char; 4] = [' ', '\t', '\n', '\r'];

/// The layout that keeps a new member on the line of its neighbors.
const INLINE: Layout<'static> = Layout {
    newline: "",
    unit: "",
};

#[derive(Debug, thiserror::Error)]
pub enum JsoncError {
    /// The document is neither empty nor valid JSON with comments.
    #[error("the document is not valid JSON with comments: {0}")]
    Document(#[source] serde_json::Error),
    /// The value to set is not valid JSON.
    #[error("the new value is not valid JSON: {0}")]
    Value(#[source] serde_json::Error),
    /// The path names no member.
    #[error("a member path needs at least one key")]
    EmptyPath,
    /// A value on the path is not an object, so setting the member would have to replace it.
    #[error("{0} is not a JSON object")]
    NotObject(String),
    /// A key on the path appears twice in its object, so the member it names is ambiguous.
    #[error("{0} appears more than once")]
    DuplicateKey(String),
}

/// Replaces comments and trailing commas outside strings with spaces, so that valid JSON with comments becomes plain JSON.
///
/// Line breaks stay in place, so parse errors in the result point at the lines of the original.
/// An unterminated comment or string stays as it is, so the result fails to parse like the original.
#[must_use]
pub fn strip(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut blanks = Vec::new();
    let mut previous = b'[';
    let mut at = 0;
    while let Some(&byte) = bytes.get(at) {
        if let Some(end) = comment_end(bytes, at) {
            blanks.push(at..end);
            at = end;
        } else {
            if byte == b','
                && !matches!(previous, b'[' | b'{' | b',' | b':')
                && matches!(
                    bytes.get(skip_trivia(bytes, at.saturating_add(1))),
                    Some(b']' | b'}')
                )
            {
                blanks.push(at..at.saturating_add(1));
            }
            if !matches!(byte, b' ' | b'\t' | b'\n' | b'\r') {
                previous = byte;
            }
            at = if byte == b'"' {
                string_end(bytes, at)
            } else {
                at.saturating_add(1)
            };
        }
    }
    let mut pending = blanks.into_iter().peekable();
    text.char_indices()
        .map(|(offset, character)| {
            while pending.next_if(|range| range.end <= offset).is_some() {}
            let blank = pending.peek().is_some_and(|range| range.contains(&offset));
            if blank && !matches!(character, '\n' | '\r') {
                ' '
            } else {
                character
            }
        })
        .collect()
}

/// Sets the member at `path` to `value_json`, creating the objects on the way that are missing.
///
/// An existing value is replaced in place.
/// A new member follows the last member of its object, in the indentation and line endings of the document.
/// An empty document, or one with only comments, gains a root object.
/// Setting the same text again returns the document unchanged.
pub fn set_member(document: &str, path: &[&str], value_json: &str) -> Result<String, JsoncError> {
    ingress::foreign_json::<IgnoredAny>(value_json).map_err(JsoncError::Value)?;
    let value = value_json.trim_matches(SPACE);
    match walk(document, path)? {
        Target::Found { member, .. } => Ok(splice(document, &[(member.value..member.end, value)])),
        Target::Missing(missing) => Ok(insert(document, &missing, value)),
        Target::Empty => {
            let comments = document.trim_end_matches(SPACE);
            let line = newline(document);
            let root = if comments.is_empty() {
                format!("{{}}{line}")
            } else {
                format!("{comments}{line}{{}}{line}")
            };
            set_member(&root, path, value_json)
        }
    }
}

/// Removes the member at `path` with its comma, and with its lines when it has them to itself.
///
/// A document without that member, including one where a value on the path is not an object, comes back unchanged.
pub fn remove_member(document: &str, path: &[&str]) -> Result<String, JsoncError> {
    match walk(document, path) {
        Ok(Target::Found { previous, member }) => Ok(removal(document, previous, member)),
        Ok(Target::Missing(_) | Target::Empty) | Err(JsoncError::NotObject(_)) => {
            Ok(String::from(document))
        }
        Err(error) => Err(error),
    }
}

/// Where a member sits in the text of its object.
#[derive(Clone, Copy)]
struct Member {
    /// The opening quote of the key.
    start: usize,
    /// The first byte of the value.
    value: usize,
    /// The byte after the value.
    end: usize,
    /// The comma after the value.
    comma: Option<usize>,
    /// Whether the key is the one being looked up.
    wanted: bool,
}

/// An object whose braces are at `open` and `close`.
struct Object {
    open: usize,
    close: usize,
    members: Vec<Member>,
}

/// An object without the member `key`, whose new value nests the set value under the keys in `rest`.
struct Missing<'p> {
    object: Object,
    key: &'p str,
    rest: &'p [&'p str],
}

enum Target<'p> {
    /// The document has no value yet.
    Empty,
    Missing(Missing<'p>),
    /// The member exists, after `previous` unless it is the first.
    Found {
        previous: Option<Member>,
        member: Member,
    },
}

/// How new lines are broken and indented.
#[derive(Clone, Copy)]
struct Layout<'t> {
    newline: &'t str,
    unit: &'t str,
}

/// Validates `text` and follows `path` through its objects.
fn walk<'p>(text: &str, path: &'p [&'p str]) -> Result<Target<'p>, JsoncError> {
    let plain = strip(text);
    if plain.trim_matches(SPACE).is_empty() {
        return if path.is_empty() {
            Err(JsoncError::EmptyPath)
        } else {
            Ok(Target::Empty)
        };
    }
    ingress::foreign_json::<IgnoredAny>(&plain).map_err(JsoncError::Document)?;
    let mut open = skip_trivia(text.as_bytes(), 0);
    for (depth, &key) in path.iter().enumerate() {
        let object = parse_object(text, open, key)?
            .ok_or_else(|| JsoncError::NotObject(describe(path, depth)))?;
        let mut matching = object
            .members
            .iter()
            .enumerate()
            .filter(|(_, member)| member.wanted);
        let Some((index, &member)) = matching.next() else {
            let rest = path.get(depth.saturating_add(1)..).unwrap_or_default();
            return Ok(Target::Missing(Missing { object, key, rest }));
        };
        if matching.next().is_some() {
            return Err(JsoncError::DuplicateKey(describe(
                path,
                depth.saturating_add(1),
            )));
        }
        if depth.saturating_add(1) == path.len() {
            let previous = index
                .checked_sub(1)
                .and_then(|before| object.members.get(before))
                .copied();
            return Ok(Target::Found { previous, member });
        }
        open = member.value;
    }
    Err(JsoncError::EmptyPath)
}

/// Names the value at the first `count` keys of `path` for error messages.
fn describe(path: &[&str], count: usize) -> String {
    match path.get(..count) {
        Some(keys @ [_, ..]) => format!("`{}`", keys.join(".")),
        Some([]) | None => String::from("the document root"),
    }
}

/// Reads the members of the object that opens at `open`, marking those named `key`, or returns `None` when no object opens there.
fn parse_object(text: &str, open: usize, key: &str) -> Result<Option<Object>, JsoncError> {
    let bytes = text.as_bytes();
    if bytes.get(open) != Some(&b'{') {
        return Ok(None);
    }
    let mut members = Vec::new();
    let mut at = skip_trivia(bytes, open.saturating_add(1));
    while bytes.get(at) == Some(&b'"') {
        let key_end = string_end(bytes, at);
        let name: String = ingress::foreign_json(text.get(at..key_end).unwrap_or_default())
            .map_err(JsoncError::Document)?;
        let value = skip_trivia(bytes, skip_trivia(bytes, key_end).saturating_add(1));
        let end = value_end(bytes, value);
        let after = skip_trivia(bytes, end);
        let comma = (bytes.get(after) == Some(&b',')).then_some(after);
        members.push(Member {
            start: at,
            value,
            end,
            comma,
            wanted: name == key,
        });
        at = comma.map_or(after, |position| {
            skip_trivia(bytes, position.saturating_add(1))
        });
    }
    Ok(Some(Object {
        open,
        close: at,
        members,
    }))
}

/// Returns the end of the comment that starts at `at`, or `None` when no terminated comment starts there.
fn comment_end(bytes: &[u8], at: usize) -> Option<usize> {
    let rest = bytes.get(at..)?;
    if rest.starts_with(b"//") {
        let length = rest
            .iter()
            .position(|byte| matches!(byte, b'\n' | b'\r'))
            .unwrap_or(rest.len());
        Some(at.saturating_add(length))
    } else if rest.starts_with(b"/*") {
        let length = rest.get(2..)?.windows(2).position(|pair| pair == b"*/")?;
        Some(at.saturating_add(length).saturating_add(4))
    } else {
        None
    }
}

/// Returns the first position from `at` that is neither whitespace nor a comment.
fn skip_trivia(bytes: &[u8], mut at: usize) -> usize {
    loop {
        if let Some(end) = comment_end(bytes, at) {
            at = end;
        } else if bytes
            .get(at)
            .is_some_and(|byte| matches!(byte, b' ' | b'\t' | b'\n' | b'\r'))
        {
            at = at.saturating_add(1);
        } else {
            return at;
        }
    }
}

/// Returns the first position from `at` that is neither a space nor a tab.
fn blank_end(bytes: &[u8], at: usize) -> usize {
    let blanks = bytes
        .iter()
        .skip(at)
        .take_while(|byte| matches!(byte, b' ' | b'\t'))
        .count();
    at.saturating_add(blanks)
}

/// Returns the position after the string whose opening quote is at `at`.
fn string_end(bytes: &[u8], at: usize) -> usize {
    let mut escaped = false;
    for (offset, &byte) in bytes.iter().enumerate().skip(at.saturating_add(1)) {
        match byte {
            _ if escaped => escaped = false,
            b'\\' => escaped = true,
            b'"' => return offset.saturating_add(1),
            _ => {}
        }
    }
    bytes.len()
}

/// Returns the position after the value that starts at `at`.
fn value_end(bytes: &[u8], at: usize) -> usize {
    match bytes.get(at) {
        Some(b'"') => string_end(bytes, at),
        Some(b'{' | b'[') => container_end(bytes, at),
        Some(_) | None => {
            let literal = bytes
                .iter()
                .skip(at)
                .take_while(|byte| {
                    byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'+' | b'-')
                })
                .count();
            at.saturating_add(literal)
        }
    }
}

/// Returns the position after the object or array that opens at `at`.
fn container_end(bytes: &[u8], at: usize) -> usize {
    let mut depth = 0_usize;
    let mut cursor = at;
    while let Some(&byte) = bytes.get(cursor) {
        let next = cursor.saturating_add(1);
        cursor = match byte {
            b'"' => string_end(bytes, cursor),
            b'/' => comment_end(bytes, cursor).unwrap_or(next),
            b'{' | b'[' => {
                depth = depth.saturating_add(1);
                next
            }
            b'}' | b']' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return next;
                }
                next
            }
            _ => next,
        };
    }
    cursor
}

/// Adds the missing member after the last member of its object, in the layout around it.
fn insert(text: &str, missing: &Missing<'_>, value: &str) -> String {
    let Missing { object, key, rest } = missing;
    let anchor = text
        .get(..object.close)
        .unwrap_or_default()
        .trim_end_matches(SPACE)
        .len();
    let broken = text
        .get(anchor..object.close)
        .unwrap_or_default()
        .contains(['\n', '\r']);
    let last = object.members.last();
    let inline = last.is_some() && !broken;
    let layout = if inline {
        INLINE
    } else {
        Layout {
            newline: newline(text),
            unit: unit(text),
        }
    };
    let indent = if inline {
        ""
    } else {
        line_indent(text, object.open)
    };
    let inner = format!("{indent}{}", layout.unit);
    let trailing = if last.is_some_and(|member| member.comma.is_some()) {
        ","
    } else {
        ""
    };
    let nested = nest(rest, value, &inner, layout);
    let member = format!("{}: {nested}{trailing}", serde_json::Value::from(*key));
    let mut edits = Vec::new();
    if let Some(before) = last
        && before.comma.is_none()
    {
        edits.push((before.end..before.end, String::from(",")));
    }
    edits.push(if inline {
        (anchor..anchor, format!(" {member}"))
    } else if broken {
        (anchor..anchor, format!("{}{inner}{member}", layout.newline))
    } else {
        let line = layout.newline;
        (
            anchor..object.close,
            format!("{line}{inner}{member}{line}{indent}"),
        )
    });
    splice(text, &edits)
}

/// Renders `value` inside one object per key, each object on its own lines unless the layout is inline.
fn nest(keys: &[&str], value: &str, indent: &str, layout: Layout<'_>) -> String {
    match keys.split_first() {
        None => String::from(value),
        Some((key, rest)) => {
            let inner = format!("{indent}{}", layout.unit);
            let nested = nest(rest, value, &inner, layout);
            let line = layout.newline;
            format!(
                "{{{line}{inner}{}: {nested}{line}{indent}}}",
                serde_json::Value::from(*key)
            )
        }
    }
}

/// Deletes `member` with its comma, its lines when it has them to itself, and the comma it would leave trailing after `previous`.
fn removal(text: &str, previous: Option<Member>, member: Member) -> String {
    let own = member
        .comma
        .map_or(member.end, |comma| comma.saturating_add(1));
    let dangling = previous
        .and_then(|before| before.comma)
        .filter(|_| member.comma.is_none());
    let edits = match (whole_lines(text, member.start, own), dangling) {
        (Some(lines), Some(comma)) => vec![(comma..comma.saturating_add(1), ""), (lines, "")],
        (Some(lines), None) => vec![(lines, "")],
        (None, Some(comma)) => vec![(comma..member.end, "")],
        (None, None) => vec![(member.start..blank_end(text.as_bytes(), own), "")],
    };
    splice(text, &edits)
}

/// Returns the lines from `start` to `end` when they hold nothing else but blanks and a trailing line comment.
fn whole_lines(text: &str, start: usize, end: usize) -> Option<Range<usize>> {
    let bytes = text.as_bytes();
    let head = text.get(..start)?;
    let line = head
        .rfind('\n')
        .map_or(0, |position| position.saturating_add(1));
    if !head
        .get(line..)?
        .bytes()
        .all(|byte| matches!(byte, b' ' | b'\t'))
    {
        return None;
    }
    let mut after = blank_end(bytes, end);
    if bytes.get(after..)?.starts_with(b"//") {
        after = comment_end(bytes, after)?;
    }
    after = after.saturating_add(usize::from(bytes.get(after) == Some(&b'\r')));
    (bytes.get(after) == Some(&b'\n')).then(|| line..after.saturating_add(1))
}

/// Returns the spaces and tabs that start the line containing `at`.
fn line_indent(text: &str, at: usize) -> &str {
    let line = text
        .get(..at)
        .and_then(|head| head.rsplit('\n').next())
        .unwrap_or_default();
    line.strip_suffix(line.trim_start_matches([' ', '\t']))
        .unwrap_or_default()
}

/// Returns the indentation of the first indented line that starts with a key, or two spaces.
fn unit(text: &str) -> &str {
    text.lines()
        .find_map(|line| {
            let content = line.trim_start_matches([' ', '\t']);
            let indent = line.strip_suffix(content)?;
            (content.starts_with('"') && !indent.is_empty()).then_some(indent)
        })
        .unwrap_or("  ")
}

/// Returns the line ending of the document.
fn newline(text: &str) -> &'static str {
    if text.contains("\r\n") { "\r\n" } else { "\n" }
}

/// Applies `edits`, which are ordered by position and do not overlap, to `text`.
fn splice(text: &str, edits: &[(Range<usize>, impl AsRef<str>)]) -> String {
    let mut spliced = String::with_capacity(text.len());
    let mut cursor = 0;
    for (range, replacement) in edits {
        spliced.push_str(text.get(cursor..range.start).unwrap_or_default());
        spliced.push_str(replacement.as_ref());
        cursor = range.end;
    }
    spliced.push_str(text.get(cursor..).unwrap_or_default());
    spliced
}

#[cfg(test)]
mod tests {
    use super::{JsoncError, remove_member, set_member, strip};
    use crate::ingress;
    use alloc::format;
    use alloc::string::{String, ToString};
    use serde_json::{Value, json};

    const PATH: [&str; 2] = ["mcp", "domyjob"];
    const SERVER: &str = r#"{"type":"local","command":["/bin/domyjob","mcp"],"enabled":true}"#;

    fn parse(text: &str) -> Value {
        ingress::foreign_json(&strip(text)).unwrap()
    }

    /// Returns `document` with `/mcp/domyjob` set to `server`, or removed without one.
    fn with_server(mut document: Value, server: Option<Value>) -> Value {
        let root = document.as_object_mut().unwrap();
        let mcp = root.entry("mcp").or_insert_with(|| json!({}));
        let servers = mcp.as_object_mut().unwrap();
        if let Some(value) = server {
            servers.insert(String::from("domyjob"), value);
        } else {
            servers.remove("domyjob");
        }
        document
    }

    /// Whether deleting characters from `outer` can leave `inner`.
    fn survives_in(inner: &str, outer: &str) -> bool {
        let mut rest = outer.chars();
        inner
            .chars()
            .all(|wanted| rest.any(|character| character == wanted))
    }

    #[test]
    fn strip_blanks_comments_and_trailing_commas_outside_strings() {
        assert_eq!(
            strip("[1, /* c */ 2, // d\n]"),
            format!("[1,{}2{}\n]", " ".repeat(9), " ".repeat(6))
        );
        let text = "// Header.\n{\n  \"url\": \"https://example.test/*x*/\", /* Block\n  comment. */\n  \"quote\": \"say \\\"hi\\\" // still text\",\n  \"slash\": \"ends with \\\\\",\n  \"nested\": [1, [2,], {\"a\": null,},],\n}\n";
        assert_eq!(strip(text).lines().count(), text.lines().count());
        assert_eq!(
            parse(text),
            json!({
                "url": "https://example.test/*x*/",
                "quote": "say \"hi\" // still text",
                "slash": "ends with \\",
                "nested": [1, [2], {"a": null}]
            })
        );
        for invalid in [
            "[,]",
            "{,}",
            "{\"a\": 1,,}",
            "{\"a\": 1} /* open",
            "{\"a\": \"open}",
            "{\"a\": 1 / 2}",
            "{\"a\": 1}\r// A line comment ends at a lone CR.\r{}",
        ] {
            ingress::foreign_json::<Value>(&strip(invalid)).unwrap_err();
        }
    }

    #[test]
    fn set_member_keeps_comments_and_other_servers_and_is_idempotent() {
        let original = r#"// Keep this header.
{
  "$schema": "https://opencode.ai/config.json",
  "mcp": {
    "other": {"type": "remote", "url": "https://example.test"}, // Keep this server.
  },
  "model": "provider/model" /* Keep this choice. */
}
"#;
        let updated = set_member(original, &PATH, SERVER).unwrap();
        let added = format!("server.\n    \"domyjob\": {SERVER},\n");
        assert_eq!(updated, original.replace("server.\n", &added));
        let value = parse(&updated);
        assert_eq!(value.pointer("/mcp/domyjob"), Some(&parse(SERVER)));
        assert_eq!(
            value.pointer("/mcp/other/url"),
            Some(&json!("https://example.test"))
        );
        assert_eq!(value.pointer("/model"), Some(&json!("provider/model")));
        assert_eq!(set_member(&updated, &PATH, SERVER).unwrap(), updated);
        assert_eq!(remove_member(&updated, &PATH).unwrap(), original);
    }

    #[test]
    fn set_member_creates_what_is_missing_in_the_layout_of_the_document() {
        let created = "{\n  \"mcp\": {\n    \"domyjob\": 1\n  }\n}";
        for (original, expected) in [
            ("", format!("{created}\n")),
            (" \n\t", format!("{created}\n")),
            ("{}", String::from(created)),
            (
                "// Only a comment.\n",
                format!("// Only a comment.\n{created}\n"),
            ),
            (
                "{\n  \"theme\": \"system\" // Keep this note.\n}\n",
                String::from(
                    "{\n  \"theme\": \"system\", // Keep this note.\n  \"mcp\": {\n    \"domyjob\": 1\n  }\n}\n",
                ),
            ),
            (
                "{\"theme\": \"system\"}",
                String::from("{\"theme\": \"system\", \"mcp\": {\"domyjob\": 1}}"),
            ),
            (
                "{\"theme\": \"system\",}",
                String::from("{\"theme\": \"system\", \"mcp\": {\"domyjob\": 1},}"),
            ),
            (
                "{\r\n\t\"mcp\": {}\r\n}\r\n",
                String::from("{\r\n\t\"mcp\": {\r\n\t\t\"domyjob\": 1\r\n\t}\r\n}\r\n"),
            ),
            (
                "{\n    \"mcp\": { /* Servers. */ },\n}",
                String::from("{\n    \"mcp\": { /* Servers. */\n        \"domyjob\": 1\n    },\n}"),
            ),
            (
                "{\n  \"mcp\": {\n    // Servers.\n  }\n}",
                String::from("{\n  \"mcp\": {\n    // Servers.\n    \"domyjob\": 1\n  }\n}"),
            ),
        ] {
            assert_eq!(
                set_member(original, &PATH, " 1 ").unwrap(),
                expected,
                "{original:?}"
            );
        }
        assert_eq!(
            set_member("{\"a\": 0}", &["b", "c", "d"], "1").unwrap(),
            "{\"a\": 0, \"b\": {\"c\": {\"d\": 1}}}"
        );
        assert_eq!(
            set_member("{\n}", &["b", "c", "d"], "1").unwrap(),
            "{\n  \"b\": {\n    \"c\": {\n      \"d\": 1\n    }\n  }\n}"
        );
    }

    #[test]
    fn set_member_replaces_only_the_value_of_an_existing_member() {
        let original = "{\n  \"mcp\": {\n    \"domyjob\": {\n      \"type\": \"remote\" // Old.\n    }, // Ours.\n    \"other\": true\n  }\n}";
        assert_eq!(
            set_member(original, &PATH, SERVER).unwrap(),
            format!(
                "{{\n  \"mcp\": {{\n    \"domyjob\": {SERVER}, // Ours.\n    \"other\": true\n  }}\n}}"
            )
        );
        let escaped = "{\"\\u006dcp\": {\"domyjob\": 0}}";
        assert_eq!(
            set_member(escaped, &PATH, "1").unwrap(),
            "{\"\\u006dcp\": {\"domyjob\": 1}}"
        );
        let added = set_member("{}", &["say \"hi\"", "back\\slash"], "true").unwrap();
        assert_eq!(parse(&added), json!({"say \"hi\"": {"back\\slash": true}}));
    }

    #[test]
    fn edits_refuse_documents_they_cannot_change_safely() {
        let cases: [(&str, &[&str], &str, &str); 9] = [
            ("[]", &PATH, "1", "the document root is not a JSON object"),
            ("{\"mcp\": 5}", &PATH, "1", "`mcp` is not a JSON object"),
            (
                "{\"mcp\": {\"a\": []}}",
                &["mcp", "a", "b"],
                "1",
                "`mcp.a` is not a JSON object",
            ),
            (
                "{\"mcp\": {}, \"mcp\": {}}",
                &PATH,
                "1",
                "`mcp` appears more than once",
            ),
            (
                "{\"mcp\": {\"domyjob\": 1, \"domyjob\": 2}}",
                &PATH,
                "1",
                "`mcp.domyjob` appears more than once",
            ),
            (
                "{\"a\": 1 \"b\": 2}",
                &PATH,
                "1",
                "the document is not valid JSON with comments: ",
            ),
            (
                "{} /* open",
                &PATH,
                "1",
                "the document is not valid JSON with comments: ",
            ),
            ("{}", &PATH, "{oops}", "the new value is not valid JSON: "),
            ("{}", &[], "1", "a member path needs at least one key"),
        ];
        for (document, path, value, message) in cases {
            let error = set_member(document, path, value).unwrap_err().to_string();
            assert!(error.starts_with(message), "{error}");
        }
        assert!(matches!(
            remove_member("{", &PATH),
            Err(JsoncError::Document(_))
        ));
        assert!(matches!(remove_member("", &[]), Err(JsoncError::EmptyPath)));
    }

    #[test]
    fn remove_member_deletes_the_member_its_lines_and_the_comma_it_leaves() {
        let cases: [(&str, &[&str], &str); 8] = [
            (
                "{\n  \"a\": 1,\n  \"b\": 2, // About b.\n  \"c\": 3\n}",
                &["b"],
                "{\n  \"a\": 1,\n  \"c\": 3\n}",
            ),
            (
                "{\n  \"a\": 1, // About a.\n  \"b\": 2\n}",
                &["b"],
                "{\n  \"a\": 1 // About a.\n}",
            ),
            (
                "{\r\n  \"a\": 1,\r\n  \"b\": 2\r\n}",
                &["a"],
                "{\r\n  \"b\": 2\r\n}",
            ),
            (
                "{\"a\": 1, \"b\": 2, \"c\": 3}",
                &["b"],
                "{\"a\": 1, \"c\": 3}",
            ),
            ("{\"a\": 1, \"b\": 2}", &["b"], "{\"a\": 1}"),
            ("{\"a\": 1, \"b\": 2,}", &["b"], "{\"a\": 1, }"),
            ("{ \"a\": [1, {\"x\": \"}\"}] }", &["a"], "{ }"),
            (
                "{\"a\": {\"b\": 1}} // End.",
                &["a", "b"],
                "{\"a\": {}} // End.",
            ),
        ];
        for (original, path, expected) in cases {
            assert_eq!(
                remove_member(original, path).unwrap(),
                expected,
                "{original:?}"
            );
        }
        for unchanged in [
            "",
            "// Nothing.\n",
            "[]",
            "{\"mcp\": 5}",
            "{\"mcp\": {\"other\": 1}}",
        ] {
            assert_eq!(remove_member(unchanged, &PATH).unwrap(), unchanged);
        }
    }

    #[test]
    fn edits_change_only_the_member_across_documents() {
        for original in [
            "",
            "{}",
            "// Config.\n{\n  \"$schema\": \"https://opencode.ai/config.json\", // Schema.\n}\n",
            "{\"mcp\": {\"other\": {\"enabled\": false}}} /* End. */",
            "{\n\t\"mcp\": {\n\t\t\"other\": [\"/* not a comment */\", \"// nor this\"],\n\t},\n}",
            "/* a */ { /* b */ \"theme\" /* c */ : /* d */ \"x\" /* e */ } /* f */",
            "{\"theme\": 1 // A lone CR ends this comment.\r}",
        ] {
            let before = if original.is_empty() {
                json!({})
            } else {
                parse(original)
            };
            let updated = set_member(original, &PATH, SERVER).unwrap();
            assert!(survives_in(original, &updated), "{updated}");
            assert_eq!(
                parse(&updated),
                with_server(before.clone(), Some(parse(SERVER)))
            );
            assert_eq!(set_member(&updated, &PATH, SERVER).unwrap(), updated);
            let removed = remove_member(&updated, &PATH).unwrap();
            assert_eq!(parse(&removed), with_server(before, None), "{removed}");
        }
    }
}
