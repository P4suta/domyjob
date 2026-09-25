use std::io::{Read, Write};

#[test]
fn a_socket_answers_until_its_listener_is_gone() {
    let short = if cfg!(windows) {
        std::env::temp_dir()
    } else {
        std::path::PathBuf::from("/tmp")
    };
    let tmp = tempfile::tempdir_in(short).unwrap();
    let path = tmp.path().join("s");
    assert!(matches!(
        domyjob::local_socket::Stream::connect(&path).unwrap(),
        domyjob::local_socket::Reach::NobodyListening
    ));
    let listener = domyjob::local_socket::Listener::bind(&path).unwrap();
    let serving = std::thread::spawn(move || {
        let stream = listener.accept().unwrap();
        let mut text = String::new();
        (&stream).read_to_string(&mut text).unwrap();
        (&stream).write_all(text.to_uppercase().as_bytes()).unwrap();
    });
    let domyjob::local_socket::Reach::Reached(stream) =
        domyjob::local_socket::Stream::connect(&path).unwrap()
    else {
        panic!("the listener did not answer");
    };
    (&stream).write_all(b"hello").unwrap();
    stream.close_sending().unwrap();
    let mut reply = String::new();
    (&stream).read_to_string(&mut reply).unwrap();
    assert_eq!(reply, "HELLO");
    serving.join().unwrap();
    assert!(matches!(
        domyjob::local_socket::Stream::connect(&path).unwrap(),
        domyjob::local_socket::Reach::NobodyListening
    ));
}

#[test]
fn a_path_too_long_for_a_socket_says_so() {
    let long = std::path::PathBuf::from("/tmp").join("x".repeat(domyjob::local_socket::PATH_LIMIT));
    let error = domyjob::local_socket::Listener::bind(&long).unwrap_err();
    assert!(error.to_string().contains("DOMYJOB_STATE"));
}
