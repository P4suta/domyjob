use syn::visit::Visit;

const EFFECT_MODULES: &[&str] = &["domain", "proc", "spawn", "template"];

#[derive(Default)]
struct OldImports {
    findings: Vec<String>,
}

impl<'ast> Visit<'ast> for OldImports {
    fn visit_path(&mut self, path: &'ast syn::Path) {
        let mut segments = path.segments.iter();
        if segments
            .next()
            .is_some_and(|first| first.ident == "domyjob")
            && let Some(module) = segments.next()
            && !EFFECT_MODULES
                .iter()
                .any(|allowed| module.ident == *allowed)
        {
            self.findings.push(format!(
                "{}: the new binary may use only legacy effect adapters, not domyjob::{}",
                module.ident.span().start().line,
                module.ident
            ));
        }
        syn::visit::visit_path(self, path);
    }
}

pub fn check(source: &str) -> Result<Vec<String>, syn::Error> {
    let parsed = syn::parse_file(source)?;
    let mut imports = OldImports::default();
    imports.visit_file(&parsed);
    Ok(imports.findings)
}

#[cfg(test)]
mod tests {
    use super::check;

    #[test]
    fn application_logic_cannot_reenter_the_legacy_client() {
        assert!(
            check("fn f() { domyjob::proc::terminate(1); }")
                .unwrap()
                .is_empty()
        );
        assert_eq!(check("fn f() { domyjob::cli::run(); }").unwrap().len(), 1);
        assert_eq!(
            check("fn f() { domyjob::snapshot::archive(); }")
                .unwrap()
                .len(),
            1
        );
    }
}
