use super::{Model, Worker};
use crate::chat::store::observer::Snapshot;
use crate::platform::clock::{self, Waited};

#[test]
fn a_single_pending_request_prevents_overlapping_refreshes() {
    let (started, entered) = std::sync::mpsc::sync_channel(1);
    let (release, wait) = std::sync::mpsc::sync_channel(1);
    let worker = Worker::with_observer(move |_| {
        started.send(()).unwrap();
        wait.recv().unwrap();
        Ok(Snapshot::default())
    })
    .unwrap();
    let mut model = Model::default();
    worker.refresh(&mut model);
    assert!(matches!(
        clock::receive(&entered, clock::Deadline::after_seconds(5)),
        Waited::Received(())
    ));
    let generation = model.refresh.pending();
    worker.refresh(&mut model);
    assert_eq!(model.refresh.pending(), generation);
    release.send(()).unwrap();
    let completed = match clock::receive(&worker.completed, clock::Deadline::after_seconds(5)) {
        Waited::Received(completed) => completed,
        Waited::Expired | Waited::Closed => panic!("observer failed to return its bounded request"),
    };
    model.accept(
        completed.work.generation,
        &completed.work.request,
        completed.result,
    );
    assert!(model.refresh.pending().is_none());
}

#[test]
fn observer_errors_are_kept_and_do_not_replace_the_last_snapshot() {
    let worker = Worker::with_observer(|_| Err("injected read failure".to_owned())).unwrap();
    let mut model = Model::default();
    model.snapshot.observed = Some(7);
    worker.refresh(&mut model);
    let completed = match clock::receive(&worker.completed, clock::Deadline::after_seconds(5)) {
        Waited::Received(completed) => completed,
        Waited::Expired | Waited::Closed => panic!("observer failed to report its injected error"),
    };
    model.accept(
        completed.work.generation,
        &completed.work.request,
        completed.result,
    );
    assert_eq!(model.error.as_deref(), Some("injected read failure"));
    assert_eq!(model.snapshot.observed, Some(7));
}
