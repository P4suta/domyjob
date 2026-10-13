use domyjob_core::process_group::ProcessGroup;

#[test]
fn scalar_identifiers_preserve_the_admitted_native_group() {
    for value in [i32::MIN, -1, 0, 1, 17, i32::MAX] {
        match ProcessGroup::new(value) {
            Ok(group) => {
                assert!(value > 0);
                assert_eq!(group.get(), value);
            }
            Err(error) => {
                assert!(value <= 0);
                assert_eq!(error.to_string(), "invalid process group");
            }
        }
    }
}

#[test]
fn textual_ingress_keeps_exact_parsing_and_canonical_round_trips() {
    for (text, expected) in [
        ("1", 1),
        ("+17", 17),
        ("0017", 17),
        ("2147483647", i32::MAX),
    ] {
        let group: ProcessGroup = text.parse().unwrap();
        assert_eq!(group.get(), expected);
        assert_eq!(group.get().to_string().parse(), Ok(group));
    }
    for text in [
        "",
        "0",
        "-1",
        "2147483648",
        "17\n",
        " 17",
        "\u{2003}17",
        "invalid",
    ] {
        let error = text.parse::<ProcessGroup>().unwrap_err();
        assert_eq!(error.to_string(), "invalid process group");
    }
}
