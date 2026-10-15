#![no_main]

use domyjob_core::chat::handoff::{Descriptor, ReadLimit, ReadRefusal, Scope, Target};
use domyjob_core::chat::id::{AgentId, Origin};
use domyjob_core::ingress;
use libfuzzer_sys::fuzz_target;

const MAX_DESCRIPTOR_BYTES: usize = 32 * 1024;
const ORIGIN_ENTROPY_BYTES: usize = 16;

fuzz_target!(|data: &[u8]| {
    if let Ok(target) = ingress::json::<Target>(data, MAX_DESCRIPTOR_BYTES) {
        let encoded = serde_json::to_vec(&target).unwrap();
        assert_eq!(
            ingress::json::<Target>(&encoded, MAX_DESCRIPTOR_BYTES).unwrap(),
            target
        );
    }
    let Ok(descriptor) = ingress::json::<Descriptor>(data, MAX_DESCRIPTOR_BYTES) else {
        return;
    };
    let encoded = serde_json::to_vec(&descriptor).unwrap();
    assert_eq!(
        ingress::json::<Descriptor>(&encoded, MAX_DESCRIPTOR_BYTES).unwrap(),
        descriptor
    );
    let metadata = descriptor.metadata();
    assert!(metadata.audience().includes(metadata.sender().origin()));
    for reader in metadata.permitted_readers().agents() {
        assert!(metadata.audience().includes(reader.origin()));
        let permit = descriptor
            .authorize(reader, &descriptor, ReadLimit::MAX)
            .unwrap();
        assert_eq!(permit.observation().reader(), reader);
        assert_eq!(permit.observation().handoff(), descriptor.id());
        let other = AgentId::new(
            reader.name().clone(),
            Origin::from_entropy([u8::MAX; ORIGIN_ENTROPY_BYTES]),
        );
        let admitted = descriptor
            .authorize(&other, &descriptor, ReadLimit::MAX)
            .is_ok();
        assert_eq!(admitted, metadata.permitted_readers().contains(&other));
    }
    for origin in metadata.audience().members() {
        assert!(
            origin == metadata.sender().origin()
                || metadata
                    .permitted_readers()
                    .agents()
                    .iter()
                    .any(|reader| reader.origin() == origin)
        );
    }
    let foreign_scope = if metadata.scope().line().as_str() == "foreign" {
        "another"
    } else {
        "foreign"
    };
    let mut selected = serde_json::to_value(&descriptor).unwrap();
    *selected.pointer_mut("/metadata/scope").unwrap() =
        serde_json::to_value(Scope::try_from(foreign_scope.to_owned()).unwrap()).unwrap();
    let selected: Descriptor = ingress::value(selected).unwrap();
    let reader = metadata.permitted_readers().agents().first().unwrap();
    assert!(matches!(
        descriptor.authorize(reader, &selected, ReadLimit::MAX),
        Err(ReadRefusal::Scope)
    ));
});
