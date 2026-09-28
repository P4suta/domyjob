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

fn path_ends_with(path: &syn::Path, tail: &[&str]) -> bool {
    path.segments.len() >= tail.len()
        && path
            .segments
            .iter()
            .rev()
            .zip(tail.iter().rev())
            .all(|(actual, expected)| actual.ident == *expected)
}

fn mentions_any_identifier(
    tokens: &proc_macro2::TokenStream,
    names: &std::collections::BTreeSet<String>,
    include_self: bool,
) -> bool {
    tokens.clone().into_iter().any(|token| match token {
        proc_macro2::TokenTree::Ident(ident) => {
            names.contains(&ident.to_string()) || (include_self && ident == "Self")
        }
        proc_macro2::TokenTree::Group(group) => {
            mentions_any_identifier(&group.stream(), names, include_self)
        }
        proc_macro2::TokenTree::Punct(_) | proc_macro2::TokenTree::Literal(_) => false,
    })
}

fn contains_enum_keyword(tokens: &proc_macro2::TokenStream) -> bool {
    tokens.clone().into_iter().any(|token| match token {
        proc_macro2::TokenTree::Ident(ident) => ident == "enum",
        proc_macro2::TokenTree::Group(group) => contains_enum_keyword(&group.stream()),
        proc_macro2::TokenTree::Punct(_) | proc_macro2::TokenTree::Literal(_) => false,
    })
}

fn pattern_mentions_enum(
    pat: &syn::Pat,
    names: &std::collections::BTreeSet<String>,
    include_self: bool,
) -> bool {
    struct Finder<'a> {
        names: &'a std::collections::BTreeSet<String>,
        include_self: bool,
        found: bool,
    }

    impl<'ast> Visit<'ast> for Finder<'_> {
        fn visit_path(&mut self, path: &'ast syn::Path) {
            if path
                .segments
                .iter()
                .take(path.segments.len().saturating_sub(1))
                .any(|segment| {
                    self.names.contains(&segment.ident.to_string())
                        || (self.include_self && segment.ident == "Self")
                })
            {
                self.found = true;
            }
            syn::visit::visit_path(self, path);
        }
    }

    let mut finder = Finder {
        names,
        include_self,
        found: false,
    };
    finder.visit_pat(pat);
    finder.found
}

fn pattern_catches_all(pat: &syn::Pat) -> bool {
    if let syn::Pat::Wild(_) = pat {
        return true;
    }
    if let syn::Pat::Ident(ident) = pat {
        return ident.subpat.is_none()
            && ident
                .ident
                .to_string()
                .chars()
                .next()
                .is_some_and(|first| first.is_lowercase() || first == '_');
    }
    if let syn::Pat::Tuple(tuple) = pat {
        return tuple.elems.iter().all(pattern_catches_all);
    }
    if let syn::Pat::Or(or) = pat {
        return or.cases.iter().any(pattern_catches_all);
    }
    if let syn::Pat::Paren(paren) = pat {
        return pattern_catches_all(&paren.pat);
    }
    if let syn::Pat::Reference(reference) = pat {
        return pattern_catches_all(&reference.pat);
    }
    false
}

fn same_constructor(left: &syn::Path, right: &syn::Path) -> bool {
    left.segments
        .last()
        .zip(right.segments.last())
        .is_some_and(|(left, right)| left.ident == right.ident)
}

fn struct_variant_absorbed(
    left: &syn::PatStruct,
    right: &syn::PatStruct,
    names: &std::collections::BTreeSet<String>,
    include_self: bool,
) -> bool {
    same_constructor(&left.path, &right.path)
        && left.fields.iter().any(|field| {
            if !pattern_mentions_enum(&field.pat, names, include_self) {
                return false;
            }
            match right
                .fields
                .iter()
                .find(|other| other.member == field.member)
            {
                Some(other) => enum_variant_absorbed(&field.pat, &other.pat, names, include_self),
                None => right.rest.is_some(),
            }
        })
}

fn sequence_variant_absorbed(
    left: &syn::punctuated::Punctuated<syn::Pat, syn::Token![,]>,
    right: &syn::punctuated::Punctuated<syn::Pat, syn::Token![,]>,
    names: &std::collections::BTreeSet<String>,
    include_self: bool,
) -> bool {
    let left_rest = left.iter().position(|pat| matches!(pat, syn::Pat::Rest(_)));
    let right_rest = right
        .iter()
        .position(|pat| matches!(pat, syn::Pat::Rest(_)));
    if let Some(rest) = left_rest {
        let left_suffix = left.len().saturating_sub(rest).saturating_sub(1);
        if right_rest.is_none() && right.len() < left.len().saturating_sub(1) {
            return false;
        }
        let right_prefix = right_rest.unwrap_or(right.len());
        let right_suffix = right_rest.map_or(right.len(), |index| {
            right.len().saturating_sub(index).saturating_sub(1)
        });
        return left.iter().take(rest).enumerate().any(|(index, pat)| {
            if right_rest.is_some() && index >= right_prefix {
                pattern_mentions_enum(pat, names, include_self)
            } else {
                right
                    .iter()
                    .nth(index)
                    .is_some_and(|other| enum_variant_absorbed(pat, other, names, include_self))
            }
        }) || left
            .iter()
            .rev()
            .take(left_suffix)
            .enumerate()
            .any(|(index, pat)| {
                if right_rest.is_some() && index >= right_suffix {
                    pattern_mentions_enum(pat, names, include_self)
                } else {
                    right
                        .iter()
                        .rev()
                        .nth(index)
                        .is_some_and(|other| enum_variant_absorbed(pat, other, names, include_self))
                }
            });
    }
    let Some(rest) = right_rest else {
        return left.len() == right.len()
            && left
                .iter()
                .zip(right)
                .any(|(left, right)| enum_variant_absorbed(left, right, names, include_self));
    };
    let suffix = right.len().saturating_sub(rest).saturating_sub(1);
    let Some(fixed) = rest.checked_add(suffix) else {
        return false;
    };
    if left.len() < fixed {
        return false;
    }
    left.iter()
        .take(rest)
        .zip(right.iter().take(rest))
        .any(|(left, right)| enum_variant_absorbed(left, right, names, include_self))
        || left
            .iter()
            .rev()
            .take(suffix)
            .zip(right.iter().rev().take(suffix))
            .any(|(left, right)| enum_variant_absorbed(left, right, names, include_self))
        || left
            .iter()
            .skip(rest)
            .take(left.len().saturating_sub(fixed))
            .any(|pat| pattern_mentions_enum(pat, names, include_self))
}

fn enum_variant_absorbed(
    explicit: &syn::Pat,
    fallback: &syn::Pat,
    names: &std::collections::BTreeSet<String>,
    include_self: bool,
) -> bool {
    if !pattern_mentions_enum(explicit, names, include_self) {
        return false;
    }
    if pattern_catches_all(fallback) {
        return true;
    }
    if let syn::Pat::Or(or) = explicit {
        return or
            .cases
            .iter()
            .any(|case| enum_variant_absorbed(case, fallback, names, include_self));
    }
    if let syn::Pat::Or(or) = fallback {
        return or
            .cases
            .iter()
            .any(|case| enum_variant_absorbed(explicit, case, names, include_self));
    }
    if let syn::Pat::Guard(guard) = explicit {
        return enum_variant_absorbed(&guard.pat, fallback, names, include_self);
    }
    if let syn::Pat::Paren(paren) = explicit {
        return enum_variant_absorbed(&paren.pat, fallback, names, include_self);
    }
    if let syn::Pat::Paren(paren) = fallback {
        return enum_variant_absorbed(explicit, &paren.pat, names, include_self);
    }
    if let syn::Pat::Reference(reference) = explicit {
        return enum_variant_absorbed(&reference.pat, fallback, names, include_self);
    }
    if let syn::Pat::Reference(reference) = fallback {
        return enum_variant_absorbed(explicit, &reference.pat, names, include_self);
    }
    if let syn::Pat::Ident(ident) = explicit
        && let Some((_, inner)) = &ident.subpat
    {
        return enum_variant_absorbed(inner, fallback, names, include_self);
    }
    if let syn::Pat::Ident(ident) = fallback
        && let Some((_, inner)) = &ident.subpat
    {
        return enum_variant_absorbed(explicit, inner, names, include_self);
    }
    if let (syn::Pat::Tuple(left), syn::Pat::Tuple(right)) = (explicit, fallback) {
        return sequence_variant_absorbed(&left.elems, &right.elems, names, include_self);
    }
    if let (syn::Pat::TupleStruct(left), syn::Pat::TupleStruct(right)) = (explicit, fallback) {
        return same_constructor(&left.path, &right.path)
            && sequence_variant_absorbed(&left.elems, &right.elems, names, include_self);
    }
    if let (syn::Pat::Slice(left), syn::Pat::Slice(right)) = (explicit, fallback) {
        return sequence_variant_absorbed(&left.elems, &right.elems, names, include_self);
    }
    if let (syn::Pat::Struct(left), syn::Pat::Struct(right)) = (explicit, fallback) {
        return struct_variant_absorbed(left, right, names, include_self);
    }
    false
}

fn unary_call_argument<'a>(expr: &'a syn::Expr, tail: &[&str]) -> Option<&'a syn::Expr> {
    let syn::Expr::Call(call) = expr else {
        return None;
    };
    if !matches!(call.func.as_ref(), syn::Expr::Path(path) if path_ends_with(&path.path, tail))
        || call.args.len() != 1
    {
        return None;
    }
    call.args.first()
}

fn same_error_value(expr: &syn::Expr, binding: &syn::Ident) -> bool {
    if let syn::Expr::Path(path) = expr {
        return path.path.is_ident(binding);
    }
    if let syn::Expr::MethodCall(call) = expr {
        return call.method == "into"
            && call.args.is_empty()
            && same_error_value(&call.receiver, binding);
    }
    unary_call_argument(expr, &["ClientError", "from"])
        .is_some_and(|argument| same_error_value(argument, binding))
}

fn error_forwarded(expr: &syn::Expr, binding: &syn::Ident) -> bool {
    if let syn::Expr::Return(ret) = expr {
        return ret
            .expr
            .as_ref()
            .is_some_and(|value| error_forwarded(value, binding));
    }
    unary_call_argument(expr, &["Err"]).is_some_and(|argument| same_error_value(argument, binding))
}

fn forwards_original_error(arm: &syn::Arm) -> bool {
    if let syn::Pat::Ident(binding) = &arm.pat
        && binding.subpat.is_none()
        && let syn::Expr::Path(value) = arm.body.as_ref()
    {
        return value.path.is_ident(&binding.ident);
    }
    if let syn::Pat::TupleStruct(result) = &arm.pat
        && path_ends_with(&result.path, &["Err"])
        && result.elems.len() == 1
        && let Some(syn::Pat::Ident(binding)) = result.elems.first()
        && binding.subpat.is_none()
    {
        return error_forwarded(&arm.body, &binding.ident);
    }
    false
}

fn forwards_nested_error(
    explicit: &syn::Pat,
    fallback: &syn::Arm,
    names: &std::collections::BTreeSet<String>,
    include_self: bool,
) -> bool {
    struct Context<'a> {
        body: &'a syn::Expr,
        names: &'a std::collections::BTreeSet<String>,
        include_self: bool,
    }

    fn is_error_variant(pat: &syn::Pat) -> bool {
        let path = if let syn::Pat::Path(path) = pat {
            &path.path
        } else if let syn::Pat::Struct(strukt) = pat {
            &strukt.path
        } else if let syn::Pat::TupleStruct(tuple) = pat {
            &tuple.path
        } else {
            return false;
        };
        path.segments
            .iter()
            .rev()
            .nth(1)
            .is_some_and(|segment| segment.ident.to_string().ends_with("Error"))
    }

    fn in_pattern(
        explicit: &syn::Pat,
        fallback: &syn::Pat,
        context: &Context<'_>,
        under_error: bool,
    ) -> bool {
        if !pattern_mentions_enum(explicit, context.names, context.include_self) {
            return false;
        }
        if let syn::Pat::Ident(binding) = fallback {
            return (under_error || is_error_variant(explicit))
                && binding.subpat.is_none()
                && error_forwarded(context.body, &binding.ident);
        }
        match (explicit, fallback) {
            (syn::Pat::Guard(left), right) => in_pattern(&left.pat, right, context, under_error),
            (syn::Pat::Tuple(left), syn::Pat::Tuple(right)) => {
                left.elems.len() == right.elems.len()
                    && left
                        .elems
                        .iter()
                        .zip(&right.elems)
                        .any(|(left, right)| in_pattern(left, right, context, under_error))
            }
            (syn::Pat::TupleStruct(left), syn::Pat::TupleStruct(right)) => {
                let under_error = under_error || path_ends_with(&left.path, &["Err"]);
                same_constructor(&left.path, &right.path)
                    && left.elems.len() == right.elems.len()
                    && left
                        .elems
                        .iter()
                        .zip(&right.elems)
                        .any(|(left, right)| in_pattern(left, right, context, under_error))
            }
            (syn::Pat::Paren(left), right) => in_pattern(&left.pat, right, context, under_error),
            (left, syn::Pat::Paren(right)) => in_pattern(left, &right.pat, context, under_error),
            _ => false,
        }
    }

    let context = Context {
        body: &fallback.body,
        names,
        include_self,
    };
    in_pattern(explicit, &fallback.pat, &context, false)
}

fn calls_path(block: &syn::Block, tail: &'static [&'static str]) -> bool {
    struct Finder {
        tail: &'static [&'static str],
        found: bool,
    }

    impl<'ast> Visit<'ast> for Finder {
        fn visit_expr_call(&mut self, call: &'ast syn::ExprCall) {
            if matches!(call.func.as_ref(), syn::Expr::Path(path)
                if path_ends_with(&path.path, self.tail))
            {
                self.found = true;
            }
            syn::visit::visit_expr_call(self, call);
        }
    }

    let mut finder = Finder { tail, found: false };
    finder.visit_block(block);
    finder.found
}

fn import_ends_with(path: &[String], tail: &[&str]) -> bool {
    path.len() >= tail.len()
        && path
            .iter()
            .rev()
            .zip(tail.iter().rev())
            .all(|(actual, expected)| actual == expected)
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

fn conditionally_derives_time_comparison(attrs: &[syn::Attribute]) -> bool {
    attrs.iter().filter(|attr| attr.path().is_ident("cfg_attr")).any(|attr| {
        let Ok(arguments) = attr.parse_args_with(
            syn::punctuated::Punctuated::<syn::Meta, syn::Token![,]>::parse_terminated,
        ) else {
            return true;
        };
        let mut arguments = arguments.iter();
        let test_only = matches!(arguments.next(), Some(syn::Meta::Path(path)) if path.is_ident("test"));
        !test_only && arguments.any(|argument| {
            matches!(argument, syn::Meta::List(derive) if derive.path.is_ident("derive")
                && derive.parse_args_with(
                    syn::punctuated::Punctuated::<syn::Path, syn::Token![,]>::parse_terminated
                ).is_ok_and(|traits| traits.iter().any(|name|
                    name.segments.last().is_some_and(|segment|
                        ["PartialEq", "Eq", "PartialOrd", "Ord"].contains(&segment.ident.to_string().as_str())))))
        })
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
    function: Option<String>,
    current_impl: Option<String>,
    legacy_bool_hits: Vec<usize>,
    state_impl: bool,
    state_enums: std::collections::BTreeSet<String>,
    defined_types: std::collections::BTreeSet<String>,
}

struct Restriction {
    path: &'static [&'static str],
    allowed_in: &'static [&'static str],
    rule: &'static str,
}

#[derive(Clone, Copy)]
enum FieldShape {
    Named(&'static str),
    Generic(&'static str, &'static [&'static str]),
    OptionMap(&'static str, &'static str),
}

struct RequiredField {
    file: &'static str,
    owner: &'static str,
    field: &'static str,
    shape: FieldShape,
    rule: &'static str,
}

struct StateConstructor {
    file: &'static str,
    function: &'static str,
    value: FieldShape,
}

const STATE_CONSTRUCTORS: &[StateConstructor] = &[
    StateConstructor {
        file: "audit.rs",
        function: "head_file",
        value: FieldShape::Named("Head"),
    },
    StateConstructor {
        file: "audit.rs",
        function: "base_file",
        value: FieldShape::Named("Head"),
    },
    StateConstructor {
        file: "audit.rs",
        function: "rotation_file",
        value: FieldShape::Named("Rotation"),
    },
    StateConstructor {
        file: "audit.rs",
        function: "archive_base_file",
        value: FieldShape::Named("Head"),
    },
    StateConstructor {
        file: "client.rs",
        function: "origin_file",
        value: FieldShape::Named("ClientOriginId"),
    },
    StateConstructor {
        file: "dist.rs",
        function: "high_water_file",
        value: FieldShape::Named("HighWater"),
    },
    StateConstructor {
        file: "keystore.rs",
        function: "stored_file",
        value: FieldShape::Named("StoredKey"),
    },
    StateConstructor {
        file: "pull.rs",
        function: "plan_file",
        value: FieldShape::Named("Record"),
    },
    StateConstructor {
        file: "remote.rs",
        function: "witness_file",
        value: FieldShape::Named("Head"),
    },
    StateConstructor {
        file: "remote.rs",
        function: "facts_file",
        value: FieldShape::Named("Facts"),
    },
    StateConstructor {
        file: "store.rs",
        function: "settings_file",
        value: FieldShape::Named("Settings"),
    },
    StateConstructor {
        file: "store.rs",
        function: "spec_file",
        value: FieldShape::Named("Spec"),
    },
    StateConstructor {
        file: "store.rs",
        function: "staged_spec_file",
        value: FieldShape::Named("Spec"),
    },
    StateConstructor {
        file: "store.rs",
        function: "phase_file",
        value: FieldShape::Named("Phase"),
    },
    StateConstructor {
        file: "store.rs",
        function: "staged_phase_file",
        value: FieldShape::Named("Phase"),
    },
    StateConstructor {
        file: "store.rs",
        function: "env_file",
        value: FieldShape::Named("StoredEnv"),
    },
    StateConstructor {
        file: "store.rs",
        function: "launch_env_file",
        value: FieldShape::Named("LaunchEnv"),
    },
    StateConstructor {
        file: "store.rs",
        function: "sequence_file",
        value: FieldShape::Named("Sequence"),
    },
    StateConstructor {
        file: "store.rs",
        function: "left_file",
        value: FieldShape::Named("StoredLeft"),
    },
    StateConstructor {
        file: "supervisor.rs",
        function: "applied_file",
        value: FieldShape::Named("Applied"),
    },
    StateConstructor {
        file: "trust.rs",
        function: "file",
        value: FieldShape::Named("Self"),
    },
];

const REQUIRED_FIELDS: &[RequiredField] = &[
    RequiredField {
        file: "authz.rs",
        owner: "Authorized",
        field: "nature",
        shape: FieldShape::Named("Nature"),
        rule: AUTHORIZED_COMMAND_RULE,
    },
    RequiredField {
        file: "authz.rs",
        owner: "RoutedRequest",
        field: "nature",
        shape: FieldShape::Named("Nature"),
        rule: AUTHORIZED_COMMAND_RULE,
    },
    RequiredField {
        file: "project.rs",
        owner: "Project",
        field: "jobs",
        shape: FieldShape::Generic("BTreeMap", &["JobName", "RepositoryRequest"]),
        rule: PROJECT_APPROVAL_RULE,
    },
    RequiredField {
        file: "project.rs",
        owner: "RepositoryRequest",
        field: "on",
        shape: FieldShape::Named("ProjectSelector"),
        rule: PROJECT_APPROVAL_RULE,
    },
    RequiredField {
        file: "project.rs",
        owner: "RepositoryRequest",
        field: "run",
        shape: FieldShape::Generic("Vec", &["RepositoryText"]),
        rule: REPOSITORY_PROVENANCE_RULE,
    },
    RequiredField {
        file: "project.rs",
        owner: "RepositoryRequest",
        field: "runner",
        shape: FieldShape::Generic("Option", &["RepositoryText"]),
        rule: REPOSITORY_PROVENANCE_RULE,
    },
    RequiredField {
        file: "project.rs",
        owner: "RepositoryRequest",
        field: "dir",
        shape: FieldShape::Generic("Option", &["RelPath"]),
        rule: PROJECT_APPROVAL_RULE,
    },
    RequiredField {
        file: "project.rs",
        owner: "RepositoryRequest",
        field: "env",
        shape: FieldShape::OptionMap("EnvName", "RepositoryText"),
        rule: REPOSITORY_PROVENANCE_RULE,
    },
    RequiredField {
        file: "project.rs",
        owner: "ProjectOrderParts",
        field: "root",
        shape: FieldShape::Named("PathBuf"),
        rule: PROJECT_APPROVAL_RULE,
    },
    RequiredField {
        file: "project.rs",
        owner: "ProjectOrderParts",
        field: "words",
        shape: FieldShape::Generic("Vec", &["ApprovedProjectWord"]),
        rule: REPOSITORY_PROVENANCE_RULE,
    },
    RequiredField {
        file: "client.rs",
        owner: "Order",
        field: "targets",
        shape: FieldShape::Named("Targets"),
        rule: PROJECT_APPROVAL_RULE,
    },
    RequiredField {
        file: "config.rs",
        owner: "Config",
        field: "project_jobs",
        shape: FieldShape::Generic("Vec", &["LocalPolicy"]),
        rule: PROJECT_APPROVAL_RULE,
    },
    RequiredField {
        file: "node.rs",
        owner: "Marks",
        field: "ids",
        shape: FieldShape::Generic("BTreeMap", &["BlobId", "BlobUse"]),
        rule: CAS_MARK_RULE,
    },
    RequiredField {
        file: "node.rs",
        owner: "Marks",
        field: "limit",
        shape: FieldShape::Named("usize"),
        rule: CAS_MARK_RULE,
    },
];

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
        path: &["serde_json", "Deserializer", "from_slice"],
        allowed_in: &["ingress.rs"],
        rule: "decode input only in ingress.rs, into a type that implements Ingress",
    },
    Restriction {
        path: &["serde_json", "Deserializer", "from_str"],
        allowed_in: &["ingress.rs"],
        rule: "decode input only in ingress.rs, into a type that implements Ingress",
    },
    Restriction {
        path: &["serde_json", "Deserializer", "from_reader"],
        allowed_in: &["ingress.rs"],
        rule: "decode input only in ingress.rs, into a type that implements Ingress",
    },
    Restriction {
        path: &["serde_json", "de", "from_slice"],
        allowed_in: &["ingress.rs"],
        rule: "decode input only in ingress.rs, into a type that implements Ingress",
    },
    Restriction {
        path: &["serde_json", "de", "from_str"],
        allowed_in: &["ingress.rs"],
        rule: "decode input only in ingress.rs, into a type that implements Ingress",
    },
    Restriction {
        path: &["serde_json", "de", "from_reader"],
        allowed_in: &["ingress.rs"],
        rule: "decode input only in ingress.rs, into a type that implements Ingress",
    },
    Restriction {
        path: &["serde_json", "value", "from_value"],
        allowed_in: &["ingress.rs"],
        rule: "decode input only in ingress.rs, into a type that implements Ingress",
    },
    Restriction {
        path: &["toml", "from_str"],
        allowed_in: &["ingress.rs", "build_config.rs"],
        rule: "decode external TOML only at its named input boundary",
    },
    Restriction {
        path: &["toml", "de", "from_str"],
        allowed_in: &["ingress.rs"],
        rule: "decode input only in ingress.rs, into a type that implements Ingress",
    },
    Restriction {
        path: &["toml_edit", "de", "from_str"],
        allowed_in: &["ingress.rs"],
        rule: "decode input only in ingress.rs, into a type that implements Ingress",
    },
    Restriction {
        path: &["DocumentMut", "from_str"],
        allowed_in: &["ingress.rs"],
        rule: "decode editable TOML only in ingress.rs",
    },
    Restriction {
        path: &["InsecureUnsigned", "acknowledged_on_the_command_line"],
        allowed_in: &["cli.rs"],
        rule: "only an explicit command-line flag may accept an unsigned binary",
    },
    Restriction {
        path: &["InsecureUnsigned", "from_local_checkout"],
        allowed_in: &["remote.rs"],
        rule: "automatic source installation requires the matching local checkout",
    },
    Restriction {
        path: &["Dirs", "with_supervisor_paths"],
        allowed_in: &["cli.rs"],
        rule: SUPERVISOR_PATH_RULE,
    },
    Restriction {
        path: &["Detected", "from_hook_root"],
        allowed_in: &["hook.rs"],
        rule: "only the local hook may pass its canonical source root to snapshot detection",
    },
    Restriction {
        path: &["Command", "current_dir"],
        allowed_in: &["spawn.rs"],
        rule: WORKING_DIRECTORY_RULE,
    },
    Restriction {
        path: &["read_audit_bytes"],
        allowed_in: &["audit.rs"],
        rule: "only audit.rs may request the larger audit state-file budget",
    },
    Restriction {
        path: &["read_history_bytes"],
        allowed_in: &["client.rs"],
        rule: "only client.rs may request the larger history state-file budget",
    },
    Restriction {
        path: &["state_file", "publish_dir"],
        allowed_in: &["store.rs"],
        rule: "publish staged jobs only through Store so collection sees every job",
    },
    Restriction {
        path: &["state_file", "read_json"],
        allowed_in: &["state_file.rs"],
        rule: "read JSON state through StateFile<T> so its value type stays attached to the path",
    },
    Restriction {
        path: &["state_file", "write_json"],
        allowed_in: &["state_file.rs"],
        rule: "replace JSON state through StateFile<T> or its locked guard",
    },
];

const TIME_RULE: &str = "time decides nothing; wait for the event itself, and observe the clock only through clock::Timestamp";
const CLOCK_FILES: &[&str] = &["clock.rs"];
const LIVENESS_FILES: &[&str] = &["liveness.rs"];
const LIVENESS_TIME: &[&str] = &["Instant", "Duration", "wait_timeout", "elapsed"];
const TIME_TYPES: &[&str] = &["SystemTime", "Instant", "Duration", "UNIX_EPOCH"];
const TIME_METHODS: &[&str] = &["sleep", "modified", "accessed", "elapsed"];
const TIME_METHOD_PARTS: &[&str] = &["timeout", "deadline"];
const OPAQUE_TIME_RULE: &str = "keep Timestamp and Elapsed opaque; time values may be recorded or displayed, never returned as numeric decision inputs";
const EXPLICIT_PRESENTATION_TIME_RULE: &str =
    "protocol timing methods take an explicit observation; presentation owns the clock read";
const UNBOUNDED_READ_RULE: &str = "read input only through bounded.rs with an explicit byte budget";
const UNBOUNDED_CHILD_OUTPUT_RULE: &str =
    "capture child output only through bounded.rs with an explicit byte budget";
const RELEASE_STATE_RULE: &str =
    "release high-water state must be read and written through a locked StateFile";
const ORIGIN_STATE_RULE: &str = "the client origin must be initialized through a locked StateFile";
const EDITABLE_TOML_RULE: &str = "decode editable TOML only in ingress.rs";
const EDITABLE_TOML_TYPE_RULE: &str =
    "keep the editable TOML parser private inside the ingress-owned value type";
const PROTECTED_IMPORT_RULE: &str = "do not import or alias protected decoders and effect namespaces; keep their source visible to the syntax gate";
const IMPORT_GLOB_RULE: &str =
    "production glob imports hide the origin of decoders and effects from the syntax gate";
const STATE_CONSTRUCTOR_RULE: &str =
    "construct StateFile<T> only in its typed owner method; keep the path and value type together";
const UNBOUNDED_TEXT_RULE: &str =
    "read text files only through bounded::text_file with an explicit byte budget";
const UNBOUNDED_FILE_RULE: &str =
    "read binary files through bounded::file_bytes with an explicit byte budget";
const EXCLUSIVE_CREATE_RULE: &str =
    "create_new is only for approved exclusive file creation, never a hand-made lock";
const INCOMING_RULE: &str =
    "setup staging paths must include the transfer ID; a fixed incoming name mixes operations";
const FAULT_SCENARIO_RULE: &str =
    "test fault scenarios belong only in faults.rs; use a path-scoped guard elsewhere";
const REFERENCE_TYPE_RULE: &str =
    "trigger references and patterns must use their bounded domain types";
const PROJECT_APPROVAL_RULE: &str =
    "project jobs must pass from RepositoryRequest through local approval before becoming an Order";
const CAS_MARK_RULE: &str = "CAS collection keeps a bounded, typed map of marked blob IDs";
const REPOSITORY_PROVENANCE_RULE: &str =
    "repository text must retain its origin until local project approval";
const PROJECT_PROOF_PRIVACY_RULE: &str = "project approval proof and request fields must stay private so callers cannot forge or reclassify them";
const SUPERVISOR_PATH_RULE: &str =
    "only the local supervisor launcher may select its directory paths";
const SAFE_PATH_RULE: &str =
    "process path arguments require the sealed SafePath trait and its reviewed local sources";
const AUTHORIZED_COMMAND_RULE: &str = "node effects require exact authorization proofs from authz";
const PEER_INGRESS_RULE: &str =
    "decode inbound requests only as an opaque PeerRequest and open them only in authorization";
const EXACT_INGRESS_RULE: &str =
    "implement Ingress only for a concrete input type defined in the same module";
const WORKING_DIRECTORY_RULE: &str =
    "set process working directories only through Invocation::in_dir with a SafePath";
const JOB_PROCESS_RULE: &str =
    "job process creation requires the private proof that its environment was cleared and assigned";
const REVOCATION_RULE: &str =
    "trust revocation requires a private selector parsed before the locked state change";
const SAFE_PATH_TYPES: &[&str] = &[
    "Executable",
    "LocalDirectory",
    "ServiceLog",
    "Detected",
    "DistributionPath",
    "WorkingDirectory",
];
const MCP_LINE_RULE: &str = "MCP input lines must use bounded::line with an explicit byte budget";
const MCP_WORKER_RULE: &str =
    "MCP workers must be spawned only through dispatch with an McpDispatch permit";
const SNAPSHOT_BUDGET_RULE: &str =
    "snapshot file reads and worker counts must use fixed source budgets";
const QUEUE_WATCH_RULE: &str =
    "queue lock watchers must be registered through bounded QueueWatchers";
const SURVEY_BUFFER_RULE: &str =
    "watch surveys must accumulate only through the bounded LineBuffer";
const LIVE_UPDATE_RULE: &str = "live watch updates must use a one-item channel";
const WATCH_WAKE_RULE: &str = "node watch wakes must use a one-item channel";
const SUPERVISOR_EVENT_RULE: &str =
    "supervisor events must use a bounded channel with a private sender";
const CLI_FANOUT_RULE: &str = "short CLI fanout must use the shared sixteen-worker scheduler";
const JOB_IDS_RULE: &str = "production job ID traversal must stream through JobIds";
const SLOT_SCAN_RULE: &str =
    "discover only the bounded canonical job slots through Store::held_slots";
const WORKSPACE_SLOT_RULE: &str = "workspace cleanup must enumerate private canonical SlotIndex values, never parse directory names";
const WORKSPACE_ORDER_RULE: &str =
    "idle workspace cleanup must visit projects in bounded sorted order";
const VERSION_DECISION_RULE: &str = "remote version comparison must distinguish newer, older, and unparsable versions before installation";
const PHASE_TRANSITION_RULE: &str = "job phase transitions must use an exhaustive typed decision";
const STATE_CLASSIFICATION_RULE: &str =
    "classify domain enums through exhaustive matches or typed methods, never matches!";
const STATE_PATTERN_RULE: &str =
    "classify domain enums through exhaustive matches or typed methods, never conditional patterns";
const STATE_FALLBACK_RULE: &str =
    "name every domain enum variant in match arms instead of absorbing future variants";
const STATE_IMPORT_RULE: &str =
    "keep covered enum and variant names visible to the state classification gate";
const STATE_MACRO_RULE: &str =
    "declare product enums as parsed items and review item macros before use";
const STATE_ENUMS: &[&str] = &[
    "Phase",
    "State",
    "DiskSpace",
    "Supervisor",
    "QueueMode",
    "Publication",
    "Blocker",
    "Probe",
    "Principal",
    "Request",
    "Reply",
    "RefusalCode",
    "ClientError",
    "ServiceAction",
    "Location",
    "KillState",
    "AddressScope",
    "PairingState",
    "Consideration",
    "Deliverable",
    "Output",
    "Verdict",
    "Answered",
    "Checked",
    "Availability",
    "MissingManifest",
    "Entry",
    "Reach",
];
const DIRECTORY_OWNER_RULE: &str =
    "read directories only in their reviewed streaming owner functions";
const DIRECTORY_READ_OWNERS: &[(&str, &str)] = &[
    ("build_stamp.rs", "inputs"),
    ("cas.rs", "for_each_stored_matching"),
    ("node.rs", "entries"),
    ("node.rs", "cached_version_dirs"),
    ("pull.rs", "for_each_entry"),
    ("state_file.rs", "make_removable"),
    ("platform.rs", "plan_legacy_acl"),
    ("store.rs", "iter_ids"),
    ("user_files.rs", "sweep_retired"),
];
const WINDOWS_PRIVATE_DIR_RULE: &str =
    "Windows private directories must be created with their ACL, without spawning icacls";
const SNAPSHOT_TRANSFER_RULE: &str =
    "source blobs must arrive within one Submit exchange under the collection lock";
const EXCLUSIVE_CREATE: &[(&str, &str)] = &[
    ("crates/domyjob/src/state_file.rs", "create_empty"),
    ("crates/domyjob/src/durable.rs", "beside"),
    ("crates/domyjob/src/tree.rs", "create_file"),
    ("xtask/src/release.rs", "keygen"),
];

fn is_time_method(name: &str) -> bool {
    TIME_METHODS.contains(&name) || TIME_METHOD_PARTS.iter().any(|part| name.contains(part))
}

const DECISION_FILES: &[&str] = &[
    "authz.rs",
    "trust.rs",
    "audit.rs",
    "supervisor.rs",
    "serve.rs",
    "store.rs",
    "tree.rs",
    "pull.rs",
    "workspace.rs",
    "mcp.rs",
    "node.rs",
    "client.rs",
    "remote.rs",
];
const DECISION_SIGNATURE_RULE: &str = "security decisions return an enum, never bool";
const LEGACY_BOOL_BASELINE_RULE: &str =
    "legacy boolean exception count changed; review and update its exact baseline";
const LEGACY_BOOL_SIGNATURES: &[(&str, &str, &[&str])] = &[
    ("board.rs", "is_quiet", &["Board"]),
    ("board.rs", "is_live", &["Board"]),
    ("build_stamp.rs", "source_dir", &[]),
    ("build_stamp.rs", "source_file", &[]),
    ("cas.rs", "has", &["Cas"]),
    ("cli.rs", "is_output_broken_pipe", &["CliError"]),
    ("cli.rs", "wants_json", &[]),
    ("dist.rs", "run", &[]),
    ("dist.rs", "fetch", &[]),
    ("domain.rs", "portable_component", &[]),
    ("domain.rs", "crockford", &[]),
    ("domain.rs", "matches", &["RefPattern", "JobId"]),
    ("liveness.rs", "silenced", &["Watchdog"]),
    ("paths.rs", "enabled", &["Availability"]),
    ("paths.rs", "links", &["Family"]),
    ("paths.rs", "modes", &["Family"]),
    ("paths.rs", "load_average", &["Family"]),
    ("paths.rs", "agent_socket", &["Family"]),
    ("paths.rs", "replaces_running_executables", &["Family"]),
    ("platform.rs", "is_reparse_point", &[]),
    ("platform.rs", "elevated", &[]),
    ("proc.rs", "inside_remote_session", &[]),
    ("protocol.rs", "counts_as_running", &["State"]),
    ("protocol.rs", "has_timing_sample", &["State"]),
    ("protocol.rs", "is_settled", &["Job"]),
    ("protocol.rs", "succeeded", &["Job"]),
    ("service.rs", "is_installed", &[]),
    ("shell.rs", "on_path", &[]),
    ("snapshot.rs", "metadata", &[]),
    ("template.rs", "is_empty", &["Argv"]),
    ("terminal.rs", "dangerous", &[]),
    ("ui.rs", "unicode", &[]),
    ("ui.rs", "stderr_is_live", &[]),
    ("user_files.rs", "present", &[]),
    ("view.rs", "stdout_is_a_person", &[]),
];

fn expected_legacy_bool_count(file: &str, name: &str) -> usize {
    match (file, name) {
        ("domain.rs", "matches") | ("platform.rs", "is_reparse_point" | "elevated") => 2,
        _ => 1,
    }
}
const TERMINAL_FILES: &[&str] = &["view.rs", "ui.rs", "board.rs", "history.rs", "cli.rs"];
const FAILURE_FILES: &[&str] = &[
    "failure.rs",
    "xtask/src/lib.rs",
    "xtask/src/dependencies.rs",
    "xtask/src/proverif.rs",
    "xtask/src/release.rs",
    "xtask/src/workflows.rs",
];

fn carries_io_source(fields: &syn::Fields) -> bool {
    let has_path = fields
        .iter()
        .any(|field| field.ident.as_ref().is_some_and(|name| name == "path"));
    has_path
        && fields.iter().any(|field| {
            field.ident.as_ref().is_some_and(|name| name == "source")
                && matches!(&field.ty, syn::Type::Path(path)
                if path.path.segments.len() >= 2
                    && path.path.segments.last().is_some_and(|last| last.ident == "Error")
                    && path.path.segments.iter().rev().nth(1).is_some_and(|io| io.ident == "io"))
        })
}
const WIRE_FILES: &[&str] = &["protocol.rs"];
const JSON_RULE: &str = "an ad hoc JSON shape drifts from the others; give it a type in output.rs and print it through output";
const JSON_FILES: &[&str] = &["mcp.rs", "protocol.rs"];
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

fn cfg_requires_test(condition: &syn::Meta) -> bool {
    match condition {
        syn::Meta::Path(path) => path.is_ident("test"),
        syn::Meta::List(list) if list.path.is_ident("all") || list.path.is_ident("any") => {
            let Ok(parts) = list.parse_args_with(
                syn::punctuated::Punctuated::<syn::Meta, syn::Token![,]>::parse_terminated,
            ) else {
                return false;
            };
            if parts.is_empty() {
                return false;
            }
            if list.path.is_ident("all") {
                parts.iter().any(cfg_requires_test)
            } else {
                parts.iter().all(cfg_requires_test)
            }
        }
        syn::Meta::List(_) | syn::Meta::NameValue(_) => false,
    }
}

fn is_test_module(attrs: &[syn::Attribute]) -> bool {
    attrs.iter().any(|attr| {
        attr.path().is_ident("cfg")
            && attr
                .parse_args::<syn::Meta>()
                .is_ok_and(|condition| cfg_requires_test(&condition))
    })
}

fn type_carries_bool(ty: &syn::Type) -> bool {
    let syn::Type::Path(path) = ty else {
        return false;
    };
    if path.path.is_ident("bool") {
        return true;
    }
    let Some(segment) = path.path.segments.last() else {
        return false;
    };
    if segment.ident != "Result" && segment.ident != "Option" {
        return false;
    }
    let syn::PathArguments::AngleBracketed(arguments) = &segment.arguments else {
        return false;
    };
    if let Some(syn::GenericArgument::Type(inner)) = arguments.args.first() {
        type_carries_bool(inner)
    } else {
        false
    }
}

#[expect(
    clippy::wildcard_enum_match_arm,
    reason = "syn::Type is non-exhaustive, so a foreign crate forces the default arm"
)]
fn type_carries_numeric(ty: &syn::Type) -> bool {
    match ty {
        syn::Type::Path(path) => path.path.segments.last().is_some_and(|segment| {
            let name = segment.ident.to_string();
            if [
                "u8", "u16", "u32", "u64", "u128", "usize", "i8", "i16", "i32", "i64", "i128",
                "isize", "f32", "f64",
            ]
            .contains(&name.as_str())
            {
                return true;
            }
            matches!(&segment.arguments, syn::PathArguments::AngleBracketed(arguments)
                if arguments.args.iter().any(|argument|
                    matches!(argument, syn::GenericArgument::Type(inner)
                        if type_carries_numeric(inner))))
        }),
        syn::Type::Reference(reference) => type_carries_numeric(&reference.elem),
        syn::Type::Paren(paren) => type_carries_numeric(&paren.elem),
        syn::Type::Group(group) => type_carries_numeric(&group.elem),
        syn::Type::Tuple(tuple) => tuple.elems.iter().any(type_carries_numeric),
        _ => false,
    }
}

fn is_named_type(ty: &syn::Type, name: &str) -> bool {
    matches!(ty, syn::Type::Path(path) if path.path.segments.last().is_some_and(|segment| segment.ident == name))
}

fn is_safe_path_argument(ty: &syn::Type) -> bool {
    is_reference_to_impl_trait(ty, "SafePath")
}

fn is_reference_to_impl_trait(ty: &syn::Type, name: &str) -> bool {
    matches!(ty, syn::Type::Reference(reference)
        if matches!(reference.elem.as_ref(), syn::Type::ImplTrait(implemented)
            if implemented.bounds.iter().any(|bound|
                matches!(bound, syn::TypeParamBound::Trait(trait_bound)
                    if path_ends_with(&trait_bound.path, &[name])))))
}

fn signature_has_type(sig: &syn::Signature, name: &str) -> bool {
    sig.inputs.iter().any(
        |input| matches!(input, syn::FnArg::Typed(argument) if is_named_type(&argument.ty, name)),
    )
}

fn signature_has_watch_batch(sig: &syn::Signature) -> bool {
    sig.inputs.iter().any(|input| {
        let syn::FnArg::Typed(argument) = input else {
            return false;
        };
        let syn::Type::Reference(reference) = argument.ty.as_ref() else {
            return false;
        };
        let syn::Type::Path(path) = reference.elem.as_ref() else {
            return false;
        };
        let Some(segment) = path
            .path
            .segments
            .last()
            .filter(|segment| segment.ident == "ConcurrentBatch")
        else {
            return false;
        };
        let syn::PathArguments::AngleBracketed(generics) = &segment.arguments else {
            return false;
        };
        let arguments: Vec<_> = generics.args.iter().collect();
        let [
            syn::GenericArgument::Lifetime(_),
            syn::GenericArgument::Type(item_type),
            limit,
        ] = arguments.as_slice()
        else {
            return false;
        };
        is_named_type(item_type, "String")
            && (matches!(limit, syn::GenericArgument::Type(limit_type)
                if is_named_type(limit_type, "MAX_WATCHES"))
                || matches!(limit, syn::GenericArgument::Const(syn::Expr::Path(limit_path))
                    if limit_path.path.is_ident("MAX_WATCHES")))
    })
}

fn signature_has_authority(sig: &syn::Signature) -> bool {
    sig.inputs.iter().any(|input| {
        matches!(input, syn::FnArg::Typed(argument)
            if is_reference_to_impl_trait(&argument.ty, "CommandAuthority"))
    })
}

fn has_single_safe_path_argument(sig: &syn::Signature) -> bool {
    let typed: Vec<_> = sig
        .inputs
        .iter()
        .filter_map(|input| match input {
            syn::FnArg::Typed(argument) => Some(argument.ty.as_ref()),
            syn::FnArg::Receiver(_) => None,
        })
        .collect();
    matches!(typed.as_slice(), [path] if is_safe_path_argument(path))
}

fn is_vec_of(ty: &syn::Type, name: &str) -> bool {
    is_generic_of(ty, "Vec", &[name])
}

#[expect(
    clippy::wildcard_enum_match_arm,
    reason = "syn::GenericArgument is non-exhaustive, so a foreign crate forces the default arm"
)]
fn generic_types<'a>(ty: &'a syn::Type, outer: &str) -> Option<Vec<&'a syn::Type>> {
    let syn::Type::Path(path) = ty else {
        return None;
    };
    let segment = path
        .path
        .segments
        .last()
        .filter(|part| part.ident == outer)?;
    let syn::PathArguments::AngleBracketed(arguments) = &segment.arguments else {
        return None;
    };
    arguments
        .args
        .iter()
        .map(|argument| match argument {
            syn::GenericArgument::Type(inner) => Some(inner),
            _ => None,
        })
        .collect()
}

fn is_generic_of(ty: &syn::Type, outer: &str, inner: &[&str]) -> bool {
    generic_types(ty, outer).is_some_and(|types| {
        types.len() == inner.len()
            && types
                .iter()
                .zip(inner)
                .all(|(argument, expected)| is_named_type(argument, expected))
    })
}

fn is_option_map_of(ty: &syn::Type, key: &str, value: &str) -> bool {
    generic_types(ty, "Option")
        .is_some_and(|types| matches!(types.as_slice(), [inner] if is_generic_of(inner, "BTreeMap", &[key, value])))
}

fn field_is(fields: &syn::Fields, name: &str, valid: impl Fn(&syn::Type) -> bool) -> bool {
    fields
        .iter()
        .any(|field| field.ident.as_ref().is_some_and(|id| id == name) && valid(&field.ty))
}

fn matches_shape(ty: &syn::Type, shape: FieldShape) -> bool {
    match shape {
        FieldShape::Named(name) => is_named_type(ty, name),
        FieldShape::Generic(outer, inner) => is_generic_of(ty, outer, inner),
        FieldShape::OptionMap(key, value) => is_option_map_of(ty, key, value),
    }
}

fn returns_bool(output: &syn::ReturnType) -> bool {
    match output {
        syn::ReturnType::Type(_, ty) => type_carries_bool(ty),
        syn::ReturnType::Default => false,
    }
}

fn explicit_phase_case(pat: &syn::Pat) -> bool {
    match pat {
        syn::Pat::Path(_) | syn::Pat::Struct(_) | syn::Pat::TupleStruct(_) => true,
        syn::Pat::Ident(ident) => {
            ident.subpat.is_none()
                && ["Queued", "Preparing", "Starting", "Running", "Finished"]
                    .iter()
                    .any(|name| ident.ident == name)
        }
        syn::Pat::Or(or) => !or.cases.is_empty() && or.cases.iter().all(explicit_phase_case),
        syn::Pat::Paren(paren) => explicit_phase_case(&paren.pat),
        syn::Pat::Const(_)
        | syn::Pat::Guard(_)
        | syn::Pat::Lit(_)
        | syn::Pat::Macro(_)
        | syn::Pat::Range(_)
        | syn::Pat::Reference(_)
        | syn::Pat::Rest(_)
        | syn::Pat::Slice(_)
        | syn::Pat::Tuple(_)
        | syn::Pat::Type(_)
        | syn::Pat::Verbatim(_)
        | syn::Pat::Wild(_)
        | _ => false,
    }
}

fn explicit_phase_pair(pat: &syn::Pat) -> bool {
    match pat {
        syn::Pat::Tuple(pair) => {
            pair.elems.len() == 2 && pair.elems.iter().all(explicit_phase_case)
        }
        syn::Pat::Or(or) => !or.cases.is_empty() && or.cases.iter().all(explicit_phase_pair),
        syn::Pat::Paren(paren) => explicit_phase_pair(&paren.pat),
        syn::Pat::Const(_)
        | syn::Pat::Guard(_)
        | syn::Pat::Ident(_)
        | syn::Pat::Lit(_)
        | syn::Pat::Macro(_)
        | syn::Pat::Path(_)
        | syn::Pat::Range(_)
        | syn::Pat::Reference(_)
        | syn::Pat::Rest(_)
        | syn::Pat::Slice(_)
        | syn::Pat::Struct(_)
        | syn::Pat::TupleStruct(_)
        | syn::Pat::Type(_)
        | syn::Pat::Verbatim(_)
        | syn::Pat::Wild(_)
        | _ => false,
    }
}

fn exhaustive_phase_match(block: &syn::Block) -> bool {
    let Some((last, prefix)) = block.stmts.split_last() else {
        return false;
    };
    if !prefix
        .iter()
        .all(|stmt| matches!(stmt, syn::Stmt::Item(syn::Item::Use(_))))
    {
        return false;
    }
    let syn::Stmt::Expr(syn::Expr::Match(decision), None) = last else {
        return false;
    };
    let syn::Expr::Tuple(pair) = decision.expr.as_ref() else {
        return false;
    };
    let mut elements = pair.elems.iter();
    matches!((elements.next(), elements.next(), elements.next()),
        (Some(syn::Expr::Path(from)), Some(syn::Expr::Path(to)), None)
            if from.path.is_ident("from") && to.path.is_ident("to"))
        && !decision.arms.is_empty()
        && decision
            .arms
            .iter()
            .all(|arm| explicit_phase_pair(&arm.pat))
}

impl Gate {
    fn file_is(&self, names: &[&str]) -> bool {
        let crate_file = self
            .file
            .strip_prefix("crates/domyjob/src/")
            .or_else(|| self.file.strip_prefix("src/"));
        names.iter().any(|name| {
            if name.contains('/') {
                self.file == *name
            } else {
                crate_file == Some(*name)
            }
        })
    }

    fn check_special_method_access(&mut self, call: &syn::ExprMethodCall) {
        if self.test_depth == 0
            && self.file.starts_with("crates/domyjob/src/")
            && !self.file_is(&["spawn.rs"])
            && call.method == "current_dir"
        {
            self.flag(call.method.span(), WORKING_DIRECTORY_RULE);
        }
        if self.file_is(&["node.rs"])
            && self.function.as_deref() == Some("visit_project_workspaces")
            && call.method == "parse"
        {
            self.flag(call.method.span(), WORKSPACE_SLOT_RULE);
        }
    }

    fn check_workspace_project_order(&mut self, sig: &syn::Signature, block: &syn::Block) {
        if self.file_is(&["node.rs"])
            && sig.ident == "visit_idle_workspaces"
            && !calls_path(block, &["SortedScan", "new"])
        {
            self.flag(sig.ident.span(), WORKSPACE_ORDER_RULE);
        }
    }

    fn check_version_function(&mut self, item: &syn::ItemFn) {
        if self.file_is(&["protocol.rs"])
            && (item.sig.ident == "is_newer"
                || (item.sig.ident == "version_relation"
                    && !matches!(&item.sig.output, syn::ReturnType::Type(_, ty)
                        if is_named_type(ty, "VersionRelation"))))
        {
            self.flag(item.sig.ident.span(), VERSION_DECISION_RULE);
        }
        if self.file_is(&["remote.rs"])
            && item.sig.ident == "outdated_speaker"
            && (!calls_path(&item.block, &["version_relation"])
                || !matches!(&item.sig.output, syn::ReturnType::Type(_, ty)
                    if is_generic_of(ty, "Result", &["Speaker", "RemoteError"])))
        {
            self.flag(item.sig.ident.span(), VERSION_DECISION_RULE);
        }
    }

    fn check_version_method(&mut self, item: &syn::ImplItemFn) {
        if self.file_is(&["remote.rs"])
            && item.sig.ident == "discover"
            && !calls_path(&item.block, &["outdated_speaker"])
        {
            self.flag(item.sig.ident.span(), VERSION_DECISION_RULE);
        }
    }

    fn check_authorized_effect_method(&mut self, item: &syn::ImplItemFn) {
        self.check_job_process_spawn(item);
        self.check_revocation_method(item);
        if self.file_is(&["authz.rs"])
            && item.sig.ident == "route"
            && (!matches!(item.sig.inputs.first(), Some(syn::FnArg::Receiver(receiver)) if matches!(&receiver.kind, syn::ReceiverKind::Value))
                || item.sig.inputs.len() != 1
                || !matches!(&item.sig.output, syn::ReturnType::Type(_, ty) if is_named_type(ty, "Routed")))
        {
            self.flag(item.sig.ident.span(), AUTHORIZED_COMMAND_RULE);
        }
        if self.file_is(&["node.rs"])
            && self.test_depth == 0
            && ["accept", "submit_snapshot", "accept_with_lock"]
                .iter()
                .any(|name| item.sig.ident == *name)
            && !signature_has_type(&item.sig, "AuthorizedSubmission")
        {
            self.flag(item.sig.ident.span(), AUTHORIZED_COMMAND_RULE);
        }
        if self.file_is(&["node.rs"])
            && self.test_depth == 0
            && ((["handle_query", "reply_query"]
                .iter()
                .any(|name| item.sig.ident == *name)
                && !signature_has_type(&item.sig, "Queried"))
                || (["handle_command", "reply_command"]
                    .iter()
                    .any(|name| item.sig.ident == *name)
                    && !signature_has_type(&item.sig, "Commanded")))
        {
            self.flag(item.sig.ident.span(), AUTHORIZED_COMMAND_RULE);
        }
        if self.file_is(&["node.rs"])
            && self.test_depth == 0
            && ["upkeep", "upkeep_including", "make_room"]
                .iter()
                .any(|name| item.sig.ident == *name)
            && !signature_has_authority(&item.sig)
        {
            self.flag(item.sig.ident.span(), AUTHORIZED_COMMAND_RULE);
        }
        if self.file_is(&["node.rs"])
            && self.test_depth == 0
            && let Some(proof) = match item.sig.ident.to_string().as_str() {
                "configure" => Some("AuthorizedConfigure"),
                "clean" => Some("AuthorizedClean"),
                "retry" => Some("AuthorizedRetry"),
                "kill" => Some("AuthorizedKill"),
                _ => None,
            }
            && !item.sig.inputs.iter().any(|input| {
                matches!(input, syn::FnArg::Typed(argument)
                    if matches!(argument.ty.as_ref(), syn::Type::Reference(reference)
                        if is_named_type(&reference.elem, proof)))
            })
        {
            self.flag(item.sig.ident.span(), AUTHORIZED_COMMAND_RULE);
        }
        if self.file_is(&["authz.rs"])
            && item.sig.ident == "into_submission"
            && !matches!(&item.sig.output, syn::ReturnType::Type(_, ty)
                if is_generic_of(ty, "Result", &["AuthorizedSubmission", "NotSubmission"]))
        {
            self.flag(item.sig.ident.span(), AUTHORIZED_COMMAND_RULE);
        }
        if self.file_is(&["authz.rs"])
            && item.sig.ident == "into_action"
            && !matches!(&item.sig.output, syn::ReturnType::Type(_, ty)
                if is_generic_of(ty, "Result", &["CommandAction", "NotCommandAction"]))
        {
            self.flag(item.sig.ident.span(), AUTHORIZED_COMMAND_RULE);
        }
    }

    fn check_job_process_spawn(&mut self, item: &syn::ImplItemFn) {
        if self.file_is(&["proc.rs"])
            && item.sig.ident == "spawn"
            && !matches!(item.sig.inputs.iter().collect::<Vec<_>>().as_slice(),
                [syn::FnArg::Typed(prepared), syn::FnArg::Typed(output)]
                    if is_named_type(&prepared.ty, "PreparedJobCommand")
                        && is_named_type(&output.ty, "PipeWriter"))
        {
            self.flag(item.sig.ident.span(), JOB_PROCESS_RULE);
        }
    }

    fn check_job_process_extraction(&mut self, call: &syn::ExprMethodCall) {
        if self.test_depth == 0
            && self.file.starts_with("crates/domyjob/src/")
            && !self.file_is(&["proc.rs"])
            && call.method == "into_command"
        {
            self.flag(call.method.span(), JOB_PROCESS_RULE);
        }
    }

    fn check_revocation_method(&mut self, item: &syn::ImplItemFn) {
        if self.file_is(&["trust.rs"])
            && item.sig.ident == "remove"
            && !matches!(item.sig.inputs.iter().collect::<Vec<_>>().as_slice(),
                [syn::FnArg::Typed(_dirs), syn::FnArg::Typed(selector)]
                    if matches!(selector.ty.as_ref(), syn::Type::Reference(reference)
                        if is_named_type(&reference.elem, "RevocationSelector")))
        {
            self.flag(item.sig.ident.span(), REVOCATION_RULE);
        }
    }

    fn check_revocation_construction(&mut self, expr: &syn::ExprStruct) {
        if self.test_depth == 0
            && self.file_is(&["trust.rs"])
            && path_ends_with(&expr.path, &["RevocationSelector"])
            && self.function.as_deref() != Some("parse")
        {
            self.flag(
                expr.path
                    .segments
                    .last()
                    .map_or_else(Span::call_site, |segment| segment.ident.span()),
                REVOCATION_RULE,
            );
        }
    }

    fn check_origin_struct(&mut self, item: &syn::ItemStruct) {
        if self.file_is(&["trust.rs"])
            && item.ident == "RevocationSelector"
            && !matches!(&item.fields, syn::Fields::Named(fields)
                if fields.named.len() == 2
                    && fields.named.iter().all(|field| matches!(field.vis, syn::Visibility::Inherited))
                    && field_is(&item.fields, "raw", |ty| matches!(ty, syn::Type::Reference(reference)
                        if is_named_type(&reference.elem, "str")))
                    && field_is(&item.fields, "kind", |ty| is_named_type(ty, "SelectorKind")))
        {
            self.flag(item.ident.span(), REVOCATION_RULE);
        }
        if self.file_is(&["store.rs"])
            && item.ident == "PreparedJobCommand"
            && !matches!(&item.fields, syn::Fields::Named(fields)
                if matches!(fields.named.iter().collect::<Vec<_>>().as_slice(), [field]
                    if field.ident.as_ref().is_some_and(|ident| ident == "command")
                        && is_named_type(&field.ty, "Command")
                        && matches!(field.vis, syn::Visibility::Inherited)))
        {
            self.flag(item.ident.span(), JOB_PROCESS_RULE);
        }
        if self.file_is(&["authz.rs"])
            && (item.ident == "Authorized"
                || item.ident == "RoutedRequest"
                || item.ident == "AuthorizedCommand"
                || item.ident == "Queried"
                || item.ident == "Commanded"
                || item.ident == "AuthorizedSubmission")
            && item
                .fields
                .iter()
                .any(|field| !matches!(field.vis, syn::Visibility::Inherited))
        {
            self.flag(item.ident.span(), AUTHORIZED_COMMAND_RULE);
        }
        if self.file_is(&["ingress.rs"])
            && item.ident == "PeerRequest"
            && !matches!(&item.fields, syn::Fields::Unnamed(fields)
                if fields.unnamed.len() == 1
                    && fields.unnamed.first().is_some_and(|field|
                        is_named_type(&field.ty, "Request")
                            && matches!(field.vis, syn::Visibility::Inherited)))
        {
            self.flag(item.ident.span(), PEER_INGRESS_RULE);
        }
    }

    fn check_survey_buffer(&mut self, item: &syn::ItemStruct) {
        if self.file_is(&["client.rs"])
            && item.ident == "Surveys"
            && !item.fields.iter().any(|field| {
                field.ident.as_ref().is_some_and(|name| name == "lines")
                    && is_named_type(&field.ty, "LineBuffer")
                    && matches!(field.vis, syn::Visibility::Inherited)
            })
        {
            self.flag(item.ident.span(), SURVEY_BUFFER_RULE);
        }
    }

    fn check_supervisor_event_sender(&mut self, item: &syn::ItemStruct) {
        if self.file_is(&["supervisor.rs"])
            && item.ident == "Shared"
            && !item.fields.iter().any(|field| {
                field.ident.as_ref().is_some_and(|name| name == "events")
                    && is_generic_of(&field.ty, "SyncSender", &["Event"])
                    && matches!(field.vis, syn::Visibility::Inherited)
            })
        {
            self.flag(item.ident.span(), SUPERVISOR_EVENT_RULE);
        }
    }

    fn check_editable_toml_type(&mut self, item: &syn::ItemStruct) {
        if self.file_is(&["ingress.rs"])
            && item.ident == "EditableToml"
            && !matches!(&item.fields, syn::Fields::Unnamed(fields)
                if fields.unnamed.len() == 1
                    && fields.unnamed.first().is_some_and(|field|
                        matches!(&field.ty, syn::Type::Path(path)
                            if path_ends_with(&path.path, &["toml_edit", "DocumentMut"])
                                && matches!(field.vis, syn::Visibility::Inherited))))
        {
            self.flag(item.ident.span(), EDITABLE_TOML_TYPE_RULE);
        }
    }

    fn reviewed_variant_import(&self, path: &[String]) -> bool {
        let parent = path.iter().rev().nth(1).map(String::as_str);
        (self.file_is(&["store.rs"])
            && self.function.as_deref() == Some("phase_transition")
            && parent.is_some_and(|name| ["Phase", "PhaseTransition"].contains(&name)))
            || (self.file_is(&["authz.rs"])
                && self.function.as_deref() == Some("nature")
                && parent.is_some_and(|name| {
                    ["Access", "Audit", "Capability", "Effect"].contains(&name)
                }))
    }

    fn check_import(&mut self, path: &[String], span: Span, aliased: bool) {
        if self.test_depth > 0 {
            return;
        }
        if self.file.starts_with("crates/domyjob/src/") {
            let renamed_enum = aliased
                && path
                    .last()
                    .is_some_and(|name| self.state_enums.contains(name));
            let imported_variant = path
                .iter()
                .rev()
                .nth(1)
                .is_some_and(|name| self.state_enums.contains(name))
                && !self.reviewed_variant_import(path);
            if renamed_enum || imported_variant {
                self.flag(span, STATE_IMPORT_RULE);
            }
        }
        for restriction in RESTRICTIONS {
            if import_ends_with(path, restriction.path) && !self.file_is(restriction.allowed_in) {
                self.flag(span, restriction.rule);
            }
        }
        if !self.file_is(&["ingress.rs"])
            && [
                &["serde_json", "Deserializer"][..],
                &["serde_json", "de", "Deserializer"],
                &["serde_json", "de"],
                &["serde_json", "value"],
                &["toml", "de"],
                &["toml_edit", "DocumentMut"],
            ]
            .iter()
            .any(|tail| import_ends_with(path, tail))
        {
            self.flag(span, PROTECTED_IMPORT_RULE);
        }
        if self.file.starts_with("crates/domyjob/src/") && !self.file_is(&["bounded.rs"]) {
            if import_ends_with(path, &["fs", "read"]) {
                self.flag(span, UNBOUNDED_FILE_RULE);
            }
            if import_ends_with(path, &["fs", "read_to_string"]) {
                self.flag(span, UNBOUNDED_TEXT_RULE);
            }
        }
        if self.file.starts_with("crates/domyjob/src/")
            && import_ends_with(path, &["fs", "read_dir"])
        {
            self.flag(span, DIRECTORY_OWNER_RULE);
        }
        if self.file_is(&["supervisor.rs"])
            && (import_ends_with(path, &["OsLock", "probe"])
                || (aliased && import_ends_with(path, &["OsLock"])))
        {
            self.flag(span, SLOT_SCAN_RULE);
        }
        if self.file_is(&["cli.rs"])
            && (import_ends_with(path, &["thread", "scope"])
                || import_ends_with(path, &["thread", "spawn"])
                || import_ends_with(path, &["thread", "Builder"])
                || import_ends_with(path, &["thread", "Scope"]))
        {
            self.flag(span, CLI_FANOUT_RULE);
        }
        if aliased
            && [
                &["serde_json"][..],
                &["toml"],
                &["toml_edit"],
                &["std", "fs"],
                &["std", "time"],
                &["std", "thread"],
                &["crate", "state_file"],
            ]
            .iter()
            .any(|tail| import_ends_with(path, tail))
        {
            self.flag(span, PROTECTED_IMPORT_RULE);
        }
    }

    fn check_import_tree(&mut self, tree: &syn::UseTree, path: &mut Vec<String>) {
        match tree {
            syn::UseTree::Path(branch) => {
                path.push(branch.ident.to_string());
                self.check_import_tree(&branch.tree, path);
                path.pop();
            }
            syn::UseTree::Name(name) => {
                if name.ident != "self" {
                    path.push(name.ident.to_string());
                }
                self.check_import(path, name.ident.span(), false);
                if name.ident != "self" {
                    path.pop();
                }
            }
            syn::UseTree::Rename(rename) => {
                if rename.ident != "self" {
                    path.push(rename.ident.to_string());
                }
                self.check_import(path, rename.rename.span(), true);
                if rename.ident != "self" {
                    path.pop();
                }
            }
            syn::UseTree::Glob(glob) => {
                if self.test_depth == 0 {
                    self.flag(glob.star_token.span, IMPORT_GLOB_RULE);
                }
            }
            syn::UseTree::Group(group) => {
                for child in &group.items {
                    self.check_import_tree(child, path);
                }
            }
        }
    }

    fn check_time_path(&mut self, path: &syn::Path) {
        if self.test_depth == 0
            && self.file_is(&["protocol.rs"])
            && path
                .segments
                .last()
                .is_some_and(|segment| segment.ident == "observe")
            && let Some(segment) = path.segments.last()
        {
            self.flag(segment.ident.span(), EXPLICIT_PRESENTATION_TIME_RULE);
        }
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

    fn check_state_file_path(&mut self, path: &syn::Path) {
        if self.file.starts_with("crates/domyjob/src/")
            && !self.file_is(&["state_file.rs"])
            && path_ends_with(path, &["StateFile", "at"])
            && !STATE_CONSTRUCTORS.iter().any(|owner| {
                self.file_is(&[owner.file]) && self.function.as_deref() == Some(owner.function)
            })
        {
            self.flag(
                path.segments
                    .last()
                    .map_or_else(Span::call_site, |segment| segment.ident.span()),
                STATE_CONSTRUCTOR_RULE,
            );
        }
    }

    fn check_slot_path(&mut self, path: &syn::Path) {
        if self.file_is(&["supervisor.rs"]) && path_ends_with(path, &["OsLock", "probe"]) {
            self.flag(
                path.segments
                    .last()
                    .map_or_else(Span::call_site, |segment| segment.ident.span()),
                SLOT_SCAN_RULE,
            );
        }
    }

    fn check_cli_fanout_path(&mut self, path: &syn::Path) {
        if self.file_is(&["cli.rs"])
            && self.function.as_deref() != Some("live")
            && self.function.as_deref() != Some("run_watches")
            && (path_ends_with(path, &["thread", "scope"])
                || path_ends_with(path, &["thread", "spawn"])
                || path_ends_with(path, &["thread", "Builder", "spawn"])
                || path_ends_with(path, &["thread", "Scope", "spawn"]))
            && let Some(segment) = path.segments.last()
        {
            self.flag(segment.ident.span(), CLI_FANOUT_RULE);
        }
    }

    fn check_cli_fanout_method(&mut self, call: &syn::ExprMethodCall) {
        if self.file_is(&["cli.rs"])
            && self.function.as_deref() != Some("live")
            && self.function.as_deref() != Some("run_watches")
            && call.method == "spawn"
        {
            self.flag(call.method.span(), CLI_FANOUT_RULE);
        }
    }

    fn check_directory_path(&mut self, path: &syn::Path) {
        if self.file.starts_with("crates/domyjob/src/")
            && path_ends_with(path, &["fs", "read_dir"])
            && !DIRECTORY_READ_OWNERS.iter().any(|(file, function)| {
                self.file_is(&[file]) && self.function.as_deref() == Some(*function)
            })
        {
            self.flag(
                path.segments
                    .last()
                    .map_or_else(Span::call_site, |segment| segment.ident.span()),
                DIRECTORY_OWNER_RULE,
            );
        }
    }

    fn check_bounded_path(&mut self, path: &syn::Path) {
        if self.file.starts_with("crates/domyjob/src/")
            && !self.file_is(&["bounded.rs"])
            && let Some(segment) = path
                .segments
                .last()
                .filter(|segment| segment.ident == "read_to_string")
        {
            self.flag(segment.ident.span(), UNBOUNDED_TEXT_RULE);
        }
        if self.file.starts_with("crates/domyjob/src/")
            && !self.file_is(&["bounded.rs"])
            && path_ends_with(path, &["fs", "read"])
            && let Some(segment) = path.segments.last()
        {
            self.flag(segment.ident.span(), UNBOUNDED_FILE_RULE);
        }
        if !self.file_is(&["bounded.rs"])
            && let Some(segment) = path.segments.last().filter(|segment| {
                segment.ident == "read_to_end"
                    || segment.ident == "read_until"
                    || segment.ident == "fill_buf"
            })
        {
            self.flag(segment.ident.span(), UNBOUNDED_READ_RULE);
        }
        if self.file.starts_with("crates/domyjob/src/")
            && !self.file_is(&["bounded.rs"])
            && (path_ends_with(path, &["Command", "output"])
                || path_ends_with(path, &["Child", "wait_with_output"]))
            && let Some(segment) = path.segments.last()
        {
            self.flag(segment.ident.span(), UNBOUNDED_CHILD_OUTPUT_RULE);
        }
        if !self.file_is(&["bounded.rs"])
            && let Some(segment) = path
                .segments
                .last()
                .filter(|segment| segment.ident == "read_line")
            && path
                .segments
                .iter()
                .rev()
                .nth(1)
                .is_some_and(|prior| prior.ident == "BufRead")
        {
            self.flag(segment.ident.span(), UNBOUNDED_READ_RULE);
        }
    }

    fn check_path(&mut self, path: &syn::Path) {
        self.check_time_path(path);
        if self.test_depth > 0 {
            return;
        }
        self.check_slot_path(path);
        self.check_cli_fanout_path(path);
        self.check_directory_path(path);
        self.check_checkout_path(path);
        self.check_mcp_path(path);
        self.check_snapshot_path(path);
        self.check_queue_path(path);
        self.check_locked_state_path(path);
        self.check_state_file_path(path);
        self.check_bounded_path(path);
        if let Some(segment) = path
            .segments
            .last()
            .filter(|segment| segment.ident == "create_new")
            && !self.exclusive_create_allowed()
        {
            self.flag(segment.ident.span(), EXCLUSIVE_CREATE_RULE);
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

    fn check_mcp_path(&mut self, path: &syn::Path) {
        if self.file_is(&["mcp.rs"])
            && (path_ends_with(path, &["BufRead", "lines"])
                || path_ends_with(path, &["thread", "spawn"]))
            && let Some(segment) = path.segments.last()
        {
            let rule = if segment.ident == "lines" {
                MCP_LINE_RULE
            } else {
                MCP_WORKER_RULE
            };
            self.flag(segment.ident.span(), rule);
        }
    }

    fn check_checkout_path(&mut self, path: &syn::Path) {
        if self.file_is(&["remote.rs"])
            && self.function.as_deref() != Some("local_source")
            && path_ends_with(path, &["InsecureUnsigned", "from_local_checkout"])
        {
            self.flag(
                path.segments
                    .last()
                    .map_or_else(Span::call_site, |segment| segment.ident.span()),
                "automatic source installation requires the matching local checkout",
            );
        }
    }

    fn check_snapshot_path(&mut self, path: &syn::Path) {
        if self.file_is(&["snapshot.rs"])
            && path_ends_with(path, &["thread", "available_parallelism"])
            && let Some(segment) = path.segments.last()
        {
            self.flag(segment.ident.span(), SNAPSHOT_BUDGET_RULE);
        }
    }

    fn check_queue_path(&mut self, path: &syn::Path) {
        if self.file_is(&["supervisor.rs"])
            && self.function.as_deref() == Some("queue")
            && path_ends_with(path, &["thread", "spawn"])
            && let Some(segment) = path.segments.last()
        {
            self.flag(segment.ident.span(), QUEUE_WATCH_RULE);
        }
    }

    fn check_locked_state_path(&mut self, path: &syn::Path) {
        if self.file_is(&["dist.rs"])
            && let Some(segment) = path
                .segments
                .last()
                .filter(|segment| segment.ident == "read_json" || segment.ident == "write_json")
        {
            self.flag(segment.ident.span(), RELEASE_STATE_RULE);
        }
        if self.file_is(&["client.rs"])
            && self.function.as_deref() == Some("origin")
            && let Some(segment) = path
                .segments
                .last()
                .filter(|segment| segment.ident == "read_bytes" || segment.ident == "write_bytes")
        {
            self.flag(segment.ident.span(), ORIGIN_STATE_RULE);
        }
    }

    fn check_signature(&mut self, sig: &syn::Signature, attrs: &[syn::Attribute]) {
        let legacy = if self.file_is(DECISION_FILES) {
            None
        } else {
            LEGACY_BOOL_SIGNATURES
                .iter()
                .enumerate()
                .find_map(|(index, (file, name, owners))| {
                    (self.file_is(&[file])
                        && sig.ident == *name
                        && match self.current_impl.as_deref() {
                            Some(owner) => owners.contains(&owner),
                            None => owners.is_empty(),
                        })
                    .then_some(index)
                })
        };
        if self.test_depth == 0
            && !is_test_module(attrs)
            && (self.file_is(DECISION_FILES) || self.file.starts_with("crates/domyjob/src/"))
            && returns_bool(&sig.output)
        {
            if let Some(index) = legacy {
                let Some(hit) = self.legacy_bool_hits.get_mut(index) else {
                    self.flag(sig.ident.span(), LEGACY_BOOL_BASELINE_RULE);
                    return;
                };
                *hit = hit.saturating_add(1);
            } else {
                self.flag(sig.ident.span(), DECISION_SIGNATURE_RULE);
            }
        }
    }

    fn check_state_constructor(&mut self, sig: &syn::Signature) {
        if self.test_depth > 0 {
            return;
        }
        let Some(owner) = STATE_CONSTRUCTORS
            .iter()
            .find(|owner| self.file_is(&[owner.file]) && sig.ident == owner.function)
        else {
            return;
        };
        let valid = matches!(&sig.output, syn::ReturnType::Type(_, ty)
            if generic_types(ty, "StateFile")
                .is_some_and(|types| matches!(types.as_slice(), [value] if matches_shape(value, owner.value))));
        if !valid {
            self.flag(sig.ident.span(), STATE_CONSTRUCTOR_RULE);
        }
    }

    fn check_project_structure(&mut self, item: &syn::ItemStruct) {
        if self.file_is(&["lock.rs"])
            && item.ident == "SlotIndex"
            && item
                .fields
                .iter()
                .any(|field| !matches!(field.vis, syn::Visibility::Inherited))
        {
            self.flag(item.ident.span(), WORKSPACE_SLOT_RULE);
        }
        for required in REQUIRED_FIELDS {
            if self.file_is(&[required.file]) && item.ident == required.owner {
                let valid = field_is(&item.fields, required.field, |ty| {
                    matches_shape(ty, required.shape)
                });
                if !valid {
                    self.flag(item.ident.span(), required.rule);
                }
            }
        }
        let proof_fields = (self.file_is(&["project.rs"])
            && [
                "RepositoryRequest",
                "ProjectSelector",
                "ApprovedProjectTargets",
                "ApprovedProjectWord",
                "ApprovedProjectJob",
            ]
            .iter()
            .any(|name| item.ident == name))
            || (self.file_is(&["config.rs"]) && item.ident == "LocalPolicy")
            || (self.file_is(&["client.rs"]) && item.ident == "Targets");
        if proof_fields
            && item
                .fields
                .iter()
                .any(|field| !matches!(field.vis, syn::Visibility::Inherited))
        {
            self.flag(item.ident.span(), PROJECT_PROOF_PRIVACY_RULE);
        }
    }

    fn check_opaque_time_impl(&mut self, item: &syn::ItemImpl) {
        if !is_named_type(&item.self_ty, "Timestamp") && !is_named_type(&item.self_ty, "Elapsed") {
            return;
        }
        if !self.file_is(CLOCK_FILES) {
            self.flag(item.impl_token.span, OPAQUE_TIME_RULE);
            return;
        }
        if let Some((trait_path, _)) = &item.trait_
            && trait_path.segments.last().is_some_and(|segment| {
                let name = segment.ident.to_string();
                ["PartialEq", "Eq", "PartialOrd", "Ord", "Deref", "AsRef"].contains(&name.as_str())
            })
        {
            self.flag(item.impl_token.span, OPAQUE_TIME_RULE);
        }
        for member in &item.items {
            if let syn::ImplItem::Fn(method) = member
                && matches!(method.vis, syn::Visibility::Public(_))
                && matches!(&method.sig.output, syn::ReturnType::Type(_, ty)
                    if type_carries_numeric(ty))
            {
                self.flag(method.sig.ident.span(), OPAQUE_TIME_RULE);
            }
        }
    }

    fn check_opaque_time_struct(&mut self, item: &syn::ItemStruct) {
        if !self.file_is(CLOCK_FILES) || (item.ident != "Timestamp" && item.ident != "Elapsed") {
            return;
        }
        let private_i64 = matches!(&item.fields, syn::Fields::Unnamed(fields)
            if fields.unnamed.len() == 1
                && fields.unnamed.iter().all(|field|
                    matches!(field.vis, syn::Visibility::Inherited)
                        && is_named_type(&field.ty, "i64")));
        let comparable = ["PartialEq", "Eq", "PartialOrd", "Ord"]
            .iter()
            .any(|trait_name| derives(&item.attrs, trait_name))
            || conditionally_derives_time_comparison(&item.attrs);
        if !private_i64 || comparable {
            self.flag(item.ident.span(), OPAQUE_TIME_RULE);
        }
    }

    fn check_io_failure_struct(&mut self, item: &syn::ItemStruct) {
        if carries_io_source(&item.fields) && !self.file_is(FAILURE_FILES) {
            self.flag(
                item.ident.span(),
                "an I/O failure is failure::IoFailure, with the action and path it happened at",
            );
        }
    }

    fn exclusive_create_allowed(&self) -> bool {
        EXCLUSIVE_CREATE.iter().any(|(file, function)| {
            self.file == *file && self.function.as_deref() == Some(*function)
        })
    }
}

impl<'ast> Visit<'ast> for Gate {
    fn visit_expr_match(&mut self, expr: &'ast syn::ExprMatch) {
        if self.test_depth == 0
            && self.file.starts_with("crates/domyjob/src/")
            && expr.arms.iter().any(|explicit| {
                expr.arms.iter().any(|fallback| {
                    !forwards_original_error(fallback)
                        && !forwards_nested_error(
                            &explicit.pat,
                            fallback,
                            &self.state_enums,
                            self.state_impl,
                        )
                        && enum_variant_absorbed(
                            &explicit.pat,
                            &fallback.pat,
                            &self.state_enums,
                            self.state_impl,
                        )
                })
            })
        {
            self.flag(expr.match_token.span, STATE_FALLBACK_RULE);
        }
        syn::visit::visit_expr_match(self, expr);
    }

    fn visit_expr_let(&mut self, expr: &'ast syn::ExprLet) {
        if self.test_depth == 0
            && self.file.starts_with("crates/domyjob/src/")
            && (pattern_mentions_enum(&expr.pat, &self.state_enums, self.state_impl)
                || (self.file_is(&["authz.rs"]) && self.function.as_deref() == Some("nature")))
        {
            self.flag(expr.let_token.span, STATE_PATTERN_RULE);
        }
        syn::visit::visit_expr_let(self, expr);
    }

    fn visit_local(&mut self, local: &'ast syn::Local) {
        if self.test_depth == 0
            && self.file.starts_with("crates/domyjob/src/")
            && local
                .init
                .as_ref()
                .is_some_and(|init| init.diverge.is_some())
            && (pattern_mentions_enum(&local.pat, &self.state_enums, self.state_impl)
                || (self.file_is(&["authz.rs"]) && self.function.as_deref() == Some("nature")))
        {
            self.flag(local.let_token.span, STATE_PATTERN_RULE);
        }
        syn::visit::visit_local(self, local);
    }

    fn visit_lit_str(&mut self, literal: &'ast syn::LitStr) {
        if self.test_depth == 0 && self.file.starts_with("crates/domyjob/src/") {
            let value = literal.value();
            if self.file_is(&["platform.rs"]) && value == "icacls" {
                self.flag(literal.span(), WINDOWS_PRIVATE_DIR_RULE);
            }
            if ["domyjob.incoming", ".incoming"]
                .into_iter()
                .any(|stem| value.split(stem).skip(1).any(|tail| !tail.starts_with('-')))
            {
                self.flag(literal.span(), INCOMING_RULE);
            }
        }
        syn::visit::visit_lit_str(self, literal);
    }

    fn visit_macro(&mut self, mac: &'ast syn::Macro) {
        if self.test_depth == 0
            && self.file.starts_with("crates/domyjob/src/")
            && path_ends_with(&mac.path, &["matches"])
            && (mentions_any_identifier(
                &mac.tokens,
                &self.state_enums,
                self.state_impl || self.file_is(&["paths.rs"]),
            ) || (self.file_is(&["authz.rs"]) && self.function.as_deref() == Some("nature")))
            && let Some(last) = mac.path.segments.last()
        {
            self.flag(last.ident.span(), STATE_CLASSIFICATION_RULE);
        }
        if self.test_depth == 0
            && self.file_is(&["store.rs"])
            && self.function.as_deref() == Some("set_phase")
            && let Some(last) = mac.path.segments.last()
            && last.ident == "matches"
        {
            self.flag(last.ident.span(), PHASE_TRANSITION_RULE);
        }
        if self.test_depth == 0
            && !self.file_is(JSON_FILES)
            && let Some(last) = mac.path.segments.last()
            && last.ident == "json"
        {
            self.flag(last.ident.span(), JSON_RULE);
        }
        syn::visit::visit_macro(self, mac);
    }

    fn visit_item_macro(&mut self, item: &'ast syn::ItemMacro) {
        if self.test_depth == 0 && self.file.starts_with("crates/domyjob/src/") {
            let path = &item.mac.path;
            let reviewed = if path.is_ident("macro_rules") {
                !contains_enum_keyword(&item.mac.tokens)
            } else {
                (path_ends_with(path, &["text_newtype"]) && self.file_is(&["domain.rs"]))
                    || (path_ends_with(path, &["per_os"]) && self.file_is(&["config.rs"]))
                    || (path_ends_with(path, &["bounded_u32"]) && self.file_is(&["mcp.rs"]))
                    || (path_ends_with(path, &["approved_word"]) && self.file_is(&["template.rs"]))
                    || (path_ends_with(path, &["thread_local"])
                        && self.file_is(&["liveness.rs", "pq.rs"]))
            };
            if !reviewed {
                self.flag(item.mac.bang_token.span, STATE_MACRO_RULE);
            }
        }
        syn::visit::visit_item_macro(self, item);
    }

    fn visit_item_mod(&mut self, item: &'ast syn::ItemMod) {
        if self.file_is(&["template.rs"])
            && item.ident == "approved_path"
            && !matches!(item.vis, syn::Visibility::Inherited)
        {
            self.flag(item.ident.span(), SAFE_PATH_RULE);
        }
        if self.file_is(&["authz.rs"])
            && item.ident == "command_authority"
            && !matches!(item.vis, syn::Visibility::Inherited)
        {
            self.flag(item.ident.span(), AUTHORIZED_COMMAND_RULE);
        }
        let test = is_test_module(&item.attrs);
        if test {
            self.test_depth = self.test_depth.saturating_add(1);
        }
        syn::visit::visit_item_mod(self, item);
        if test {
            self.test_depth = self.test_depth.saturating_sub(1);
        }
    }

    fn visit_item_use(&mut self, item: &'ast syn::ItemUse) {
        self.check_import_tree(&item.tree, &mut Vec::new());
        syn::visit::visit_item_use(self, item);
    }

    fn visit_item_extern_crate(&mut self, item: &'ast syn::ItemExternCrate) {
        if self.test_depth == 0
            && item.rename.is_some()
            && (item.ident == "serde_json" || item.ident == "toml" || item.ident == "toml_edit")
        {
            self.flag(item.ident.span(), PROTECTED_IMPORT_RULE);
        }
        syn::visit::visit_item_extern_crate(self, item);
    }

    fn visit_path(&mut self, path: &'ast syn::Path) {
        if path_ends_with(path, &["fail", "FailScenario", "setup"]) && !self.file_is(&["faults.rs"])
        {
            self.flag(
                path.segments
                    .last()
                    .map_or_else(Span::call_site, |segment| segment.ident.span()),
                FAULT_SCENARIO_RULE,
            );
        }
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
        self.check_special_method_access(call);
        self.check_job_process_extraction(call);
        if self.test_depth == 0 {
            self.check_cli_fanout_method(call);
        }
        if self.test_depth == 0
            && self.file.starts_with("crates/domyjob/src/")
            && !self.file_is(&["cli.rs"])
            && method == "with_supervisor_paths"
        {
            self.flag(call.method.span(), SUPERVISOR_PATH_RULE);
        }
        if self.test_depth == 0
            && !self.file_is(&["ingress.rs"])
            && method == "parse"
            && call.turbofish.as_ref().is_some_and(|arguments| {
                arguments.args.iter().any(|argument| {
                    matches!(argument, syn::GenericArgument::Type(ty) if is_named_type(ty, "DocumentMut"))
                })
            })
        {
            self.flag(call.method.span(), EDITABLE_TOML_RULE);
        }
        if self.test_depth == 0 && self.file_is(&["snapshot.rs"]) && method == "update_mmap" {
            self.flag(call.method.span(), SNAPSHOT_BUDGET_RULE);
        }
        if self.test_depth == 0
            && self.file_is(&["supervisor.rs"])
            && self.function.as_deref() == Some("queue")
            && method == "spawn"
        {
            self.flag(call.method.span(), QUEUE_WATCH_RULE);
        }
        if self.test_depth == 0 && self.file_is(&["mcp.rs"]) {
            if method == "lines" {
                self.flag(call.method.span(), MCP_LINE_RULE);
            }
            if method == "spawn" && self.function.as_deref() != Some("dispatch") {
                self.flag(call.method.span(), MCP_WORKER_RULE);
            }
        }
        if method == "create_new" && self.test_depth == 0 && !self.exclusive_create_allowed() {
            self.flag(call.method.span(), EXCLUSIVE_CREATE_RULE);
        }
        if self.test_depth == 0
            && !self.file_is(&["bounded.rs"])
            && (method == "read_to_end"
                || method == "read_line"
                || method == "read_until"
                || method == "fill_buf")
        {
            self.flag(call.method.span(), UNBOUNDED_READ_RULE);
        }
        if self.test_depth == 0
            && self.file.starts_with("crates/domyjob/src/")
            && !self.file_is(&["bounded.rs"])
            && method == "read_to_string"
        {
            self.flag(call.method.span(), UNBOUNDED_TEXT_RULE);
        }
        if self.test_depth == 0
            && self.file.starts_with("crates/domyjob/src/")
            && !self.file_is(&["bounded.rs"])
            && (method == "output" || method == "wait_with_output")
        {
            self.flag(call.method.span(), UNBOUNDED_CHILD_OUTPUT_RULE);
        }
        if method == "as_raw_str" && self.file_is(TERMINAL_FILES) {
            self.flag(
                call.method.span(),
                "remote text reaches a terminal only through its Display, which neutralizes control characters",
            );
        }
        let exempt = self.file_is(CLOCK_FILES)
            || (self.file_is(LIVENESS_FILES) && LIVENESS_TIME.contains(&method.as_str()));
        if !exempt && is_time_method(&method) {
            self.flag(call.method.span(), TIME_RULE);
        }
        syn::visit::visit_expr_method_call(self, call);
    }

    fn visit_item_const(&mut self, item: &'ast syn::ItemConst) {
        if self.file_is(&["cli.rs"])
            && item.ident == "MAX_WATCHES"
            && !matches!(item.expr.as_ref(), syn::Expr::Lit(value)
                if matches!(&value.lit, syn::Lit::Int(number)
                    if matches!(number.base10_parse::<usize>(), Ok(64))))
        {
            self.flag(item.ident.span(), CLI_FANOUT_RULE);
        }
        syn::visit::visit_item_const(self, item);
    }

    fn visit_item_fn(&mut self, item: &'ast syn::ItemFn) {
        self.check_signature(&item.sig, &item.attrs);
        self.check_state_constructor(&item.sig);
        self.check_version_function(item);
        if self.file_is(&["store.rs"])
            && item.sig.ident == "phase_transition"
            && (!matches!(&item.sig.output, syn::ReturnType::Type(_, ty)
                if is_named_type(ty, "PhaseTransition"))
                || !exhaustive_phase_match(&item.block))
        {
            self.flag(item.sig.ident.span(), PHASE_TRANSITION_RULE);
        }
        if self.file_is(&["node.rs"])
            && item.sig.ident == "visit_project_workspaces"
            && !calls_path(&item.block, &["SlotIndex", "all"])
        {
            self.flag(item.sig.ident.span(), WORKSPACE_SLOT_RULE);
        }
        if self.file_is(&["cli.rs"])
            && item.sig.ident == "run_watches"
            && !signature_has_watch_batch(&item.sig)
        {
            self.flag(item.sig.ident.span(), CLI_FANOUT_RULE);
        }
        if self.file_is(&["authz.rs"])
            && item.sig.ident == "authorize"
            && !signature_has_type(&item.sig, "PeerRequest")
        {
            self.flag(item.sig.ident.span(), PEER_INGRESS_RULE);
        }
        if self.file_is(&["ingress.rs"])
            && item.sig.ident == "editable_toml"
            && !matches!(&item.sig.output, syn::ReturnType::Type(_, ty)
                if is_generic_of(ty, "Result", &["EditableToml", "TomlError"]))
        {
            self.flag(item.sig.ident.span(), EDITABLE_TOML_TYPE_RULE);
        }
        if self.file_is(&["mcp.rs"])
            && item.sig.ident == "dispatch"
            && !item.sig.inputs.iter().any(|argument| {
                matches!(argument, syn::FnArg::Typed(typed)
                    if is_named_type(&typed.ty, "McpDispatch"))
            })
        {
            self.flag(item.sig.ident.span(), MCP_WORKER_RULE);
        }
        let before = self.function.replace(item.sig.ident.to_string());
        syn::visit::visit_item_fn(self, item);
        self.function = before;
    }

    fn visit_impl_item_fn(&mut self, item: &'ast syn::ImplItemFn) {
        self.check_signature(&item.sig, &item.attrs);
        self.check_state_constructor(&item.sig);
        self.check_workspace_project_order(&item.sig, &item.block);
        self.check_version_method(item);
        if self.file_is(&["store.rs"])
            && item.sig.ident == "set_phase"
            && !calls_path(&item.block, &["phase_transition"])
        {
            self.flag(item.sig.ident.span(), PHASE_TRANSITION_RULE);
        }
        self.check_authorized_effect_method(item);
        if self.file_is(&["template.rs"])
            && item.sig.ident == "path"
            && !has_single_safe_path_argument(&item.sig)
        {
            self.flag(item.sig.ident.span(), SAFE_PATH_RULE);
        }
        if self.file_is(&["spawn.rs"])
            && item.sig.ident == "in_dir"
            && !has_single_safe_path_argument(&item.sig)
        {
            self.flag(item.sig.ident.span(), WORKING_DIRECTORY_RULE);
        }
        if self.file_is(&["provenance.rs"]) && item.sig.ident == "after_project_approval" {
            let proof = item.sig.inputs.iter().any(|input| {
                matches!(input, syn::FnArg::Typed(argument)
                    if matches!(argument.ty.as_ref(), syn::Type::Reference(reference)
                        if is_named_type(&reference.elem, "ApprovedProjectTargets")))
            });
            let approved = matches!(&item.sig.output, syn::ReturnType::Type(_, ty)
                if is_generic_of(ty, "Labeled", &["String", "ApprovedRepository"]));
            if !proof || !approved {
                self.flag(item.sig.ident.span(), REPOSITORY_PROVENANCE_RULE);
            }
        }
        if self.file_is(&["store.rs"])
            && (item.sig.ident == "ids" || item.sig.ident == "staged_ids")
            && !is_test_module(&item.attrs)
        {
            self.flag(item.sig.ident.span(), JOB_IDS_RULE);
        }
        if self.file_is(&["client.rs"]) && item.sig.ident == "from_project" {
            let inputs: Vec<_> = item
                .sig
                .inputs
                .iter()
                .filter_map(|argument| match argument {
                    syn::FnArg::Typed(typed) => Some(typed.ty.as_ref()),
                    syn::FnArg::Receiver(_) => None,
                })
                .collect();
            let approved = matches!(inputs.as_slice(), [job, name, revision]
                if is_named_type(job, "ApprovedProjectJob")
                    && is_named_type(name, "JobName")
                    && is_generic_of(revision, "Option", &["Revision"]));
            if !approved {
                self.flag(item.sig.ident.span(), PROJECT_APPROVAL_RULE);
            }
        }
        if self.file_is(&["client.rs"])
            && item.sig.ident == "project"
            && !matches!(item.vis, syn::Visibility::Inherited)
        {
            self.flag(item.sig.ident.span(), PROJECT_PROOF_PRIVACY_RULE);
        }
        let before = self.function.replace(item.sig.ident.to_string());
        syn::visit::visit_impl_item_fn(self, item);
        self.function = before;
    }

    fn visit_item_trait(&mut self, item: &'ast syn::ItemTrait) {
        let test = is_test_module(&item.attrs);
        if test {
            self.test_depth = self.test_depth.saturating_add(1);
        }
        if self.file_is(&["template.rs"])
            && item.ident == "SafePath"
            && !item.supertraits.iter().any(|bound| {
                matches!(bound, syn::TypeParamBound::Trait(trait_bound)
                    if path_ends_with(&trait_bound.path, &["approved_path", "Sealed"]))
            })
        {
            self.flag(item.ident.span(), SAFE_PATH_RULE);
        }
        if self.file_is(&["authz.rs"])
            && item.ident == "CommandAuthority"
            && !item.supertraits.iter().any(|bound| {
                matches!(bound, syn::TypeParamBound::Trait(trait_bound)
                    if path_ends_with(&trait_bound.path, &["command_authority", "Sealed"]))
            })
        {
            self.flag(item.ident.span(), AUTHORIZED_COMMAND_RULE);
        }
        syn::visit::visit_item_trait(self, item);
        if test {
            self.test_depth = self.test_depth.saturating_sub(1);
        }
    }

    fn visit_trait_item_fn(&mut self, item: &'ast syn::TraitItemFn) {
        self.check_signature(&item.sig, &item.attrs);
        syn::visit::visit_trait_item_fn(self, item);
    }

    fn visit_item_impl(&mut self, item: &'ast syn::ItemImpl) {
        let test = is_test_module(&item.attrs);
        if test {
            self.test_depth = self.test_depth.saturating_add(1);
        }
        self.check_opaque_time_impl(item);
        if self.test_depth == 0
            && self.file.starts_with("crates/domyjob/src/")
            && let Some((trait_path, _)) = &item.trait_
            && path_ends_with(trait_path, &["Ingress"])
        {
            let named = matches!(item.self_ty.as_ref(), syn::Type::Path(ty)
                if ty.qself.is_none()
                    && ty.path.segments.len() == 1
                    && ty.path.segments.first().is_some_and(|segment|
                        matches!(segment.arguments, syn::PathArguments::None)
                            && self.defined_types.contains(&segment.ident.to_string())));
            if !named || !item.generics.params.is_empty() {
                self.flag(item.impl_token.span, EXACT_INGRESS_RULE);
            }
        }
        if self.file_is(&["protocol.rs"])
            && let Some((trait_path, _)) = &item.trait_
            && path_ends_with(trait_path, &["Ingress"])
            && (is_named_type(&item.self_ty, "Request")
                || is_named_type(&item.self_ty, "Submission"))
        {
            self.flag(item.impl_token.span, PEER_INGRESS_RULE);
        }
        if self.file_is(&["template.rs"])
            && let Some((trait_path, _)) = &item.trait_
            && (path_ends_with(trait_path, &["SafePath"])
                || path_ends_with(trait_path, &["approved_path", "Sealed"]))
            && !SAFE_PATH_TYPES
                .iter()
                .any(|name| is_named_type(&item.self_ty, name))
        {
            self.flag(item.impl_token.span, SAFE_PATH_RULE);
        }
        if self.file_is(&["authz.rs"])
            && let Some((trait_path, _)) = &item.trait_
            && (path_ends_with(trait_path, &["CommandAuthority"])
                || path_ends_with(trait_path, &["command_authority", "Sealed"]))
            && !is_generic_of(&item.self_ty, "RoutedRequest", &["CommandEffect"])
            && !is_generic_of(&item.self_ty, "AuthorizedCommand", &["K", "P"])
            && !is_named_type(&item.self_ty, "AuthorizedSubmission")
        {
            self.flag(item.impl_token.span, AUTHORIZED_COMMAND_RULE);
        }
        if self.file_is(&["authz.rs"])
            && item.items.iter().any(|member| {
                matches!(member, syn::ImplItem::Fn(method) if method.sig.ident == "into_parts")
            })
            && !is_generic_of(&item.self_ty, "RoutedRequest", &["QueryEffect"])
            && !is_named_type(&item.self_ty, "AuthorizedSubmission")
        {
            self.flag(item.impl_token.span, AUTHORIZED_COMMAND_RULE);
        }
        let previous_state_impl = self.state_impl;
        let owner = if let syn::Type::Path(ty) = item.self_ty.as_ref() {
            ty.path.segments.last().map(|part| part.ident.to_string())
        } else {
            None
        };
        let previous_impl = std::mem::replace(&mut self.current_impl, owner);
        self.state_impl = self
            .state_enums
            .iter()
            .any(|name| is_named_type(&item.self_ty, name));
        syn::visit::visit_item_impl(self, item);
        self.state_impl = previous_state_impl;
        self.current_impl = previous_impl;
        if test {
            self.test_depth = self.test_depth.saturating_sub(1);
        }
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

    fn visit_variant(&mut self, variant: &'ast syn::Variant) {
        if carries_io_source(&variant.fields) && !self.file_is(FAILURE_FILES) {
            self.flag(
                variant.ident.span(),
                "an I/O failure is failure::IoFailure, with the action and path it happened at",
            );
        }
        syn::visit::visit_variant(self, variant);
    }

    fn visit_item_struct(&mut self, item: &'ast syn::ItemStruct) {
        self.check_opaque_time_struct(item);
        self.check_io_failure_struct(item);
        self.check_input(
            &item.attrs,
            item.ident.span(),
            has_named_fields(&item.fields),
        );
        self.check_project_structure(item);
        self.check_editable_toml_type(item);
        self.check_origin_struct(item);
        self.check_survey_buffer(item);
        self.check_supervisor_event_sender(item);
        if self.file_is(&["provenance.rs"])
            && item.ident == "Labeled"
            && item
                .fields
                .iter()
                .any(|field| !matches!(field.vis, syn::Visibility::Inherited))
        {
            self.flag(item.ident.span(), REPOSITORY_PROVENANCE_RULE);
        }
        if self.file_is(&["supervisor.rs"]) && item.ident == "QueueWatchers" {
            let slots = item.fields.iter().any(|field| {
                field.ident.as_ref().is_some_and(|name| name == "slots")
                    && is_generic_of(&field.ty, "BTreeSet", &["PathBuf"])
                    && matches!(field.vis, syn::Visibility::Inherited)
            });
            let earlier = item.fields.iter().any(|field| {
                field.ident.as_ref().is_some_and(|name| name == "earlier")
                    && is_generic_of(&field.ty, "Option", &["PathBuf"])
                    && matches!(field.vis, syn::Visibility::Inherited)
            });
            if !slots || !earlier {
                self.flag(item.ident.span(), QUEUE_WATCH_RULE);
            }
        }
        if self.file_is(&["mcp.rs"])
            && item.ident == "McpDispatch"
            && !item.fields.iter().any(|field| {
                field.ident.as_ref().is_some_and(|name| name == "permit")
                    && is_named_type(&field.ty, "McpPermit")
                    && matches!(field.vis, syn::Visibility::Inherited)
            })
        {
            self.flag(item.ident.span(), MCP_WORKER_RULE);
        }
        let required = if self.file_is(&["config.rs"]) && item.ident == "TriggerConf" {
            Some(("refs", "RefPattern", true))
        } else if self.file_is(&["hook.rs"]) && item.ident == "Event" {
            Some(("reference", "EventRef", false))
        } else {
            None
        };
        if let Some((field_name, type_name, vec)) = required {
            let valid = item.fields.iter().any(|field| {
                field.ident.as_ref().is_some_and(|name| name == field_name)
                    && if vec {
                        is_vec_of(&field.ty, type_name)
                    } else {
                        is_named_type(&field.ty, type_name)
                    }
            });
            if !valid {
                self.flag(item.ident.span(), REFERENCE_TYPE_RULE);
            }
        }
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

    fn visit_item_type(&mut self, item: &'ast syn::ItemType) {
        if self.test_depth == 0
            && self.file.starts_with("crates/domyjob/src/")
            && let syn::Type::Path(target) = item.ty.as_ref()
            && target
                .path
                .segments
                .last()
                .is_some_and(|segment| self.state_enums.contains(&segment.ident.to_string()))
            && !(self.file_is(&["cli.rs"])
                && item.ident == "Checked"
                && is_generic_of(&item.ty, "Answered", &["Reached"]))
        {
            self.flag(item.ident.span(), STATE_IMPORT_RULE);
        }
        if self.test_depth == 0
            && !self.file_is(&["ingress.rs"])
            && let syn::Type::Path(target) = item.ty.as_ref()
            && [
                &["serde_json", "Deserializer"][..],
                &["serde_json", "de", "Deserializer"],
                &["toml_edit", "DocumentMut"],
            ]
            .iter()
            .any(|tail| path_ends_with(&target.path, tail))
        {
            self.flag(item.ident.span(), PROTECTED_IMPORT_RULE);
        }
        if self.file_is(&["project.rs"]) {
            let expected = if item.ident == "RepositoryText" {
                Some("Repository")
            } else if item.ident == "ApprovedProjectWord" {
                Some("ApprovedRepository")
            } else {
                None
            };
            if let Some(origin) = expected
                && !is_generic_of(&item.ty, "Labeled", &["String", origin])
            {
                self.flag(item.ident.span(), REPOSITORY_PROVENANCE_RULE);
            }
        }
        if self.file_is(&["authz.rs"]) {
            let valid = if item.ident == "Queried" {
                is_generic_of(&item.ty, "RoutedRequest", &["QueryEffect"])
            } else if item.ident == "Commanded" {
                is_generic_of(&item.ty, "RoutedRequest", &["CommandEffect"])
            } else if item.ident == "AuthorizedConfigure" {
                is_generic_of(&item.ty, "AuthorizedCommand", &["ConfigureKind", "Change"])
            } else if item.ident == "AuthorizedRetry" {
                is_generic_of(&item.ty, "AuthorizedCommand", &["RetryKind", "JobRef"])
            } else if item.ident == "AuthorizedKill" {
                is_generic_of(&item.ty, "AuthorizedCommand", &["KillKind", "JobRef"])
            } else if item.ident == "AuthorizedClean" {
                is_generic_of(
                    &item.ty,
                    "AuthorizedCommand",
                    &["CleanKind", "CleanOptions"],
                )
            } else {
                true
            };
            if !valid {
                self.flag(item.ident.span(), AUTHORIZED_COMMAND_RULE);
            }
        }
        syn::visit::visit_item_type(self, item);
    }

    fn visit_expr_struct(&mut self, expr: &'ast syn::ExprStruct) {
        self.check_revocation_construction(expr);
        if self.test_depth == 0
            && self.file_is(&["store.rs"])
            && path_ends_with(&expr.path, &["PreparedJobCommand"])
            && self.function.as_deref() != Some("prepare")
        {
            self.flag(
                expr.path
                    .segments
                    .last()
                    .map_or_else(Span::call_site, |segment| segment.ident.span()),
                JOB_PROCESS_RULE,
            );
        }
        if self.test_depth == 0
            && self.file_is(&["authz.rs"])
            && ((path_ends_with(&expr.path, &["Authorized"])
                && self.function.as_deref() != Some("authorize"))
                || (path_ends_with(&expr.path, &["Queried"])
                    && self.function.as_deref() != Some("route"))
                || (path_ends_with(&expr.path, &["Commanded"])
                    && self.function.as_deref() != Some("route"))
                || (path_ends_with(&expr.path, &["RoutedRequest"])
                    && self.function.as_deref() != Some("route"))
                || (path_ends_with(&expr.path, &["AuthorizedCommand"])
                    && self.function.as_deref() != Some("new"))
                || (path_ends_with(&expr.path, &["AuthorizedSubmission"])
                    && self.function.as_deref() != Some("into_submission")))
        {
            self.flag(
                expr.path
                    .segments
                    .last()
                    .map_or_else(Span::call_site, |segment| segment.ident.span()),
                AUTHORIZED_COMMAND_RULE,
            );
        }
        if self.test_depth == 0
            && self.file_is(&["mcp.rs"])
            && path_ends_with(&expr.path, &["McpPermit"])
            && self.function.as_deref() != Some("take")
        {
            self.flag(
                expr.path
                    .segments
                    .last()
                    .map_or_else(Span::call_site, |segment| segment.ident.span()),
                MCP_WORKER_RULE,
            );
        }
        if self.file_is(&["cli.rs"])
            && self.function.as_deref() == Some("run_named")
            && path_ends_with(&expr.path, &["Order"])
        {
            self.flag(
                expr.path
                    .segments
                    .first()
                    .map_or_else(Span::call_site, |part| part.ident.span()),
                PROJECT_APPROVAL_RULE,
            );
        }
        syn::visit::visit_expr_struct(self, expr);
    }

    fn visit_expr_call(&mut self, call: &'ast syn::ExprCall) {
        let channel_rule = if self.file_is(&["cli.rs"]) && self.function.as_deref() == Some("live")
        {
            Some(LIVE_UPDATE_RULE)
        } else if self.file_is(&["node.rs"]) && self.function.as_deref() == Some("watch") {
            Some(WATCH_WAKE_RULE)
        } else {
            None
        };
        if self.test_depth == 0
            && let Some(rule) = channel_rule
            && let syn::Expr::Path(path) = call.func.as_ref()
            && let Some(segment) = path.path.segments.last()
            && (segment.ident == "channel" || segment.ident == "sync_channel")
            && (segment.ident != "sync_channel"
                || !matches!(call.args.first(), Some(syn::Expr::Lit(syn::ExprLit {
                    lit: syn::Lit::Int(value), ..
                })) if matches!(value.base10_parse::<usize>(), Ok(1))))
        {
            self.flag(call.paren_token.span.open(), rule);
        }
        if self.file_is(&["node.rs"])
            && self.function.as_deref() == Some("visit_project_workspaces")
            && matches!(call.func.as_ref(), syn::Expr::Path(path)
                if path.path.is_ident("entries"))
        {
            self.flag(call.paren_token.span.open(), WORKSPACE_SLOT_RULE);
        }
        if self.test_depth == 0
            && self.file.starts_with("crates/domyjob/src/")
            && !self.file_is(&["proc.rs"])
            && matches!(call.func.as_ref(), syn::Expr::Path(path)
                if path_ends_with(&path.path, &["PreparedJobCommand", "into_command"]))
        {
            self.flag(call.paren_token.span.open(), JOB_PROCESS_RULE);
        }
        if self.test_depth == 0
            && self.file_is(&["authz.rs"])
            && self.function.as_deref() != Some("into_action")
            && matches!(call.func.as_ref(), syn::Expr::Path(path)
                if path_ends_with(&path.path, &["AuthorizedCommand", "new"]))
        {
            self.flag(call.paren_token.span.open(), AUTHORIZED_COMMAND_RULE);
        }
        syn::visit::visit_expr_call(self, call);
    }

    fn visit_item_enum(&mut self, item: &'ast syn::ItemEnum) {
        let struct_variants = item.variants.iter().any(|v| has_named_fields(&v.fields));
        self.check_input(&item.attrs, item.ident.span(), struct_variants);
        if self.file_is(&["authz.rs"])
            && let Some(expected) = match item.ident.to_string().as_str() {
                "Routed" => Some(&[("Query", "Queried"), ("Command", "Commanded")][..]),
                "CommandAction" => Some(
                    &[
                        ("Configure", "AuthorizedConfigure"),
                        ("Clean", "AuthorizedClean"),
                        ("Retry", "AuthorizedRetry"),
                        ("Kill", "AuthorizedKill"),
                    ][..],
                ),
                _ => None,
            }
            && (item.variants.len() != expected.len()
                || expected.iter().any(|(name, ty)| {
                    !item.variants.iter().any(|variant| {
                        variant.ident == *name
                            && matches!(&variant.fields, syn::Fields::Unnamed(fields)
                                if fields.unnamed.len() == 1
                                    && fields.unnamed.first().is_some_and(|field|
                                        is_named_type(&field.ty, ty)))
                    })
                }))
        {
            self.flag(item.ident.span(), AUTHORIZED_COMMAND_RULE);
        }
        if self.file_is(&["protocol.rs"]) && item.ident == "Request" {
            for variant in &item.variants {
                if variant.ident == "Missing" || variant.ident == "Upload" {
                    self.flag(variant.ident.span(), SNAPSHOT_TRANSFER_RULE);
                }
            }
        }
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

const OS_WORDS: &[&str] = &["unix", "windows", "target_os", "target_family"];

fn mentions_os(tokens: &proc_macro2::TokenStream) -> bool {
    tokens
        .to_string()
        .split(|c: char| !c.is_alphanumeric() && c != '_')
        .any(|word| OS_WORDS.contains(&word))
}

#[derive(Debug, Default)]
struct OsBranches {
    count: usize,
}

impl<'ast> Visit<'ast> for OsBranches {
    fn visit_attribute(&mut self, attr: &'ast syn::Attribute) {
        if (attr.path().is_ident("cfg") || attr.path().is_ident("cfg_attr"))
            && let syn::Meta::List(list) = &attr.meta
            && mentions_os(&list.tokens)
        {
            self.count = self.count.saturating_add(1);
        }
        syn::visit::visit_attribute(self, attr);
    }

    fn visit_macro(&mut self, mac: &'ast syn::Macro) {
        if mac.path.is_ident("cfg") && mentions_os(&mac.tokens) {
            self.count = self.count.saturating_add(1);
        }
        syn::visit::visit_macro(self, mac);
    }

    fn visit_path(&mut self, path: &'ast syn::Path) {
        let names: Vec<String> = path.segments.iter().map(|s| s.ident.to_string()).collect();
        if names.ends_with(&["consts".to_owned(), "OS".to_owned()]) {
            self.count = self.count.saturating_add(1);
        }
        syn::visit::visit_path(self, path);
    }
}

pub fn os_branches(source: &str) -> Result<usize, syn::Error> {
    let file = syn::parse_file(source)?;
    let mut branches = OsBranches::default();
    branches.visit_file(&file);
    Ok(branches.count)
}

pub fn check(source: &str) -> Result<Vec<Finding>, syn::Error> {
    check_file(source, "")
}

pub fn check_file(source: &str, name: &str) -> Result<Vec<Finding>, syn::Error> {
    check_file_with_enums(source, name, &std::collections::BTreeSet::new())
}

pub fn enum_names(source: &str) -> Result<std::collections::BTreeSet<String>, syn::Error> {
    struct Finder {
        names: std::collections::BTreeSet<String>,
    }

    impl<'ast> Visit<'ast> for Finder {
        fn visit_item_mod(&mut self, item: &'ast syn::ItemMod) {
            if !is_test_module(&item.attrs) {
                syn::visit::visit_item_mod(self, item);
            }
        }

        fn visit_item_enum(&mut self, item: &'ast syn::ItemEnum) {
            if !is_test_module(&item.attrs) {
                self.names.insert(item.ident.to_string());
            }
        }
    }

    let file = syn::parse_file(source)?;
    let mut finder = Finder {
        names: std::collections::BTreeSet::new(),
    };
    finder.visit_file(&file);
    Ok(finder.names)
}

pub fn check_file_with_enums(
    source: &str,
    name: &str,
    known_enums: &std::collections::BTreeSet<String>,
) -> Result<Vec<Finding>, syn::Error> {
    check_file_with_enums_and_baseline(source, name, known_enums, false)
}

pub fn check_repository_file_with_enums(
    source: &str,
    name: &str,
    known_enums: &std::collections::BTreeSet<String>,
) -> Result<Vec<Finding>, syn::Error> {
    check_file_with_enums_and_baseline(source, name, known_enums, true)
}

fn check_file_with_enums_and_baseline(
    source: &str,
    name: &str,
    known_enums: &std::collections::BTreeSet<String>,
    baseline: bool,
) -> Result<Vec<Finding>, syn::Error> {
    let file = syn::parse_file(source)?;
    let mut state_enums: std::collections::BTreeSet<String> = STATE_ENUMS
        .iter()
        .map(|enum_name| (*enum_name).to_owned())
        .collect();
    state_enums.extend(known_enums.iter().cloned());
    state_enums.extend(file.items.iter().filter_map(|item| {
        let syn::Item::Enum(item) = item else {
            return None;
        };
        (!is_test_module(&item.attrs)).then(|| item.ident.to_string())
    }));
    let defined_types = file
        .items
        .iter()
        .filter_map(|item| match item {
            syn::Item::Struct(item) => Some(item.ident.to_string()),
            syn::Item::Enum(item) => Some(item.ident.to_string()),
            syn::Item::Union(item) => Some(item.ident.to_string()),
            syn::Item::Macro(item)
                if name == "crates/domyjob/src/domain.rs"
                    && path_ends_with(&item.mac.path, &["text_newtype"]) =>
            {
                item.mac.tokens.clone().into_iter().find_map(|token| {
                    if let proc_macro2::TokenTree::Ident(ident) = token {
                        Some(ident.to_string())
                    } else {
                        None
                    }
                })
            }
            syn::Item::Const(_)
            | syn::Item::ExternCrate(_)
            | syn::Item::Fn(_)
            | syn::Item::ForeignMod(_)
            | syn::Item::Impl(_)
            | syn::Item::Macro(_)
            | syn::Item::Mod(_)
            | syn::Item::Static(_)
            | syn::Item::Trait(_)
            | syn::Item::TraitAlias(_)
            | syn::Item::Type(_)
            | syn::Item::Use(_)
            | syn::Item::Verbatim(_)
            | _ => None,
        })
        .collect();
    let mut gate = Gate {
        findings: Vec::new(),
        file: name.to_owned(),
        test_depth: 0,
        function: None,
        current_impl: None,
        legacy_bool_hits: vec![0; LEGACY_BOOL_SIGNATURES.len()],
        state_impl: false,
        state_enums,
        defined_types,
    };
    gate.visit_file(&file);
    if baseline {
        for ((legacy_file, legacy_name, _), actual) in
            LEGACY_BOOL_SIGNATURES.iter().zip(&gate.legacy_bool_hits)
        {
            if gate.file_is(&[legacy_file])
                && *actual != expected_legacy_bool_count(legacy_file, legacy_name)
            {
                gate.findings.push(Finding {
                    line: 1,
                    rule: LEGACY_BOOL_BASELINE_RULE,
                });
            }
        }
    }
    Ok(gate.findings)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rules(source: &str) -> Vec<&'static str> {
        check(source).unwrap().into_iter().map(|f| f.rule).collect()
    }

    fn assert_rule(source: &str, file: &str, rule: &'static str) {
        assert_eq!(
            check_file(source, file)
                .unwrap()
                .into_iter()
                .map(|finding| finding.rule)
                .collect::<Vec<_>>(),
            [rule],
            "{source}",
        );
    }

    #[test]
    fn every_way_of_asking_which_system_this_is_is_counted() {
        let source = "#[cfg(unix)]\nfn a() {}\n#[cfg(not(windows))]\nfn b() { let _ = cfg!(target_os = \"macos\"); let _ = std::env::consts::OS; }\n#[cfg(test)]\nmod tests {}\n#[cfg_attr(unix, inline)]\nfn c() {}";
        assert_eq!(os_branches(source).unwrap(), 5);
        assert_eq!(
            os_branches("#[cfg(feature = \"x\")]\nfn a() {}").unwrap(),
            0
        );
    }

    #[test]
    fn remote_text_is_never_printed_raw() {
        let source = "fn f(t: &RemoteText) -> String { t.as_raw_str().to_owned() }";
        assert_eq!(check_file(source, "src/view.rs").unwrap().len(), 1);
        assert!(check_file(source, "src/remote.rs").unwrap().is_empty());
    }

    #[test]
    fn supervisor_directory_override_has_one_local_caller() {
        let method = "fn f(dirs: Dirs) { let _ = dirs.with_supervisor_paths(None, None); }";
        let associated =
            "fn f(dirs: Dirs) { let _ = Dirs::with_supervisor_paths(dirs, None, None); }";
        for source in [method, associated] {
            assert_rule(source, "crates/domyjob/src/remote.rs", SUPERVISOR_PATH_RULE);
            assert!(
                check_file(source, "crates/domyjob/src/cli.rs")
                    .unwrap()
                    .is_empty()
            );
        }
    }

    #[test]
    fn hook_root_construction_has_one_caller() {
        let source = "fn f(name: &str, conf: &SourceConf, root: PathBuf) { let _ = Detected::from_hook_root(name, conf, root); }";
        assert_rule(
            source,
            "crates/domyjob/src/remote.rs",
            "only the local hook may pass its canonical source root to snapshot detection",
        );
        assert!(
            check_file(source, "crates/domyjob/src/hook.rs")
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn process_path_sources_stay_sealed() {
        let file = "crates/domyjob/src/template.rs";
        for source in [
            "impl Arg { fn path(path: &std::path::Path) -> Self { todo!() } }",
            "pub mod approved_path { pub(super) trait Sealed {} }",
            "pub trait SafePath { fn safe_path(&self) -> &std::path::Path; }",
            "impl SafePath for std::path::PathBuf { fn safe_path(&self) -> &std::path::Path { self } }",
        ] {
            assert_rule(source, file, SAFE_PATH_RULE);
        }
        let safe = "impl Arg { fn path(path: &impl SafePath) -> Self { todo!() } }";
        assert!(check_file(safe, file).unwrap().is_empty());
    }

    #[test]
    fn process_working_directories_require_local_path_proof() {
        let raw = "impl Invocation { fn in_dir(self, dir: &std::path::Path) -> Self { self } }";
        let typed = "impl Invocation { fn in_dir(self, dir: &impl SafePath) -> Self { self } }";
        assert_rule(raw, "crates/domyjob/src/spawn.rs", WORKING_DIRECTORY_RULE);
        assert!(
            check_file(typed, "crates/domyjob/src/spawn.rs")
                .unwrap()
                .is_empty()
        );
        let bypass = "fn f(command: &mut Command, path: &Path) { command.current_dir(path); }";
        assert_rule(
            bypass,
            "crates/domyjob/src/supervisor.rs",
            WORKING_DIRECTORY_RULE,
        );
        let associated =
            "fn f(command: &mut Command, path: &Path) { Command::current_dir(command, path); }";
        assert_rule(
            associated,
            "crates/domyjob/src/supervisor.rs",
            WORKING_DIRECTORY_RULE,
        );
    }

    #[test]
    fn job_process_spawn_requires_an_environment_proof() {
        assert_rule(
            "impl Group { fn spawn(command: Command, output: PipeWriter) {} }",
            "crates/domyjob/src/proc.rs",
            JOB_PROCESS_RULE,
        );
        assert_rule(
            "impl Group { fn spawn(command: Command, proof: PreparedJobCommand, output: PipeWriter) {} }",
            "crates/domyjob/src/proc.rs",
            JOB_PROCESS_RULE,
        );
        assert!(
            check_file(
                "impl Group { fn spawn(command: PreparedJobCommand, output: PipeWriter) {} }",
                "crates/domyjob/src/proc.rs"
            )
            .unwrap()
            .is_empty()
        );
        for source in [
            "pub struct PreparedJobCommand { pub command: Command }",
            "pub struct PreparedJobCommand;",
            "pub struct PreparedJobCommand { proof: bool }",
            "fn forge() { let _ = PreparedJobCommand { command: Command::new(\"sh\") }; }",
            "fn bypass(prepared: PreparedJobCommand) { let _ = prepared.into_command(); }",
            "fn bypass(prepared: PreparedJobCommand) { let _ = PreparedJobCommand::into_command(prepared); }",
        ] {
            assert_rule(source, "crates/domyjob/src/store.rs", JOB_PROCESS_RULE);
        }
    }

    #[test]
    fn trust_revocation_requires_a_private_parsed_selector() {
        let file = "crates/domyjob/src/trust.rs";
        for source in [
            "impl Trust { fn remove(dirs: &Dirs, who: &str) {} }",
            "pub(crate) struct RevocationSelector;",
            "pub(crate) struct RevocationSelector<'a> { pub raw: &'a str, kind: SelectorKind<'a> }",
            "fn forge() { let _ = RevocationSelector { raw: \"x\", kind: SelectorKind::Either(\"x\") }; }",
        ] {
            assert_rule(source, file, REVOCATION_RULE);
        }
        for source in [
            "impl Trust { fn remove(dirs: &Dirs, selector: &RevocationSelector<'_>) {} }",
            "pub(crate) struct RevocationSelector<'a> { raw: &'a str, kind: SelectorKind<'a> }",
        ] {
            assert!(check_file(source, file).unwrap().is_empty(), "{source}");
        }
    }

    #[test]
    fn command_authority_cannot_be_forged_or_added_at_a_sink() {
        let authz = "crates/domyjob/src/authz.rs";
        for source in [
            "pub struct Authorized { pub request: Request, nature: Nature }",
            "pub struct Queried { pub request: Request }",
            "pub struct Commanded { pub request: Request }",
            "pub struct RoutedRequest<E> { pub request: Request, nature: Nature, effect: PhantomData<E> }",
            "pub struct AuthorizedCommand<K, P> { pub payload: P, kind: PhantomData<K> }",
            "pub mod command_authority { pub(super) trait Sealed {} }",
            "pub trait CommandAuthority {}",
            "impl CommandAuthority for Unchecked {}",
            "impl CommandAuthority for RoutedRequest<QueryEffect> {}",
            "fn f() { let _ = Authorized { principal: p, request: r }; }",
            "fn f() { let _ = Queried { principal: p, request: r }; }",
            "fn f() { let _ = Commanded { principal: p, request: r }; }",
            "fn f() { let _ = RoutedRequest { principal: p, request: r, effect: PhantomData }; }",
            "fn f() { let _ = AuthorizedCommand { principal: p, payload: x, kind: PhantomData }; }",
            "fn f() { let _ = AuthorizedCommand::new(p, x); }",
            "type Queried = RoutedRequest<CommandEffect>;",
            "type AuthorizedRetry = AuthorizedCommand<KillKind, JobRef>;",
            "type AuthorizedClean = AuthorizedCommand<CleanKind, (bool, bool, bool)>;",
            "impl<E> RoutedRequest<E> { fn into_parts(self) -> (Principal, Request) { todo!() } }",
            "enum Routed { Query(Request), Command(Commanded) }",
            "enum CommandAction { Configure(Change), Clean(AuthorizedClean), Retry(AuthorizedRetry), Kill(AuthorizedKill) }",
            "impl Authorized { fn route(request: Request) -> Routed { todo!() } }",
            "impl Commanded { fn into_submission(self) -> Option<Submission> { None } }",
        ] {
            assert_rule(source, authz, AUTHORIZED_COMMAND_RULE);
        }
        for source in [
            "struct Authorized { principal: Principal, request: Request }",
            "struct RoutedRequest<E> { principal: Principal, request: Request, effect: PhantomData<E> }",
            "struct Authorized { principal: Principal, request: Request, nature: bool }",
        ] {
            assert_rule(source, authz, AUTHORIZED_COMMAND_RULE);
        }
        let node = "crates/domyjob/src/node.rs";
        for source in [
            "impl Node { fn accept(&self, submission: Submission) {} }",
            "impl Node { fn upkeep(&self, request: &Request) {} }",
            "impl Node { fn handle_query(&self, request: Request) {} }",
            "impl Node { fn reply_command(&self, request: Request) {} }",
            "impl Node { fn configure(&self, change: Change) {} }",
            "impl Node { fn clean(&self, flags: (bool, bool, bool)) {} }",
            "impl Node { fn retry(&self, id: &JobId) {} }",
            "impl Node { fn kill(&self, id: &JobId) {} }",
        ] {
            assert_rule(source, node, AUTHORIZED_COMMAND_RULE);
        }
    }

    #[test]
    fn inbound_requests_keep_their_peer_origin_until_authorization() {
        let cases = [
            (
                "crates/domyjob/src/ingress.rs",
                "pub struct PeerRequest(pub Request);",
            ),
            (
                "crates/domyjob/src/protocol.rs",
                "enum Request {} impl crate::ingress::Ingress for Request {}",
            ),
            (
                "crates/domyjob/src/protocol.rs",
                "struct Submission; impl crate::ingress::Ingress for Submission {}",
            ),
            (
                "crates/domyjob/src/authz.rs",
                "fn authorize(principal: Principal, request: Request) {}",
            ),
        ];
        for (file, source) in cases {
            assert_rule(source, file, PEER_INGRESS_RULE);
        }
        assert!(
            check_file(
                "pub struct PeerRequest(Request);",
                "crates/domyjob/src/ingress.rs",
            )
            .unwrap()
            .is_empty()
        );
    }

    #[test]
    fn ingress_requires_an_explicit_named_schema() {
        let file = "crates/domyjob/src/ingress.rs";
        for source in [
            "impl Ingress for String {}",
            "impl Ingress for u8 {}",
            "impl Ingress for Vec<Change> {}",
            "impl Ingress for std::collections::BTreeMap<EnvName, String> {}",
            "impl<T: Ingress> Ingress for Container<T> {}",
            "type Raw = String; impl Ingress for Raw {}",
            "use other::Input; impl Ingress for Input {}",
        ] {
            assert_rule(source, file, EXACT_INGRESS_RULE);
        }
        assert!(
            check_file(
                "pub struct PeerRequest(Request); impl Ingress for PeerRequest {}",
                file
            )
            .unwrap()
            .is_empty()
        );
        assert!(
            check_file(
                "text_newtype!(EnvName, valid, EnvName); impl Ingress for EnvName {}",
                "crates/domyjob/src/domain.rs"
            )
            .unwrap()
            .is_empty()
        );
    }

    #[test]
    fn setup_staging_cannot_use_a_fixed_incoming_name() {
        let fixed = r#"fn f() { let _ = "domyjob.incoming.exe"; }"#;
        let fixed_versioned = r#"fn f() { let _ = "domyjob-key.incoming.exe"; }"#;
        let scoped = r#"fn f() { let _ = "domyjob.incoming-"; }"#;
        let scoped_versioned = r#"fn f() { let _ = "domyjob-key.incoming-"; }"#;
        let file = "crates/domyjob/src/remote.rs";
        assert_eq!(
            check_file(fixed, file).unwrap().first().map(|f| f.rule),
            Some(INCOMING_RULE)
        );
        assert_eq!(
            check_file(fixed_versioned, file)
                .unwrap()
                .first()
                .map(|f| f.rule),
            Some(INCOMING_RULE)
        );
        assert!(check_file(scoped, file).unwrap().is_empty());
        assert!(check_file(scoped_versioned, file).unwrap().is_empty());
    }

    #[test]
    fn tests_cannot_install_a_process_global_fault_scenario() {
        let source = "#[cfg(test)] mod tests { fn f() { fail::FailScenario::setup(); } }";
        let file = "crates/domyjob/src/pull.rs";
        assert_eq!(
            check_file(source, file).unwrap().first().map(|f| f.rule),
            Some(FAULT_SCENARIO_RULE)
        );
        assert!(
            check_file(source, "crates/domyjob/src/faults.rs")
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn trigger_refs_and_event_reference_cannot_regress_to_plain_strings() {
        let config = "struct TriggerConf { refs: Vec<String> }";
        let hook = "struct Event { reference: String }";
        assert_eq!(
            check_file(config, "crates/domyjob/src/config.rs")
                .unwrap()
                .first()
                .map(|finding| finding.rule),
            Some(REFERENCE_TYPE_RULE)
        );
        assert_eq!(
            check_file(hook, "crates/domyjob/src/hook.rs")
                .unwrap()
                .first()
                .map(|finding| finding.rule),
            Some(REFERENCE_TYPE_RULE)
        );
        assert!(
            check_file(
                "struct TriggerConf { refs: Vec<RefPattern> }",
                "crates/domyjob/src/config.rs"
            )
            .unwrap()
            .is_empty()
        );
        assert!(
            check_file(
                "struct Event { reference: EventRef }",
                "crates/domyjob/src/hook.rs"
            )
            .unwrap()
            .is_empty()
        );
    }

    #[test]
    fn repository_jobs_cannot_regress_to_plain_order_inputs() {
        for (source, file) in [
            (
                "struct Project { jobs: BTreeMap<JobName, JobDef> }",
                "crates/domyjob/src/project.rs",
            ),
            (
                "struct RepositoryRequest { on: String, dir: Option<String>, env: Option<BTreeMap<String, String>> }",
                "crates/domyjob/src/project.rs",
            ),
            (
                "struct Order { targets: String }",
                "crates/domyjob/src/client.rs",
            ),
            (
                "struct Config { project_jobs: Vec<String> }",
                "crates/domyjob/src/config.rs",
            ),
            (
                "fn run_named() { let order = Order { targets: \"@all\".into() }; }",
                "crates/domyjob/src/cli.rs",
            ),
            (
                "struct ApprovedProjectJob { pub parts: ProjectOrderParts }",
                "crates/domyjob/src/project.rs",
            ),
            (
                "struct Targets { pub source: TargetSource }",
                "crates/domyjob/src/client.rs",
            ),
            (
                "impl Targets { pub fn project(x: ApprovedProjectTargets) -> Self { todo!() } }",
                "crates/domyjob/src/client.rs",
            ),
            (
                "impl Order { fn from_project(approved: ApprovedProjectJob, root: PathBuf, name: JobName, rev: Option<Revision>) -> Self { todo!() } }",
                "crates/domyjob/src/client.rs",
            ),
        ] {
            assert!(
                !check_file(source, file).unwrap().is_empty(),
                "{file}: {source}"
            );
        }
        assert!(
            check_file(
                "struct RepositoryRequest { on: ProjectSelector, run: Vec<RepositoryText>, runner: Option<RepositoryText>, dir: Option<RelPath>, env: Option<BTreeMap<EnvName, RepositoryText>> }",
                "crates/domyjob/src/project.rs"
            )
            .unwrap()
            .is_empty()
        );
        assert!(
            !check_file(
                "type RepositoryText = String;",
                "crates/domyjob/src/project.rs"
            )
            .unwrap()
            .is_empty()
        );
        assert!(
            !check_file(
                "type ApprovedProjectWord = String;",
                "crates/domyjob/src/project.rs"
            )
            .unwrap()
            .is_empty()
        );
        assert!(
            !check_file(
                "impl Labeled<String, Repository> { fn after_project_approval(&self) -> String { todo!() } }",
                "crates/domyjob/src/provenance.rs"
            )
            .unwrap()
            .is_empty()
        );
    }

    #[test]
    fn automatic_source_authority_stays_in_the_checked_checkout_path() {
        let forged = "fn forged() { let _ = InsecureUnsigned::from_local_checkout(); }";
        assert!(
            !check_file(forged, "crates/domyjob/src/remote.rs")
                .unwrap()
                .is_empty()
        );
        assert!(
            !check_file(forged, "crates/domyjob/src/client.rs")
                .unwrap()
                .is_empty()
        );
        let checked = "fn local_source() { let _ = InsecureUnsigned::from_local_checkout(); }";
        assert!(
            check_file(checked, "crates/domyjob/src/remote.rs")
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn production_text_file_reads_require_a_byte_budget() {
        let raw = "fn f() { let _ = std::fs::read_to_string(path); }";
        let bounded = "fn f() { let _ = crate::bounded::text_file(path, 1024); }";
        let file = "crates/domyjob/src/config.rs";
        assert_eq!(
            check_file(raw, file)
                .unwrap()
                .first()
                .map(|finding| finding.rule),
            Some(UNBOUNDED_TEXT_RULE)
        );
        assert!(check_file(bounded, file).unwrap().is_empty());
        assert!(
            check_file(raw, "crates/domyjob/src/bounded.rs")
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn production_binary_file_reads_require_a_byte_budget() {
        let raw = "fn f() { let _ = std::fs::read(path); }";
        let bounded = "fn f() { let _ = crate::bounded::file_bytes(path, 1024); }";
        let file = "crates/domyjob/src/remote.rs";
        assert_eq!(
            check_file(raw, file)
                .unwrap()
                .first()
                .map(|finding| finding.rule),
            Some(UNBOUNDED_FILE_RULE)
        );
        assert!(check_file(bounded, file).unwrap().is_empty());
        assert_eq!(
            check_file(raw, "crates/domyjob/src/cas.rs")
                .unwrap()
                .first()
                .map(|finding| finding.rule),
            Some(UNBOUNDED_FILE_RULE)
        );
        assert_eq!(
            check_file(raw, "crates/domyjob/src/snapshot.rs")
                .unwrap()
                .first()
                .map(|finding| finding.rule),
            Some(UNBOUNDED_FILE_RULE)
        );
    }

    #[test]
    fn an_io_error_travels_only_inside_io_failure() {
        let variant =
            "enum E { Io { action: &'static str, path: PathBuf, source: std::io::Error } }";
        assert_eq!(rules(variant).len(), 1);
        assert_eq!(
            rules("struct S { path: PathBuf, source: std::io::Error }").len(),
            1
        );
        assert!(
            rules("enum E { Listen { address: SocketAddr, source: std::io::Error } }").is_empty()
        );
        assert!(rules("enum E { Output(std::io::Error) }").is_empty());
        assert!(
            check_file(
                "struct IoFailure { source: std::io::Error }",
                "x/failure.rs"
            )
            .unwrap()
            .is_empty()
        );
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
        assert_eq!(check_file(decode, "src/fake_ingress.rs").unwrap().len(), 1);
        assert_eq!(check_file(decode, "xtask/src/ingress.rs").unwrap().len(), 1);
        let build_config = "fn parse(t: &str) { let _: toml::Value = toml::from_str(t).unwrap(); }";
        assert!(
            check_file(build_config, "crates/domyjob/src/build_config.rs")
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            check_file(build_config, "crates/domyjob/src/config.rs")
                .unwrap()
                .len(),
            1
        );
        for decoder in [
            "serde_json::Deserializer::from_slice(b)",
            "toml_edit::DocumentMut::from_str(t)",
            "t.parse::<toml_edit::DocumentMut>()",
        ] {
            let source = format!("fn f(b: &[u8], t: &str) {{ let _ = {decoder}; }}");
            assert_eq!(check_file(&source, "src/config.rs").unwrap().len(), 1);
            assert!(check_file(&source, "src/ingress.rs").unwrap().is_empty());
        }
        assert!(
            check_file(
                &format!("#[cfg(test)]\nmod tests {{ {decode} }}"),
                "src/node.rs"
            )
            .unwrap()
            .is_empty()
        );
        for file in [
            "src/authz.rs",
            "src/supervisor.rs",
            "src/serve.rs",
            "src/store.rs",
            "src/node.rs",
            "src/client.rs",
            "src/remote.rs",
        ] {
            for signature in [
                "fn answer() -> bool { true }",
                "fn answer() -> Result<bool, Error> { Ok(true) }",
                "fn answer() -> Option<bool> { Some(true) }",
                "fn answer() -> Result<Option<bool>, Error> { Ok(Some(true)) }",
            ] {
                assert_eq!(
                    check_file(signature, file).unwrap().len(),
                    1,
                    "{file}: {signature}"
                );
            }
        }
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
    fn new_boolean_signatures_are_checked_across_product_modules() {
        let file = "crates/domyjob/src/new_area.rs";
        for source in [
            "fn decision() -> bool { true }",
            "fn decision() -> Result<bool, Error> { Ok(true) }",
            "trait Policy { fn decision(&self) -> bool; }",
            "#[cfg(any(test, unix))] trait Policy { fn decision(&self) -> bool; }",
            "#[cfg(not(test))] fn decision() -> bool { true }",
            "#[cfg(any(test, unix))] fn decision() -> bool { true }",
            "#[cfg(any(test, unix))] impl Policy { fn decision(&self) -> bool { true } }",
        ] {
            assert_rule(source, file, DECISION_SIGNATURE_RULE);
        }
        for source in [
            "#[cfg(test)] fn fixture() -> bool { true }",
            "#[cfg(all(test, unix))] fn fixture() -> bool { true }",
            "#[cfg(any(all(test, unix), all(test, windows)))] fn fixture() -> bool { true }",
            "#[cfg(test)] impl Policy { fn fixture(&self) -> bool { true } }",
            "#[cfg(test)] trait Policy { fn fixture(&self) -> bool; }",
        ] {
            assert!(check_file(source, file).unwrap().is_empty(), "{source}");
        }
        assert!(
            check_file(
                "fn source_dir(path: &Path) -> bool { path.is_dir() }",
                "crates/domyjob/src/build_stamp.rs"
            )
            .unwrap()
            .is_empty()
        );
        assert_rule(
            "impl Other { fn source_dir(&self) -> bool { true } }",
            "crates/domyjob/src/build_stamp.rs",
            DECISION_SIGNATURE_RULE,
        );
        assert!(
            check_file(
                "impl Board { fn is_quiet(&self) -> bool { true } }",
                "crates/domyjob/src/board.rs"
            )
            .unwrap()
            .is_empty()
        );
        assert_rule(
            "impl Other { fn is_quiet(&self) -> bool { true } }",
            "crates/domyjob/src/board.rs",
            DECISION_SIGNATURE_RULE,
        );
        assert!(
            check_file(
                "impl RefPattern { fn matches(&self) -> bool { true } } impl JobId { fn matches(&self) -> bool { true } }",
                "crates/domyjob/src/domain.rs"
            )
            .unwrap()
            .is_empty()
        );
        assert_rule(
            "impl Other { fn matches(&self) -> bool { true } }",
            "crates/domyjob/src/domain.rs",
            DECISION_SIGNATURE_RULE,
        );
        assert_rule(
            "fn source_dir(path: &Path) -> bool { path.is_dir() }",
            file,
            DECISION_SIGNATURE_RULE,
        );
        let names = enum_names(
            "#[cfg(not(test))] enum Production { Ready } #[cfg(test)] enum Fixture { Ready }",
        )
        .unwrap();
        assert!(names.contains("Production"));
        assert!(!names.contains("Fixture"));
    }

    #[test]
    fn legacy_boolean_exceptions_require_the_reviewed_definition_count() {
        let file = "crates/domyjob/src/board.rs";
        let checked = |source| {
            check_repository_file_with_enums(source, file, &std::collections::BTreeSet::new())
                .unwrap()
                .into_iter()
                .map(|finding| finding.rule)
                .collect::<Vec<_>>()
        };
        let complete =
            "impl Board { fn is_quiet(&self) -> bool { true } fn is_live(&self) -> bool { true } }";
        assert!(checked(complete).is_empty());
        assert_eq!(
            checked("impl Board { fn is_quiet(&self) -> bool { true } }"),
            [LEGACY_BOOL_BASELINE_RULE]
        );
        assert_eq!(
            checked(
                "impl Board { fn is_quiet(&self) -> bool { true } fn is_live(&self) -> bool { true } fn is_live(&self) -> bool { true } }"
            ),
            [LEGACY_BOOL_BASELINE_RULE]
        );
        assert_eq!(
            checked(
                "impl Board { fn is_quiet(&self) -> bool { true } fn is_live(&self) -> State { todo!() } }"
            ),
            [LEGACY_BOOL_BASELINE_RULE]
        );
    }

    #[test]
    fn imports_cannot_hide_protected_decoders_or_file_reads() {
        let file = "crates/domyjob/src/node.rs";
        for (source, rule) in [
            (
                "use serde_json::from_slice as decode;",
                "decode input only in ingress.rs, into a type that implements Ingress",
            ),
            (
                "use serde_json::{from_slice as decode, Value};",
                "decode input only in ingress.rs, into a type that implements Ingress",
            ),
            (
                "use serde_json::de::from_slice;",
                "decode input only in ingress.rs, into a type that implements Ingress",
            ),
            ("use serde_json as json;", PROTECTED_IMPORT_RULE),
            ("use serde_json::de as parser;", PROTECTED_IMPORT_RULE),
            ("use serde_json::de;", PROTECTED_IMPORT_RULE),
            ("use serde_json::value;", PROTECTED_IMPORT_RULE),
            ("use toml::de;", PROTECTED_IMPORT_RULE),
            ("use serde_json::Deserializer;", PROTECTED_IMPORT_RULE),
            ("use toml_edit::DocumentMut;", PROTECTED_IMPORT_RULE),
            ("use std::fs::read as slurp;", UNBOUNDED_FILE_RULE),
            ("use std::fs::{read as slurp, File};", UNBOUNDED_FILE_RULE),
            ("use std::fs as disk;", PROTECTED_IMPORT_RULE),
            ("use serde_json::*;", IMPORT_GLOB_RULE),
            ("extern crate serde_json as json;", PROTECTED_IMPORT_RULE),
            (
                "type Decoder = serde_json::Deserializer<serde_json::de::IoRead<std::io::Empty>>;",
                PROTECTED_IMPORT_RULE,
            ),
            (
                "type Document = toml_edit::DocumentMut;",
                PROTECTED_IMPORT_RULE,
            ),
        ] {
            let found: Vec<_> = check_file(source, file)
                .unwrap()
                .into_iter()
                .map(|finding| finding.rule)
                .collect();
            assert_eq!(found, [rule], "{source}");
        }
        for source in [
            "use serde_json::Value;",
            "use serde_json::Value as JsonValue;",
            "use std::fs::File;",
            "#[cfg(test)] mod tests { use serde_json::*; }",
        ] {
            assert!(check_file(source, file).unwrap().is_empty(), "{source}");
        }
        assert!(
            check_file("use serde_json::from_slice;", "src/ingress.rs")
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn editable_toml_parser_stays_behind_its_ingress_value() {
        let file = "crates/domyjob/src/ingress.rs";
        let private = "pub struct EditableToml(toml_edit::DocumentMut);";
        assert!(check_file(private, file).unwrap().is_empty());
        let exposed = "pub struct EditableToml(pub toml_edit::DocumentMut);";
        assert_eq!(
            check_file(exposed, file)
                .unwrap()
                .first()
                .map(|finding| finding.rule),
            Some(EDITABLE_TOML_TYPE_RULE)
        );
        let raw_return = "fn editable_toml() -> Result<toml_edit::DocumentMut, toml_edit::TomlError> { todo!() }";
        assert_eq!(
            check_file(raw_return, file)
                .unwrap()
                .first()
                .map(|finding| finding.rule),
            Some(EDITABLE_TOML_TYPE_RULE)
        );
    }

    #[test]
    fn larger_state_read_budgets_are_confined_to_their_owners() {
        let audit = "fn f(p: &Path) { state_file::read_audit_bytes(p); }";
        let history = "fn f(p: &Path) { state_file::read_history_bytes(p); }";
        assert_eq!(check_file(audit, "src/node.rs").unwrap().len(), 1);
        assert!(check_file(audit, "src/audit.rs").unwrap().is_empty());
        assert_eq!(check_file(history, "src/node.rs").unwrap().len(), 1);
        assert!(check_file(history, "src/client.rs").unwrap().is_empty());
        assert_eq!(
            check_file("fn f(p: &Path) { read_history_bytes(p); }", "src/node.rs")
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn json_state_keeps_its_type_at_the_file_capability() {
        for operation in ["read_json(p)", "write_json(p, &value)"] {
            let source = format!("fn f(p: &Path) {{ state_file::{operation}; }}");
            assert_eq!(check_file(&source, "src/node.rs").unwrap().len(), 1);
            assert!(check_file(&source, "src/state_file.rs").unwrap().is_empty());
            assert!(
                check_file(
                    &format!("#[cfg(test)] mod tests {{ {source} }}"),
                    "src/node.rs"
                )
                .unwrap()
                .is_empty()
            );
        }
        let source = "fn f(p: &Path) { StateFile::<Head>::at(p).read(); }";
        assert_eq!(
            check_file(source, "crates/domyjob/src/node.rs")
                .unwrap()
                .first()
                .map(|finding| finding.rule),
            Some(STATE_CONSTRUCTOR_RULE)
        );
        let owner = "fn head_file(p: &Path) -> StateFile<Head> { StateFile::at(p) }";
        assert!(
            check_file(owner, "crates/domyjob/src/audit.rs")
                .unwrap()
                .is_empty()
        );
        let wrong = "fn head_file(p: &Path) -> StateFile<String> { StateFile::at(p) }";
        assert_eq!(
            check_file(wrong, "crates/domyjob/src/audit.rs")
                .unwrap()
                .first()
                .map(|finding| finding.rule),
            Some(STATE_CONSTRUCTOR_RULE)
        );
    }

    #[test]
    fn cas_marks_keep_their_bounded_value_shape() {
        let typed = "struct Marks { ids: BTreeMap<BlobId, BlobUse>, limit: usize }";
        assert!(
            check_file(typed, "crates/domyjob/src/node.rs")
                .unwrap()
                .is_empty()
        );
        let untyped = "struct Marks { ids: Vec<BlobId>, limit: usize }";
        assert_eq!(
            check_file(untyped, "crates/domyjob/src/node.rs")
                .unwrap()
                .first()
                .map(|finding| finding.rule),
            Some(CAS_MARK_RULE)
        );
    }

    #[test]
    fn unbounded_read_calls_are_confined_to_bounded() {
        for source in [
            "fn f(r: &mut R, out: &mut Vec<u8>) { r.read_to_end(out); }",
            "fn f(r: &mut R, out: &mut Vec<u8>) { r.read_until(b'\\n', out); }",
            "fn f(r: &mut R) { r.fill_buf(); }",
            "fn f(r: &mut R, out: &mut String) { r.read_line(out); }",
            "fn f(r: &mut R, out: &mut Vec<u8>) { std::io::Read::read_to_end(r, out); }",
            "fn f(r: &mut R, out: &mut Vec<u8>) { std::io::BufRead::read_until(r, b'\\n', out); }",
            "fn f(r: &mut R) { std::io::BufRead::fill_buf(r); }",
            "fn f(r: &mut R, out: &mut String) { std::io::BufRead::read_line(r, out); }",
        ] {
            assert_eq!(check_file(source, "src/remote.rs").unwrap().len(), 1);
            assert!(check_file(source, "src/bounded.rs").unwrap().is_empty());
            assert!(
                check_file(
                    &format!("#[cfg(test)] mod tests {{ {source} }}"),
                    "src/remote.rs"
                )
                .unwrap()
                .is_empty()
            );
        }
    }

    #[test]
    fn mcp_input_and_worker_spawns_require_the_bounded_dispatch_path() {
        for source in [
            "fn serve(input: R) { for line in input.lines() {} }",
            "fn serve(scope: S) { scope.spawn(|| {}); }",
            "fn serve() { std::thread::spawn(|| {}); }",
            "fn dispatch(scope: S) { scope.spawn(|| {}); }",
            "struct McpDispatch { other: McpPermit }",
            "fn serve() { let permit = McpPermit { calls, key }; }",
        ] {
            assert!(
                !check_file(source, "crates/domyjob/src/mcp.rs")
                    .unwrap()
                    .is_empty(),
                "{source}"
            );
        }
        assert!(
            check_file(
                "fn dispatch(job: McpDispatch<'_, W>, scope: S) { scope.spawn(|| {}); }",
                "crates/domyjob/src/mcp.rs"
            )
            .unwrap()
            .is_empty()
        );
    }

    #[test]
    fn snapshot_hashing_cannot_bypass_fixed_resource_budgets() {
        for source in [
            "fn f() { std::thread::available_parallelism(); }",
            "fn f(hash: &mut H, path: &Path) { hash.update_mmap(path); }",
        ] {
            assert_eq!(
                check_file(source, "crates/domyjob/src/snapshot.rs")
                    .unwrap()
                    .first()
                    .map(|finding| finding.rule),
                Some(SNAPSHOT_BUDGET_RULE)
            );
        }
    }

    #[test]
    fn watch_survey_accumulation_requires_a_bounded_type() {
        assert_rule(
            "struct Surveys { pending: Vec<u8> }",
            "crates/domyjob/src/client.rs",
            SURVEY_BUFFER_RULE,
        );
        assert!(
            check_file(
                "struct Surveys { lines: LineBuffer }",
                "crates/domyjob/src/client.rs"
            )
            .unwrap()
            .is_empty()
        );
    }

    #[test]
    fn live_watch_updates_cannot_use_an_unbounded_channel() {
        assert_rule(
            "fn live() { std::sync::mpsc::channel(); }",
            "crates/domyjob/src/cli.rs",
            LIVE_UPDATE_RULE,
        );
        assert_rule(
            "fn live() { std::sync::mpsc::sync_channel(64); }",
            "crates/domyjob/src/cli.rs",
            LIVE_UPDATE_RULE,
        );
        assert!(
            check_file(
                "fn live() { std::sync::mpsc::sync_channel(1); }",
                "crates/domyjob/src/cli.rs"
            )
            .unwrap()
            .is_empty()
        );
    }

    #[test]
    fn node_watch_wakes_cannot_accumulate_without_a_limit() {
        assert_rule(
            "impl Node { fn watch(&self) { std::sync::mpsc::channel::<WatchWake>(); } }",
            "crates/domyjob/src/node.rs",
            WATCH_WAKE_RULE,
        );
        assert!(
            check_file(
                "impl Node { fn watch(&self) { std::sync::mpsc::sync_channel::<WatchWake>(1); } }",
                "crates/domyjob/src/node.rs"
            )
            .unwrap()
            .is_empty()
        );
    }

    #[test]
    fn supervisor_events_require_a_bounded_sender() {
        assert_rule(
            "struct Shared { events: Sender<Event> }",
            "crates/domyjob/src/supervisor.rs",
            SUPERVISOR_EVENT_RULE,
        );
        assert!(
            check_file(
                "struct Shared { events: SyncSender<Event> }",
                "crates/domyjob/src/supervisor.rs"
            )
            .unwrap()
            .is_empty()
        );
    }

    #[test]
    fn queue_and_store_resource_paths_stay_typed_and_bounded() {
        for (source, file, rule) in [
            (
                "fn queue() { std::thread::spawn(|| {}); }",
                "crates/domyjob/src/supervisor.rs",
                QUEUE_WATCH_RULE,
            ),
            (
                "struct QueueWatchers { slots: Vec<PathBuf>, earlier: Vec<PathBuf> }",
                "crates/domyjob/src/supervisor.rs",
                QUEUE_WATCH_RULE,
            ),
            (
                "impl Store { pub fn ids(&self) -> Vec<JobId> { vec![] } }",
                "crates/domyjob/src/store.rs",
                JOB_IDS_RULE,
            ),
            (
                "impl Store { fn slot_holders(&self) { std::fs::read_dir(p); } }",
                "crates/domyjob/src/store.rs",
                DIRECTORY_OWNER_RULE,
            ),
            (
                "fn open() { std::fs::read_dir(p); }",
                "crates/domyjob/src/pull.rs",
                DIRECTORY_OWNER_RULE,
            ),
            (
                "fn queue() { OsLock::probe(p); }",
                "crates/domyjob/src/supervisor.rs",
                SLOT_SCAN_RULE,
            ),
            (
                "use std::fs::read_dir as list;",
                "crates/domyjob/src/store.rs",
                DIRECTORY_OWNER_RULE,
            ),
            (
                "use crate::lock::OsLock as Lock;",
                "crates/domyjob/src/supervisor.rs",
                SLOT_SCAN_RULE,
            ),
        ] {
            assert_eq!(
                check_file(source, file)
                    .unwrap()
                    .first()
                    .map(|finding| finding.rule),
                Some(rule),
                "{source}"
            );
        }
        assert!(
            check_file(
                "impl Store { #[cfg(test)] pub fn ids(&self) -> Vec<JobId> { vec![] } }",
                "crates/domyjob/src/store.rs"
            )
            .unwrap()
            .is_empty()
        );
        for (source, file) in [
            (
                "fn iter_ids() { std::fs::read_dir(p); }",
                "crates/domyjob/src/store.rs",
            ),
            (
                "fn held_slots() { OsLock::probe(p); }",
                "crates/domyjob/src/store.rs",
            ),
            (
                "fn for_each_entry() { std::fs::read_dir(p); }",
                "crates/domyjob/src/pull.rs",
            ),
        ] {
            assert!(check_file(source, file).unwrap().is_empty(), "{source}");
        }
    }

    #[test]
    fn workspace_cleanup_cannot_restore_numeric_directory_aliases() {
        let file = "crates/domyjob/src/node.rs";
        for source in [
            "fn visit_project_workspaces() { entries(project); }",
            "fn visit_project_workspaces() { SlotIndex::all(); entries(project); }",
            "fn visit_project_workspaces() { SlotIndex::all(); name.parse::<usize>(); }",
        ] {
            assert!(
                check_file(source, file)
                    .unwrap()
                    .iter()
                    .any(|finding| finding.rule == WORKSPACE_SLOT_RULE),
                "{source}"
            );
        }
        assert_rule(
            "pub struct SlotIndex(pub u32);",
            "crates/domyjob/src/lock.rs",
            WORKSPACE_SLOT_RULE,
        );
        assert!(
            check_file(
                "fn visit_project_workspaces() { for slot in SlotIndex::all() { slot.lock_path(dir); } }",
                file
            )
            .unwrap()
            .is_empty()
        );
        assert_rule(
            "impl Node { fn visit_idle_workspaces(&self) { entries(work); } }",
            file,
            WORKSPACE_ORDER_RULE,
        );
        assert!(
            check_file(
                "impl Node { fn visit_idle_workspaces(&self) { SortedScan::<PathBuf>::new(); } }",
                file
            )
            .unwrap()
            .is_empty()
        );
    }

    #[test]
    fn remote_upgrade_cannot_treat_unparsable_versions_as_old() {
        let protocol = "crates/domyjob/src/protocol.rs";
        let remote = "crates/domyjob/src/remote.rs";
        for (source, file) in [
            ("fn is_newer() -> bool { false }", protocol),
            ("fn version_relation() -> bool { false }", protocol),
            (
                "fn outdated_speaker() -> Result<Speaker, RemoteError> { Ok(speaker) }",
                remote,
            ),
            ("impl Link { fn discover(&self) {} }", remote),
        ] {
            assert!(
                check_file(source, file)
                    .unwrap()
                    .iter()
                    .any(|finding| finding.rule == VERSION_DECISION_RULE),
                "{source}"
            );
        }
        for (source, file) in [
            (
                "fn version_relation() -> VersionRelation { VersionRelation::Newer }",
                protocol,
            ),
            (
                "fn outdated_speaker() -> Result<Speaker, RemoteError> { version_relation(v); Ok(speaker) }",
                remote,
            ),
            (
                "impl Link { fn discover(&self) { outdated_speaker(); } }",
                remote,
            ),
        ] {
            assert!(check_file(source, file).unwrap().is_empty(), "{source}");
        }
    }

    #[test]
    fn job_phase_transitions_require_a_typed_exhaustive_decision() {
        let store = "crates/domyjob/src/store.rs";
        for source in [
            "fn phase_transition() -> Result<PhaseTransition, Error> { Ok(PhaseTransition::Advance) }",
            "fn phase_transition() -> PhaseTransition { if true { PhaseTransition::Advance } else { PhaseTransition::Refuse } }",
            "fn phase_transition() -> PhaseTransition { PhaseTransition::Refuse }",
            "fn phase_transition(from: &Phase, to: &Phase) -> PhaseTransition { match (from, to) { (_, _) => PhaseTransition::Refuse } }",
            "fn phase_transition(from: &Phase, to: &Phase) -> PhaseTransition { match (from, to) { (left, right) => PhaseTransition::Refuse } }",
            "impl Store { fn set_phase(&self) {} }",
            "impl Store { fn set_phase(&self) { matches!(phase, Some(_)); phase_transition(); } }",
        ] {
            assert_rule(source, store, PHASE_TRANSITION_RULE);
        }
        assert!(check_file(
            "fn phase_transition(from: &Phase, to: &Phase) -> PhaseTransition { match (from, to) { (Phase::Queued, Phase::Queued) => PhaseTransition::Refuse } } impl Store { fn set_phase(&self) { phase_transition(); } }",
            store,
        )
        .unwrap()
        .is_empty());
    }

    #[test]
    fn state_classification_cannot_silently_absorb_new_variants() {
        for file in [
            "crates/domyjob/src/protocol.rs",
            "crates/domyjob/src/store.rs",
            "crates/domyjob/src/node.rs",
        ] {
            for variant in [
                "Phase::Finished { .. }",
                "Supervisor::Alive",
                "QueueMode::Ordinary",
                "Publication::Published",
                "Blocker::Active",
                "Probe::Held",
                "Principal::Owner",
                "Request::Kill { .. }",
                "Reply::Refused(_)",
                "RefusalCode::MissingContent",
                "ClientError::Unknown(_)",
                "ServiceAction::Install",
                "Location::Home",
                "KillState::Asked",
                "AddressScope::Tailnet",
                "PairingState::Closed",
                "Consideration::Accepts",
                "Deliverable::Source { .. }",
                "Output::Tail(_)",
                "Verdict::Failed(_)",
                "State::Running",
                "DiskSpace::Unavailable { .. }",
                "Checked::Answer(_)",
                "Availability::Available",
                "MissingManifest::Ignore",
            ] {
                let source = format!("fn inspect(value: State) {{ matches!(value, {variant}); }}");
                assert_rule(&source, file, STATE_CLASSIFICATION_RULE);
            }
            assert!(
                check_file("fn inspect(phase: Phase) { phase.kind(); }", file,)
                    .unwrap()
                    .is_empty()
            );
        }
        assert!(check_file(
            "#[cfg(test)] mod tests { fn inspect(phase: Phase) { matches!(phase, Phase::Finished { .. }); } }",
            "crates/domyjob/src/node.rs",
        )
        .unwrap()
        .is_empty());
        assert_rule(
            "impl Family { fn links(self) -> bool { matches!(self, Self::Unix) } }",
            "crates/domyjob/src/paths.rs",
            STATE_CLASSIFICATION_RULE,
        );
        for source in [
            "use crate::protocol::Phase as P;",
            "use crate::protocol::Phase::Finished;",
            "use crate::protocol::Phase::Finished as Done;",
            "type P = crate::protocol::Phase;",
        ] {
            assert_rule(source, "crates/domyjob/src/node.rs", STATE_IMPORT_RULE);
        }
        for source in [
            "fn inspect(entry: Entry) { if let Entry::File { .. } = entry {} }",
            "fn inspect(entry: Option<Entry>) { if let Some(Entry::File { .. }) = entry {} }",
            "fn inspect(entry: Entry) { let Entry::File { .. } = entry else { return; }; }",
            "fn inspect(reach: Reach<()>) { if let Reach::Reached(_) = reach {} }",
            "impl Entry { fn inspect(&self) { if let Self::File { .. } = self {} } }",
        ] {
            assert_rule(source, "crates/domyjob/src/node.rs", STATE_PATTERN_RULE);
        }
        assert_rule(
            "impl Entry { fn inspect(&self) { matches!(self, Self::File { .. }); } }",
            "crates/domyjob/src/snapshot.rs",
            STATE_CLASSIFICATION_RULE,
        );
        assert!(check_file(
            "fn inspect(entry: Entry) { match entry { Entry::File { .. } => {}, Entry::Symlink { .. } => {} } }",
            "crates/domyjob/src/node.rs",
        )
        .unwrap()
        .is_empty());
    }

    #[test]
    fn state_gate_discovers_new_enums_across_files() {
        let known = enum_names(
            "pub enum NewState { Active, Inactive } #[cfg(test)] mod tests { enum Fixture { Value } }",
        )
        .unwrap();
        assert!(known.contains("NewState"));
        assert!(!known.contains("Fixture"));
        for source in [
            "fn inspect(value: NewState) { matches!(value, NewState::Active); }",
            "fn inspect(value: NewState) { if let NewState::Active = value {} }",
        ] {
            let findings =
                check_file_with_enums(source, "crates/domyjob/src/node.rs", &known).unwrap();
            assert_eq!(findings.len(), 1, "{source}");
        }
        let source =
            "fn inspect(value: NewState) { match value { NewState::Active => {}, other => {} } }";
        let findings = check_file_with_enums(source, "crates/domyjob/src/node.rs", &known).unwrap();
        assert!(
            findings
                .iter()
                .any(|finding| finding.rule == STATE_FALLBACK_RULE)
        );
        let exhaustive = "fn inspect(value: NewState) { match value { NewState::Active => {}, NewState::Inactive => {} } }";
        assert!(
            check_file_with_enums(exhaustive, "crates/domyjob/src/node.rs", &known)
                .unwrap()
                .is_empty()
        );
        for nested in [
            "fn inspect(value: Option<NewState>) { match value { Some(NewState::Active) => {}, Some(_) => {}, None => {} } }",
            "fn inspect(value: (NewState, bool)) { match value { (NewState::Active, true) => {}, (_, _) => {} } }",
            "fn inspect(value: &[NewState]) { match value { [NewState::Active] => {}, _ => {} } }",
            "fn inspect(value: &[NewState]) { match value { [NewState::Active] => {}, [..] => {} } }",
            "fn inspect(value: &[NewState]) { match value { [NewState::Active, ..] => {}, [_, ..] => {} } }",
            "fn inspect(value: &[NewState]) { match value { [.., NewState::Active] => {}, [.., _] => {} } }",
            "fn inspect(value: &[NewState]) { match value { [NewState::Active, ..] => {}, [_, 7] => {} } }",
            "fn inspect(value: Option<NewState>) { match value { Some(NewState::Active) => {}, Some(..) => {}, None => {} } }",
            "fn inspect(value: Envelope) { match value { Envelope { state: NewState::Active, .. } => {}, Envelope { state: _, .. } => {} } }",
            "fn inspect(value: Envelope) { match value { Envelope { state: NewState::Active, .. } => {}, Envelope { .. } => {} } }",
        ] {
            let nested_findings =
                check_file_with_enums(nested, "crates/domyjob/src/node.rs", &known).unwrap();
            assert!(
                nested_findings
                    .iter()
                    .any(|finding| finding.rule == STATE_FALLBACK_RULE),
                "{nested}"
            );
        }
        let separate = "fn inspect(value: Result<NewState, Failure>) { match value { Ok(NewState::Active) => {}, Ok(NewState::Inactive) => {}, Err(_) => {} } }";
        let separate_findings =
            check_file_with_enums(separate, "crates/domyjob/src/node.rs", &known).unwrap();
        assert!(separate_findings.is_empty(), "{separate_findings:?}");
    }

    #[test]
    fn state_gate_exempts_only_original_error_propagation() {
        let known = enum_names("enum NewState { Active, Inactive }").unwrap();
        for forward in [
            "fn inspect(value: Result<(), NewState>) -> Result<(), NewState> { match value { Err(NewState::Active) => Err(NewState::Active), Err(error) => Err(error), Ok(()) => Ok(()) } }",
            "fn inspect(value: (Result<(), NewState>, bool)) -> Result<(), NewState> { match value { (Err(NewState::Active), _) => Err(NewState::Active), (Err(error), _) => Err(error), (Ok(()), _) => Ok(()) } }",
        ] {
            assert!(
                check_file_with_enums(forward, "crates/domyjob/src/node.rs", &known)
                    .unwrap()
                    .is_empty(),
                "{forward}"
            );
        }
        let changed = "fn inspect(value: Result<(), NewState>) { match value { Err(NewState::Active) => {}, Err(error) => report(error), Ok(()) => {} } }";
        assert!(
            check_file_with_enums(changed, "crates/domyjob/src/node.rs", &known)
                .unwrap()
                .iter()
                .any(|finding| finding.rule == STATE_FALLBACK_RULE)
        );
        let reclassified = "fn inspect(value: (NewState, bool)) -> Result<(), NewState> { match value { (NewState::Active, _) => Ok(()), (state, _) => Err(state) } }";
        assert!(
            check_file_with_enums(reclassified, "crates/domyjob/src/node.rs", &known)
                .unwrap()
                .iter()
                .any(|finding| finding.rule == STATE_FALLBACK_RULE)
        );
        let errors = enum_names("enum NewError { Active, Inactive }").unwrap();
        let propagated = "fn inspect(value: (NewError, bool)) -> Result<(), NewError> { match value { (NewError::Active, _) => Ok(()), (error, _) => Err(error) } }";
        assert!(
            check_file_with_enums(propagated, "crates/domyjob/src/node.rs", &errors)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn state_gate_requires_review_of_enum_generating_macros() {
        for source in [
            "macro_rules! generated { () => { enum Hidden { Active, Inactive } } }",
            "generated!(Hidden);",
        ] {
            assert_eq!(
                check_file(source, "crates/domyjob/src/node.rs")
                    .unwrap()
                    .first()
                    .map(|finding| finding.rule),
                Some(STATE_MACRO_RULE),
                "{source}"
            );
        }
        let fixture = "#[cfg(test)] mod tests { generated!(Hidden); }";
        assert!(
            check_file(fixture, "crates/domyjob/src/node.rs")
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn short_cli_operations_cannot_start_their_own_fanout() {
        for source in [
            "fn clean() { std::thread::scope(|scope| scope.spawn(|| {})); }",
            "fn clean() { std::thread::spawn(|| {}); }",
            "fn clean(scope: S) { scope.spawn(|| {}); }",
            "use std::thread::scope as run;",
            "use std::thread::Builder as Worker;",
            "fn clean(builder: std::thread::Builder) { std::thread::Builder::spawn(builder, || {}); }",
            "fn watch() { std::thread::scope(|scope| scope.spawn(|| {})); }",
            "fn run_watches(jobs: &[String]) { std::thread::scope(|scope| scope.spawn(|| {})); }",
            "fn run_watches(jobs: &ConcurrentBatch<'_, String, 1000>) { std::thread::scope(|scope| scope.spawn(|| {})); }",
            "const MAX_WATCHES: usize = 1000;",
        ] {
            assert_eq!(
                check_file(source, "crates/domyjob/src/cli.rs")
                    .unwrap()
                    .first()
                    .map(|finding| finding.rule),
                Some(CLI_FANOUT_RULE),
                "{source}"
            );
        }
        for source in [
            "fn live() { std::thread::scope(|scope| scope.spawn(|| {})); }",
            "fn run_watches(jobs: &ConcurrentBatch<'_, String, MAX_WATCHES>) { std::thread::scope(|scope| scope.spawn(|| {})); }",
        ] {
            assert!(
                check_file(source, "crates/domyjob/src/cli.rs")
                    .unwrap()
                    .is_empty(),
                "{source}"
            );
        }
    }

    #[test]
    fn windows_private_directories_cannot_return_to_an_icacls_child_process() {
        let source = "fn create_private_dir_in() { Arg::literal(\"icacls\"); }";
        assert_eq!(
            check_file(source, "crates/domyjob/src/platform.rs")
                .unwrap()
                .first()
                .map(|finding| finding.rule),
            Some(WINDOWS_PRIVATE_DIR_RULE)
        );
    }

    #[test]
    fn publication_is_confined_to_the_store_lock() {
        let bypass = "fn f(staged: &Path, target: &Path) { crate::state_file::publish_dir(staged, target); }";
        assert_eq!(
            check_file(bypass, "crates/domyjob/src/node.rs")
                .unwrap()
                .first()
                .map(|finding| finding.rule),
            Some("publish staged jobs only through Store so collection sees every job")
        );
        assert!(
            check_file(bypass, "crates/domyjob/src/store.rs")
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn a_split_source_transfer_cannot_return_to_the_wire_protocol() {
        let request =
            "enum Request { Missing { blobs: Vec<BlobId> }, Upload { count: u64 }, Submit }";
        let findings = check_file(request, "crates/domyjob/src/protocol.rs").unwrap();
        assert_eq!(findings.len(), 2);
        assert!(
            findings
                .iter()
                .all(|finding| finding.rule == SNAPSHOT_TRANSFER_RULE)
        );
    }

    #[test]
    fn child_output_capture_is_confined_to_bounded() {
        for source in [
            "fn f(command: &mut Command) { command.output(); }",
            "fn f(child: Child) { child.wait_with_output(); }",
            "fn f(command: &mut Command) { Command::output(command); }",
        ] {
            assert_eq!(
                check_file(source, "crates/domyjob/src/snapshot.rs")
                    .unwrap()
                    .len(),
                1
            );
            assert!(
                check_file(source, "crates/domyjob/src/bounded.rs")
                    .unwrap()
                    .is_empty()
            );
            assert!(
                check_file(
                    &format!("#[cfg(test)] mod tests {{ {source} }}"),
                    "crates/domyjob/src/snapshot.rs"
                )
                .unwrap()
                .is_empty()
            );
        }
    }

    #[test]
    fn release_high_water_requires_the_locked_state_file() {
        for source in [
            "fn f(path: &Path) { state_file::read_json::<HighWater>(path); }",
            "fn f(path: &Path, value: &HighWater) { state_file::write_json(path, value); }",
        ] {
            let findings = check_file(source, "crates/domyjob/src/dist.rs").unwrap();
            assert_eq!(findings.len(), 2);
            assert!(
                findings
                    .iter()
                    .any(|finding| finding.rule == RELEASE_STATE_RULE)
            );
            assert!(
                check_file(
                    &format!("#[cfg(test)] mod tests {{ {source} }}"),
                    "crates/domyjob/src/dist.rs"
                )
                .unwrap()
                .is_empty()
            );
        }
    }

    #[test]
    fn client_origin_requires_locked_initialization() {
        for source in [
            "fn origin() { state_file::read_bytes(path); }",
            "fn origin() { state_file::write_bytes(path, bytes); }",
        ] {
            assert_eq!(
                check_file(source, "crates/domyjob/src/client.rs")
                    .unwrap()
                    .len(),
                1
            );
            assert!(
                check_file(source, "crates/domyjob/src/store.rs")
                    .unwrap()
                    .is_empty()
            );
        }
    }

    #[test]
    fn exclusive_create_is_not_a_lock_primitive() {
        let unapproved = "fn f(options: &mut OpenOptions) { options.create_new(true); }";
        assert_eq!(
            check_file(unapproved, "crates/domyjob/src/lock.rs")
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            check_file(
                "fn f(o: &mut O) { O::create_new(o, true); }",
                "crates/domyjob/src/lock.rs"
            )
            .unwrap()
            .len(),
            1
        );
        for (file, function) in EXCLUSIVE_CREATE {
            let source =
                format!("fn {function}(options: &mut OpenOptions) {{ options.create_new(true); }}");
            assert!(check_file(&source, file).unwrap().is_empty(), "{file}");
            assert_eq!(
                check_file(source.as_str(), "crates/domyjob/src/lock.rs")
                    .unwrap()
                    .len(),
                1
            );
        }
    }

    #[test]
    fn json_is_shaped_by_a_type_not_by_hand() {
        for source in [
            "fn f() { let _v = serde_json::json!({\"a\": 1}); }",
            "fn f() { let _v = json!([]); }",
        ] {
            assert_eq!(rules(source), [JSON_RULE], "{source}");
        }
        assert!(
            check_file("fn f() { let _v = json!({}); }", "src/mcp.rs")
                .unwrap()
                .is_empty()
        );
        assert!(rules("#[cfg(test)] mod tests { fn f() { let _v = json!(1); } }").is_empty());
        assert!(rules("fn f() { let _v = serde_json::to_value(&x); }").is_empty());
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
        assert_eq!(
            check_file(
                "fn f() { let _t = std::time::SystemTime::now(); }",
                "src/fake_clock.rs"
            )
            .unwrap()
            .len(),
            1
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
    fn clock_values_cannot_expose_numeric_decision_inputs() {
        let file = "src/clock.rs";
        for source in [
            "pub struct Timestamp(pub i64);",
            "pub struct Elapsed(i32);",
            "#[derive(PartialOrd)] pub struct Timestamp(i64);",
            "#[derive(PartialEq)] pub struct Timestamp(i64);",
            "#[cfg_attr(not(test), derive(PartialEq, Eq))] pub struct Timestamp(i64);",
            "#[derive(PartialEq)] pub struct Elapsed(i64);",
            "pub struct Elapsed(i64); impl Elapsed { pub fn millis(self) -> i64 { self.0 } }",
            "pub struct Timestamp(i64); impl Timestamp { pub fn raw(&self) -> Option<&i64> { Some(&self.0) } }",
            "pub struct Elapsed(i64); impl PartialOrd for Elapsed {}",
        ] {
            assert_rule(source, file, OPAQUE_TIME_RULE);
        }
        assert!(
            check_file(
                "#[cfg_attr(test, derive(PartialEq, Eq))] pub struct Timestamp(i64); pub struct Elapsed(i64); impl Elapsed { pub fn label(self) -> String { String::new() } }",
                file
            )
            .unwrap()
            .is_empty()
        );
        assert_rule(
            "impl PartialEq for Timestamp {}",
            "src/protocol.rs",
            OPAQUE_TIME_RULE,
        );
    }

    #[test]
    fn protocol_timing_requires_an_explicit_observation() {
        assert_rule(
            "fn timing() { let _ = Timestamp::observe(); }",
            "src/protocol.rs",
            EXPLICIT_PRESENTATION_TIME_RULE,
        );
        assert_rule(
            "fn timing() { let _ = Clock::observe(); }",
            "src/protocol.rs",
            EXPLICIT_PRESENTATION_TIME_RULE,
        );
        assert!(
            check_file(
                "fn timing(now: Timestamp) -> Timestamp { now }",
                "src/protocol.rs"
            )
            .unwrap()
            .is_empty()
        );
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
