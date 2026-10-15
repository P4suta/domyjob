pub(crate) const MAX_OBJECT_BYTES: u64 = 256 * 1024 * 1024;
pub(crate) const MAX_TOTAL_BYTES: u64 = 1024 * 1024 * 1024;
pub(crate) const MAX_MEMBERS: u32 = 1024;
pub(crate) const DIGEST_BYTES: usize = 32;
pub(crate) const OBJECT_BITS: u32 = 16;
pub(crate) const HEADER_PREFIX_BYTES: usize = 32;
pub(crate) const LOADER_TAIL_PARTS: usize = 3;
const WORD_BYTES: usize = 4;
const ELF_TAG: [u8; 7] = [0x7f, b'E', b'L', b'F', 2, 1, 1];
const ELF_MACHINE: [u8; 2] = [62, 0];
const ELF_MACHINE_OFFSET: usize = 18;
const ELF_VERSION_OFFSET: usize = 20;
const ELF_TYPE_OFFSET: usize = 16;
const ELF_EXECUTABLE_TYPE: [u8; 2] = [2, 0];
const ELF_DYNAMIC_TYPE: [u8; 2] = [3, 0];
const ELF_HEADER_BYTES: u64 = 64;
const MACH_TAG: [u8; WORD_BYTES] = [0xcf, 0xfa, 0xed, 0xfe];
const MACH_MACHINE_OFFSET: usize = 4;
const MACH_MACHINE_ARM64: u32 = 0x0100_000c;
const MACH_TYPE_OFFSET: usize = 12;
const MACH_EXECUTABLE_TYPE: u32 = 2;
const MACH_LIBRARY_TYPE: u32 = 6;
const MACH_HEADER_BYTES: u64 = 32;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(kani, derive(kani::Arbitrary))]
pub(crate) enum Target {
    LinuxAmd64,
    MacArm64,
}

impl Target {
    pub(crate) const fn triple(self) -> &'static str {
        match self {
            Self::LinuxAmd64 => "x86_64-unknown-linux-gnu",
            Self::MacArm64 => "aarch64-apple-darwin",
        }
    }

    pub(crate) const fn cbmc_os(self) -> &'static str {
        match self {
            Self::LinuxAmd64 => "x86_64 linux unix",
            Self::MacArm64 => "aarch64 macos unix",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(kani, derive(kani::Arbitrary))]
pub(crate) enum Object {
    Executable,
    Library,
    Support,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Refusal {
    Object,
    Count,
    Aggregate,
    Architecture,
    DuplicateRole,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Budget {
    members: u32,
    bytes: u64,
}

impl Budget {
    pub(crate) const fn new() -> Self {
        Self {
            members: 0,
            bytes: 0,
        }
    }

    pub(crate) fn from_observation(members: u32, bytes: u64) -> Option<Self> {
        if members <= MAX_MEMBERS
            && bytes <= MAX_TOTAL_BYTES
            && bytes <= u64::from(members) * MAX_OBJECT_BYTES
        {
            Some(Self { members, bytes })
        } else {
            None
        }
    }

    pub(crate) const fn include(self, bytes: u64) -> Result<Self, Refusal> {
        if bytes > MAX_OBJECT_BYTES {
            return Err(Refusal::Object);
        }
        let Some(members) = self.members.checked_add(1) else {
            return Err(Refusal::Count);
        };
        if members > MAX_MEMBERS {
            return Err(Refusal::Count);
        }
        let Some(total) = self.bytes.checked_add(bytes) else {
            return Err(Refusal::Aggregate);
        };
        if total > MAX_TOTAL_BYTES {
            return Err(Refusal::Aggregate);
        }
        Ok(Self {
            members,
            bytes: total,
        })
    }

    pub(crate) const fn members(self) -> u32 {
        self.members
    }

    pub(crate) const fn bytes(self) -> u64 {
        self.bytes
    }
}

impl Default for Budget {
    fn default() -> Self {
        Self::new()
    }
}

fn word(bytes: &[u8], offset: usize) -> Option<u32> {
    let end = offset.checked_add(WORD_BYTES)?;
    let Ok(value): Result<[u8; WORD_BYTES], _> = bytes.get(offset..end)?.try_into() else {
        return None;
    };
    Some(u32::from_le_bytes(value))
}

pub(crate) fn object(
    target: Target,
    kind: Object,
    bytes: &[u8],
    length: u64,
) -> Result<(), Refusal> {
    if kind == Object::Support {
        return Ok(());
    }
    let admitted = match target {
        Target::LinuxAmd64 => {
            let tag = bytes.get(..ELF_TAG.len()) == Some(&ELF_TAG);
            let machine = bytes.get(ELF_MACHINE_OFFSET..ELF_VERSION_OFFSET) == Some(&ELF_MACHINE);
            let version = word(bytes, ELF_VERSION_OFFSET) == Some(1);
            let observed_type = bytes.get(ELF_TYPE_OFFSET..ELF_MACHINE_OFFSET);
            let format = match kind {
                Object::Executable => {
                    observed_type == Some(&ELF_EXECUTABLE_TYPE)
                        || observed_type == Some(&ELF_DYNAMIC_TYPE)
                }
                Object::Library => observed_type == Some(&ELF_DYNAMIC_TYPE),
                Object::Support => false,
            };
            length >= ELF_HEADER_BYTES
                && bytes.len() >= HEADER_PREFIX_BYTES
                && tag
                && machine
                && version
                && format
        }
        Target::MacArm64 => {
            let tag = bytes.get(..MACH_TAG.len()) == Some(&MACH_TAG);
            let machine = word(bytes, MACH_MACHINE_OFFSET) == Some(MACH_MACHINE_ARM64);
            let format = match kind {
                Object::Executable => word(bytes, MACH_TYPE_OFFSET) == Some(MACH_EXECUTABLE_TYPE),
                Object::Library => word(bytes, MACH_TYPE_OFFSET) == Some(MACH_LIBRARY_TYPE),
                Object::Support => false,
            };
            length >= MACH_HEADER_BYTES
                && bytes.len() >= HEADER_PREFIX_BYTES
                && tag
                && machine
                && format
        }
    };
    if admitted {
        Ok(())
    } else {
        Err(Refusal::Architecture)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(kani, derive(kani::Arbitrary))]
pub(crate) struct Identity {
    pub(crate) path: [u8; DIGEST_BYTES],
    pub(crate) content: [u8; DIGEST_BYTES],
    pub(crate) device: u64,
    pub(crate) inode: u64,
    pub(crate) bytes: u64,
    pub(crate) mode: u32,
    pub(crate) owner: u32,
    pub(crate) group: u32,
}

pub(crate) fn unchanged(before: &Identity, after: &Identity) -> bool {
    before == after
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(kani, derive(kani::Arbitrary))]
pub(crate) enum Role {
    Mise,
    Observer,
    OuterCargo,
    CargoProxy,
    StandaloneProxy,
    Driver,
    Compiler,
    Cbmc,
    GotoCc,
    GotoInstrument,
    LinkedCargo,
    LinkedRustc,
    RustcRuntime,
    LlvmRuntime,
    CompilerMarker,
    ToolchainMarker,
    KaniLibC,
    Support,
}

impl Role {
    const MISE: u32 = 1;
    const OBSERVER: u32 = 2;
    const OUTER_CARGO: u32 = 4;
    const CARGO_PROXY: u32 = 8;
    const STANDALONE_PROXY: u32 = 16;
    const DRIVER: u32 = 32;
    const COMPILER: u32 = 64;
    const CBMC: u32 = 128;
    const GOTO_CC: u32 = 256;
    const GOTO_INSTRUMENT: u32 = 512;
    const LINKED_CARGO: u32 = 1024;
    const LINKED_RUSTC: u32 = 2048;
    const RUSTC_RUNTIME: u32 = 4096;
    const LLVM_RUNTIME: u32 = 8192;
    const COMPILER_MARKER: u32 = 16384;
    const TOOLCHAIN_MARKER: u32 = 32768;
    const KANI_LIB_C: u32 = 65536;

    pub(crate) const fn bit(self) -> u32 {
        match self {
            Self::Mise => Self::MISE,
            Self::Observer => Self::OBSERVER,
            Self::OuterCargo => Self::OUTER_CARGO,
            Self::CargoProxy => Self::CARGO_PROXY,
            Self::StandaloneProxy => Self::STANDALONE_PROXY,
            Self::Driver => Self::DRIVER,
            Self::Compiler => Self::COMPILER,
            Self::Cbmc => Self::CBMC,
            Self::GotoCc => Self::GOTO_CC,
            Self::GotoInstrument => Self::GOTO_INSTRUMENT,
            Self::LinkedCargo => Self::LINKED_CARGO,
            Self::LinkedRustc => Self::LINKED_RUSTC,
            Self::RustcRuntime => Self::RUSTC_RUNTIME,
            Self::LlvmRuntime => Self::LLVM_RUNTIME,
            Self::CompilerMarker => Self::COMPILER_MARKER,
            Self::ToolchainMarker => Self::TOOLCHAIN_MARKER,
            Self::KaniLibC => Self::KANI_LIB_C,
            Self::Support => 0,
        }
    }
}

pub(crate) const REQUIRED_ROLES: u32 = Role::Mise.bit()
    | Role::Observer.bit()
    | Role::OuterCargo.bit()
    | Role::CargoProxy.bit()
    | Role::StandaloneProxy.bit()
    | Role::Driver.bit()
    | Role::Compiler.bit()
    | Role::Cbmc.bit()
    | Role::GotoCc.bit()
    | Role::GotoInstrument.bit()
    | Role::LinkedCargo.bit()
    | Role::LinkedRustc.bit()
    | Role::RustcRuntime.bit()
    | Role::LlvmRuntime.bit()
    | Role::CompilerMarker.bit()
    | Role::ToolchainMarker.bit()
    | Role::KaniLibC.bit();

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Roles(u32);

impl Roles {
    pub(crate) const fn from_observation(mask: u32) -> Self {
        Self(mask)
    }

    pub(crate) const fn include(self, role: Role) -> Result<Self, Refusal> {
        let bit = role.bit();
        if bit != 0 && self.0 & bit != 0 {
            Err(Refusal::DuplicateRole)
        } else {
            Ok(Self(self.0 | bit))
        }
    }

    pub(crate) const fn admits(self) -> bool {
        self.0 == REQUIRED_ROLES
    }

    pub(crate) const fn mask(self) -> u32 {
        self.0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(kani, derive(kani::Arbitrary))]
pub(crate) enum Solver {
    Cadical,
    Other,
    Missing,
}

pub(crate) const fn configuration(solver: Solver, object_bits: u32, version_is_null: bool) -> bool {
    matches!(solver, Solver::Cadical) && object_bits == OBJECT_BITS && version_is_null
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(kani, derive(kani::Arbitrary))]
pub(crate) enum LoaderPart {
    Library,
    Toolchains,
    Other,
    Missing,
}

pub(crate) const fn retain_loader_tail(
    [last, middle, parent]: [LoaderPart; LOADER_TAIL_PARTS],
) -> bool {
    !(matches!(last, LoaderPart::Library)
        && !matches!(middle, LoaderPart::Missing)
        && matches!(parent, LoaderPart::Toolchains))
}

#[cfg(test)]
mod tests {
    use super::{
        Budget, MAX_MEMBERS, MAX_OBJECT_BYTES, MAX_TOTAL_BYTES, REQUIRED_ROLES, Refusal, Role,
        Roles,
    };

    #[test]
    fn every_required_role_is_unique_and_individually_necessary() {
        let roles = [
            Role::Mise,
            Role::Observer,
            Role::OuterCargo,
            Role::CargoProxy,
            Role::StandaloneProxy,
            Role::Driver,
            Role::Compiler,
            Role::Cbmc,
            Role::GotoCc,
            Role::GotoInstrument,
            Role::LinkedCargo,
            Role::LinkedRustc,
            Role::RustcRuntime,
            Role::LlvmRuntime,
            Role::CompilerMarker,
            Role::ToolchainMarker,
            Role::KaniLibC,
        ];
        let mut observed = Roles::from_observation(0);
        for role in roles {
            observed = observed.include(role).unwrap();
            assert_eq!(observed.include(role), Err(Refusal::DuplicateRole));
            assert!(!Roles::from_observation(REQUIRED_ROLES & !role.bit()).admits());
        }
        assert!(observed.admits());
        assert_eq!(observed.include(Role::Support).unwrap(), observed);
        assert!(!Roles::from_observation(u32::MAX).admits());
    }

    #[test]
    fn revised_tool_policy_refuses_both_exact_boundaries_and_overflow_inputs() {
        let full = Budget::from_observation(MAX_MEMBERS, MAX_TOTAL_BYTES).unwrap();
        assert_eq!(full.include(0), Err(Refusal::Count));
        assert_eq!(
            Budget::new().include(MAX_OBJECT_BYTES + 1),
            Err(Refusal::Object)
        );
        assert!(Budget::from_observation(u32::MAX, 0).is_none());
        assert!(Budget::from_observation(1, u64::MAX).is_none());
        assert!(Budget::from_observation(0, 1).is_none());
        let minimum_full_members =
            u32::try_from(MAX_TOTAL_BYTES.div_ceil(MAX_OBJECT_BYTES)).unwrap();
        let almost = Budget::from_observation(minimum_full_members, MAX_TOTAL_BYTES - 1).unwrap();
        assert_eq!(almost.include(2), Err(Refusal::Aggregate));
        assert_eq!(almost.include(1).unwrap().bytes(), MAX_TOTAL_BYTES);
    }
}
