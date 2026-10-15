use super::MAX_SEQUENCE_BYTES;

#[derive(Debug, Clone, Copy, Default)]
pub(super) struct Sequence {
    bytes: [u8; MAX_SEQUENCE_BYTES],
    used: usize,
}

impl Sequence {
    pub(super) const fn clear(&mut self) {
        self.used = 0;
    }

    pub(super) const fn len(&self) -> usize {
        self.used
    }

    pub(super) fn content(&self) -> Option<&[u8]> {
        self.bytes.get(..self.used)
    }

    pub(super) fn push(&mut self, byte: u8) -> bool {
        let Some(slot) = self.bytes.get_mut(self.used) else {
            return false;
        };
        *slot = byte;
        self.used = self.used.saturating_add(1);
        true
    }
}

#[cfg(kani)]
mod proofs {
    use super::*;

    #[kani::proof]
    #[kani::unwind(34)]
    fn every_sequence_append_preserves_its_buffer_invariant() {
        assert_eq!(Sequence::default().len(), 0);
        let mut buffer = Sequence {
            bytes: kani::any(),
            used: kani::any(),
        };
        kani::assume(buffer.used <= MAX_SEQUENCE_BYTES);
        let before = buffer;
        let byte: u8 = kani::any();
        let appended = buffer.push(byte);
        assert_eq!(appended, before.used < MAX_SEQUENCE_BYTES);
        assert!(buffer.len() <= MAX_SEQUENCE_BYTES);
        assert_eq!(buffer.content().unwrap().len(), buffer.len());
        if appended {
            assert_eq!(buffer.len(), before.len() + 1);
            assert_eq!(buffer.bytes.get(before.used), Some(&byte));
        } else {
            assert_eq!(buffer.bytes, before.bytes);
            assert_eq!(buffer.len(), before.len());
        }
        let position: usize = kani::any();
        kani::assume(position < MAX_SEQUENCE_BYTES);
        if position != before.used {
            assert_eq!(buffer.bytes.get(position), before.bytes.get(position));
        }
        buffer.clear();
        assert_eq!(buffer.len(), 0);
        assert!(buffer.content().unwrap().is_empty());
        kani::cover!(before.used == 0 && appended);
        kani::cover!(before.used == MAX_SEQUENCE_BYTES - 1 && appended);
        kani::cover!(before.used == MAX_SEQUENCE_BYTES && !appended);
    }
}
