use super::{Harness, Inventory, Negative};

const PREFIX: &str = "chat::handoff::decision::proofs::";
const CONTRACTS: [(&str, u64, &str, &str); 3] = [
    (
        "only_a_delivered_uncancelled_body_records_an_observation",
        4,
        "rejects_an_observation_for_a_replaced_body",
        "NEGATIVE_HANDOFF_DELIVERY_CONTROL",
    ),
    (
        "reads_require_the_exact_grant_scope_and_bound",
        9,
        "rejects_ungranted_handoff_body_access",
        "NEGATIVE_HANDOFF_READ_CONTROL",
    ),
    (
        "observations_require_the_actual_author_and_grant",
        6,
        "rejects_a_foreign_handoff_reader_observation",
        "NEGATIVE_HANDOFF_OBSERVATION_CONTROL",
    ),
];

fn select(inventory: &Inventory) -> Result<(Vec<Harness>, Vec<Negative>), String> {
    let changed = || "handoff proof inventory changed its exact contract".to_owned();
    let positives = inventory
        .harnesses
        .iter()
        .filter(|item| item.name.starts_with(PREFIX))
        .count();
    let negatives = inventory
        .negatives
        .iter()
        .filter(|item| item.harness.starts_with(PREFIX))
        .count();
    if positives != CONTRACTS.len() || negatives != CONTRACTS.len() {
        return Err(changed());
    }
    let pairs = CONTRACTS
        .into_iter()
        .map(|(name, covers, harness, assertion)| {
            let positive = inventory
                .harnesses
                .iter()
                .find(|item| item.name.strip_prefix(PREFIX) == Some(name))
                .ok_or_else(changed)?;
            let negative = inventory
                .negatives
                .iter()
                .find(|item| item.harness.strip_prefix(PREFIX) == Some(harness))
                .ok_or_else(changed)?;
            if positive.source.is_some()
                || negative.source.is_some()
                || positive.covers != covers
                || negative.assertion != assertion
            {
                return Err(changed());
            }
            Ok((positive.clone(), negative.clone()))
        });
    Ok(pairs
        .collect::<Result<Vec<_>, String>>()?
        .into_iter()
        .unzip())
}

pub(super) fn run(root: &std::path::Path) -> Result<(), String> {
    let before = super::fingerprint(root)?;
    super::bound_attempt(
        || {
            super::check_parent_refinement(root)
                .and_then(|()| super::inventory(root))
                .and_then(|inventory| select(&inventory))
                .and_then(|(positives, negatives)| {
                    crate::resource_proofs::run_inventory(root, &positives, &negatives)
                })
        },
        || super::unchanged_source(root, before),
    )?;
    eprintln!(
        "verification: source={before}; handoffs; 3 positives; 3 named negatives; 19 covers; source and physical tools bound before and after; full verification remains required"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{Inventory, select};

    fn fixture() -> serde_json::Value {
        domyjob_core::ingress::foreign_json(include_str!("../../../verification/obligations.json"))
            .unwrap()
    }

    fn replace_field(entry: &mut serde_json::Value, field: &str, value: serde_json::Value) {
        let _previous = entry
            .as_object_mut()
            .unwrap()
            .insert(field.to_owned(), value);
    }

    #[test]
    fn actual_handoffs_select_only_the_three_core_contract_pairs() {
        let inventory: Inventory = domyjob_core::ingress::value(fixture()).unwrap();
        let (positives, negatives) = select(&inventory).unwrap();
        assert_eq!((positives.len(), negatives.len()), (3, 3));
        assert_eq!(positives.iter().map(|item| item.covers).sum::<u64>(), 19);
        assert!(positives.iter().all(|item| item.source.is_none()));
        assert!(negatives.iter().all(|item| item.source.is_none()));
    }

    #[test]
    fn handoff_selection_refuses_missing_extra_duplicate_and_changed_obligations() {
        #[derive(Clone, Copy, Debug)]
        enum Mutation {
            Missing,
            Extra,
            Duplicate,
            Source,
            Contract,
            Name,
        }

        for group in ["harnesses", "negatives"] {
            for mutation in [
                Mutation::Missing,
                Mutation::Extra,
                Mutation::Duplicate,
                Mutation::Source,
                Mutation::Contract,
                Mutation::Name,
            ] {
                let mut value = fixture();
                let entries = value.get_mut(group).unwrap().as_array_mut().unwrap();
                let field = if group == "harnesses" {
                    "name"
                } else {
                    "harness"
                };
                let index = entries
                    .iter()
                    .position(|entry| {
                        entry
                            .get(field)
                            .and_then(serde_json::Value::as_str)
                            .is_some_and(|name| name.starts_with(super::PREFIX))
                    })
                    .unwrap();
                let original = entries.get(index).unwrap().clone();
                match mutation {
                    Mutation::Missing => {
                        entries.remove(index);
                    }
                    Mutation::Extra => {
                        let mut extra = original.clone();
                        replace_field(
                            &mut extra,
                            field,
                            format!("{}foreign", super::PREFIX).into(),
                        );
                        entries.push(extra);
                    }
                    Mutation::Duplicate => entries.push(original),
                    Mutation::Source => replace_field(
                        entries.get_mut(index).unwrap(),
                        "source",
                        "verification/foreign.rs".into(),
                    ),
                    Mutation::Contract if group == "harnesses" => {
                        replace_field(entries.get_mut(index).unwrap(), "covers", 1.into());
                    }
                    Mutation::Contract => replace_field(
                        entries.get_mut(index).unwrap(),
                        "assertion",
                        "NEGATIVE_FOREIGN".into(),
                    ),
                    Mutation::Name => replace_field(
                        entries.get_mut(index).unwrap(),
                        field,
                        "foreign::proofs::substitute".into(),
                    ),
                }
                let inventory: Inventory = domyjob_core::ingress::value(value).unwrap();
                assert!(select(&inventory).is_err(), "{group}/{mutation:?}");
            }
        }
    }

    #[test]
    fn handoff_command_refuses_arguments_before_reading_source() {
        let absent = std::path::Path::new("absent-handoff-selector-fixture");
        for option in [
            "extra",
            "--harness=foreign",
            "verification/foreign.rs",
            "--solver=z3",
            "--harness-timeout=120s",
        ] {
            assert!(
                super::super::dispatch(absent, &["handoffs", option])
                    .unwrap_err()
                    .starts_with("usage:")
            );
        }
    }
}
