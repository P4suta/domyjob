use domyjob::lock::OsLock;

#[test]
fn locks_exclude_each_other_and_free_on_release() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("slots");
    let (first, held) = OsLock::first_free(&dir).unwrap().unwrap();
    let (second, other) = OsLock::first_free(&dir).unwrap().unwrap();
    assert_eq!(first.to_string(), "0");
    assert_eq!(second.to_string(), "1");
    let (third, next) = OsLock::first_free(&dir).unwrap().unwrap();
    assert_eq!(third.to_string(), "2");
    held.release().unwrap();
    let (reused, released) = OsLock::first_free(&dir).unwrap().unwrap();
    assert_eq!(reused, first);
    released.release().unwrap();
    drop(next);
    drop(other);
    assert!(
        OsLock::try_exclusive(&dir.join("1.lock"))
            .unwrap()
            .is_some()
    );
}

#[test]
fn a_blocked_waiter_wakes_exactly_when_the_holder_lets_go() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("live").join("alive.lock");
    let holder = OsLock::exclusive(&path).unwrap();
    let (sender, receiver) = std::sync::mpsc::channel();
    let waiting_path = path;
    let waiter = std::thread::spawn(move || {
        let lock = OsLock::exclusive(&waiting_path).unwrap();
        sender.send(()).unwrap();
        lock.release().unwrap();
    });
    assert!(receiver.try_recv().is_err());
    holder.release().unwrap();
    receiver.recv().unwrap();
    waiter.join().unwrap();
}
