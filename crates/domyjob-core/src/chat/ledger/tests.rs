use core::convert::Infallible;

use super::{Admitted, Cursor, Ledger, Parent, Rejection, Resolution, receive};
use crate::chat::event::{Body, Digest, Sealed};
use crate::chat::fixtures::origin;
use crate::chat::id::{EventId, Origin};
use crate::chat::model::Model;

struct Limited {
    model: Model,
    capacity: usize,
}

impl Ledger for Limited {
    type Error = Infallible;

    fn local(&self) -> &Origin {
        self.model.local()
    }

    fn cursor(&self, origin: &Origin) -> Result<Cursor, Infallible> {
        self.model.cursor(origin)
    }

    fn digest(&self, id: &EventId) -> Result<Option<Digest>, Infallible> {
        self.model.digest(id)
    }

    fn parent(&self, id: &EventId) -> Result<Parent, Infallible> {
        self.model.parent(id)
    }

    fn resolution(&self, request: &EventId) -> Result<Option<Resolution>, Infallible> {
        self.model.resolution(request)
    }

    fn clock(&self) -> Result<u64, Infallible> {
        self.model.clock()
    }

    fn has_room(&self) -> Result<bool, Infallible> {
        Ok(self.model.ordered().len() < self.capacity)
    }

    fn commit(&mut self, sealed: &Sealed, admitted: Admitted) -> Result<(), Infallible> {
        self.model.commit(sealed, admitted)
    }
}

#[test]
fn a_storage_limit_preserves_the_admitted_prefix_for_a_later_retry() {
    let mut sender = Model::new(origin('b'));
    let events = [
        sender.write(Body::Omitted {}).unwrap(),
        sender.write(Body::Omitted {}).unwrap(),
    ];
    let mut receiver = Limited {
        model: Model::new(origin('a')),
        capacity: 1,
    };
    let Ok(first) = receive(&mut receiver, &origin('b'), &events);
    assert_eq!(first.appended, 1);
    assert_eq!(first.seen, 1);
    assert_eq!(first.rejected, Some(Rejection::ResourceLimit));
    assert_eq!(
        receiver.model.ordered(),
        events.first().into_iter().collect::<alloc::vec::Vec<_>>()
    );
    receiver.capacity = 2;
    let Ok(retry) = receive(&mut receiver, &origin('b'), &events);
    assert_eq!(retry.appended, 1);
    assert_eq!(retry.seen, 2);
    assert_eq!(retry.rejected, None);
    assert_eq!(
        receiver.model.ordered(),
        events.iter().collect::<alloc::vec::Vec<_>>()
    );
}
