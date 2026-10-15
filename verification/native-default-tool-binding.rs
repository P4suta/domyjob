#[path = "../xtask/src/proof_tools/native_default.rs"]
mod contract;

mod native_default {
    use super::contract as native_default;
    use native_default::{Budget, Identity, Object, Role, Roles, Solver, Target};

    #[kani::proof]
    #[kani::unwind(40)]
    fn native_default_target_and_object_admission() {
        let target: Target = kani::any();
        let kind: Object = kani::any();
        let header: [u8; 32] = kani::any();
        let prefix: u8 = kani::any();
        let bytes = header.get(..usize::from(prefix)).unwrap_or(&[]);
        let length: u64 = kani::any();
        let admitted = native_default::object(target, kind, bytes, length).is_ok();
        let elf = bytes.get(..7) == Some(&[127, 69, 76, 70, 2, 1, 1])
            && bytes.get(18..20) == Some(&[62, 0])
            && bytes.get(20..24) == Some(&[1, 0, 0, 0]);
        let mach = bytes.get(..8) == Some(&[207, 250, 237, 254, 12, 0, 0, 1]);
        let expected = match (target, kind) {
            (_, Object::Support) => true,
            (Target::LinuxAmd64, Object::Executable) => {
                length >= 64
                    && bytes.len() >= 32
                    && elf
                    && matches!(bytes.get(16..18), Some([2 | 3, 0]))
            }
            (Target::LinuxAmd64, Object::Library) => {
                length >= 64 && bytes.len() >= 32 && elf && bytes.get(16..18) == Some(&[3, 0])
            }
            (Target::MacArm64, Object::Executable) => {
                length >= 32
                    && bytes.len() >= 32
                    && mach
                    && bytes.get(12..16) == Some(&[2, 0, 0, 0])
            }
            (Target::MacArm64, Object::Library) => {
                length >= 32
                    && bytes.len() >= 32
                    && mach
                    && bytes.get(12..16) == Some(&[6, 0, 0, 0])
            }
        };
        assert_eq!(admitted, expected);
        kani::cover!(
            admitted && kind != Object::Support,
            "native object architecture admitted"
        );
        kani::cover!(!admitted, "foreign or incomplete native object refused");
    }

    #[kani::proof]
    fn native_default_required_roles_and_limits() {
        let mask: u32 = kani::any();
        let role: Role = kani::any();
        let other: Role = kani::any();
        assert_eq!(role.bit() == 0, role == Role::Support);
        assert_eq!(role.bit() == other.bit(), role == other);
        assert_eq!(role.bit() & !native_default::REQUIRED_ROLES, 0);
        let before = Roles::from_observation(mask);
        assert_eq!(before.admits(), mask == native_default::REQUIRED_ROLES);
        let result = before.include(role);
        assert_eq!(result.is_ok(), role.bit() == 0 || mask & role.bit() == 0);
        if let Ok(after) = result {
            assert_eq!(after.mask(), mask | role.bit());
        }
        let members: u32 = kani::any();
        let total: u64 = kani::any();
        let bytes: u64 = kani::any();
        let checkpoint = Budget::from_observation(members, total);
        let expected = members <= native_default::MAX_MEMBERS
            && total <= native_default::MAX_TOTAL_BYTES
            && total <= u64::from(members) * native_default::MAX_OBJECT_BYTES;
        assert_eq!(checkpoint.is_some(), expected);
        if let Some(before) = checkpoint {
            let result = before.include(bytes);
            let expected = bytes <= native_default::MAX_OBJECT_BYTES
                && members < native_default::MAX_MEMBERS
                && total
                    .checked_add(bytes)
                    .is_some_and(|sum| sum <= native_default::MAX_TOTAL_BYTES);
            assert_eq!(result.is_ok(), expected);
            if let Ok(after) = result {
                assert_eq!(after.members(), members + 1);
                assert_eq!(Some(after.bytes()), total.checked_add(bytes));
            }
            kani::cover!(result.is_ok(), "bounded actual tool member admitted");
            kani::cover!(result.is_err(), "tool member bound refused");
        }
        kani::cover!(before.admits(), "all actual required roles present");
        kani::cover!(!before.admits(), "missing or unknown required role refused");
    }

    #[kani::proof]
    #[kani::unwind(40)]
    fn native_default_full_identity_recheck() {
        let before: Identity = kani::any();
        let after: Identity = kani::any();
        let mut same_bytes = true;
        for index in 0..native_default::DIGEST_BYTES {
            same_bytes = same_bytes
                && before.path.get(index) == after.path.get(index)
                && before.content.get(index) == after.content.get(index);
        }
        let expected = same_bytes
            && before.device == after.device
            && before.inode == after.inode
            && before.bytes == after.bytes
            && before.mode == after.mode
            && before.owner == after.owner
            && before.group == after.group;
        let actual = native_default::unchanged(&before, &after);
        assert_eq!(actual, expected);
        kani::cover!(actual, "whole actual tool identity unchanged");
        kani::cover!(!actual, "changed tool identity refused");
    }

    #[kani::proof]
    fn native_default_builtin_configuration() {
        let solver: Solver = kani::any();
        let bits: u32 = kani::any();
        let explicit_null: bool = kani::any();
        let actual = native_default::configuration(solver, bits, explicit_null);
        assert_eq!(
            actual,
            solver == Solver::Cadical && bits == 16 && explicit_null
        );
        kani::cover!(actual, "actual builtin Cadical configuration admitted");
        kani::cover!(!actual, "conflicting builtin configuration refused");
    }

    #[kani::proof]
    fn native_default_loader_search_path_filter() {
        use native_default::LoaderPart;
        let tail: [LoaderPart; native_default::LOADER_TAIL_PARTS] = kani::any();
        let actual = native_default::retain_loader_tail(tail);
        let filtered = matches!(
            tail,
            [
                LoaderPart::Library,
                LoaderPart::Library | LoaderPart::Toolchains | LoaderPart::Other,
                LoaderPart::Toolchains
            ]
        );
        assert_eq!(actual, !filtered);
        kani::cover!(actual, "non-toolchain native loader path retained");
        kani::cover!(!actual, "actual toolchain lib suffix filtered");
    }

    #[kani::proof]
    fn rejects_unfiltered_native_default_toolchain_loader() {
        use native_default::LoaderPart;
        kani::assert(
            native_default::retain_loader_tail([
                LoaderPart::Library,
                LoaderPart::Other,
                LoaderPart::Toolchains,
            ]),
            "NEGATIVE_DEFAULT_LOADER_TAIL",
        );
    }

    #[kani::proof]
    fn rejects_foreign_native_default_architecture() {
        kani::assert(
            native_default::object(Target::LinuxAmd64, Object::Executable, &[0; 32], 64).is_ok(),
            "NEGATIVE_DEFAULT_ARCHITECTURE",
        );
    }

    #[kani::proof]
    fn rejects_missing_native_default_required_role() {
        kani::assert(
            Roles::from_observation(native_default::REQUIRED_ROLES & !Role::Compiler.bit())
                .admits(),
            "NEGATIVE_DEFAULT_REQUIRED_ROLE",
        );
    }

    #[kani::proof]
    #[kani::unwind(40)]
    fn rejects_changed_native_default_identity() {
        let before: Identity = kani::any();
        let after = Identity {
            group: before.group.wrapping_add(1),
            ..before
        };
        kani::assert(
            native_default::unchanged(&before, &after),
            "NEGATIVE_DEFAULT_IDENTITY_CHANGE",
        );
    }

    #[kani::proof]
    fn rejects_foreign_native_default_configuration() {
        kani::assert(
            native_default::configuration(Solver::Other, native_default::OBJECT_BITS, true),
            "NEGATIVE_DEFAULT_CONFIGURATION",
        );
    }
}
