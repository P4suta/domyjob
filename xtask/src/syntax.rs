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
    function: Option<String>,
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
        path: &["InsecureUnsigned", "acknowledged_on_the_command_line"],
        allowed_in: &["cli.rs"],
        rule: "only an explicit command-line flag may accept an unsigned binary",
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
];

const TIME_RULE: &str = "time decides nothing; wait for the event itself, and observe the clock only through clock::Timestamp";
const CLOCK_FILES: &[&str] = &["clock.rs"];
const LIVENESS_FILES: &[&str] = &["liveness.rs"];
const LIVENESS_TIME: &[&str] = &["Instant", "Duration", "wait_timeout", "elapsed"];
const TIME_TYPES: &[&str] = &["SystemTime", "Instant", "Duration", "UNIX_EPOCH"];
const TIME_METHODS: &[&str] = &["sleep", "modified", "accessed", "elapsed"];
const TIME_METHOD_PARTS: &[&str] = &["timeout", "deadline"];
const UNBOUNDED_READ_RULE: &str = "read input only through bounded.rs with an explicit byte budget";
const UNBOUNDED_CHILD_OUTPUT_RULE: &str =
    "capture child output only through bounded.rs with an explicit byte budget";
const RELEASE_STATE_RULE: &str =
    "release high-water state must be read and written through a locked StateFile";
const ORIGIN_STATE_RULE: &str = "the client origin must be initialized through a locked StateFile";
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
const PROJECT_PROOF_PRIVACY_RULE: &str = "project approval proof and request fields must stay private so callers cannot forge or reclassify them";
const MCP_LINE_RULE: &str = "MCP input lines must use bounded::line with an explicit byte budget";
const MCP_WORKER_RULE: &str =
    "MCP workers must be spawned only through dispatch with an McpDispatch permit";
const SNAPSHOT_BUDGET_RULE: &str =
    "snapshot file reads and worker counts must use fixed source budgets";
const QUEUE_WATCH_RULE: &str =
    "queue lock watchers must be registered through bounded QueueWatchers";
const JOB_IDS_RULE: &str = "production job ID traversal must stream through JobIds";
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
];
const TERMINAL_FILES: &[&str] = &["view.rs", "ui.rs", "board.rs", "history.rs", "cli.rs"];
const FAILURE_FILES: &[&str] = &[
    "failure.rs",
    "xtask/src/lib.rs",
    "xtask/src/proverif.rs",
    "xtask/src/release.rs",
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

fn is_test_module(attrs: &[syn::Attribute]) -> bool {
    attrs.iter().any(|attr| {
        attr.path().is_ident("cfg")
            && match &attr.meta {
                syn::Meta::List(list) => list.tokens.to_string().contains("test"),
                syn::Meta::Path(_) | syn::Meta::NameValue(_) => false,
            }
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

fn is_named_type(ty: &syn::Type, name: &str) -> bool {
    matches!(ty, syn::Type::Path(path) if path.path.segments.last().is_some_and(|segment| segment.ident == name))
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

fn returns_bool(output: &syn::ReturnType) -> bool {
    match output {
        syn::ReturnType::Type(_, ty) => type_carries_bool(ty),
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
        self.check_mcp_path(path);
        self.check_snapshot_path(path);
        self.check_queue_path(path);
        self.check_locked_state_path(path);
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

    fn check_signature(&mut self, sig: &syn::Signature) {
        if self.test_depth == 0 && self.file_is(DECISION_FILES) && returns_bool(&sig.output) {
            self.flag(
                sig.ident.span(),
                "security decisions return an enum, never bool",
            );
        }
    }

    fn check_project_structure(&mut self, item: &syn::ItemStruct) {
        let project_shape = if self.file_is(&["project.rs"]) && item.ident == "Project" {
            Some(field_is(&item.fields, "jobs", |ty| {
                is_generic_of(ty, "BTreeMap", &["JobName", "RepositoryRequest"])
            }))
        } else if self.file_is(&["project.rs"]) && item.ident == "RepositoryRequest" {
            Some(
                field_is(&item.fields, "on", |ty| {
                    is_named_type(ty, "ProjectSelector")
                }) && field_is(&item.fields, "dir", |ty| {
                    is_generic_of(ty, "Option", &["RelPath"])
                }) && field_is(&item.fields, "env", |ty| {
                    is_option_map_of(ty, "EnvName", "String")
                }),
            )
        } else if self.file_is(&["client.rs"]) && item.ident == "Order" {
            Some(field_is(&item.fields, "targets", |ty| {
                is_named_type(ty, "Targets")
            }))
        } else if self.file_is(&["project.rs"]) && item.ident == "ProjectOrderParts" {
            Some(field_is(&item.fields, "root", |ty| {
                is_named_type(ty, "PathBuf")
            }))
        } else if self.file_is(&["config.rs"]) && item.ident == "Config" {
            Some(field_is(&item.fields, "project_jobs", |ty| {
                is_vec_of(ty, "LocalPolicy")
            }))
        } else {
            None
        };
        if project_shape == Some(false) {
            self.flag(item.ident.span(), PROJECT_APPROVAL_RULE);
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

    fn exclusive_create_allowed(&self) -> bool {
        EXCLUSIVE_CREATE.iter().any(|(file, function)| {
            self.file == *file && self.function.as_deref() == Some(*function)
        })
    }
}

impl<'ast> Visit<'ast> for Gate {
    fn visit_lit_str(&mut self, literal: &'ast syn::LitStr) {
        if self.test_depth == 0 && self.file.starts_with("crates/domyjob/src/") {
            let value = literal.value();
            let stem = "domyjob.incoming";
            if value.split(stem).skip(1).any(|tail| !tail.starts_with('-')) {
                self.flag(literal.span(), INCOMING_RULE);
            }
        }
        syn::visit::visit_lit_str(self, literal);
    }

    fn visit_macro(&mut self, mac: &'ast syn::Macro) {
        if self.test_depth == 0
            && !self.file_is(JSON_FILES)
            && let Some(last) = mac.path.segments.last()
            && last.ident == "json"
        {
            self.flag(last.ident.span(), JSON_RULE);
        }
        syn::visit::visit_macro(self, mac);
    }

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

    fn visit_item_fn(&mut self, item: &'ast syn::ItemFn) {
        self.check_signature(&item.sig);
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
        self.check_signature(&item.sig);
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
        if carries_io_source(&item.fields) && !self.file_is(FAILURE_FILES) {
            self.flag(
                item.ident.span(),
                "an I/O failure is failure::IoFailure, with the action and path it happened at",
            );
        }
        self.check_input(
            &item.attrs,
            item.ident.span(),
            has_named_fields(&item.fields),
        );
        self.check_project_structure(item);
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

    fn visit_expr_struct(&mut self, expr: &'ast syn::ExprStruct) {
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
    let file = syn::parse_file(source)?;
    let mut gate = Gate {
        findings: Vec::new(),
        file: name.to_owned(),
        test_depth: 0,
        function: None,
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
    fn setup_staging_cannot_use_a_fixed_incoming_name() {
        let fixed = r#"fn f() { let _ = "domyjob.incoming.exe"; }"#;
        let scoped = r#"fn f() { let _ = "domyjob.incoming-"; }"#;
        let file = "crates/domyjob/src/remote.rs";
        assert_eq!(
            check_file(fixed, file).unwrap().first().map(|f| f.rule),
            Some(INCOMING_RULE)
        );
        assert!(check_file(scoped, file).unwrap().is_empty());
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
                "struct RepositoryRequest { on: ProjectSelector, dir: Option<RelPath>, env: Option<BTreeMap<EnvName, String>> }",
                "crates/domyjob/src/project.rs"
            )
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
            assert_eq!(
                check_file(source, "crates/domyjob/src/dist.rs")
                    .unwrap()
                    .len(),
                1
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
