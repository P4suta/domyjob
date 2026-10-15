#[cfg(test)]
mod tests {
    use domyjob_core::chat::event::Members;
    use domyjob_core::chat::handoff::{
        Descriptor, Metadata, Project, ReadLimit, ReadRefusal, Scope, Target, TeamId,
    };
    use domyjob_core::chat::id::{AgentId, AgentName, EventId, Origin, Text};
    use std::num::NonZeroU64;

    fn agent(name: &str, marker: u8) -> AgentId {
        AgentId::new(
            AgentName::try_from(name.to_owned()).unwrap(),
            Origin::from_entropy([marker; 16]),
        )
    }

    fn metadata(target: Target) -> Metadata {
        Metadata::new(
            agent("sender", 1),
            target,
            (
                Project::try_from("owner/project".to_owned()).unwrap(),
                Scope::try_from("issue-47".to_owned()).unwrap(),
            ),
            Members::try_from(vec![agent("previous", 2), agent("continuing", 3)]).unwrap(),
        )
        .unwrap()
    }

    fn descriptor() -> Descriptor {
        Descriptor::new(
            EventId::new(agent("sender", 1).origin().clone(), NonZeroU64::MIN),
            metadata(Target::Agent {
                agent: agent("previous", 2),
            }),
            &Text::try_from("scoped handoff".to_owned()).unwrap(),
        )
        .unwrap()
    }

    #[test]
    fn a_continuing_reader_uses_its_own_identity_and_exact_audience() {
        let descriptor = descriptor();
        let continuing = agent("continuing", 3);
        let permit = descriptor
            .authorize(&continuing, &descriptor, ReadLimit::MAX)
            .unwrap();
        assert_eq!(permit.reader(), &continuing);
        assert_eq!(permit.observation().reader(), &continuing);
        assert_eq!(permit.observation().handoff(), descriptor.id());
        let audience = descriptor.metadata().audience();
        for marker in [1, 2, 3] {
            assert!(audience.includes(&Origin::from_entropy([marker; 16])));
        }
        assert_eq!(audience.members().len(), 3);
        assert!(!audience.includes(agent("unrelated", 4).origin()));
    }

    #[test]
    fn an_identical_name_on_an_unapproved_machine_cannot_read_the_body() {
        let descriptor = descriptor();
        let wrong_origin = agent("continuing", 4);
        assert!(matches!(
            descriptor.authorize(&wrong_origin, &descriptor, ReadLimit::MAX),
            Err(ReadRefusal::Reader)
        ));
    }

    #[test]
    fn selected_project_scope_and_event_are_bound_before_body_access() {
        let descriptor = descriptor();
        let reader = agent("continuing", 3);
        for (pointer, replacement, expected) in [
            (
                "/metadata/project",
                serde_json::json!("unrelated/project"),
                ReadRefusal::Project,
            ),
            (
                "/metadata/scope",
                serde_json::json!("unrelated-scope"),
                ReadRefusal::Scope,
            ),
            (
                "/id",
                serde_json::json!(EventId::new(
                    descriptor.id().origin().clone(),
                    NonZeroU64::new(2).unwrap()
                )),
                ReadRefusal::Descriptor,
            ),
        ] {
            let mut selected = serde_json::to_value(&descriptor).unwrap();
            *selected.pointer_mut(pointer).unwrap() = replacement;
            let selected: Descriptor = domyjob_core::ingress::value(selected).unwrap();
            assert_eq!(
                descriptor
                    .authorize(&reader, &selected, ReadLimit::MAX)
                    .unwrap_err(),
                expected
            );
        }
    }

    #[test]
    fn byte_limits_refuse_multibyte_text_before_body_access() {
        let text = Text::try_from("é".to_owned()).unwrap();
        let descriptor = Descriptor::new(
            EventId::new(agent("sender", 1).origin().clone(), NonZeroU64::MIN),
            metadata(Target::Agent {
                agent: agent("previous", 2),
            }),
            &text,
        )
        .unwrap();
        let reader = agent("continuing", 3);
        assert!(matches!(
            descriptor.authorize(&reader, &descriptor, ReadLimit::try_from(1).unwrap()),
            Err(ReadRefusal::Body)
        ));
        let permit = descriptor
            .authorize(&reader, &descriptor, ReadLimit::try_from(2).unwrap())
            .unwrap();
        permit
            .check_body(descriptor.id(), descriptor.metadata(), &text)
            .unwrap();
        let foreign = EventId::new(
            descriptor.id().origin().clone(),
            NonZeroU64::new(2).unwrap(),
        );
        assert_eq!(
            permit.check_body(&foreign, descriptor.metadata(), &text),
            Err(ReadRefusal::Descriptor)
        );
    }

    #[test]
    fn observations_cannot_impersonate_the_intended_recipient() {
        let descriptor = descriptor();
        let reader = agent("continuing", 3);
        let observation = descriptor
            .authorize(&reader, &descriptor, ReadLimit::MAX)
            .unwrap()
            .observation();
        descriptor
            .admit_observation(
                &observation,
                (&reader, descriptor.metadata().audience()),
                (1, 2),
            )
            .unwrap();
        assert_eq!(
            descriptor.admit_observation(
                &observation,
                (&agent("previous", 2), descriptor.metadata().audience()),
                (1, 2)
            ),
            Err(ReadRefusal::Reader)
        );
        assert_eq!(
            descriptor.admit_observation(
                &observation,
                (&reader, descriptor.metadata().audience()),
                (2, 2)
            ),
            Err(ReadRefusal::Order)
        );
    }

    #[test]
    fn a_team_label_does_not_grant_new_members_access() {
        let team =
            TeamId::try_from(format!("room:{}:developers", agent("sender", 1).origin())).unwrap();
        let descriptor = Descriptor::new(
            EventId::new(agent("sender", 1).origin().clone(), NonZeroU64::MIN),
            metadata(Target::Team { team }),
            &Text::try_from("handoff".to_owned()).unwrap(),
        )
        .unwrap();
        assert!(matches!(
            descriptor.authorize(&agent("new-member", 4), &descriptor, ReadLimit::MAX),
            Err(ReadRefusal::Reader)
        ));
    }

    #[test]
    fn metadata_rejects_extra_audience_and_ungranted_intended_recipients() {
        let metadata = metadata(Target::Agent {
            agent: agent("previous", 2),
        });
        let mut raw = serde_json::to_value(&metadata).unwrap();
        raw.as_object_mut().unwrap().insert(
            "audience".to_owned(),
            serde_json::json!([agent("unrelated", 4).origin()]),
        );
        domyjob_core::ingress::value::<Metadata>(raw).unwrap_err();
        let mut foreign_recipient = serde_json::to_value(&metadata).unwrap();
        *foreign_recipient
            .pointer_mut("/intended_recipient/agent")
            .unwrap() = serde_json::json!(agent("unrelated", 4));
        domyjob_core::ingress::value::<Metadata>(foreign_recipient).unwrap_err();
    }

    #[test]
    fn bounds_and_contexts_refuse_empty_overlong_or_ambiguous_values() {
        use domyjob_core::chat::handoff::{MAX_BODY_BYTES, MAX_CONTEXT_BYTES};
        for bytes in [0, MAX_BODY_BYTES + 1, usize::MAX] {
            ReadLimit::try_from(bytes).unwrap_err();
        }
        assert_eq!(ReadLimit::try_from(MAX_BODY_BYTES).unwrap(), ReadLimit::MAX);
        for value in [
            String::new(),
            " scoped".to_owned(),
            "scoped\u{1f}other".to_owned(),
            "s".repeat(MAX_CONTEXT_BYTES + 1),
        ] {
            Scope::try_from(value.clone()).unwrap_err();
            Project::try_from(value).unwrap_err();
        }
    }

    #[test]
    fn the_actual_ledger_checks_the_parent_audience_and_reader_projection() {
        use domyjob_core::chat::event::{Body, Stamp};
        use domyjob_core::chat::model::Model;
        use domyjob_core::chat::policy::audience;
        let descriptor = descriptor();
        let reader = agent("continuing", 3);
        let observation = descriptor
            .authorize(&reader, &descriptor, ReadLimit::MAX)
            .unwrap()
            .observation();
        let mut sender = Model::new(descriptor.id().origin().clone());
        let event = sender
            .write(Body::Handoff {
                metadata: descriptor.metadata().clone(),
                text: Text::try_from("scoped handoff".to_owned()).unwrap(),
                at: Stamp::from_unix_millis(1),
            })
            .unwrap();
        assert_eq!(event.id(), descriptor.id());
        let mut continuing = Model::new(reader.origin().clone());
        continuing
            .write(Body::HandoffRead {
                observation: observation.clone(),
                audience: descriptor.metadata().audience().clone(),
            })
            .unwrap_err();
        continuing.sync_with(&mut sender, 2).unwrap();
        continuing
            .write(Body::HandoffRead {
                observation: observation.clone(),
                audience: audience([
                    descriptor.metadata().sender(),
                    &reader,
                    &agent("unrelated", 4),
                ])
                .unwrap(),
            })
            .unwrap_err();
        continuing
            .write(Body::HandoffRead {
                observation,
                audience: descriptor.metadata().audience().clone(),
            })
            .unwrap();
        continuing.sync_with(&mut sender, 2).unwrap();
        let expected = std::collections::BTreeSet::from([(descriptor.id().clone(), reader)]);
        assert_eq!(sender.handoff_reads(), &expected);
        assert_eq!(continuing.handoff_reads(), &expected);
    }

    #[test]
    fn the_preserved_metadata_corpus_contains_admitted_agent_and_team_descriptors() {
        let continuing =
            AgentId::try_from("continuing@cccccccccccccccccccccccccccccccc".to_owned()).unwrap();
        for bytes in [
            include_bytes!("../../../fuzz/corpus/handoff_metadata/granted").as_slice(),
            include_bytes!("../../../fuzz/corpus/handoff_metadata/team").as_slice(),
        ] {
            let descriptor: Descriptor =
                domyjob_core::ingress::json(bytes, domyjob_core::ingress::MAX_FOREIGN_BYTES)
                    .unwrap();
            let permit = descriptor
                .authorize(&continuing, &descriptor, ReadLimit::MAX)
                .unwrap();
            assert_eq!(permit.observation().reader(), &continuing);
        }
        domyjob_core::ingress::json::<Descriptor>(
            include_bytes!("../../../fuzz/corpus/handoff_metadata/malformed"),
            domyjob_core::ingress::MAX_FOREIGN_BYTES,
        )
        .unwrap_err();
    }
}
