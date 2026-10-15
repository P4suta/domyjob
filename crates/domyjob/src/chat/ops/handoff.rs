use domyjob_core::chat::event::{Body, Members};
use domyjob_core::chat::handoff::{Descriptor, Metadata};
use domyjob_core::chat::policy::Priority;

use super::{OpsError, Session};
use crate::chat::args::{HandoffCreateArgs, HandoffListArgs, HandoffReadArgs};
use crate::chat::store::handoff::{self, MAX_HANDOFFS, PendingObservation};
use crate::chat::view::Outcome;
use crate::platform::clock;

pub(crate) fn create_handoff(
    session: &Session,
    args: &HandoffCreateArgs,
) -> Result<Outcome, OpsError> {
    let metadata = Metadata::new(
        session.me()?.clone(),
        args.intended_recipient.0.clone(),
        (args.project.clone(), args.scope.clone()),
        Members::try_from(args.permitted_readers.clone())?,
    )?;
    let event = session.store.write(|tx| {
        tx.author(
            Priority::Ordinary,
            Body::Handoff {
                metadata: metadata.clone(),
                text: args.text.clone(),
                at: clock::stamp(),
            },
        )
    })?;
    Ok(Outcome::HandoffCreated(Descriptor::new(
        event.id().clone(),
        metadata,
        &args.text,
    )?))
}

pub(crate) fn handoffs(session: &Session, args: &HandoffListArgs) -> Result<Outcome, OpsError> {
    let reader = session.me()?.clone();
    let limit = args.limit.unwrap_or(MAX_HANDOFFS);
    if limit == 0 || limit > MAX_HANDOFFS {
        return Err(OpsError::Usage("handoff limit must be between 1 and 32"));
    }
    let descriptors = session.store.read(|transaction| {
        handoff::discover(transaction, &reader, (&args.project, &args.scope), limit)
    })?;
    Ok(Outcome::HandoffMetadata {
        reader,
        descriptors,
    })
}

pub(crate) fn read_handoff(session: &Session, args: &HandoffReadArgs) -> Result<Outcome, OpsError> {
    let reader = session.me()?;
    Ok(Outcome::HandoffBody(session.store.read(|transaction| {
        handoff::read(transaction, reader, &args.descriptor.0, args.max_bytes)
    })?))
}

pub(crate) fn observe_handoff(
    session: &Session,
    pending: &PendingObservation,
) -> Result<(), OpsError> {
    if session.me()? != pending.reader() {
        return Err(OpsError::Usage(
            "the delivered handoff belongs to another acting identity",
        ));
    }
    session.store.write(|tx| pending.commit(tx))?;
    Ok(())
}

#[cfg(test)]
pub(crate) mod tests;
