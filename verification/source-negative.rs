#[path = "../crates/domyjob-core/src/source_paths/metadata.rs"]
mod metadata;

#[kani::proof]
#[kani::unwind(16)]
fn rejects_repository_metadata() {
    kani::assert(
        !metadata::metadata_component(".git"),
        "NEGATIVE_SOURCE_CONTROL",
    );
}
