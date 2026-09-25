# Symbolic proofs of the paired connection

The models in this directory describe domyjob's paired connection: a fresh ML-KEM-768 exchange whose shared secret keys a Noise IKpsk2 handshake, with the exchange's transcript bound into the prologue.
Each model gives the attacker the network: it reads, drops, replays, and forges every message, the ML-KEM messages included.
Run them with ProVerif 2.05 or later: `proverif connect-x25519-broken.pv` and `proverif connect-ml-kem-broken.pv`.

| Model | What the attacker also gets | What is proved |
| --- | --- | --- |
| `connect-x25519-broken.pv` | After the session ends, every X25519 secret: both long-term keys and the ephemeral one | What the client sent stays secret: a recording made today cannot be read by someone who breaks X25519 later |
| `connect-ml-kem-broken.pv` | The client's ML-KEM secret key, from the start | What the client sent stays secret, and a server that accepts a session accepts only one its client started |

Both results hold as ProVerif reports them: `not attacker_p1(payload[]) is true`, `not attacker(payload[]) is true`, and `event(ServerAccepted(..)) ==> event(ClientSent(..)) is true`.

## What the models simplify

- X25519 is an ideal Diffie-Hellman group and ML-KEM an ideal KEM; neither model says anything about the security of the primitives themselves.
- Noise's chaining of keys is collapsed into one key derivation per direction that takes every Diffie-Hellman output, the ML-KEM secret, and the ML-KEM transcript.
  The per-direction keys and the server's authenticated reply are kept, because the proofs depend on them: without the reply the client would send to an attacker who substituted the ML-KEM ciphertext, and with one key for both directions the attacker could reflect the server's reply back to it.
- Authentication rests on X25519 alone.
  An attacker who could break X25519 during a session could impersonate a server; the hybrid protects recordings, which is the threat a future quantum computer poses to today's traffic.

## What is not modelled yet

- Pairing: SPAKE2 over the four-word code, the ML-KEM exchange, Noise XXpsk3, and the confirmation words.
- The framing layer, which the tests and the fuzz targets cover instead.
