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
    matches!(kind, syn::Type::Path(path) if path.path.segments.last().is_some_and(|part| CAPABILITIES.contains(&part.ident.to_string().as_str())))
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
    )
}

#[derive(Default)]
struct RawResource(bool);

impl<'ast> Visit<'ast> for RawResource {
    fn visit_type_path(&mut self, path: &'ast syn::TypePath) {
        if path.path.segments.last().is_some_and(|part| {
            matches!(
                part.ident.to_string().as_str(),
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

struct Ownership<'a> {
    owner: &'a str,
    raw: bool,
    findings: Vec<String>,
}

impl Ownership<'_> {
    fn owned_type(
        &mut self,
        name: &syn::Ident,
        attributes: &[syn::Attribute],
        fields: &syn::Fields,
    ) {
        if !capability_owner(self.owner) || !CAPABILITIES.contains(&name.to_string().as_str()) {
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
            if attribute.path().is_ident("cfg_attr") {
                self.findings.push(format!(
                    "{}: ownership capability declarations cannot conditionally change attributes",
                    name.span().start().line
                ));
            }
            if attribute.path().is_ident("derive")
                && attribute
                    .parse_args_with(
                        syn::punctuated::Punctuated::<syn::Path, syn::Token![,]>::parse_terminated,
                    )
                    .is_ok_and(|paths| {
                        paths.iter().any(|path| {
                            path.segments.last().is_some_and(|part| {
                                name == "Secret" && part.ident == "Debug"
                                    || !matches!(
                                        part.ident.to_string().as_str(),
                                        "Debug" | "Clone" | "Copy" | "PartialEq" | "Eq"
                                    )
                                    || !COPYABLE_CAPABILITIES.contains(&name.to_string().as_str())
                                        && matches!(
                                            part.ident.to_string().as_str(),
                                            "Clone" | "Copy"
                                        )
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
                        REPLACEMENT | "crates/domyjob/src/state_io.rs" | DISTRIBUTION
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
        for token in tokens {
            match token {
                proc_macro2::TokenTree::Ident(ident) => {
                    let name = ident.to_string();
                    self.sdk(&name, ident.span());
                    if previous {
                        self.operation(&name, ident.span());
                    }
                    previous = false;
                }
                proc_macro2::TokenTree::Punct(punct) => {
                    previous = matches!(punct.as_char(), '.' | ':');
                }
                proc_macro2::TokenTree::Group(group) => {
                    self.tokens(group.stream());
                    previous = false;
                }
                proc_macro2::TokenTree::Literal(_) => previous = false,
            }
        }
    }
}

impl<'ast> Visit<'ast> for Ownership<'_> {
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
                .is_some_and(|(path, _)| !path.is_ident("Drop"))
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
        if module.ident == "raw" {
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
    }

    fn visit_expr_method_call(&mut self, expression: &'ast syn::ExprMethodCall) {
        self.operation(&expression.method.to_string(), expression.method.span());
        syn::visit::visit_expr_method_call(self, expression);
    }

    fn visit_expr_path(&mut self, expression: &'ast syn::ExprPath) {
        if expression.path.segments.len() > 1
            && let Some(last) = expression.path.segments.last()
        {
            self.operation(&last.ident.to_string(), last.ident.span());
        }
        syn::visit::visit_expr_path(self, expression);
    }

    fn visit_path(&mut self, path: &'ast syn::Path) {
        for part in &path.segments {
            self.sdk(&part.ident.to_string(), part.ident.span());
        }
        syn::visit::visit_path(self, path);
    }

    fn visit_use_tree(&mut self, tree: &'ast syn::UseTree) {
        if capability_owner(self.owner) {
            let hidden_type = match tree {
                syn::UseTree::Rename(rename) if rename.rename != "_" => Some(rename.ident.span()),
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
            syn::UseTree::Rename(rename) if rename.rename != "_" => Some(rename.ident.span()),
            syn::UseTree::Name(name)
                if name.ident != "io"
                    && name.ident != "self"
                    && name.ident.to_string().starts_with(char::is_lowercase) =>
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
            syn::UseTree::Path(path) => self.sdk(&path.ident.to_string(), path.ident.span()),
            syn::UseTree::Name(name) => self.sdk(&name.ident.to_string(), name.ident.span()),
            syn::UseTree::Rename(rename) => {
                self.sdk(&rename.ident.to_string(), rename.ident.span());
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
        findings: Vec::new(),
    };
    policy.visit_file(file);
    policy.findings
}

#[cfg(test)]
mod tests {
    use super::{
        CI, DESCRIPTOR, DISTRIBUTION, KERNEL, LOCK, MAIN, RELEASE, RELEASE_ORCHESTRATION,
        RELEASE_QUEUE, REPLACEMENT, check,
    };

    fn rejected(owner: &str, source: &str) {
        assert!(
            !check(owner, &syn::parse_file(source).unwrap()).is_empty(),
            "{source}"
        );
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
            (DISTRIBUTION, &["StagedArchive"][..]),
            (CI, &["ChildBuildDirectory"][..]),
            (RELEASE_QUEUE, &["ExtractedBundle", "StagedReceipt"][..]),
            (RELEASE_ORCHESTRATION, &["Origin", "VerifiedHandoff"][..]),
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
