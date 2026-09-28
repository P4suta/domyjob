//! Lint exceptions are declarations of ownership, and they take one shape.
//!
//! An effect the lints reserve, such as writing a file or reading the clock, happens only inside a private inline module named `raw` whose inner attribute expects the reserved lints and whose functions each forward in one statement.
//! Its owner wraps those functions with its policy, and every other line stays fully linted,
//! so an exception can never hide an unrelated violation.
//! Only foreign-function modules and the end-to-end harness declare exceptions for a whole file.

use syn::punctuated::Punctuated;
use syn::visit::Visit;

/// The lints a `raw` module may expect.
const EFFECT_LINTS: &[&str] = &[
    "clippy::disallowed_methods",
    "clippy::disallowed_types",
    "clippy::disallowed_macros",
];

/// The files that may expect lints for the whole file, and which lints.
const FILE_EXCEPTIONS: &[(&str, &[&str])] = &[
    (
        "crates/domyjob/src/platform/windows_acl.rs",
        &["unsafe_code"],
    ),
    ("crates/domyjob/src/process/windows.rs", &["unsafe_code"]),
    (
        "crates/domyjob/tests/e2e.rs",
        &[
            "clippy::disallowed_methods",
            "clippy::disallowed_types",
            "clippy::disallowed_macros",
        ],
    ),
];

fn line(attribute: &syn::Attribute) -> usize {
    attribute
        .path()
        .segments
        .first()
        .map_or(1, |part| part.ident.span().start().line)
}

/// The lints an `expect` attribute names, or `None` for any other attribute.
fn expected(attribute: &syn::Attribute) -> Option<Vec<String>> {
    if !attribute.path().is_ident("expect") {
        return None;
    }
    let Ok(arguments) =
        attribute.parse_args_with(Punctuated::<syn::Meta, syn::Token![,]>::parse_terminated)
    else {
        return Some(Vec::new());
    };
    Some(
        arguments
            .iter()
            .filter_map(|argument| match argument {
                syn::Meta::Path(path) => Some(
                    path.segments
                        .iter()
                        .map(|segment| segment.ident.to_string())
                        .collect::<Vec<_>>()
                        .join("::"),
                ),
                syn::Meta::List(_) | syn::Meta::NameValue(_) => None,
            })
            .collect(),
    )
}

fn conditional_exception(attribute: &syn::Attribute) -> bool {
    attribute.path().is_ident("cfg_attr")
        && attribute.meta.require_list().is_ok_and(|list| {
            list.tokens.clone().into_iter().any(|token| {
                matches!(&token, proc_macro2::TokenTree::Ident(ident) if ident == "expect" || ident == "allow")
            })
        })
}

/// Whether an item of a `raw` module only imports, defines a constant, or forwards in one statement.
fn thin(item: &syn::Item) -> bool {
    let restricted = |visibility: &syn::Visibility| match visibility {
        syn::Visibility::Inherited => true,
        syn::Visibility::Restricted(scope) => scope.path.is_ident("super"),
        syn::Visibility::Public(_) => false,
    };
    if let syn::Item::Fn(function) = item {
        return restricted(&function.vis)
            && function.block.stmts.len() <= 1
            && !matches!(function.block.stmts.first(), Some(syn::Stmt::Local(_)));
    }
    if let syn::Item::Const(constant) = item {
        return restricted(&constant.vis);
    }
    if let syn::Item::Static(value) = item {
        return restricted(&value.vis);
    }
    matches!(item, syn::Item::Use(_))
}

#[derive(Debug, Default)]
struct Exceptions {
    findings: Vec<String>,
}

impl Exceptions {
    fn check_raw(&mut self, module: &syn::ItemMod) {
        let at = module.ident.span().start().line;
        if !matches!(module.vis, syn::Visibility::Inherited) {
            self.findings
                .push(format!("{at}: a `raw` module is private to its owner"));
        }
        let Some((_, items)) = &module.content else {
            self.findings.push(format!(
                "{at}: a `raw` module is written inline beside its owner"
            ));
            return;
        };
        let mut declared = false;
        for attribute in &module.attrs {
            match (attribute.style, expected(attribute)) {
                (syn::AttrStyle::Inner(_), Some(lints))
                    if lints
                        .iter()
                        .all(|lint| EFFECT_LINTS.contains(&lint.as_str())) =>
                {
                    declared = true;
                }
                (_, Some(_)) => {
                    declared = true;
                    self.findings.push(format!(
                        "{}: a `raw` module expects only the lints that reserve effects",
                        line(attribute)
                    ));
                }
                (_, None) => self.visit_attribute(attribute),
            }
        }
        if !declared {
            self.findings
                .push(format!("{at}: a `raw` module declares the effects it owns"));
        }
        for item in items {
            if !thin(item) {
                self.findings.push(format!(
                    "{at}: a `raw` module only forwards to what it owns, one statement per function"
                ));
            }
            self.visit_item(item);
        }
    }
}

impl<'ast> Visit<'ast> for Exceptions {
    fn visit_attribute(&mut self, attribute: &'ast syn::Attribute) {
        if expected(attribute).is_some() || conditional_exception(attribute) {
            self.findings.push(format!(
                "{}: lint exceptions belong in a private `raw` module that owns the effect",
                line(attribute)
            ));
        }
        syn::visit::visit_attribute(self, attribute);
    }

    fn visit_item_mod(&mut self, module: &'ast syn::ItemMod) {
        if module.ident == "raw" {
            self.check_raw(module);
        } else {
            syn::visit::visit_item_mod(self, module);
        }
    }
}

/// The misplaced or misshapen lint exceptions in the file at `path`.
#[must_use]
pub fn check(path: &str, file: &syn::File) -> Vec<String> {
    let mut exceptions = Exceptions::default();
    let allowed = FILE_EXCEPTIONS
        .iter()
        .find(|(owner, _)| *owner == path)
        .map_or(&[][..], |(_, lints)| *lints);
    for attribute in &file.attrs {
        match expected(attribute) {
            Some(lints) if lints.iter().all(|lint| allowed.contains(&lint.as_str())) => {}
            Some(_) => exceptions.findings.push(format!(
                "{}: only foreign-function modules and the end-to-end harness expect lints file-wide",
                line(attribute)
            )),
            None => exceptions.visit_attribute(attribute),
        }
    }
    for item in &file.items {
        exceptions.visit_item(item);
    }
    exceptions.findings
}

#[cfg(test)]
mod tests {
    use super::check;

    fn findings(source: &str) -> usize {
        check(
            "crates/domyjob/src/example.rs",
            &syn::parse_file(source).unwrap(),
        )
        .len()
    }

    #[test]
    fn exceptions_live_only_in_thin_private_raw_modules() {
        let owned = r#"
            mod raw {
                #![expect(clippy::disallowed_methods, reason = "this module owns file writes")]
                use std::path::Path;
                pub(super) fn write(path: &Path) -> std::io::Result<()> { std::fs::write(path, b"") }
            }
        "#;
        assert_eq!(findings(owned), 0);
        for misplaced in [
            r#"#[expect(clippy::disallowed_methods, reason = "x")] fn write() {}"#,
            r#"#![expect(clippy::disallowed_methods, reason = "x")] fn write() {}"#,
            r#"fn f() { #[expect(clippy::cast_possible_truncation, reason = "x")] let a = 1; }"#,
            r#"#[cfg_attr(windows, expect(dead_code, reason = "x"))] fn f() {}"#,
            r#"pub mod raw { #![expect(clippy::disallowed_methods, reason = "x")] }"#,
            r#"mod raw { #![expect(clippy::too_many_lines, reason = "x")] }"#,
            r#"mod raw { #![expect(clippy::disallowed_methods, reason = "x")] pub fn f() {} }"#,
            r#"mod raw { #![expect(clippy::disallowed_methods, reason = "x")] fn f() { let a = 1; a; } }"#,
            r#"mod raw { #![expect(clippy::disallowed_methods, reason = "x")] struct Hidden; }"#,
            "mod raw { fn f() {} }",
        ] {
            assert_eq!(findings(misplaced), 1, "{misplaced}");
        }
        let foreign =
            syn::parse_file(r#"#![expect(unsafe_code, reason = "x")] fn f() {}"#).unwrap();
        assert!(check("crates/domyjob/src/process/windows.rs", &foreign).is_empty());
    }
}
