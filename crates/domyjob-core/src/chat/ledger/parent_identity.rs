use super::super::id::EventId;

pub(super) fn matches(parent: Option<&EventId>, request: &EventId) -> bool {
    match parent {
        Some(id) => id == request,
        None => true,
    }
}

#[cfg(kani)]
mod proofs {
    use alloc::borrow::ToOwned;
    use core::ascii::Char;
    use core::num::NonZeroU64;

    use super::super::Parent;
    use super::matches;
    use crate::chat::id::{EventId, Invalid, Origin};

    const ORIGIN_BYTES: usize = 32;
    const HEX_DIGITS: &[u8; 16] = b"0123456789abcdef";
    const HEX_RADIX: u8 = 16;
    const DECIMAL_DIGITS: u8 = 10;
    const ASCII_MASK: u8 = 0x7f;
    const PRESENT: u8 = 0;
    const OPAQUE: u8 = 1;
    const MISSING: u8 = 2;
    const VARIANTS: u8 = 3;
    const DIFFERENT_SEQUENCE: u64 = 2;

    struct GeneratedIdentity {
        identity: EventId,
        bytes: [u8; ORIGIN_BYTES],
    }

    fn hex_digit(byte: u8) -> u8 {
        let nibble = byte % HEX_RADIX;
        if nibble < DECIMAL_DIGITS {
            b'0'.checked_add(nibble).unwrap()
        } else {
            b'a'.checked_add(nibble.checked_sub(DECIMAL_DIGITS).unwrap())
                .unwrap()
        }
    }

    #[kani::proof]
    fn canonical_parent_hex_encoder_preserves_the_exact_full_domain() {
        let byte: u8 = kani::any();
        let encoded = hex_digit(byte);
        assert_eq!(encoded, HEX_DIGITS[usize::from(byte) % HEX_DIGITS.len()]);
        assert!(encoded.is_ascii_digit() || matches!(encoded, b'a'..=b'f'));
        assert!(encoded & ASCII_MASK == encoded);
        assert!(u8::from(Char::from_u8(encoded & ASCII_MASK).unwrap()) == encoded);
        let wanted: u8 = kani::any();
        if wanted.is_ascii_digit() {
            let inverse = wanted.checked_sub(b'0').unwrap();
            assert_eq!(hex_digit(inverse), wanted);
        } else if matches!(wanted, b'a'..=b'f') {
            let inverse = wanted
                .checked_sub(b'a')
                .unwrap()
                .checked_add(DECIMAL_DIGITS)
                .unwrap();
            assert_eq!(hex_digit(inverse), wanted);
        }
        kani::cover!(encoded == b'0');
        kani::cover!(encoded == b'f');
    }

    #[kani::proof]
    fn rejects_a_changed_parent_hex_encoder_value() {
        kani::assert(hex_digit(0) == b'f', "NEGATIVE_PARENT_HEX_ENCODER");
    }

    fn identity() -> Result<GeneratedIdentity, Invalid> {
        let choices: [u8; ORIGIN_BYTES] = kani::any();
        let bytes = choices.map(hex_digit);
        let chars = bytes.map(|byte| Char::from_u8(byte & ASCII_MASK).unwrap());
        let text = chars.as_str().to_owned();
        let origin = Origin::try_from(text)?;
        let sequence = NonZeroU64::new(kani::any()).unwrap_or(NonZeroU64::MIN);
        Ok(GeneratedIdentity {
            identity: EventId::new(origin, sequence),
            bytes,
        })
    }

    #[kani::proof]
    #[kani::unwind(34)]
    fn canonical_parent_identity_generator_preserves_every_origin_byte() {
        let generated = identity().unwrap();
        assert!(generated.identity.origin().as_str().as_bytes() == generated.bytes);
        kani::cover!(generated.identity.seq() == NonZeroU64::MIN);
    }

    #[kani::proof]
    #[kani::unwind(34)]
    fn present_parent_identity_matches_exact_origin_and_sequence() {
        let parent = identity().unwrap();
        let request = identity().unwrap();
        assert!(parent.identity.origin().as_str().as_bytes() == parent.bytes);
        assert!(request.identity.origin().as_str().as_bytes() == request.bytes);
        let origin_equal = parent.identity.origin().as_str().as_bytes()
            == request.identity.origin().as_str().as_bytes();
        let sequence_equal = parent.identity.seq() == request.identity.seq();
        let admitted = matches(Some(&parent.identity), &request.identity);
        assert_eq!(admitted, origin_equal && sequence_equal);
        kani::cover!(origin_equal && sequence_equal && admitted);
        kani::cover!(origin_equal && !sequence_equal && !admitted);
        kani::cover!(!origin_equal && sequence_equal && !admitted);
        kani::cover!(!origin_equal && !sequence_equal && !admitted);
    }

    #[kani::proof]
    #[kani::unwind(34)]
    fn absent_parent_identity_accepts_every_request() {
        let parent = identity().unwrap();
        let request = identity().unwrap();
        assert!(parent.identity.origin().as_str().as_bytes() == parent.bytes);
        assert!(request.identity.origin().as_str().as_bytes() == request.bytes);
        let opaque_admitted = Parent::Opaque.matches(&request.identity);
        let missing_admitted = Parent::Missing.matches(&request.identity);
        assert!(opaque_admitted);
        assert!(missing_admitted);
        kani::cover!(opaque_admitted);
        kani::cover!(missing_admitted);
    }

    #[kani::proof]
    #[kani::unwind(34)]
    fn projected_parent_identity_preserves_every_complete_identity_class() {
        let parent = identity().unwrap();
        let request = identity().unwrap();
        assert!(parent.identity.origin().as_str().as_bytes() == parent.bytes);
        assert!(request.identity.origin().as_str().as_bytes() == request.bytes);
        let variant = kani::any::<u8>() % VARIANTS;
        let origin_equal = parent.identity.origin().as_str().as_bytes()
            == request.identity.origin().as_str().as_bytes();
        let sequence_equal = parent.identity.seq() == request.identity.seq();
        let admitted = match variant {
            PRESENT => matches(Some(&parent.identity), &request.identity),
            OPAQUE => Parent::Opaque.matches(&request.identity),
            _ => Parent::Missing.matches(&request.identity),
        };
        assert_eq!(
            admitted,
            variant != PRESENT || (origin_equal && sequence_equal)
        );
        kani::cover!(variant == PRESENT && origin_equal && sequence_equal && admitted);
        kani::cover!(variant == PRESENT && origin_equal && !sequence_equal && !admitted);
        kani::cover!(variant == PRESENT && !origin_equal && sequence_equal && !admitted);
        kani::cover!(variant == PRESENT && !origin_equal && !sequence_equal && !admitted);
        kani::cover!(variant == OPAQUE && admitted);
        kani::cover!(variant == MISSING && admitted);
    }

    #[kani::proof]
    #[kani::unwind(34)]
    fn rejects_a_present_parent_with_a_different_sequence() {
        let generated = identity().unwrap();
        assert!(generated.identity.origin().as_str().as_bytes() == generated.bytes);
        let machine = generated.identity.origin().clone();
        let parent = EventId::new(
            machine.clone(),
            NonZeroU64::new(DIFFERENT_SEQUENCE).unwrap(),
        );
        let request = EventId::new(machine, NonZeroU64::MIN);
        kani::assert(matches(Some(&parent), &request), "NEGATIVE_PARENT_IDENTITY");
    }
}
