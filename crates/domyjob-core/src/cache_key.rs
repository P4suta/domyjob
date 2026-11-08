use serde::{Deserialize, Serialize};

pub const DIGEST_BYTES: usize = blake3::OUT_LEN;
const BITS_PER_BYTE: usize = 8;
const BITS_PER_WORD: usize = 16;
const BITS_PER_DIGIT: usize = 5;
const DIGIT_ALPHABET: &[u8; 1 << BITS_PER_DIGIT] = b"0123456789abcdefghijklmnopqrstuv";
pub const ADDRESS_BYTES: usize = (DIGEST_BYTES * BITS_PER_BYTE).div_ceil(BITS_PER_DIGIT);
pub const ROUTE_BYTES: usize = 16;
pub const HEX_ADDRESS_BYTES: usize = DIGEST_BYTES * 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheAddress {
    Route,
    Expanded,
    Legacy,
}

impl CacheAddress {
    #[must_use]
    pub const fn width(self) -> usize {
        match self {
            Self::Route => ROUTE_BYTES,
            Self::Expanded => ADDRESS_BYTES,
            Self::Legacy => HEX_ADDRESS_BYTES,
        }
    }

    #[must_use]
    pub fn admits(self, bytes: &[u8]) -> bool {
        if bytes.len() != self.width() {
            return false;
        }
        let letters = match self {
            Self::Legacy => b'f',
            Self::Route | Self::Expanded => b'v',
        };
        bytes
            .iter()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=letters).contains(byte))
            && (self != Self::Expanded || matches!(bytes.last(), Some(b'0' | b'g')))
    }
}

#[must_use]
pub fn address(bytes: [u8; DIGEST_BYTES]) -> [u8; ADDRESS_BYTES] {
    let mut encoded = [0; ADDRESS_BYTES];
    for (index, output) in encoded.iter_mut().enumerate() {
        let bit = index.saturating_mul(BITS_PER_DIGIT);
        let byte = bit / BITS_PER_BYTE;
        let word = (u16::from(bytes.get(byte).copied().unwrap_or_default()) << BITS_PER_BYTE)
            | u16::from(
                bytes
                    .get(byte.saturating_add(1))
                    .copied()
                    .unwrap_or_default(),
            );
        let shift = BITS_PER_WORD
            .saturating_sub(BITS_PER_DIGIT)
            .saturating_sub(bit % BITS_PER_BYTE);
        let digit = (word >> shift) & ((1 << BITS_PER_DIGIT) - 1);
        *output = DIGIT_ALPHABET
            .get(usize::from(digit))
            .copied()
            .unwrap_or_default();
    }
    encoded
}

#[must_use]
pub fn route(bytes: [u8; DIGEST_BYTES]) -> [u8; ROUTE_BYTES] {
    let mut route = [0; ROUTE_BYTES];
    for (output, digit) in route.iter_mut().zip(address(bytes)) {
        *output = digit;
    }
    route
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheBinding {
    Vacant,
    Matching,
    Conflicting,
}

#[must_use]
pub fn project_binding(
    requested: [u8; DIGEST_BYTES],
    recorded: Option<[u8; DIGEST_BYTES]>,
) -> CacheBinding {
    binding(requested, recorded)
}

fn binding<T: PartialEq + Copy>(requested: T, recorded: Option<T>) -> CacheBinding {
    match recorded {
        None => CacheBinding::Vacant,
        Some(recorded) if recorded == requested => CacheBinding::Matching,
        Some(_) => CacheBinding::Conflicting,
    }
}

#[must_use]
pub fn slot_binding(requested: CacheKey, recorded: Option<CacheKey>) -> CacheBinding {
    binding(requested, recorded)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CacheKey {
    project: [u8; DIGEST_BYTES],
    inputs: [u8; DIGEST_BYTES],
}

impl CacheKey {
    #[must_use]
    pub const fn new(project: [u8; DIGEST_BYTES], inputs: [u8; DIGEST_BYTES]) -> Self {
        Self { project, inputs }
    }

    #[must_use]
    pub const fn project(self) -> [u8; DIGEST_BYTES] {
        self.project
    }

    #[must_use]
    pub const fn inputs(self) -> [u8; DIGEST_BYTES] {
        self.inputs
    }
}

#[cfg(kani)]
mod proofs {
    use super::{
        ADDRESS_BYTES, BITS_PER_BYTE, BITS_PER_DIGIT, CacheAddress, CacheBinding, CacheKey,
        DIGEST_BYTES, HEX_ADDRESS_BYTES, address, project_binding, route, slot_binding,
    };

    const NUMERIC_DIGITS: u8 = 10;

    #[kani::proof]
    #[kani::unwind(65)]
    fn selection_addresses_require_canonical_digits_and_padding() {
        let bytes: [u8; HEX_ADDRESS_BYTES] = kani::any();
        let kinds = [
            CacheAddress::Route,
            CacheAddress::Expanded,
            CacheAddress::Legacy,
        ];
        let index: u8 = kani::any();
        kani::assume(usize::from(index) < kinds.len());
        let kind = kinds[usize::from(index)];
        let length: u8 = kani::any();
        kani::assume(usize::from(length) <= bytes.len());
        let candidate = &bytes[..usize::from(length)];
        let mut canonical = candidate.len() == kind.width();
        for byte in candidate {
            canonical &= byte.is_ascii_digit()
                || (byte.is_ascii_lowercase()
                    && *byte
                        <= if kind == CacheAddress::Legacy {
                            b'f'
                        } else {
                            b'v'
                        });
        }
        let padded = kind != CacheAddress::Expanded
            || candidate
                .last()
                .is_some_and(|last| *last == b'0' || *last == b'g');
        assert_eq!(kind.admits(candidate), canonical && padded);
        kani::cover!(kind == CacheAddress::Route && canonical && padded);
        kani::cover!(kind == CacheAddress::Expanded && canonical && padded);
        kani::cover!(kind == CacheAddress::Legacy && canonical && padded);
        kani::cover!(candidate.len() == kind.width() && !canonical);
        kani::cover!(canonical && !padded);
    }

    #[kani::proof]
    #[kani::unwind(65)]
    fn rejects_nul_selection_addresses() {
        kani::assert(
            CacheAddress::Legacy.admits(&[0; HEX_ADDRESS_BYTES]),
            "NEGATIVE_CACHE_SELECTION_CONTROL",
        );
    }

    #[kani::proof]
    #[kani::unwind(53)]
    fn compact_addresses_preserve_every_digest_bit() {
        let bytes: [u8; DIGEST_BYTES] = kani::any();
        let bit: u16 = kani::any();
        let bit = usize::from(bit);
        kani::assume(bit < DIGEST_BYTES * BITS_PER_BYTE);
        let encoded = address(bytes);
        let character = encoded[bit / BITS_PER_DIGIT];
        assert!(character.is_ascii_digit() || (b'a'..=b'v').contains(&character));
        let digit = if character.is_ascii_digit() {
            character - b'0'
        } else {
            character - b'a' + NUMERIC_DIGITS
        };
        let actual = (digit >> (BITS_PER_DIGIT - 1 - bit % BITS_PER_DIGIT)) & 1;
        let expected =
            (bytes[bit / BITS_PER_BYTE] >> (BITS_PER_BYTE - 1 - bit % BITS_PER_BYTE)) & 1;
        assert_eq!(actual, expected);
        assert_eq!(encoded.len(), ADDRESS_BYTES);
        kani::cover!(bit == 0);
        kani::cover!(bit == BITS_PER_BYTE);
        kani::cover!(bit == DIGEST_BYTES * BITS_PER_BYTE - 1);
    }

    #[kani::proof]
    #[kani::unwind(53)]
    fn rejects_discarding_the_last_digest_bit() {
        let zero = [0; DIGEST_BYTES];
        let mut different = zero;
        different[DIGEST_BYTES - 1] = 1;
        kani::assert(
            address(zero) == address(different),
            "NEGATIVE_CACHE_ADDRESS_CONTROL",
        );
    }

    #[kani::proof]
    #[kani::unwind(33)]
    fn cache_records_require_every_project_and_input_byte() {
        let requested = CacheKey::new(kani::any(), kani::any());
        let saved = CacheKey::new(kani::any(), kani::any());
        let present: bool = kani::any();
        let same_project = requested.project() == saved.project();
        let same_inputs = requested.inputs() == saved.inputs();
        let recorded = if present { Some(saved) } else { None };
        assert_eq!(
            slot_binding(requested, recorded),
            if !present {
                CacheBinding::Vacant
            } else if same_project && same_inputs {
                CacheBinding::Matching
            } else {
                CacheBinding::Conflicting
            }
        );
        assert_eq!(
            project_binding(
                requested.project(),
                if present { Some(saved.project()) } else { None }
            ),
            if !present {
                CacheBinding::Vacant
            } else if same_project {
                CacheBinding::Matching
            } else {
                CacheBinding::Conflicting
            }
        );
        kani::cover!(!present);
        kani::cover!(present && same_project && same_inputs);
        kani::cover!(present && !same_project && same_inputs);
        kani::cover!(present && same_project && !same_inputs);
        kani::cover!(present && !same_project && !same_inputs);
    }

    #[kani::proof]
    #[kani::unwind(53)]
    fn rejects_aliasing_distinct_keys_with_the_same_route() {
        let zero = [0; DIGEST_BYTES];
        let mut different = zero;
        different[DIGEST_BYTES - 1] = 1;
        let requested = CacheKey::new(zero, zero);
        let recorded = CacheKey::new(zero, different);
        assert_eq!(route(requested.inputs()), route(recorded.inputs()));
        kani::assert(
            slot_binding(requested, Some(recorded)) == CacheBinding::Matching,
            "NEGATIVE_CACHE_BINDING_CONTROL",
        );
    }
}

#[cfg(test)]
mod tests {
    use super::{
        ADDRESS_BYTES, CacheAddress, CacheBinding, CacheKey, DIGEST_BYTES, HEX_ADDRESS_BYTES,
        address, project_binding, route, slot_binding,
    };

    #[test]
    fn matching_routes_cannot_admit_a_different_full_project_or_input() {
        let zero = [0; DIGEST_BYTES];
        let mut different = zero;
        different[DIGEST_BYTES - 1] = 1;
        assert_eq!(route(zero), route(different));
        assert_eq!(
            project_binding(zero, Some(different)),
            CacheBinding::Conflicting
        );
        let requested = CacheKey::new(zero, zero);
        assert_eq!(slot_binding(requested, None), CacheBinding::Vacant);
        assert_eq!(
            slot_binding(requested, Some(requested)),
            CacheBinding::Matching
        );
        for recorded in [
            CacheKey::new(different, zero),
            CacheKey::new(zero, different),
        ] {
            assert_eq!(
                slot_binding(requested, Some(recorded)),
                CacheBinding::Conflicting
            );
        }
    }

    #[test]
    fn selection_addresses_refuse_wrong_lengths_case_nuls_and_nonzero_padding() {
        for kind in [
            CacheAddress::Route,
            CacheAddress::Expanded,
            CacheAddress::Legacy,
        ] {
            let valid = [b'0'; HEX_ADDRESS_BYTES];
            assert!(kind.admits(valid.get(..kind.width()).unwrap()));
            assert!(!kind.admits(valid.get(..kind.width().saturating_sub(1)).unwrap()));
            assert!(!kind.admits(&[0; HEX_ADDRESS_BYTES]));
            let mut uppercase = valid;
            uppercase[0] = b'A';
            assert!(!kind.admits(uppercase.get(..kind.width()).unwrap()));
        }
        let mut invalid_padding = [b'0'; ADDRESS_BYTES];
        invalid_padding[ADDRESS_BYTES - 1] = b'1';
        assert!(!CacheAddress::Expanded.admits(&invalid_padding));
        invalid_padding[ADDRESS_BYTES - 1] = b'g';
        assert!(CacheAddress::Expanded.admits(&invalid_padding));
    }

    #[test]
    fn addresses_use_lowercase_base32hex_without_padding_or_lost_bits() {
        let zero = [0; DIGEST_BYTES];
        assert_eq!(address(zero), [b'0'; ADDRESS_BYTES]);
        let mut different = zero;
        different[DIGEST_BYTES - 1] = 1;
        let encoded = address(different);
        assert_eq!(&encoded[..ADDRESS_BYTES - 1], &[b'0'; ADDRESS_BYTES - 1]);
        assert_eq!(encoded[ADDRESS_BYTES - 1], b'g');
        let ones = address([u8::MAX; DIGEST_BYTES]);
        assert_eq!(&ones[..ADDRESS_BYTES - 1], &[b'v'; ADDRESS_BYTES - 1]);
        assert_eq!(ones[ADDRESS_BYTES - 1], b'g');
    }
}
