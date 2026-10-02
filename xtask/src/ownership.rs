use syn::visit::Visit;

const REPLACEMENT: &str = "crates/domyjob/src/platform/replacement.rs";
const LOCK: &str = "crates/domyjob/src/lock.rs";
const MAIN: &str = "crates/domyjob/src/main.rs";
const KERNEL: &str = "crates/domyjob/src/process/windows/kernel.rs";
const DESCRIPTOR: &str = "crates/domyjob/src/platform/windows_acl/descriptor.rs";
const RELEASE: &str = "xtask/src/release.rs";
const DISTRIBUTION: &str = "xtask/src/distribution.rs";
const CI: &str = "xtask/src/ci.rs";
const RELEASE_QUEUE: &str = "xtask/src/release_queue.rs";
const RELEASE_ORCHESTRATION: &str = "xtask/src/release_orchestration.rs";
const MACOS_PACKAGE: &str = "xtask/src/macos_package.rs";
const RELEASE_READY: &str = "xtask/src/release_ready.rs";
const DEPENDENCIES: &str = "xtask/src/dependencies.rs";

const CAPABILITIES: &[&str] = &[
    "Event",
    "EventRef",
    "Process",
    "ProcessRef",
    "Job",
    "AssignedJob",
    "WatchedJob",
    "RunningJob",
    "Thread",
    "ThreadSnapshot",
    "Success",
    "SecurityDescriptor",
    "Acl",
    "SidRef",
    "Sid",
    "FileSecurity",
    "TokenUserBuffer",
    "LocalAllocation",
    "Ace",
    "StagedFile",
    "OsLock",
    "Secret",
    "PrivateFiles",
    "SigningKeychain",
    "StagedArchive",
    "ChildBuildDirectory",
    "NotaryKeyFile",
    "AcceptedToken",
    "ExtractedBundle",
    "StagedReceipt",
    "Origin",
    "VerifiedHandoff",
    "VerifiedDistribution",
    "VerifiedReady",
    "ReviewedDistribution",
    "AuditedLockfile",
    "VerifiedWindowsBinary",
    "WindowsSigningIdentity",
    "VerifiedPackage",
    "StapledPackage",
    "ReadyDistribution",
    "ExtractedPackage",
    "ReceiptDestination",
];

const COPYABLE_CAPABILITIES: &[&str] = &[
    "EventRef",
    "ProcessRef",
    "Success",
    "Acl",
    "SidRef",
    "Sid",
    "Ace",
];

fn capability(kind: &syn::Type) -> bool {
    matches!(kind, syn::Type::Path(path) if path.path.segments.last().is_some_and(|part| CAPABILITIES.contains(&identifier_name(&part.ident).as_str())))
}

fn capability_owner(owner: &str) -> bool {
    matches!(
        owner,
        KERNEL
            | DESCRIPTOR
            | LOCK
            | REPLACEMENT
            | RELEASE
            | DISTRIBUTION
            | CI
            | RELEASE_QUEUE
            | RELEASE_ORCHESTRATION
            | MACOS_PACKAGE
            | RELEASE_READY
            | DEPENDENCIES
    )
}

#[derive(Default)]
struct RawResource(bool);

impl<'ast> Visit<'ast> for RawResource {
    fn visit_type_path(&mut self, path: &'ast syn::TypePath) {
        if path.path.segments.last().is_some_and(|part| {
            matches!(
                identifier_name(&part.ident).as_str(),
                "HANDLE"
                    | "OwnedHandle"
                    | "BorrowedHandle"
                    | "File"
                    | "TempPath"
                    | "NamedTempFile"
                    | "NonNull"
                    | "c_void"
            )
        }) {
            self.0 = true;
        }
        syn::visit::visit_type_path(self, path);
    }

    fn visit_type_ptr(&mut self, pointer: &'ast syn::TypePtr) {
        self.0 = true;
        syn::visit::visit_type_ptr(self, pointer);
    }
}

const RAW_OWNERS: &[&str] = &[
    "crates/domyjob-core/src/ingress.rs",
    "crates/domyjob/build.rs",
    "crates/domyjob/src/bounded.rs",
    "crates/domyjob/src/builds.rs",
    "crates/domyjob/src/output.rs",
    "crates/domyjob/src/platform.rs",
    "crates/domyjob/src/platform/clock.rs",
    "crates/domyjob/src/process.rs",
    "crates/domyjob/src/state_io.rs",
    "crates/domyjob/src/testing.rs",
    "crates/domyjob/src/workspace.rs",
    "xtask/src/lib.rs",
    "xtask/src/main.rs",
    "xtask/src/release.rs",
    RELEASE_QUEUE,
    RELEASE_ORCHESTRATION,
    MACOS_PACKAGE,
    RELEASE_READY,
    CI,
    "xtask/src/distribution.rs",
    "xtask/tests/cli.rs",
    REPLACEMENT,
    LOCK,
    MAIN,
];

fn file_lock(name: &str) -> bool {
    matches!(
        name,
        "lock" | "lock_shared" | "try_lock" | "try_lock_shared" | "unlock"
    )
}

fn identifier_name(identifier: &syn::Ident) -> String {
    let name = identifier.to_string();
    name.strip_prefix("r#").unwrap_or(&name).to_owned()
}

fn path_is_ident(path: &syn::Path, name: &str) -> bool {
    path.leading_colon.is_none()
        && path.segments.len() == 1
        && path.segments.first().is_some_and(|part| {
            matches!(part.arguments, syn::PathArguments::None)
                && identifier_name(&part.ident) == name
        })
}

struct Ownership<'a> {
    owner: &'a str,
    raw: bool,
    module_depth: usize,
    function_depth: usize,
    ci_command_owner: bool,
    findings: Vec<String>,
}

impl Ownership<'_> {
    fn nested_callable(&mut self, visit: impl FnOnce(&mut Self)) {
        let previous = self.ci_command_owner;
        self.ci_command_owner = false;
        self.function_depth = self.function_depth.saturating_add(1);
        visit(self);
        self.function_depth = self.function_depth.saturating_sub(1);
        self.ci_command_owner = previous;
    }

    fn ci_command(&mut self, span: proc_macro2::Span) {
        if self.owner == CI && !self.ci_command_owner {
            self.findings.push(format!(
                "{}: raw CI command creation belongs to the private policy factory",
                span.start().line
            ));
        }
    }

    fn ci_constructor(&mut self, span: proc_macro2::Span) {
        if self.owner == CI {
            self.findings.push(format!(
                "{}: CI Command constructors must use the private policy factory",
                span.start().line
            ));
        }
    }

    fn owned_type(
        &mut self,
        name: &syn::Ident,
        attributes: &[syn::Attribute],
        fields: &syn::Fields,
    ) {
        let type_name = identifier_name(name);
        if !capability_owner(self.owner) || !CAPABILITIES.contains(&type_name.as_str()) {
            return;
        }
        for field in fields {
            if !matches!(field.vis, syn::Visibility::Inherited) {
                self.findings.push(format!(
                    "{}: ownership capability fields are private",
                    name.span().start().line
                ));
            }
        }
        for attribute in attributes {
            if path_is_ident(attribute.path(), "cfg_attr") {
                self.findings.push(format!(
                    "{}: ownership capability declarations cannot conditionally change attributes",
                    name.span().start().line
                ));
            }
            if path_is_ident(attribute.path(), "derive")
                && attribute
                    .parse_args_with(
                        syn::punctuated::Punctuated::<syn::Path, syn::Token![,]>::parse_terminated,
                    )
                    .is_ok_and(|paths| {
                        paths.iter().any(|path| {
                            path.segments.last().is_some_and(|part| {
                                let derive_name = identifier_name(&part.ident);
                                type_name == "Secret" && derive_name == "Debug"
                                    || !matches!(
                                        derive_name.as_str(),
                                        "Debug" | "Clone" | "Copy" | "PartialEq" | "Eq"
                                    )
                                    || !COPYABLE_CAPABILITIES.contains(&type_name.as_str())
                                        && matches!(derive_name.as_str(), "Clone" | "Copy")
                            })
                        })
                    })
            {
                self.findings.push(format!(
                    "{}: capability derives cannot manufacture defaults, expose raw access, or clone an owned resource",
                    name.span().start().line
                ));
            }
        }
    }

    fn operation(&mut self, name: &str, span: proc_macro2::Span) {
        let reserved = match name {
            "persist" | "persist_noclobber" => Some("use StagedFile for replacement"),
            "keep" if self.owner != "crates/domyjob/tests/e2e/world.rs" => {
                Some("staged cleanup is owned by StagedFile")
            }
            "into_temp_path" | "disable_cleanup" if self.owner != REPLACEMENT || !self.raw => {
                Some("staged cleanup is owned by StagedFile")
            }
            "of_process" if self.owner != MAIN || !self.raw => {
                Some("only main obtains process output")
            }
            name if self.raw && file_lock(name) && self.owner != LOCK => {
                Some("file lock operations are owned by OsLock")
            }
            "rename"
                if self.raw
                    && !matches!(
                        self.owner,
                        REPLACEMENT
                            | "crates/domyjob/src/state_io.rs"
                            | DISTRIBUTION
                            | RELEASE_READY
                    ) =>
            {
                Some(
                    "rename belongs to staged replacement, checked state directories, or release archive staging",
                )
            }
            _ => None,
        };
        if let Some(reason) = reserved {
            self.findings
                .push(format!("{}: {reason}", span.start().line));
        }
    }

    fn sdk(&mut self, name: &str, span: proc_macro2::Span) {
        if name == "windows_sys"
            && !matches!(
                self.owner,
                KERNEL | DESCRIPTOR | "crates/domyjob/tests/e2e/os/windows.rs"
            )
        {
            self.findings.push(format!(
                "{}: Windows SDK imports and calls belong in the kernel or descriptor leaf",
                span.start().line
            ));
        }
    }

    fn tokens(&mut self, tokens: proc_macro2::TokenStream) {
        let mut previous = false;
        let mut raw_path = false;
        let mut constructor_path = false;
        for token in tokens {
            match token {
                proc_macro2::TokenTree::Ident(ident) => {
                    let name = identifier_name(&ident);
                    if raw_path && name == "command" {
                        self.ci_command(ident.span());
                    }
                    if constructor_path && name == "new" {
                        self.ci_constructor(ident.span());
                    }
                    raw_path = name == "raw";
                    constructor_path = name == "Command";
                    self.sdk(&name, ident.span());
                    if previous {
                        self.operation(&name, ident.span());
                    }
                    previous = false;
                }
                proc_macro2::TokenTree::Punct(punct) => {
                    previous = matches!(punct.as_char(), '.' | ':');
                    raw_path &= punct.as_char() == ':';
                    constructor_path &= punct.as_char() == ':';
                }
                proc_macro2::TokenTree::Group(group) => {
                    self.tokens(group.stream());
                    previous = false;
                    raw_path = false;
                    constructor_path = false;
                }
                proc_macro2::TokenTree::Literal(_) => {
                    previous = false;
                    raw_path = false;
                    constructor_path = false;
                }
            }
        }
    }
}

impl<'ast> Visit<'ast> for Ownership<'_> {
    fn visit_item_fn(&mut self, item: &'ast syn::ItemFn) {
        let previous = self.ci_command_owner;
        self.ci_command_owner = self.owner == CI
            && (self.module_depth == 0
                && self.function_depth == 0
                && identifier_name(&item.sig.ident) == "command"
                && matches!(item.vis, syn::Visibility::Inherited)
                || identifier_name(&item.sig.ident)
                    == "ci_process_policy_overrides_inherited_automatic_provisioning"
                    && item
                        .attrs
                        .iter()
                        .any(|attribute| path_is_ident(attribute.path(), "test")));
        self.function_depth = self.function_depth.saturating_add(1);
        syn::visit::visit_item_fn(self, item);
        self.function_depth = self.function_depth.saturating_sub(1);
        self.ci_command_owner = previous;
    }

    fn visit_impl_item_fn(&mut self, item: &'ast syn::ImplItemFn) {
        self.nested_callable(|policy| syn::visit::visit_impl_item_fn(policy, item));
    }

    fn visit_trait_item_fn(&mut self, item: &'ast syn::TraitItemFn) {
        self.nested_callable(|policy| syn::visit::visit_trait_item_fn(policy, item));
    }

    fn visit_item_enum(&mut self, item: &'ast syn::ItemEnum) {
        self.owned_type(&item.ident, &item.attrs, &syn::Fields::Unit);
        for variant in &item.variants {
            self.owned_type(&item.ident, &[], &variant.fields);
        }
        syn::visit::visit_item_enum(self, item);
    }

    fn visit_item_type(&mut self, item: &'ast syn::ItemType) {
        if capability_owner(self.owner) {
            self.findings.push(format!(
                "{}: capability owners spell their types directly without aliases",
                item.ident.span().start().line
            ));
        }
        syn::visit::visit_item_type(self, item);
    }

    fn visit_item_struct(&mut self, item: &'ast syn::ItemStruct) {
        self.owned_type(&item.ident, &item.attrs, &item.fields);
        syn::visit::visit_item_struct(self, item);
    }

    fn visit_item_impl(&mut self, implementation: &'ast syn::ItemImpl) {
        let owns_capability = capability_owner(self.owner) && capability(&implementation.self_ty);
        if owns_capability
            && implementation
                .trait_
                .as_ref()
                .is_some_and(|(path, _)| !path_is_ident(path, "Drop"))
        {
            self.findings.push(format!(
                "{}: ownership capabilities only implement Drop explicitly; other traits cannot expose or duplicate their resource",
                implementation.impl_token.span.start().line
            ));
        }
        if owns_capability {
            for item in &implementation.items {
                if let syn::ImplItem::Fn(method) = item
                    && !matches!(method.vis, syn::Visibility::Inherited)
                    && let syn::ReturnType::Type(_, result) = &method.sig.output
                {
                    let mut raw = RawResource::default();
                    raw.visit_type(result);
                    if raw.0 {
                        self.findings.push(format!("{}: capability methods return checked values instead of their raw resource", method.sig.ident.span().start().line));
                    }
                }
            }
        }
        syn::visit::visit_item_impl(self, implementation);
    }

    fn visit_item_mod(&mut self, module: &'ast syn::ItemMod) {
        let raw = self.raw;
        self.module_depth = self.module_depth.saturating_add(1);
        if identifier_name(&module.ident) == "raw" {
            if !RAW_OWNERS.contains(&self.owner) {
                self.findings.push(format!(
                    "{}: this file does not own a raw effect exception",
                    module.ident.span().start().line
                ));
            }
            self.raw = true;
        }
        syn::visit::visit_item_mod(self, module);
        self.raw = raw;
        self.module_depth = self.module_depth.saturating_sub(1);
    }

    fn visit_expr_method_call(&mut self, expression: &'ast syn::ExprMethodCall) {
        self.operation(
            &identifier_name(&expression.method),
            expression.method.span(),
        );
        syn::visit::visit_expr_method_call(self, expression);
    }

    fn visit_expr_path(&mut self, expression: &'ast syn::ExprPath) {
        if expression.path.segments.len() > 1
            && let Some(last) = expression.path.segments.last()
        {
            self.operation(&identifier_name(&last.ident), last.ident.span());
        }
        if let Some(qualified) = &expression.qself
            && let syn::Type::Path(kind) = qualified.ty.as_ref()
            && kind
                .path
                .segments
                .last()
                .is_some_and(|part| identifier_name(&part.ident) == "Command")
            && let Some(last) = expression.path.segments.last()
            && identifier_name(&last.ident) == "new"
        {
            self.ci_constructor(last.ident.span());
        }
        syn::visit::visit_expr_path(self, expression);
    }

    fn visit_path(&mut self, path: &'ast syn::Path) {
        if let Some(last) = path.segments.last()
            && let Some(parent) = path.segments.iter().rev().nth(1)
        {
            match (
                identifier_name(&parent.ident).as_str(),
                identifier_name(&last.ident).as_str(),
            ) {
                ("raw", "command") => self.ci_command(last.ident.span()),
                ("Command", "new") => self.ci_constructor(last.ident.span()),
                _ => {}
            }
        }
        for part in &path.segments {
            self.sdk(&identifier_name(&part.ident), part.ident.span());
        }
        syn::visit::visit_path(self, path);
    }

    fn visit_use_tree(&mut self, tree: &'ast syn::UseTree) {
        if self.owner == CI
            && let syn::UseTree::Name(name) = tree
            && identifier_name(&name.ident) == "command"
        {
            self.findings.push(format!(
                "{}: CI command imports cannot bypass the private policy factory",
                name.ident.span().start().line
            ));
        }
        if capability_owner(self.owner) {
            let hidden_type = match tree {
                syn::UseTree::Rename(rename) if identifier_name(&rename.rename) != "_" => {
                    Some(rename.ident.span())
                }
                syn::UseTree::Glob(glob) => Some(glob.star_token.span),
                syn::UseTree::Path(_)
                | syn::UseTree::Name(_)
                | syn::UseTree::Rename(_)
                | syn::UseTree::Group(_) => None,
            };
            if let Some(span) = hidden_type {
                self.findings.push(format!(
                    "{}: capability imports cannot alias types or traits or use globs",
                    span.start().line
                ));
            }
        }
        let hidden_import = match tree {
            syn::UseTree::Glob(glob) => Some(glob.star_token.span),
            syn::UseTree::Rename(rename) if identifier_name(&rename.rename) != "_" => {
                Some(rename.ident.span())
            }
            syn::UseTree::Name(name)
                if !matches!(identifier_name(&name.ident).as_str(), "io" | "self")
                    && identifier_name(&name.ident).starts_with(char::is_lowercase) =>
            {
                Some(name.ident.span())
            }
            syn::UseTree::Path(_)
            | syn::UseTree::Name(_)
            | syn::UseTree::Rename(_)
            | syn::UseTree::Group(_) => None,
        };
        if self.raw
            && let Some(span) = hidden_import
        {
            self.findings.push(format!(
                "{}: raw imports name their types and modules without function imports, aliases, or globs",
                span.start().line
            ));
        }
        match tree {
            syn::UseTree::Path(path) => self.sdk(&identifier_name(&path.ident), path.ident.span()),
            syn::UseTree::Name(name) => self.sdk(&identifier_name(&name.ident), name.ident.span()),
            syn::UseTree::Rename(rename) => {
                self.sdk(&identifier_name(&rename.ident), rename.ident.span());
            }
            syn::UseTree::Glob(_) | syn::UseTree::Group(_) => {}
        }
        syn::visit::visit_use_tree(self, tree);
    }

    fn visit_macro(&mut self, invocation: &'ast syn::Macro) {
        self.tokens(invocation.tokens.clone());
        syn::visit::visit_macro(self, invocation);
    }
}

#[must_use]
pub fn check(owner: &str, file: &syn::File) -> Vec<String> {
    let mut policy = Ownership {
        owner,
        raw: false,
        module_depth: 0,
        function_depth: 0,
        ci_command_owner: false,
        findings: Vec::new(),
    };
    policy.visit_file(file);
    policy.findings
}

#[cfg(test)]
mod tests {
    use super::{
        CI, DEPENDENCIES, DESCRIPTOR, DISTRIBUTION, KERNEL, LOCK, MACOS_PACKAGE, MAIN, RELEASE,
        RELEASE_ORCHESTRATION, RELEASE_QUEUE, RELEASE_READY, REPLACEMENT, check,
    };

    fn rejected(owner: &str, source: &str) {
        assert!(
            !check(owner, &syn::parse_file(source).unwrap()).is_empty(),
            "{source}"
        );
    }

    #[test]
    fn ci_commands_keep_one_policy_factory_and_an_explicit_override_control() {
        for source in [
            "fn command() { crate::raw::command(\"mise\"); }",
            "fn r#command() { crate::r#raw::r#command(\"mise\"); }",
            "#[test] fn ci_process_policy_overrides_inherited_automatic_provisioning() { crate::raw::command(\"mise\"); }",
            "fn check() { command(\"mise\"); super::command(\"cargo\"); }",
        ] {
            assert_eq!(check(CI, &syn::parse_file(source).unwrap()).len(), 0);
        }
        for source in [
            "fn check() { crate::raw::command(\"mise\"); }",
            "fn check() { crate::raw::r#command(\"mise\"); }",
            "fn check() { crate::r#raw::command(\"mise\"); }",
            "fn check() { let create = raw::command; }",
            "fn check() { let create = crate::r#raw::r#command; }",
            "fn check() { forward!(crate::raw::command(\"mise\")); }",
            "fn check() { forward!(raw::command(\"mise\")); }",
            "fn check() { forward!(crate::r#raw::r#command(\"mise\")); }",
            "mod nested { fn command() { crate::raw::command(\"mise\"); } }",
            "fn bypass() { fn command() { crate::raw::command(\"mise\"); } }",
            "impl Context { fn command() { crate::raw::command(\"mise\"); } }",
            "fn command() { impl Context { fn bypass() { crate::raw::command(\"mise\"); } } }",
            "fn command() { trait Context { fn bypass() { crate::raw::command(\"mise\"); } } }",
            "pub fn command() { crate::raw::command(\"mise\"); }",
            "fn ci_process_policy_overrides_inherited_automatic_provisioning() { crate::raw::command(\"mise\"); }",
            "use crate::raw::command; fn check() { command(\"mise\"); }",
            "use crate::raw::r#command;",
            "use crate::r#raw::command;",
            "use crate::raw::command as create;",
            "use crate::raw as effects;",
            "use crate::raw::*;",
            "mod raw { fn check() { std::process::Command::new(\"mise\"); } }",
            "fn command() { std::process::Command::new(\"mise\"); }",
            "fn check() { let create = Command::new; }",
            "fn check() { let create = <std::process::Command>::new; }",
            "fn check() { forward!(std::process::r#Command::r#new(\"mise\")); }",
        ] {
            rejected(CI, source);
        }
        assert_eq!(
            check(
                RELEASE,
                &syn::parse_file("fn check() { std::process::Command::new(\"tool\"); }").unwrap()
            )
            .len(),
            0
        );
    }

    #[test]
    fn raw_identifier_spellings_preserve_ownership_rejections_and_valid_owners() {
        for (owner, ordinary, raw) in [
            (
                RELEASE_ORCHESTRATION,
                "struct Origin { pub resource: Resource }",
                "struct r#Origin { pub resource: Resource }",
            ),
            (
                RELEASE_ORCHESTRATION,
                "#[derive(Clone)] struct Origin(Resource);",
                "#[r#derive(r#Clone)] struct r#Origin(Resource);",
            ),
            (
                KERNEL,
                "#[cfg_attr(all(), derive(Default))] struct Success(());",
                "#[r#cfg_attr(all(), r#derive(r#Default))] struct r#Success(());",
            ),
            (
                KERNEL,
                "impl Event { pub fn file(&self) -> File { todo!() } }",
                "impl r#Event { pub fn file(&self) -> r#File { todo!() } }",
            ),
            (
                "crates/domyjob/src/process/windows.rs",
                "use windows_sys::Win32::Foundation::HANDLE;",
                "use r#windows_sys::Win32::Foundation::HANDLE;",
            ),
            (
                "crates/domyjob/src/process/windows.rs",
                "forward!(windows_sys::Win32::Foundation::CloseHandle(h));",
                "forward!(r#windows_sys::Win32::Foundation::CloseHandle(h));",
            ),
            (
                REPLACEMENT,
                "mod raw { fn f() { file.persist(path); } }",
                "mod r#raw { fn f() { file.r#persist(path); } }",
            ),
            (
                RELEASE,
                "mod raw { fn f() { forward!(file.rename(path)); } }",
                "mod r#raw { fn f() { forward!(file.r#rename(path)); } }",
            ),
        ] {
            let expected = check(owner, &syn::parse_file(ordinary).unwrap());
            assert!(!expected.is_empty(), "{ordinary}");
            assert_eq!(
                check(owner, &syn::parse_file(raw).unwrap()),
                expected,
                "{raw}"
            );
        }
        for (owner, ordinary, raw) in [
            (
                RELEASE_ORCHESTRATION,
                "struct Origin(Resource); impl Drop for Origin { fn drop(&mut self) {} }",
                "struct r#Origin(Resource); impl r#Drop for r#Origin { fn drop(&mut self) {} }",
            ),
            (
                KERNEL,
                "#[derive(Clone, Copy)] struct EventRef(Resource);",
                "#[r#derive(r#Clone, r#Copy)] struct r#EventRef(Resource);",
            ),
            (
                KERNEL,
                "use windows_sys::Win32::Foundation::HANDLE;",
                "use r#windows_sys::Win32::Foundation::HANDLE;",
            ),
            (
                REPLACEMENT,
                "mod raw { fn f() { file.into_temp_path(); file.disable_cleanup(true); } }",
                "mod r#raw { fn f() { file.r#into_temp_path(); file.r#disable_cleanup(true); } }",
            ),
        ] {
            assert_eq!(check(owner, &syn::parse_file(ordinary).unwrap()).len(), 0);
            assert_eq!(check(owner, &syn::parse_file(raw).unwrap()).len(), 0);
        }
    }

    #[test]
    fn raw_exceptions_cannot_restore_forbidden_replacement_apis() {
        for owner in [REPLACEMENT, "crates/domyjob/src/state_io.rs"] {
            for expression in [
                "file.persist(path)",
                "Alias::persist(file, path)",
                "forward!(file.persist(path))",
            ] {
                rejected(owner, &format!("mod raw {{ fn f() {{ {expression}; }} }}"));
            }
        }
        rejected("crates/domyjob/src/other.rs", "mod raw { fn f() {} }");
        for imported in [
            "use std::fs::rename as replace;",
            "use std::fs::rename;",
            "use std::fs::*;",
            "use std::fs as files;",
            "use std::fs::File as Alias;",
        ] {
            rejected(REPLACEMENT, &format!("mod raw {{ {imported} }}"));
        }
    }

    #[test]
    fn closing_and_retaining_staged_paths_has_one_owner() {
        let source =
            "mod raw { fn f(file: T) { file.into_temp_path(); file.disable_cleanup(true); } }";
        assert_eq!(
            check(REPLACEMENT, &syn::parse_file(source).unwrap()).len(),
            0
        );
        rejected("crates/domyjob/src/state_io.rs", source);
        rejected(REPLACEMENT, "fn f(file: T) { file.into_temp_path(); }");
        rejected(REPLACEMENT, "mod raw { fn f(file: T) { file.keep(); } }");
    }

    #[test]
    fn task_resources_cannot_expose_secrets_or_duplicate_checked_owners() {
        for (owner, names) in [
            (
                RELEASE,
                &[
                    "Secret",
                    "PrivateFiles",
                    "SigningKeychain",
                    "NotaryKeyFile",
                    "AcceptedToken",
                    "ReceiptDestination",
                ][..],
            ),
            (
                DISTRIBUTION,
                &[
                    "StagedArchive",
                    "VerifiedWindowsBinary",
                    "WindowsSigningIdentity",
                ][..],
            ),
            (DEPENDENCIES, &["AuditedLockfile"][..]),
            (CI, &["ChildBuildDirectory"][..]),
            (RELEASE_QUEUE, &["ExtractedBundle", "StagedReceipt"][..]),
            (
                RELEASE_ORCHESTRATION,
                &[
                    "Origin",
                    "VerifiedHandoff",
                    "VerifiedDistribution",
                    "VerifiedReady",
                    "ReviewedDistribution",
                ][..],
            ),
            (
                MACOS_PACKAGE,
                &["VerifiedPackage", "StapledPackage", "ExtractedPackage"][..],
            ),
            (RELEASE_READY, &["ReadyDistribution"][..]),
        ] {
            for name in names {
                for source in [
                    format!("struct {name} {{ pub resource: Resource }}"),
                    format!("#[derive(Clone)] struct {name}(Resource);"),
                    format!("#[derive(Default)] struct {name}(Resource);"),
                    format!("impl Deref for {name} {{ type Target = Resource; }}"),
                ] {
                    rejected(owner, &source);
                }
                let private = format!(
                    "struct {name}(Resource); impl Drop for {name} {{ fn drop(&mut self) {{}} }}"
                );
                assert_eq!(check(owner, &syn::parse_file(&private).unwrap()).len(), 0);
            }
        }
        rejected(RELEASE, "#[derive(Debug)] struct Secret(String);");
        rejected(RELEASE, "impl Debug for Secret {}");
        rejected(RELEASE, "impl Display for Secret {}");
        let staged = "mod raw { fn rename() { std::fs::rename(source, target); } }";
        assert_eq!(
            check(DISTRIBUTION, &syn::parse_file(staged).unwrap()).len(),
            0
        );
        assert_eq!(
            check(RELEASE_READY, &syn::parse_file(staged).unwrap()).len(),
            0
        );
        rejected(MACOS_PACKAGE, staged);
        rejected(RELEASE, staged);
    }

    #[test]
    fn raw_file_lock_calls_cannot_be_added_to_another_adapter() {
        for expression in [
            "file.lock()",
            "File::unlock(file)",
            "forward!(file.try_lock())",
        ] {
            let source = format!("mod raw {{ fn f() {{ {expression}; }} }}");
            assert_eq!(check(LOCK, &syn::parse_file(&source).unwrap()).len(), 0);
            rejected("crates/domyjob/src/builds.rs", &source);
        }
        assert_eq!(
            check(
                "crates/domyjob/src/store.rs",
                &syn::parse_file("fn f() { mutex.lock(); }").unwrap()
            )
            .len(),
            0
        );
    }

    #[test]
    fn output_aliases_and_macro_tokens_do_not_bypass_ownership() {
        for source in [
            "use crate::output::Output as Alias; fn f() { Alias::of_process(); }",
            "type Alias = crate::output::Output; fn f() { Alias::of_process(); }",
            "fn f() { forward!(Alias::of_process()); }",
            "fn f() { let constructor = Alias::of_process; constructor(); }",
        ] {
            rejected("crates/domyjob/src/chat/cli.rs", source);
            rejected(MAIN, source);
        }
        assert_eq!(
            check(
                MAIN,
                &syn::parse_file("mod raw { fn f() { Output::of_process(); } }").unwrap()
            )
            .len(),
            0
        );
    }

    #[test]
    fn sdk_imports_aliases_and_macro_calls_have_exact_leaf_owners() {
        for source in [
            "use windows_sys::Win32::Foundation::HANDLE;",
            "fn f() { windows_sys::Win32::Foundation::CloseHandle(h); }",
            "forward!(windows_sys::Win32::Foundation::CloseHandle(h));",
        ] {
            for owner in [KERNEL, DESCRIPTOR] {
                assert_eq!(check(owner, &syn::parse_file(source).unwrap()).len(), 0);
            }
            rejected("crates/domyjob/src/process/windows.rs", source);
        }
        for owner in [KERNEL, DESCRIPTOR, "crates/domyjob/src/process/windows.rs"] {
            rejected(owner, "use windows_sys as sdk;");
        }
    }

    #[test]
    fn capability_fields_defaults_and_raw_escape_traits_cannot_relax_contracts() {
        for source in [
            "struct Event { pub(super) handle: HANDLE }",
            "#[derive(Default)] struct Success(());",
            "#[derive(core::default::Default)] struct OsLock(());",
            "impl Default for Job<Configured> { fn default() -> Self { todo!() } }",
            "impl Deref for StagedFile { type Target = TempPath; }",
            "impl AsRawHandle for Process<Watch> {}",
            "impl OsLock { pub(crate) fn file(&self) -> &File { todo!() } }",
            "impl Event<Signal> { pub(super) fn raw(&self) -> HANDLE { todo!() } }",
            "#[derive(Clone)] struct OsLock(());",
            "#[derive(Clone)] struct LocalAllocation(NonNull<c_void>);",
            "#[derive(derive_more::Deref)] struct Event(HANDLE);",
            "impl Clone for OsLock { fn clone(&self) -> Self { todo!() } }",
            "impl Leak for OsLock { fn file(self) -> File { todo!() } }",
            "#[cfg_attr(all(), derive(Default))] struct Success(());",
            "#[derive(Default)] enum Ace { #[default] Unsupported }",
        ] {
            rejected(KERNEL, source);
        }
        assert_eq!(
            check(
                KERNEL,
                &syn::parse_file(
                    "struct Event { handle: HANDLE } impl Event { fn signal(&self) {} }"
                )
                .unwrap()
            )
            .len(),
            0
        );
    }

    #[test]
    fn capability_aliases_cannot_hide_owners_raw_returns_or_escape_traits() {
        for source in [
            "type Alias = OsLock; impl Alias { pub(crate) fn file(self) -> File { todo!() } }",
            "type Raw = File; impl OsLock { pub(crate) fn file(self) -> Raw { todo!() } }",
            "use self::OsLock as Alias; impl Alias { pub(crate) fn file(self) -> File { todo!() } }",
            "use std::fs::File as Raw; impl OsLock { pub(crate) fn file(self) -> Raw { todo!() } }",
            "use std::default::Default as Manufactured; impl Manufactured for OsLock {}",
            "use std::os::windows::io::AsRawHandle as Escape; impl Escape for Event<Signal> {}",
            "use std::fs::*;",
        ] {
            for owner in [KERNEL, DESCRIPTOR, LOCK, REPLACEMENT] {
                rejected(owner, source);
            }
        }
        assert_eq!(check(KERNEL, &syn::parse_file("use std::os::windows::io::AsRawHandle as _; impl Drop for Event<Signal> { fn drop(&mut self) {} }").unwrap()).len(), 0);
    }
}
