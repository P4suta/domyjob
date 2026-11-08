#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(kani, derive(kani::Arbitrary))]
pub enum Root {
    PrivateRetired,
    Unadmitted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(kani, derive(kani::Arbitrary))]
pub enum Entry {
    Directory,
    File,
    Link,
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(kani, derive(kani::Arbitrary))]
pub enum Owner {
    Admitted,
    Foreign,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Preparation {
    DirectoryAccess,
    Unchanged,
    Refuse,
}

pub const OWNER_DIRECTORY_ACCESS: u32 = 0o700;

#[must_use]
pub const fn prepare(root: Root, entry: Entry, owner: Owner) -> Preparation {
    match (root, entry, owner) {
        (Root::PrivateRetired, Entry::Directory, Owner::Admitted) => Preparation::DirectoryAccess,
        (Root::PrivateRetired, Entry::File, Owner::Admitted)
        | (Root::PrivateRetired, Entry::Link, _) => Preparation::Unchanged,
        _ => Preparation::Refuse,
    }
}

#[must_use]
pub const fn directory_mode(mode: u32) -> u32 {
    mode | OWNER_DIRECTORY_ACCESS
}

#[cfg(kani)]
mod proofs {
    use super::{Entry, OWNER_DIRECTORY_ACCESS, Owner, Preparation, Root, directory_mode, prepare};

    #[kani::proof]
    #[kani::unwind(2)]
    fn permission_repair_requires_an_owned_retired_directory() {
        let root: Root = kani::any();
        let entry: Entry = kani::any();
        let owner: Owner = kani::any();
        let mode: u32 = kani::any();
        let action = prepare(root, entry, owner);
        assert_eq!(
            action == Preparation::DirectoryAccess,
            root == Root::PrivateRetired && entry == Entry::Directory && owner == Owner::Admitted
        );
        if root == Root::Unadmitted || entry == Entry::Other {
            assert_eq!(action, Preparation::Refuse);
        }
        if root == Root::PrivateRetired && entry == Entry::Link {
            assert_eq!(action, Preparation::Unchanged);
        }
        if entry != Entry::Link && owner != Owner::Admitted {
            assert_eq!(action, Preparation::Refuse);
        }
        let repaired = directory_mode(mode);
        assert_eq!(repaired & OWNER_DIRECTORY_ACCESS, OWNER_DIRECTORY_ACCESS);
        assert_eq!(
            repaired & !OWNER_DIRECTORY_ACCESS,
            mode & !OWNER_DIRECTORY_ACCESS
        );
        kani::cover!(action == Preparation::DirectoryAccess);
        kani::cover!(root == Root::Unadmitted);
        kani::cover!(entry == Entry::Directory && owner == Owner::Foreign);
        kani::cover!(
            root == Root::PrivateRetired && entry == Entry::Link && owner == Owner::Unknown
        );
        kani::cover!(
            root == Root::PrivateRetired && entry == Entry::File && owner == Owner::Admitted
        );
        kani::cover!(entry == Entry::Other);
        kani::cover!(entry == Entry::Directory && owner == Owner::Unknown);
    }

    #[kani::proof]
    #[kani::unwind(2)]
    fn rejects_repairing_a_link_target() {
        kani::assert(
            prepare(Root::PrivateRetired, Entry::Link, Owner::Admitted)
                == Preparation::DirectoryAccess,
            "NEGATIVE_RETIRED_PERMISSION_CONTROL",
        );
    }
}
