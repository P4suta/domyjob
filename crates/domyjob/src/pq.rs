use ml_kem::array::Array;
use ml_kem::{Decapsulate, DecapsulationKey, EncapsulationKey, FromSeed, KeyExport, MlKem768};

use crate::secret::Secret;

#[derive(Debug, Clone, Copy, thiserror::Error)]
pub enum PqError {
    #[error("the system random source failed: {0}")]
    Random(getrandom::Error),
    #[error("the other side sent a malformed post-quantum key")]
    Key,
    #[error("the other side sent a malformed post-quantum ciphertext")]
    Ciphertext,
}

pub trait Entropy {
    fn fill(&mut self, bytes: &mut [u8]) -> Result<(), getrandom::Error>;
}

#[derive(Debug, Clone, Copy, Default)]
pub struct System;

impl Entropy for System {
    fn fill(&mut self, bytes: &mut [u8]) -> Result<(), getrandom::Error> {
        #[cfg(test)]
        if EXHAUSTED.with(std::cell::Cell::get) {
            return Err(getrandom::Error::UNSUPPORTED);
        }
        getrandom::fill(bytes)
    }
}

#[cfg(test)]
std::thread_local! {
    static EXHAUSTED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

#[cfg(test)]
pub fn exhausted<R>(body: impl FnOnce() -> R) -> R {
    EXHAUSTED.with(|flag| flag.set(true));
    let result = body();
    EXHAUSTED.with(|flag| flag.set(false));
    result
}

fn random<const N: usize>(entropy: &mut impl Entropy) -> Result<Secret<[u8; N]>, PqError> {
    let mut bytes = [0u8; N];
    entropy.fill(&mut bytes).map_err(PqError::Random)?;
    Ok(Secret::new(bytes))
}

pub struct Offer {
    decapsulation: DecapsulationKey<MlKem768>,
    public: Vec<u8>,
}

impl std::fmt::Debug for Offer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Offer").finish_non_exhaustive()
    }
}

#[derive(Debug)]
pub struct Agreement {
    secret: Secret<[u8; 32]>,
    transcript: [u8; 32],
}

impl Agreement {
    fn new(shared: &[u8], public: &[u8], ciphertext: &[u8]) -> Self {
        let mut hasher = blake3::Hasher::new_derive_key("domyjob 2026 ml-kem transcript v1");
        hasher.update(public);
        hasher.update(ciphertext);
        let transcript = *hasher.finalize().as_bytes();
        let mut keyed = blake3::Hasher::new_derive_key("domyjob 2026 ml-kem secret v1");
        keyed.update(shared);
        keyed.update(&transcript);
        Self {
            secret: Secret::new(*keyed.finalize().as_bytes()),
            transcript,
        }
    }

    #[must_use]
    pub const fn secret(&self) -> &[u8; 32] {
        self.secret.expose()
    }

    #[must_use]
    pub const fn transcript(&self) -> &[u8; 32] {
        &self.transcript
    }
}

pub fn offer() -> Result<Offer, PqError> {
    offer_with(&mut System)
}

pub fn offer_with(entropy: &mut impl Entropy) -> Result<Offer, PqError> {
    let seed = random::<64>(entropy)?;
    let (decapsulation, encapsulation) = MlKem768::from_seed(&Array::from(*seed.expose()));
    let public = encapsulation.to_bytes().to_vec();
    Ok(Offer {
        decapsulation,
        public,
    })
}

impl Offer {
    #[must_use]
    pub fn public(&self) -> &[u8] {
        &self.public
    }

    pub fn accept(self, ciphertext: &[u8]) -> Result<Agreement, PqError> {
        let shared = self
            .decapsulation
            .decapsulate_slice(ciphertext)
            .map_err(|_malformed| PqError::Ciphertext)?;
        Ok(Agreement::new(&shared, &self.public, ciphertext))
    }
}

pub fn answer(public: &[u8]) -> Result<(Vec<u8>, Agreement), PqError> {
    answer_with(public, &mut System)
}

pub fn answer_with(
    public: &[u8],
    entropy: &mut impl Entropy,
) -> Result<(Vec<u8>, Agreement), PqError> {
    let key = <&Array<u8, _>>::try_from(public).map_err(|_wrong_length| PqError::Key)?;
    let encapsulation = EncapsulationKey::<MlKem768>::new(key).map_err(|_invalid| PqError::Key)?;
    let message = random::<32>(entropy)?;
    let (ciphertext, shared) =
        encapsulation.encapsulate_deterministic(&Array::from(*message.expose()));
    let agreement = Agreement::new(&shared, public, &ciphertext);
    Ok((ciphertext.to_vec(), agreement))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn both_sides_agree_and_tampering_breaks_agreement() {
        let offered = offer().unwrap();
        let public = offered.public().to_vec();
        let (ciphertext, theirs) = answer(&public).unwrap();
        let ours = offered.accept(&ciphertext).unwrap();
        assert_eq!(ours.secret(), theirs.secret());
        assert_eq!(ours.transcript(), theirs.transcript());

        let second = offer().unwrap();
        let (mut forged, honest) = answer(second.public()).unwrap();
        if let Some(byte) = forged.first_mut() {
            *byte ^= 1;
        }
        let tampered = second.accept(&forged).unwrap();
        assert_ne!(tampered.secret(), honest.secret());

        answer(public.get(..10).unwrap()).unwrap_err();
        offer().unwrap().accept(&[0u8; 3]).unwrap_err();
    }

    struct Exhausted;

    impl Entropy for Exhausted {
        fn fill(&mut self, _bytes: &mut [u8]) -> Result<(), getrandom::Error> {
            Err(getrandom::Error::UNSUPPORTED)
        }
    }

    #[test]
    fn every_key_and_every_encapsulation_is_fresh() {
        let first = offer().unwrap();
        let second = offer().unwrap();
        assert_ne!(first.public(), second.public());
        let (once, _) = answer(first.public()).unwrap();
        let (again, _) = answer(first.public()).unwrap();
        assert_ne!(once, again);
    }

    #[test]
    fn a_failing_random_source_is_an_error_never_a_weak_key() {
        assert!(matches!(
            offer_with(&mut Exhausted),
            Err(PqError::Random(_))
        ));
        let public = offer().unwrap().public().to_vec();
        assert!(matches!(
            answer_with(&public, &mut Exhausted),
            Err(PqError::Random(_))
        ));
    }

    #[test]
    fn a_key_of_the_right_length_but_wrong_content_is_refused() {
        let public = offer().unwrap().public().to_vec();
        assert!(matches!(
            answer(&vec![0xff; public.len()]),
            Err(PqError::Key)
        ));
    }

    proptest::proptest! {
        #![proptest_config(proptest::prelude::ProptestConfig::with_cases(24))]
        #[test]
        fn any_flipped_ciphertext_bit_breaks_agreement(at in 0usize..1088, bit in 0u8..8) {
            let offered = offer().unwrap();
            let (mut ciphertext, theirs) = answer(offered.public()).unwrap();
            let byte = ciphertext.get_mut(at).unwrap();
            *byte ^= 1 << bit;
            let ours = offered.accept(&ciphertext).unwrap();
            proptest::prop_assert_ne!(ours.secret(), theirs.secret());
        }
    }
}
