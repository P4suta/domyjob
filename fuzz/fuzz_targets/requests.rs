#![no_main]

fn normalized(peer: domyjob::ingress::PeerRequest) -> Vec<u8> {
    let authorized = domyjob::authz::authorize(domyjob::authz::Principal::Owner, peer).unwrap();
    match authorized.route() {
        domyjob::authz::Routed::Query(query) => serde_json::to_vec(query.request()).unwrap(),
        domyjob::authz::Routed::Command(command) => serde_json::to_vec(command.request()).unwrap(),
    }
}

libfuzzer_sys::fuzz_target!(|bytes: &[u8]| {
    if let Ok(peer) = domyjob::ingress::json::<domyjob::ingress::PeerRequest>(bytes) {
        let again = normalized(peer);
        let back: domyjob::ingress::PeerRequest = domyjob::ingress::json(&again).unwrap();
        assert_eq!(normalized(back), again);
    }
    match domyjob::ingress::json::<domyjob::protocol::Reply>(bytes) {
        Ok(_) | Err(_) => {}
    }
});
