use syn::visit::Visit;

#[derive(Default)]
struct StdUse {
    locations: Vec<usize>,
}

impl<'ast> Visit<'ast> for StdUse {
    fn visit_item_extern_crate(&mut self, item: &'ast syn::ItemExternCrate) {
        if item.ident == "std" {
            self.locations.push(item.ident.span().start().line);
        }
        syn::visit::visit_item_extern_crate(self, item);
    }

    fn visit_path(&mut self, path: &'ast syn::Path) {
        if let Some(first) = path.segments.first()
            && first.ident == "std"
        {
            self.locations.push(first.ident.span().start().line);
        }
        syn::visit::visit_path(self, path);
    }
}

pub fn check(source: &str, crate_root: bool) -> Result<Vec<String>, syn::Error> {
    let parsed = syn::parse_file(source)?;
    let mut findings = Vec::new();
    if crate_root
        && !parsed
            .attrs
            .iter()
            .any(|attribute| attribute.path().is_ident("no_std"))
    {
        findings.push("1: domyjob-core must remain unconditionally no_std".to_owned());
    }
    let mut uses = StdUse::default();
    uses.visit_file(&parsed);
    for line in uses.locations {
        findings.push(format!("{line}: domyjob-core cannot import std"));
    }
    Ok(findings)
}

#[cfg(test)]
mod tests {
    use super::check;

    #[test]
    fn pure_core_gate_rejects_std_and_missing_crate_attribute() {
        assert!(
            check("#![no_std]\nextern crate alloc;", true)
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            check("extern crate alloc;", true).unwrap(),
            ["1: domyjob-core must remain unconditionally no_std"]
        );
        assert_eq!(
            check("\nextern crate std;", false).unwrap(),
            ["2: domyjob-core cannot import std"]
        );
        assert_eq!(
            check("\n\nfn read() { std::fs::read(\"x\"); }", false).unwrap(),
            ["3: domyjob-core cannot import std"]
        );
        check("fn broken(", false).unwrap_err();
    }
}
