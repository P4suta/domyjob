use proc_macro2::Span;
use syn::visit::Visit;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Finding {
    pub line: usize,
    pub rule: &'static str,
}

const BANNED_SERDE: &[&str] = &["untagged", "flatten", "default", "alias", "other"];

fn line_of(span: Span) -> usize {
    span.start().line
}

fn serde_words(attrs: &[syn::Attribute]) -> Vec<String> {
    let mut words = Vec::new();
    for attr in attrs.iter().filter(|a| a.path().is_ident("serde")) {
        let text = match &attr.meta {
            syn::Meta::List(list) => list.tokens.to_string(),
            syn::Meta::Path(_) | syn::Meta::NameValue(_) => continue,
        };
        for part in text.split(',') {
            let key: String = part
                .trim()
                .chars()
                .take_while(|c| c.is_alphanumeric() || *c == '_')
                .collect();
            words.push(key);
        }
    }
    words
}

fn derives(attrs: &[syn::Attribute], name: &str) -> bool {
    attrs
        .iter()
        .filter(|a| a.path().is_ident("derive"))
        .any(|attr| match &attr.meta {
            syn::Meta::List(list) => list
                .tokens
                .to_string()
                .split(',')
                .any(|d| d.trim().rsplit("::").next().map(str::trim) == Some(name)),
            syn::Meta::Path(_) | syn::Meta::NameValue(_) => false,
        })
}

const fn has_named_fields(fields: &syn::Fields) -> bool {
    matches!(fields, syn::Fields::Named(_))
}

#[derive(Debug, Default)]
pub struct Gate {
    pub findings: Vec<Finding>,
    file: String,
    test_depth: u32,
}

struct Restriction {
    path: &'static [&'static str],
    allowed_in: &'static [&'static str],
    rule: &'static str,
}

const RESTRICTIONS: &[Restriction] = &[
    Restriction {
        path: &["serde_json", "from_slice"],
        allowed_in: &["ingress.rs"],
        rule: "decode input only in ingress.rs, into a type that implements Ingress",
    },
    Restriction {
        path: &["serde_json", "from_str"],
        allowed_in: &["ingress.rs"],
        rule: "decode input only in ingress.rs, into a type that implements Ingress",
    },
    Restriction {
        path: &["serde_json", "from_value"],
        allowed_in: &["ingress.rs"],
        rule: "decode input only in ingress.rs, into a type that implements Ingress",
    },
    Restriction {
        path: &["serde_json", "from_reader"],
        allowed_in: &["ingress.rs"],
        rule: "decode input only in ingress.rs, into a type that implements Ingress",
    },
    Restriction {
        path: &["toml", "from_str"],
        allowed_in: &["ingress.rs"],
        rule: "decode input only in ingress.rs, into a type that implements Ingress",
    },
    Restriction {
        path: &["UserText", "from_cli"],
        allowed_in: &["cli.rs"],
        rule: "only the command line may declare text as the user's",
    },
    Restriction {
        path: &["UserText", "from_agent"],
        allowed_in: &["mcp.rs"],
        rule: "only the MCP server may declare text as the agent's",
    },
    Restriction {
        path: &["UserText", "from_project_job_the_user_invoked"],
        allowed_in: &["cli.rs"],
        rule: "only `domyjob do` may run a project file's command",
    },
    Restriction {
        path: &["InsecureUnsigned", "acknowledged_on_the_command_line"],
        allowed_in: &["cli.rs"],
        rule: "only an explicit command-line flag may accept an unsigned binary",
    },
];

const TIME_RULE: &str = "time decides nothing; wait for the event itself, and observe the clock only through clock::Timestamp";
const CLOCK_FILES: &[&str] = &["clock.rs"];
const LIVENESS_FILES: &[&str] = &["liveness.rs"];
const LIVENESS_TIME: &[&str] = &["Instant", "Duration", "wait_timeout", "elapsed"];
const TIME_TYPES: &[&str] = &["SystemTime", "Instant", "Duration", "UNIX_EPOCH"];
const TIME_METHODS: &[&str] = &["sleep", "modified", "accessed", "elapsed"];
const TIME_METHOD_PARTS: &[&str] = &["timeout", "deadline"];

fn is_time_method(name: &str) -> bool {
    TIME_METHODS.contains(&name) || TIME_METHOD_PARTS.iter().any(|part| name.contains(part))
}

const DECISION_FILES: &[&str] = &["authz.rs", "trust.rs", "audit.rs"];
const WIRE_FILES: &[&str] = &["protocol.rs"];
const LOCAL_ONLY_TYPES: &[&str] = &["ConfigText", "UserText", "Arg", "Rendered", "Secret"];

impl Gate {
    fn flag(&mut self, span: Span, rule: &'static str) {
        self.findings.push(Finding {
            line: line_of(span),
            rule,
        });
    }

    fn check_input(&mut self, attrs: &[syn::Attribute], span: Span, needs_exact: bool) {
        if !derives(attrs, "Deserialize") {
            return;
        }
        let words = serde_words(attrs);
        let converted = words
            .iter()
            .any(|w| w == "try_from" || w == "from" || w == "transparent");
        if needs_exact && !converted && !words.iter().any(|w| w == "deny_unknown_fields") {
            self.flag(
                span,
                "a deserialized type with named fields must say #[serde(deny_unknown_fields)]",
            );
        }
        if words.iter().any(|w| BANNED_SERDE.contains(&w.as_str())) {
            self.flag(
                span,
                "deserialized input may not use untagged, flatten, default, alias, or other",
            );
        }
    }
}

#[expect(
    clippy::wildcard_enum_match_arm,
    reason = "syn::Type is non-exhaustive, so a foreign crate forces the default arm"
)]
fn is_textual(ty: &syn::Type) -> bool {
    match ty {
        syn::Type::Path(path) => path
            .path
            .segments
            .last()
            .is_some_and(|s| s.ident == "String"),
        syn::Type::Reference(reference) => {
            matches!(&*reference.elem, syn::Type::Path(p) if p.path.is_ident("str"))
        }
        syn::Type::Tuple(tuple) => tuple.elems.is_empty(),
        _ => false,
    }
}

fn is_test_module(attrs: &[syn::Attribute]) -> bool {
    attrs.iter().any(|attr| {
        attr.path().is_ident("cfg")
            && match &attr.meta {
                syn::Meta::List(list) => list.tokens.to_string().contains("test"),
                syn::Meta::Path(_) | syn::Meta::NameValue(_) => false,
            }
    })
}

fn returns_bool(output: &syn::ReturnType) -> bool {
    match output {
        syn::ReturnType::Type(_, ty) => {
            matches!(&**ty, syn::Type::Path(p) if p.path.is_ident("bool"))
        }
        syn::ReturnType::Default => false,
    }
}

impl Gate {
    fn file_is(&self, names: &[&str]) -> bool {
        names.iter().any(|name| self.file.ends_with(name))
    }

    fn check_time_path(&mut self, path: &syn::Path) {
        if self.file_is(CLOCK_FILES) {
            return;
        }
        let liveness = self.file_is(LIVENESS_FILES);
        for segment in &path.segments {
            let name = segment.ident.to_string();
            if liveness && LIVENESS_TIME.contains(&name.as_str()) {
                continue;
            }
            if TIME_TYPES.contains(&name.as_str()) || is_time_method(&name) {
                self.flag(segment.ident.span(), TIME_RULE);
            }
        }
    }

    fn check_path(&mut self, path: &syn::Path) {
        self.check_time_path(path);
        if self.test_depth > 0 {
            return;
        }
        let segments: Vec<String> = path.segments.iter().map(|s| s.ident.to_string()).collect();
        for restriction in RESTRICTIONS {
            let tail = segments.len().saturating_sub(restriction.path.len());
            let matches = segments
                .get(tail..)
                .is_some_and(|end| end.iter().zip(restriction.path).all(|(a, b)| a == b))
                && segments.len() >= restriction.path.len();
            if matches && !self.file_is(restriction.allowed_in) {
                let span = path
                    .segments
                    .first()
                    .map_or_else(Span::call_site, |s| s.ident.span());
                self.flag(span, restriction.rule);
            }
        }
    }

    fn check_signature(&mut self, sig: &syn::Signature) {
        if self.test_depth == 0 && self.file_is(DECISION_FILES) && returns_bool(&sig.output) {
            self.flag(
                sig.ident.span(),
                "security decisions return an enum, never bool",
            );
        }
    }
}

impl<'ast> Visit<'ast> for Gate {
    fn visit_item_mod(&mut self, item: &'ast syn::ItemMod) {
        let test = is_test_module(&item.attrs);
        if test {
            self.test_depth = self.test_depth.saturating_add(1);
        }
        syn::visit::visit_item_mod(self, item);
        if test {
            self.test_depth = self.test_depth.saturating_sub(1);
        }
    }

    fn visit_path(&mut self, path: &'ast syn::Path) {
        self.check_path(path);
        if self.test_depth == 0 && self.file_is(WIRE_FILES) {
            for segment in &path.segments {
                if LOCAL_ONLY_TYPES.iter().any(|t| segment.ident == t) {
                    self.flag(
                        segment.ident.span(),
                        "wire types may not carry values that only local code may create",
                    );
                }
            }
        }
        syn::visit::visit_path(self, path);
    }

    fn visit_expr_method_call(&mut self, call: &'ast syn::ExprMethodCall) {
        let method = call.method.to_string();
        let exempt = self.file_is(CLOCK_FILES)
            || (self.file_is(LIVENESS_FILES) && LIVENESS_TIME.contains(&method.as_str()));
        if !exempt && is_time_method(&method) {
            self.flag(call.method.span(), TIME_RULE);
        }
        syn::visit::visit_expr_method_call(self, call);
    }

    fn visit_item_fn(&mut self, item: &'ast syn::ItemFn) {
        self.check_signature(&item.sig);
        syn::visit::visit_item_fn(self, item);
    }

    fn visit_impl_item_fn(&mut self, item: &'ast syn::ImplItemFn) {
        self.check_signature(&item.sig);
        syn::visit::visit_impl_item_fn(self, item);
    }

    fn visit_attribute(&mut self, attr: &'ast syn::Attribute) {
        if attr.path().is_ident("allow") {
            self.flag(
                attr.pound_token.span,
                "#[allow] hides a lint; use #[expect(lint, reason = \"...\")]",
            );
        }
        if attr.path().is_ident("doc") {
            self.flag(
                attr.pound_token.span,
                "documentation comments are not written; explain in the commit message",
            );
        }
        syn::visit::visit_attribute(self, attr);
    }

    fn visit_item_struct(&mut self, item: &'ast syn::ItemStruct) {
        self.check_input(
            &item.attrs,
            item.ident.span(),
            has_named_fields(&item.fields),
        );
        for field in &item.fields {
            let words = serde_words(&field.attrs);
            if derives(&item.attrs, "Deserialize")
                && words.iter().any(|w| BANNED_SERDE.contains(&w.as_str()))
            {
                self.flag(
                    item.ident.span(),
                    "deserialized fields may not use untagged, flatten, default, alias, or other",
                );
            }
        }
        syn::visit::visit_item_struct(self, item);
    }

    fn visit_item_enum(&mut self, item: &'ast syn::ItemEnum) {
        let struct_variants = item.variants.iter().any(|v| has_named_fields(&v.fields));
        self.check_input(&item.attrs, item.ident.span(), struct_variants);
        if item
            .variants
            .iter()
            .any(|v| v.attrs.iter().any(|a| a.path().is_ident("default")))
        {
            self.flag(
                item.ident.span(),
                "#[default] lets declaration order invent a domain state",
            );
        }
        syn::visit::visit_item_enum(self, item);
    }

    fn visit_path_segment(&mut self, segment: &'ast syn::PathSegment) {
        if segment.ident == "Result"
            && let syn::PathArguments::AngleBracketed(args) = &segment.arguments
            && let Some(syn::GenericArgument::Type(error)) = args.args.iter().nth(1)
            && is_textual(error)
        {
            self.flag(
                segment.ident.span(),
                "errors are typed enums, not String, &str, or ()",
            );
        }
        if segment.ident == "Box"
            && let syn::PathArguments::AngleBracketed(args) = &segment.arguments
            && let Some(syn::GenericArgument::Type(syn::Type::TraitObject(_))) = args.args.first()
        {
            self.flag(
                segment.ident.span(),
                "own a closed set with an enum or an open one with a generic, not Box<dyn>",
            );
        }
        syn::visit::visit_path_segment(self, segment);
    }
}

pub fn check(source: &str) -> Result<Vec<Finding>, syn::Error> {
    check_file(source, "")
}

pub fn check_file(source: &str, name: &str) -> Result<Vec<Finding>, syn::Error> {
    let file = syn::parse_file(source)?;
    let mut gate = Gate {
        findings: Vec::new(),
        file: name.to_owned(),
        test_depth: 0,
    };
    gate.visit_file(&file);
    Ok(gate.findings)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rules(source: &str) -> Vec<&'static str> {
        check(source).unwrap().into_iter().map(|f| f.rule).collect()
    }

    #[test]
    fn open_input_is_refused() {
        assert_eq!(rules("#[derive(Deserialize)]\nstruct A { x: u8 }").len(), 1);
        assert!(
            rules("#[derive(Deserialize)]\n#[serde(deny_unknown_fields)]\nstruct A { x: u8 }")
                .is_empty()
        );
        assert!(
            rules("#[derive(Deserialize)]\n#[serde(try_from = \"String\")]\nstruct A(String);")
                .is_empty()
        );
        assert_eq!(rules("#[derive(Deserialize)]\n#[serde(deny_unknown_fields)]\nstruct A { #[serde(default)] x: u8 }").len(), 1);
        assert_eq!(
            rules("#[derive(serde::Deserialize)]\n#[serde(untagged)]\nenum E { A(u8), B(String) }")
                .len(),
            1
        );
        assert!(rules("#[derive(Deserialize)]\nenum E { A, B }").is_empty());
    }

    #[test]
    fn borders_and_origins_are_enforced_per_file() {
        let decode = "fn f(b: &[u8]) { let _v: u8 = serde_json::from_slice(b).unwrap(); }";
        assert_eq!(check_file(decode, "src/node.rs").unwrap().len(), 1);
        assert!(check_file(decode, "src/ingress.rs").unwrap().is_empty());
        assert!(
            check_file(
                &format!("#[cfg(test)]\nmod tests {{ {decode} }}"),
                "src/node.rs"
            )
            .unwrap()
            .is_empty()
        );
        let declare = "fn f() { let _t = crate::input::UserText::from_cli(String::new()); }";
        assert_eq!(check_file(declare, "src/serve.rs").unwrap().len(), 1);
        assert!(check_file(declare, "src/cli.rs").unwrap().is_empty());
        assert_eq!(
            check_file("pub fn allowed() -> bool { true }", "src/authz.rs")
                .unwrap()
                .len(),
            1
        );
        assert!(
            check_file("pub fn allowed() -> bool { true }", "src/shell.rs")
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            check_file(
                "pub struct W { t: crate::input::UserText }",
                "src/protocol.rs"
            )
            .unwrap()
            .len(),
            1
        );
    }

    #[test]
    fn time_is_refused_everywhere_but_the_clock() {
        for source in [
            "fn f() { std::thread::sleep(d); }",
            "fn f() { let _t = std::time::Instant::now(); }",
            "fn f(s: &S) { s.set_read_timeout(None); }",
            "fn f(r: &R) { let _e = r.recv_timeout(x); }",
            "fn f(m: &M) { let _t = m.modified(); }",
            "fn f() -> Duration { d }",
            "#[cfg(test)] mod tests { fn f() { std::thread::sleep(d); } }",
        ] {
            assert_eq!(rules(source), [TIME_RULE], "{source}");
        }
        assert!(
            check_file(
                "fn f() { let _t = std::time::SystemTime::now(); }",
                "src/clock.rs"
            )
            .unwrap()
            .is_empty()
        );
        assert!(rules("fn f(x: &X) { x.wait(); x.recv(); }").is_empty());
        assert!(
            check_file(
                "fn f(c: &C, g: G) { let t = std::time::Instant::now(); let _ = c.wait_timeout(g, Duration::ZERO); t.elapsed(); }",
                "src/liveness.rs"
            )
            .unwrap()
            .is_empty()
        );
        for refused in [
            "fn f() { let _t = std::time::SystemTime::now(); }",
            "fn f() { std::thread::sleep(d); }",
            "fn f(s: &S) { s.set_read_timeout(None); }",
        ] {
            assert_eq!(
                check_file(refused, "src/liveness.rs")
                    .unwrap()
                    .into_iter()
                    .map(|f| f.rule)
                    .collect::<Vec<_>>(),
                [TIME_RULE],
                "{refused}"
            );
        }
    }

    #[test]
    fn escape_hatches_are_refused() {
        assert_eq!(rules("#[allow(dead_code)]\nfn f() {}").len(), 1);
        assert!(rules("#[expect(dead_code, reason = \"x\")]\nfn f() {}").is_empty());
        assert_eq!(rules("fn f() -> Result<(), String> { Ok(()) }").len(), 1);
        assert_eq!(rules("fn f() -> Result<u8, ()> { Ok(1) }").len(), 1);
        assert!(rules("fn f() -> Result<u8, E> { Ok(1) }").is_empty());
        assert_eq!(rules("struct S { f: Box<dyn Fn()> }").len(), 1);
        assert_eq!(
            rules("#[derive(Default)]\nenum E { #[default] A, B }").len(),
            1
        );
    }
}
