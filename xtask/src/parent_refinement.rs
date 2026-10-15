use syn::visit::Visit;

const SOURCE_COUNT: usize = 7;
const LEDGER_INDEX: usize = 0;
const LIBRARY_INDEX: usize = 5;
const CHAT_INDEX: usize = 6;
const ONLY_PARENT_LOOKUP: usize = 1;
pub const FILES: [&str; SOURCE_COUNT] = [
    "crates/domyjob-core/src/chat/ledger.rs",
    "crates/domyjob-core/src/chat/ledger/parent_identity.rs",
    "crates/domyjob-core/src/chat/id.rs",
    "crates/domyjob-core/src/chat/event.rs",
    "crates/domyjob/src/chat/store/views.rs",
    "crates/domyjob-core/src/lib.rs",
    "crates/domyjob-core/src/chat.rs",
];

const TEMPLATES: [&str; FILES.len()] = [
    include_str!("../../verification/parent_identity_refinement/ledger.contract"),
    include_str!("../../verification/parent_identity_refinement/identity.contract"),
    include_str!("../../verification/parent_identity_refinement/id.contract"),
    include_str!("../../verification/parent_identity_refinement/event.contract"),
    include_str!("../../verification/parent_identity_refinement/views.contract"),
    include_str!("../../verification/parent_identity_refinement/library.contract"),
    include_str!("../../verification/parent_identity_refinement/chat.contract"),
];

#[derive(Debug, thiserror::Error)]
pub enum RefinementError {
    #[error("parent refinement syntax refused in {file}: {source}")]
    Parse {
        file: &'static str,
        source: syn::Error,
    },
    #[error("parent refinement refused in {file}: {reason}")]
    Refused {
        file: &'static str,
        reason: &'static str,
    },
}

const fn refusal(file: &'static str, reason: &'static str) -> RefinementError {
    RefinementError::Refused { file, reason }
}

fn macro_name(item: &syn::ItemMacro) -> Option<String> {
    item.ident.as_ref().map(ToString::to_string).or_else(|| {
        if item.mac.path.is_ident("validated_string") {
            match item.mac.tokens.clone().into_iter().next() {
                Some(proc_macro2::TokenTree::Ident(name)) => Some(name.to_string()),
                _ => None,
            }
        } else {
            None
        }
    })
}

fn name(item: &syn::Item) -> Option<String> {
    match item {
        syn::Item::Struct(value) => Some(value.ident.to_string()),
        syn::Item::Enum(value) => Some(value.ident.to_string()),
        syn::Item::Fn(value) => Some(value.sig.ident.to_string()),
        syn::Item::Type(value) => Some(value.ident.to_string()),
        syn::Item::Trait(value) => Some(value.ident.to_string()),
        syn::Item::Mod(value) => Some(value.ident.to_string()),
        syn::Item::Macro(value) => macro_name(value),
        syn::Item::Impl(value) if value.trait_.is_none() => {
            if let syn::Type::Path(path) = &*value.self_ty {
                Some(format!(
                    "impl:{}",
                    path.path
                        .segments
                        .iter()
                        .map(|part| part.ident.to_string())
                        .collect::<Vec<_>>()
                        .join("::")
                ))
            } else {
                None
            }
        }
        syn::Item::Const(_)
        | syn::Item::ExternCrate(_)
        | syn::Item::ForeignMod(_)
        | syn::Item::Impl(_)
        | syn::Item::Static(_)
        | syn::Item::TraitAlias(_)
        | syn::Item::Union(_)
        | syn::Item::Use(_)
        | syn::Item::Verbatim(_)
        | _ => None,
    }
}

fn docs(attributes: &mut Vec<syn::Attribute>) {
    attributes.retain(|attribute| !attribute.path().is_ident("doc"));
}

fn normalized(mut item: syn::Item) -> syn::Item {
    match &mut item {
        syn::Item::Fn(value) => docs(&mut value.attrs),
        syn::Item::Struct(value) => {
            docs(&mut value.attrs);
            for field in &mut value.fields {
                docs(&mut field.attrs);
            }
        }
        syn::Item::Enum(value) => {
            docs(&mut value.attrs);
            for variant in &mut value.variants {
                docs(&mut variant.attrs);
                for field in &mut variant.fields {
                    docs(&mut field.attrs);
                }
            }
        }
        syn::Item::Impl(value) => {
            docs(&mut value.attrs);
            for member in &mut value.items {
                if let syn::ImplItem::Fn(function) = member {
                    docs(&mut function.attrs);
                }
            }
        }
        syn::Item::Use(value) => docs(&mut value.attrs),
        syn::Item::Mod(value) => docs(&mut value.attrs),
        syn::Item::Macro(value) => docs(&mut value.attrs),
        syn::Item::Const(_)
        | syn::Item::ExternCrate(_)
        | syn::Item::ForeignMod(_)
        | syn::Item::Static(_)
        | syn::Item::Trait(_)
        | syn::Item::TraitAlias(_)
        | syn::Item::Type(_)
        | syn::Item::Union(_)
        | syn::Item::Verbatim(_)
        | _ => {}
    }
    item
}

fn project_impl(
    mut actual: syn::Item,
    expected: &syn::Item,
    file: &'static str,
) -> Result<syn::Item, RefinementError> {
    if let (syn::Item::Impl(value), syn::Item::Impl(template)) = (&mut actual, expected) {
        let wanted = template
            .items
            .iter()
            .filter_map(|item| {
                if let syn::ImplItem::Fn(function) = item {
                    Some(function.sig.ident.to_string())
                } else {
                    None
                }
            })
            .collect::<Vec<_>>();
        for method in &wanted {
            let count = value.items.iter().filter(|item| matches!(item, syn::ImplItem::Fn(function) if function.sig.ident == method.as_str())).count();
            if count != ONLY_PARENT_LOOKUP {
                return Err(refusal(file, "method is missing or duplicated"));
            }
        }
        value.items.retain(|item| matches!(item, syn::ImplItem::Fn(function) if wanted.iter().any(|method| function.sig.ident == method.as_str())));
    }
    Ok(actual)
}

#[derive(Default)]
struct ParentCalls {
    count: usize,
    macros: usize,
}

impl<'ast> Visit<'ast> for ParentCalls {
    fn visit_expr_path(&mut self, path: &'ast syn::ExprPath) {
        if (path.qself.is_some() || path.path.segments.len() > ONLY_PARENT_LOOKUP)
            && path
                .path
                .segments
                .last()
                .is_some_and(|part| part.ident == "parent")
        {
            self.count = self.count.saturating_add(1);
        }
        syn::visit::visit_expr_path(self, path);
    }
    fn visit_macro(&mut self, _invocation: &'ast syn::Macro) {
        self.macros = self.macros.saturating_add(1);
    }
    fn visit_expr_method_call(&mut self, call: &'ast syn::ExprMethodCall) {
        if call.method == "parent" {
            self.count = self.count.saturating_add(1);
        }
        syn::visit::visit_expr_method_call(self, call);
    }
    fn visit_expr_call(&mut self, call: &'ast syn::ExprCall) {
        if matches!(&*call.func, syn::Expr::Path(path) if path.path.segments.last().is_some_and(|part| part.ident == "parent"))
        {
            self.count = self.count.saturating_add(1);
        }
        syn::visit::visit_expr_call(self, call);
    }
}

#[derive(Default)]
struct MacroClosure {
    definitions: Vec<syn::ItemMacro>,
    sites: Vec<MacroSite>,
    imports: bool,
}

#[derive(PartialEq, Eq)]
enum MacroSite {
    Definition(syn::ItemMacro),
    Invocation(syn::Macro),
}

fn mentions_macro_use(tokens: proc_macro2::TokenStream) -> bool {
    tokens.into_iter().any(|token| match token {
        proc_macro2::TokenTree::Ident(ident) => ident == "macro_use",
        proc_macro2::TokenTree::Group(group) => mentions_macro_use(group.stream()),
        proc_macro2::TokenTree::Punct(_) | proc_macro2::TokenTree::Literal(_) => false,
    })
}

impl<'ast> Visit<'ast> for MacroClosure {
    fn visit_item_macro(&mut self, item: &'ast syn::ItemMacro) {
        if item.ident.is_some() {
            self.definitions.push(item.clone());
            self.sites.push(MacroSite::Definition(item.clone()));
            for attribute in &item.attrs {
                self.visit_attribute(attribute);
            }
        } else {
            syn::visit::visit_item_macro(self, item);
        }
    }

    fn visit_macro(&mut self, invocation: &'ast syn::Macro) {
        self.sites.push(MacroSite::Invocation(invocation.clone()));
    }

    fn visit_attribute(&mut self, attribute: &'ast syn::Attribute) {
        self.imports |= attribute.path().is_ident("macro_use")
            || matches!(&attribute.meta, syn::Meta::List(list) if list.path.is_ident("cfg_attr") && mentions_macro_use(list.tokens.clone()));
        syn::visit::visit_attribute(self, attribute);
    }
}

fn parsed_macro_site(text: &str, file: &'static str) -> Result<MacroSite, RefinementError> {
    match syn::parse_str(text) {
        Ok(invocation) => Ok(MacroSite::Invocation(invocation)),
        Err(_invocation) => match syn::parse_str::<syn::ItemMacro>(text) {
            Ok(definition)
                if definition.ident.is_some() && definition.mac.path.is_ident("macro_rules") =>
            {
                Ok(MacroSite::Definition(definition))
            }
            Ok(_) | Err(_) => Err(refusal(file, "macro site data is not an admitted macro")),
        },
    }
}

fn declared_macro_sites(
    template: &syn::File,
    file: &'static str,
) -> Result<Option<Vec<MacroSite>>, RefinementError> {
    let declarations = template
        .items
        .iter()
        .filter_map(|item| {
            if let syn::Item::Const(value) = item {
                Some(value)
            } else {
                None
            }
        })
        .collect::<Vec<_>>();
    let declaration = match declarations.as_slice() {
        [] => return Ok(None),
        [declaration] if declaration.ident == "MACRO_SITES" => declaration,
        _ => return Err(refusal(file, "macro site data is unknown or duplicated")),
    };
    let site_type: syn::Type = syn::parse_str("&[&str]")
        .map_err(|_error| refusal(file, "macro site type contract is invalid"))?;
    if !declaration.attrs.is_empty()
        || declaration.vis != syn::Visibility::Inherited
        || declaration.modifiers.require_empty().is_err()
        || declaration.generics != syn::Generics::default()
        || *declaration.ty != site_type
    {
        return Err(refusal(
            file,
            "macro site data has an unadmitted declaration",
        ));
    }
    let syn::Expr::Reference(reference) = &*declaration.expr else {
        return Err(refusal(file, "macro site data is not a borrowed array"));
    };
    if reference.mutability.is_some() || !reference.attrs.is_empty() {
        return Err(refusal(file, "macro site array borrowing changed"));
    }
    let syn::Expr::Array(array) = &*reference.expr else {
        return Err(refusal(file, "macro site data is not an array"));
    };
    if !array.attrs.is_empty() {
        return Err(refusal(file, "macro site array attributes changed"));
    }
    array
        .elems
        .iter()
        .map(|entry| {
            let syn::Expr::Lit(literal) = entry else {
                return Err(refusal(file, "macro site data is not a literal"));
            };
            let syn::Lit::Str(text) = &literal.lit else {
                return Err(refusal(file, "macro site data is not a string literal"));
            };
            if !literal.attrs.is_empty() {
                return Err(refusal(file, "macro site entry attributes changed"));
            }
            parsed_macro_site(&text.value(), file)
        })
        .collect::<Result<Vec<_>, _>>()
        .map(Some)
}

fn check_macro_closure(
    actual: &syn::File,
    expected: &syn::File,
    file: &'static str,
) -> Result<(), RefinementError> {
    let mut admitted = MacroClosure::default();
    admitted.visit_file(expected);
    if let Some(sites) = declared_macro_sites(expected, file)? {
        admitted.sites = sites;
    }
    let mut observed = MacroClosure::default();
    observed.visit_file(actual);
    if observed.imports || admitted.imports {
        return Err(refusal(file, "unadmitted macro importing attribute"));
    }
    if observed.definitions != admitted.definitions || observed.sites != admitted.sites {
        return Err(refusal(file, "unadmitted macro definition or invocation"));
    }
    Ok(())
}

fn check_route_order(
    actual: &syn::File,
    expected: &syn::File,
    file: &'static str,
) -> Result<(), RefinementError> {
    if ![FILES[LIBRARY_INDEX], FILES[CHAT_INDEX]].contains(&file) {
        return Ok(());
    }
    let admitted = expected.items.iter().filter_map(name).collect::<Vec<_>>();
    let observed = actual
        .items
        .iter()
        .filter_map(name)
        .filter(|key| admitted.contains(key))
        .collect::<Vec<_>>();
    if observed != admitted {
        return Err(refusal(file, "compiled module declaration order changed"));
    }
    Ok(())
}

fn no_alternate_equality(actual: &syn::File, file: &'static str) -> Result<(), RefinementError> {
    for item in &actual.items {
        if let syn::Item::Impl(implementation) = item
            && let Some((equality, _)) = &implementation.trait_
            && equality
                .segments
                .last()
                .is_some_and(|part| part.ident == "Eq" || part.ident == "PartialEq")
            && matches!(&*implementation.self_ty, syn::Type::Path(path) if path.path.segments.last().is_some_and(|part| matches!(part.ident.to_string().as_str(), "EventId" | "Origin" | "Event")))
        {
            return Err(refusal(
                file,
                "derived identity equality has an alternate implementation",
            ));
        }
    }
    Ok(())
}

fn check_file(file: &'static str, text: &str, template: &str) -> Result<(), RefinementError> {
    let actual = syn::parse_file(text).map_err(|source| RefinementError::Parse { file, source })?;
    let expected =
        syn::parse_file(template).map_err(|source| RefinementError::Parse { file, source })?;
    if actual.attrs != expected.attrs {
        return Err(refusal(file, "file attributes changed"));
    }
    let uses = |source: &syn::File| {
        source
            .items
            .iter()
            .filter(|item| matches!(item, syn::Item::Use(_)))
            .cloned()
            .map(normalized)
            .collect::<Vec<_>>()
    };
    if uses(&actual) != uses(&expected) {
        return Err(refusal(file, "imports changed"));
    }
    let extern_crates = |source: &syn::File| {
        source
            .items
            .iter()
            .filter(|item| matches!(item, syn::Item::ExternCrate(_)))
            .cloned()
            .collect::<Vec<_>>()
    };
    if extern_crates(&actual) != extern_crates(&expected) {
        return Err(refusal(file, "external crate binding changed"));
    }
    check_macro_closure(&actual, &expected, file)?;
    check_route_order(&actual, &expected, file)?;
    no_alternate_equality(&actual, file)?;
    for item in &expected.items {
        let Some(key) = name(item) else {
            continue;
        };
        let candidates = actual
            .items
            .iter()
            .filter(|candidate| name(candidate).as_ref() == Some(&key))
            .collect::<Vec<_>>();
        let [candidate] = candidates.as_slice() else {
            return Err(refusal(file, "definition is missing or duplicated"));
        };
        let projected = project_impl((**candidate).clone(), item, file)?;
        if normalized(projected) != normalized(item.clone()) {
            return Err(refusal(file, "checked definition changed"));
        }
    }
    if file == FILES[LEDGER_INDEX] {
        let mut calls = ParentCalls::default();
        calls.visit_file(&actual);
        if calls.macros != 0 {
            return Err(refusal(file, "unadmitted production macro expansion"));
        }
        if calls.count != ONLY_PARENT_LOOKUP {
            return Err(refusal(
                file,
                "raw parent lookup bypasses the checked boundary",
            ));
        }
    }
    Ok(())
}

pub fn check(sources: &[String]) -> Result<(), RefinementError> {
    if sources.len() != FILES.len() {
        return Err(refusal("source closure", "required inputs are incomplete"));
    }
    for ((file, template), text) in FILES.iter().zip(TEMPLATES).zip(sources) {
        check_file(file, text, template)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use syn::visit::Visit;

    use super::{
        FILES, LIBRARY_INDEX, MacroClosure, MacroSite, RefinementError, TEMPLATES, check,
        check_file,
    };

    fn actual() -> Vec<String> {
        vec![
            include_str!("../../crates/domyjob-core/src/chat/ledger.rs").to_owned(),
            include_str!("../../crates/domyjob-core/src/chat/ledger/parent_identity.rs").to_owned(),
            include_str!("../../crates/domyjob-core/src/chat/id.rs").to_owned(),
            include_str!("../../crates/domyjob-core/src/chat/event.rs").to_owned(),
            include_str!("../../crates/domyjob/src/chat/store/views.rs").to_owned(),
            include_str!("../../crates/domyjob-core/src/lib.rs").to_owned(),
            include_str!("../../crates/domyjob-core/src/chat.rs").to_owned(),
        ]
    }

    #[test]
    fn actual_production_is_admitted_without_textual_format_dependence() {
        let sources = actual();
        let outcome = check(&sources);
        assert!(
            outcome.is_ok(),
            "{outcome:?}\n{}",
            macro_data_diagnostic(&sources)
        );
        let decorated = actual()
            .into_iter()
            .map(|source| format!("// source formatting control\n{source}\n"))
            .collect::<Vec<_>>();
        check(&decorated).unwrap();
    }

    fn macro_data_diagnostic(sources: &[String]) -> String {
        sources
            .iter()
            .zip(FILES)
            .map(|(source, file)| {
                let parsed = syn::parse_file(source).unwrap();
                let mut closure = MacroClosure::default();
                closure.visit_file(&parsed);
                let invocations = closure
                    .sites
                    .iter()
                    .filter_map(|site| match site {
                        MacroSite::Invocation(invocation) => Some(invocation),
                        MacroSite::Definition(_) => None,
                    })
                    .map(|site| {
                        let path = site
                            .path
                            .segments
                            .iter()
                            .map(|segment| segment.ident.to_string())
                            .collect::<Vec<_>>()
                            .join("::");
                        let prefix = if site.path.leading_colon.is_some() {
                            "::"
                        } else {
                            ""
                        };
                        let (open, close) = match site.delimiter {
                            syn::MacroDelimiter::Paren(_) => ("(", ")"),
                            syn::MacroDelimiter::Brace(_) => ("{", "}"),
                            syn::MacroDelimiter::Bracket(_) => ("[", "]"),
                        };
                        format!("r##\"{prefix}{path}!{open}{}{close}\"##,", site.tokens)
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                format!("{file}\n{invocations}")
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn assert_refusals(mutations: Vec<(&str, usize, String)>) {
        let mut admitted = Vec::new();
        for (label, index, changed) in mutations {
            syn::parse_file(&changed).unwrap();
            let mut sources = actual();
            *sources.get_mut(index).unwrap() = changed;
            let wanted = FILES.get(index).unwrap();
            let outcome = check(&sources);
            if !matches!(&outcome, Err(RefinementError::Refused { file, .. }) if file == wanted) {
                admitted.push(format!("{label}: {outcome:?}"));
            }
        }
        assert!(admitted.is_empty(), "{}", admitted.join("\n"));
    }

    #[test]
    fn parsed_macro_shadowing_and_imports_are_refinement_refusals() {
        let sources = actual();
        let library = sources.get(LIBRARY_INDEX).unwrap();
        let identity = sources.get(2).unwrap();
        let (_, validator) = library.split_once("macro_rules! validated_string").unwrap();
        let (validator, _) = validator.split_once("pub mod capture_policy;").unwrap();
        let validator = format!("macro_rules! validated_string{validator}")
            .replace("check(&value)?;", "let _unchecked = check;");
        let mutations = vec![
            (
                "local validator",
                2,
                identity.replacen(
                    "validated_string!(Origin",
                    &format!("{validator}\nvalidated_string!(Origin"),
                    1,
                ),
            ),
            (
                "local matches",
                2,
                identity.replacen(
                    "fn lower_hex",
                    "macro_rules! matches { ($($tokens:tt)*) => { true }; }\nfn lower_hex",
                    1,
                ),
            ),
            (
                "nested definition",
                2,
                format!(
                    "{identity}\nfn hidden() {{ macro_rules! matches {{ ($($tokens:tt)*) => {{ true }}; }} }}"
                ),
            ),
            (
                "macro import",
                5,
                format!("{library}\n#[macro_use] mod imported;"),
            ),
            (
                "conditional macro import",
                5,
                format!("{library}\n#[cfg_attr(kani, macro_use)] mod imported;"),
            ),
            (
                "nested macro import",
                2,
                format!("{identity}\nmod hidden {{ #[macro_use] mod imported; }}"),
            ),
            (
                "unlisted invocation",
                2,
                format!("{identity}\nintroduce_identity!();"),
            ),
        ];
        assert_refusals(mutations);
    }

    #[test]
    fn parsed_root_chat_route_changes_are_refinement_refusals() {
        let sources = actual();
        let source = sources.get(LIBRARY_INDEX).unwrap();
        let mutations = vec![
            (
                "conditional chat route",
                5,
                source.replacen("pub mod chat;", "#[cfg(kani)] pub mod chat;", 1),
            ),
            (
                "alternate chat route",
                5,
                source.replacen(
                    "pub mod chat;",
                    "#[path = \"alternate.rs\"] pub mod chat;",
                    1,
                ),
            ),
            (
                "missing chat route",
                5,
                source.replacen("pub mod chat;", "", 1),
            ),
            (
                "duplicate chat route",
                5,
                source.replacen("pub mod chat;", "pub mod chat; pub mod chat;", 1),
            ),
        ];
        assert_refusals(mutations);
    }

    #[test]
    fn parsed_qualified_self_parent_aliases_are_refinement_refusals() {
        let sources = actual();
        let source = sources.first().unwrap();
        let mutations = vec![
            (
                "qualified self alias",
                0,
                format!(
                    "{source}\nfn unchecked<L: Ledger>(ledger: &L, request: &EventId) -> Result<Parent, L::Error> {{ let fetch = <L>::parent; fetch(ledger, request) }}"
                ),
            ),
            (
                "qualified self parenthesized alias",
                0,
                format!(
                    "{source}\nfn unchecked<L: Ledger>(ledger: &L, request: &EventId) -> Result<Parent, L::Error> {{ let fetch = (<L>::parent); fetch(ledger, request) }}"
                ),
            ),
        ];
        assert_refusals(mutations);
    }

    #[test]
    fn an_ordinary_bare_parent_value_remains_admitted() {
        let mut sources = actual();
        sources
            .first_mut()
            .unwrap()
            .push_str("\nfn preserved_parent(parent: Parent) -> Parent { parent }\n");
        syn::parse_file(sources.first().unwrap()).unwrap();
        check(&sources).unwrap();
    }

    #[test]
    fn parsed_chat_declarations_cannot_select_alternate_modules() {
        let sources = actual();
        let source = sources.get(super::CHAT_INDEX).unwrap();
        let mut mutations = Vec::new();
        for declaration in ["pub mod id;", "pub mod event;", "pub mod ledger;"] {
            mutations.push((
                "missing declaration",
                super::CHAT_INDEX,
                source.replacen(declaration, "", 1),
            ));
            mutations.push((
                "conditional declaration",
                super::CHAT_INDEX,
                source.replacen(declaration, &format!("#[cfg(kani)] {declaration}"), 1),
            ));
            mutations.push((
                "alternate path",
                super::CHAT_INDEX,
                source.replacen(
                    declaration,
                    &format!("#[path = \"alternate.rs\"] {declaration}"),
                    1,
                ),
            ));
            mutations.push((
                "duplicate declaration",
                super::CHAT_INDEX,
                source.replacen(declaration, &format!("{declaration} {declaration}"), 1),
            ));
        }
        assert_refusals(mutations);
    }

    #[test]
    fn parsed_unadmitted_macro_sites_refuse_every_protected_file() {
        let additions = [
            "macro_rules! unadmitted { () => { true }; }",
            "fn unadmitted() { introduce_identity!(); }",
            "fn hidden() { macro_rules! matches { ($($tokens:tt)*) => { true }; } }",
            "#[cfg_attr(kani, cfg_attr(any(), macro_use))] mod imported;",
        ];
        let mut mutations = Vec::new();
        for (index, source) in actual().into_iter().enumerate() {
            for addition in additions {
                mutations.push((addition, index, format!("{source}\n{addition}")));
            }
        }
        assert_refusals(mutations);
    }

    #[test]
    fn moving_chat_before_its_lexical_validator_is_refused() {
        let mut sources = actual();
        let library = sources.get_mut(LIBRARY_INDEX).unwrap();
        *library = library.replacen("pub mod chat;", "", 1).replacen(
            "macro_rules! validated_string",
            "pub mod chat;\nmacro_rules! validated_string",
            1,
        );
        syn::parse_file(library).unwrap();
        assert!(
            matches!(check(&sources), Err(RefinementError::Refused { file, .. }) if file == FILES[LIBRARY_INDEX])
        );
    }

    #[test]
    fn malformed_duplicate_and_unknown_macro_site_data_is_refused() {
        let sources = actual();
        let source = sources.get(2).unwrap();
        let (baseline, _) = TEMPLATES[2].split_once("const MACRO_SITES").unwrap();
        for declaration in [
            "const MACRO_SITES: &[&str] = &[123];",
            "const MACRO_SITES: &[&str] = &[\"fn hidden() {}\"] ;",
            "const MACRO_SITES: &[&str] = &[\"unknown!()\"] ;",
            "const MACRO_SITES: &[&str] = &[]; const MACRO_SITES: &[&str] = &[];",
            "const UNKNOWN_SITES: &[&str] = &[];",
            "pub const MACRO_SITES: &[&str] = &[];",
            "impl MacroData { default const MACRO_SITES: &[&str] = &[]; }",
            "const MACRO_SITES<T>: &[&str] = &[];",
            "const MACRO_SITES: &[&str] = & #[cfg(kani)] [];",
            "const MACRO_SITES: &[&str] = #[cfg(kani)] &[];",
            "const MACRO_SITES: &[&str] = &mut [];",
        ] {
            let changed = format!("{baseline}\n{declaration}");
            assert!(
                syn::parse_file(&changed).is_ok(),
                "unparsed data control: {declaration}"
            );
            assert!(
                matches!(check_file(FILES[2], source, &changed), Err(RefinementError::Refused { file, .. }) if file == FILES[2])
            );
        }
    }

    fn identity_and_lookup_mutations() -> Vec<(&'static str, &'static str, &'static str)> {
        vec![
            (
                FILES[2],
                "pub struct EventId {\n    origin: Origin,",
                "pub struct EventId {",
            ),
            (
                FILES[1],
                "Some(id) => id == request",
                "Some(id) => id.seq() == request.seq()",
            ),
            (
                FILES[1],
                "Some(id) => id == request",
                "Some(id) => id.origin() == request.origin()",
            ),
            (FILES[3], "&self.id", "&self.clock"),
            (
                FILES[3],
                "&self.id",
                "{ let _clock = self.clock; &self.id }",
            ),
            (FILES[0], "Some(parent.id())", "None"),
            (
                FILES[0],
                "Self::Opaque | Self::Missing => None",
                "Self::Opaque => None, Self::Missing => Some(request)",
            ),
            (FILES[0], "impl Parent {", "#[cfg(kani)] impl Parent {"),
            (
                FILES[0],
                "fn checked_parent<L",
                "#[cfg(kani)] fn checked_parent<L",
            ),
            (
                FILES[0],
                "match checked_parent(ledger, event, request)?",
                "match ledger.parent(request).map_err(Failure::Store)?",
            ),
            (
                FILES[0],
                "ledger.parent(request).map_err(Failure::Store)?",
                "ledger.parent(request).map_err(|_error| Failure::Rejected(Rejection::Inconsistent))?",
            ),
            (
                FILES[0],
                "if !parent.matches(request) {",
                "let _extra = ledger.parent(request).map_err(Failure::Store)?; if !parent.matches(request) {",
            ),
            (
                FILES[0],
                "event: event.id().clone(),",
                "event: request.clone(),",
            ),
            (
                FILES[0],
                "let (Some(conversation), Some(audience), Some(agent)) = (",
                "let _early = checked_parent(ledger, event, request)?; let (Some(conversation), Some(audience), Some(agent)) = (",
            ),
            (
                FILES[0],
                "let current = ledger.resolution(request).map_err(Failure::Store)?;",
                "let _early = checked_parent(ledger, event, request)?; let current = ledger.resolution(request).map_err(Failure::Store)?;",
            ),
            (
                FILES[4],
                "if event.id() != id",
                "if event.id().seq() != id.seq()",
            ),
        ]
    }

    #[test]
    fn parsed_identity_and_lookup_mutations_refuse_their_actual_refinement() {
        for (file, before, after) in identity_and_lookup_mutations() {
            let mut sources = actual();
            let index = FILES.iter().position(|name| *name == file).unwrap();
            let source = sources.get_mut(index).unwrap();
            assert!(
                source.contains(before),
                "missing planted mutation: {before}"
            );
            *source = source.replacen(before, after, 1);
            syn::parse_file(source).unwrap();
            let outcome = check(&sources);
            assert!(
                matches!(&outcome, Err(RefinementError::Refused { file: refused, .. }) if *refused == file),
                "mutation in {file}: {before}; actual outcome: {outcome:?}"
            );
        }
    }

    #[test]
    fn templates_parse_and_duplicate_impls_are_semantic_refusals() {
        for template in TEMPLATES {
            syn::parse_file(template).unwrap();
        }
        let mut sources = actual();
        sources.first_mut().unwrap().push_str("\nimpl Parent {}\n");
        assert!(
            matches!(check(&sources), Err(RefinementError::Refused { file, .. }) if file == FILES[0])
        );
    }

    #[test]
    fn parent_function_aliases_parentheses_and_expression_macros_cannot_bypass_the_boundary() {
        let hidden = [
            "fn unchecked<L: Ledger>(ledger: &L, request: &EventId) -> Result<Parent, L::Error> { let fetch = L::parent; fetch(ledger, request) }",
            "fn unchecked<L: Ledger>(ledger: &L, request: &EventId) -> Result<Parent, L::Error> { (L::parent)(ledger, request) }",
            "fn unchecked<L: Ledger>(ledger: &L, request: &EventId) -> Result<Parent, L::Error> { lookup!(ledger, request) }",
            "fn unchecked<L: Ledger>(ledger: &L, request: &EventId) { lookup!(ledger, request); }",
        ];
        for function in hidden {
            let mut sources = actual();
            let ledger = sources.first_mut().unwrap();
            ledger.push_str(function);
            syn::parse_file(ledger).unwrap();
            assert!(
                matches!(check(&sources), Err(RefinementError::Refused { file, .. }) if file == FILES[0])
            );
        }
    }

    #[test]
    fn a_parsed_alternate_identity_equality_is_not_compiler_failure_credit() {
        let mut sources = actual();
        let identity = sources.get_mut(2).unwrap();
        identity.push_str(
            "\nimpl PartialEq for EventId { fn eq(&self, _other: &Self) -> bool { true } }\n",
        );
        syn::parse_file(identity).unwrap();
        assert!(
            matches!(check(&sources), Err(RefinementError::Refused { file, .. }) if file == FILES[2])
        );
    }

    #[test]
    fn parsed_missing_and_renamed_allocation_bindings_refuse_refinement() {
        for replacement in [
            "",
            "extern crate alloc as alternative;",
            "#[cfg(kani)] extern crate alloc;",
        ] {
            let mut sources = actual();
            let library = sources.get_mut(LIBRARY_INDEX).unwrap();
            assert!(library.contains("extern crate alloc;"));
            *library = library.replacen("extern crate alloc;", replacement, 1);
            syn::parse_file(library).unwrap();
            assert!(
                matches!(check(&sources), Err(RefinementError::Refused { file, .. }) if file == FILES[LIBRARY_INDEX])
            );
        }
    }
}
