use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::Arc;

use crate::domain::Invalid;
use crate::trust::{Identity, PublicKey};

pub trait Duplex: Read + Write + Send + Sized + 'static {
    fn split(&self) -> std::io::Result<Self>;
    fn close_sending(&self) -> std::io::Result<()>;
}

impl Duplex for TcpStream {
    fn split(&self) -> std::io::Result<Self> {
        self.try_clone()
    }

    fn close_sending(&self) -> std::io::Result<()> {
        self.shutdown(std::net::Shutdown::Write)
    }
}

const MAX_MESSAGE: usize = 65_535;
const TAG: usize = 16;
const MAX_PAYLOAD: usize = MAX_MESSAGE - TAG - 1;
const DATA: u8 = 0;
const CLOSE: u8 = 1;

#[derive(Debug, thiserror::Error)]
pub enum SecureError {
    #[error("the connection failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("the secure handshake failed")]
    Handshake,
    #[error("the other side is not paired with this machine")]
    Unknown,
    #[error("a frame of {0} bytes is not allowed")]
    Frame(usize),
    #[error(transparent)]
    Invalid(#[from] Invalid),
    #[error(transparent)]
    PostQuantum(#[from] crate::pq::PqError),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Purpose {
    Pair,
    Connect,
}

impl Purpose {
    #[must_use]
    pub const fn route(self) -> u8 {
        match self {
            Self::Pair => b'P',
            Self::Connect => b'C',
        }
    }

    #[must_use]
    pub const fn from_route(byte: u8) -> Option<Self> {
        match byte {
            b'P' => Some(Self::Pair),
            b'C' => Some(Self::Connect),
            _ => None,
        }
    }

    const fn pattern(self) -> &'static str {
        match self {
            Self::Pair => "Noise_XXpsk3_25519_ChaChaPoly_BLAKE2s",
            Self::Connect => "Noise_IKpsk2_25519_ChaChaPoly_BLAKE2s",
        }
    }

    const fn label(self) -> &'static [u8] {
        match self {
            Self::Pair => b"domyjob/3 pair",
            Self::Connect => b"domyjob/3 connect",
        }
    }

    fn prologue(self, agreement: &crate::pq::Agreement) -> Vec<u8> {
        let mut prologue = self.label().to_vec();
        prologue.extend_from_slice(agreement.transcript());
        prologue
    }
}

fn send_frame(stream: &mut impl Write, bytes: &[u8]) -> Result<(), SecureError> {
    let len = u16::try_from(bytes.len()).map_err(|_too_long| SecureError::Frame(bytes.len()))?;
    stream.write_all(&len.to_be_bytes())?;
    stream.write_all(bytes)?;
    stream.flush()?;
    Ok(())
}

fn receive_frame(stream: &mut impl Read) -> Result<Vec<u8>, SecureError> {
    let mut len = [0u8; 2];
    stream.read_exact(&mut len)?;
    let mut frame = vec![0u8; usize::from(u16::from_be_bytes(len))];
    stream.read_exact(&mut frame)?;
    Ok(frame)
}

fn start_route(stream: &mut impl Write, purpose: Purpose) -> Result<(), SecureError> {
    stream.write_all(&[purpose.route()])?;
    Ok(())
}

pub fn read_route(stream: &mut impl Read) -> Result<Option<Purpose>, SecureError> {
    let mut byte = [0u8; 1];
    stream.read_exact(&mut byte)?;
    Ok(byte.first().copied().and_then(Purpose::from_route))
}

fn builder<'a>(
    purpose: Purpose,
    identity: &'a Identity,
    prologue: &'a [u8],
) -> Result<snow::Builder<'a>, SecureError> {
    let params = purpose
        .pattern()
        .parse()
        .map_err(|_unsupported| SecureError::Handshake)?;
    snow::Builder::new(params)
        .local_private_key(identity.secret())
        .and_then(|b| b.prologue(prologue))
        .map_err(|_rejected| SecureError::Handshake)
}

fn agree(
    stream: &mut (impl Read + Write),
    side: Side,
) -> Result<crate::pq::Agreement, SecureError> {
    match side {
        Side::Initiator => {
            let offer = crate::pq::offer()?;
            send_frame(stream, offer.public())?;
            let ciphertext = receive_frame(stream)?;
            Ok(offer.accept(&ciphertext)?)
        }
        Side::Responder => {
            let public = receive_frame(stream)?;
            let (ciphertext, agreement) = crate::pq::answer(&public)?;
            send_frame(stream, &ciphertext)?;
            Ok(agreement)
        }
    }
}

fn write_message(
    state: &mut snow::HandshakeState,
    stream: &mut impl Write,
) -> Result<(), SecureError> {
    let mut buffer = vec![0u8; MAX_MESSAGE];
    let len = state
        .write_message(&[], &mut buffer)
        .map_err(|_failed| SecureError::Handshake)?;
    send_frame(stream, buffer.get(..len).unwrap_or(&[]))
}

fn read_message(
    state: &mut snow::HandshakeState,
    stream: &mut impl Read,
) -> Result<(), SecureError> {
    let frame = receive_frame(stream)?;
    let mut payload = vec![0u8; MAX_MESSAGE];
    state
        .read_message(&frame, &mut payload)
        .map_err(|_failed| SecureError::Handshake)?;
    Ok(())
}

fn finish<S: Duplex>(state: snow::HandshakeState, stream: S) -> Result<Channel<S>, SecureError> {
    let remote = PublicKey::from_slice(state.get_remote_static().ok_or(SecureError::Handshake)?)?;
    let transcript = state.get_handshake_hash().to_vec();
    let transport = Arc::new(
        state
            .into_stateless_transport_mode()
            .map_err(|_failed| SecureError::Handshake)?,
    );
    let read_half = stream.split()?;
    Ok(Channel {
        remote,
        transcript,
        reader: Reader {
            stream: read_half,
            transport: Arc::clone(&transport),
            nonce: 0,
            state: ReadState::Open(Vec::new(), 0),
        },
        writer: Writer {
            stream,
            transport,
            nonce: 0,
            closed: false,
        },
    })
}

pub fn connect<S: Duplex>(
    mut stream: S,
    identity: &Identity,
    server: &PublicKey,
) -> Result<Channel<S>, SecureError> {
    start_route(&mut stream, Purpose::Connect)?;
    let agreement = agree(&mut stream, Side::Initiator)?;
    let prologue = Purpose::Connect.prologue(&agreement);
    let mut state = builder(Purpose::Connect, identity, &prologue)?
        .psk(2, agreement.secret())
        .and_then(|b| b.remote_public_key(server.as_bytes()))
        .and_then(snow::Builder::build_initiator)
        .map_err(|_rejected| SecureError::Handshake)?;
    write_message(&mut state, &mut stream)?;
    read_message(&mut state, &mut stream)?;
    finish(state, stream)
}

pub fn accept<S: Duplex, P>(
    mut stream: S,
    identity: &Identity,
    admit: impl FnOnce(&PublicKey) -> Option<P>,
) -> Result<(Channel<S>, P), SecureError> {
    let agreement = agree(&mut stream, Side::Responder)?;
    let prologue = Purpose::Connect.prologue(&agreement);
    let mut state = builder(Purpose::Connect, identity, &prologue)?
        .psk(2, agreement.secret())
        .and_then(snow::Builder::build_responder)
        .map_err(|_rejected| SecureError::Handshake)?;
    read_message(&mut state, &mut stream)?;
    let claimed = PublicKey::from_slice(state.get_remote_static().ok_or(SecureError::Handshake)?)?;
    let principal = admit(&claimed).ok_or(SecureError::Unknown)?;
    write_message(&mut state, &mut stream)?;
    Ok((finish(state, stream)?, principal))
}

#[derive(Debug)]
pub struct PairingCode(String);

impl PairingCode {
    pub fn generate() -> Result<Self, Invalid> {
        let mut random = [0u8; 4];
        crate::pq::Entropy::fill(&mut crate::pq::System, &mut random).map_err(Invalid::Random)?;
        let words: Vec<&str> = random.iter().map(|b| crate::words::word(*b)).collect();
        Ok(Self(words.join("-")))
    }

    pub fn parse(text: &str) -> Result<Self, Invalid> {
        let normalized = text.trim().to_ascii_lowercase();
        let parts: Vec<&str> = normalized.split('-').collect();
        if parts.len() == 4 && parts.iter().all(|p| crate::words::WORDS.contains(p)) {
            Ok(Self(normalized))
        } else {
            Err(Invalid::PairingCode(text.to_owned()))
        }
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sas(String);

impl Sas {
    fn from_transcript(transcript: &[u8]) -> Self {
        let digest = blake3::derive_key("domyjob 2026 pairing confirmation v1", transcript);
        let words: Vec<&str> = digest
            .iter()
            .take(5)
            .map(|b| crate::words::word(*b))
            .collect();
        Self(words.join(" "))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug)]
pub struct Pending<S: Duplex = TcpStream> {
    pub channel: Channel<S>,
    pub sas: Sas,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    Initiator,
    Responder,
}

fn pake_psk(stream: &mut (impl Read + Write), code: &PairingCode) -> Result<[u8; 32], SecureError> {
    use spake2::{Ed25519Group, Identity as SpakeId, Password, Spake2};
    let (spake, outbound) = Spake2::<Ed25519Group>::start_symmetric(
        &Password::new(code.as_str().as_bytes()),
        &SpakeId::new(b"domyjob/3 pairing"),
    );
    send_frame(stream, &outbound)?;
    let inbound = receive_frame(stream)?;
    let shared = spake
        .finish(&inbound)
        .map_err(|_bad_message| SecureError::Handshake)?;
    Ok(blake3::derive_key("domyjob 2026 pairing spake v2", &shared))
}

#[derive(Debug, Clone, Copy)]
pub struct Attempt<'a> {
    pub code: &'a PairingCode,
    pub side: Side,
}

pub fn pair<S: Duplex>(
    mut stream: S,
    identity: &Identity,
    attempt: Attempt<'_>,
) -> Result<Pending<S>, SecureError> {
    let Attempt { code, side } = attempt;
    if side == Side::Initiator {
        start_route(&mut stream, Purpose::Pair)?;
    }
    let spake = crate::secret::Secret::new(pake_psk(&mut stream, code)?);
    let agreement = agree(&mut stream, side)?;
    let mut mixed = blake3::Hasher::new_derive_key("domyjob 2026 pairing psk v2");
    mixed.update(spake.expose());
    mixed.update(agreement.secret());
    let psk = crate::secret::Secret::new(*mixed.finalize().as_bytes());
    let prologue = Purpose::Pair.prologue(&agreement);
    let base = builder(Purpose::Pair, identity, &prologue)?
        .psk(3, psk.expose())
        .map_err(|_rejected| SecureError::Handshake)?;
    let mut state = match side {
        Side::Initiator => base.build_initiator(),
        Side::Responder => base.build_responder(),
    }
    .map_err(|_rejected| SecureError::Handshake)?;
    let mut ours = side == Side::Initiator;
    while !state.is_handshake_finished() {
        if ours {
            write_message(&mut state, &mut stream)?;
        } else {
            read_message(&mut state, &mut stream)?;
        }
        ours = !ours;
    }
    let mut channel = finish(state, stream)?;
    confirm_keys(&mut channel, side)?;
    let sas = Sas::from_transcript(&channel.transcript);
    Ok(Pending { channel, sas })
}

fn confirm_keys<S: Duplex>(channel: &mut Channel<S>, side: Side) -> Result<(), SecureError> {
    const MARK: &[u8; 16] = b"domyjob/2 ready!";
    let mut echo = [0u8; 16];
    match side {
        Side::Initiator => {
            channel.writer.write_all(MARK)?;
            channel.reader.read_exact(&mut echo)?;
        }
        Side::Responder => {
            channel.reader.read_exact(&mut echo)?;
            channel.writer.write_all(MARK)?;
        }
    }
    if &echo == MARK {
        Ok(())
    } else {
        Err(SecureError::Handshake)
    }
}

pub struct Channel<S: Duplex = TcpStream> {
    remote: PublicKey,
    transcript: Vec<u8>,
    pub reader: Reader<S>,
    pub writer: Writer<S>,
}

impl<S: Duplex> std::fmt::Debug for Channel<S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Channel")
            .field("remote", &self.remote)
            .finish_non_exhaustive()
    }
}

impl<S: Duplex> Channel<S> {
    #[must_use]
    pub const fn remote(&self) -> &PublicKey {
        &self.remote
    }

    #[must_use]
    pub fn into_halves(self) -> Halves<S> {
        Halves {
            reader: self.reader,
            writer: self.writer,
        }
    }
}

#[derive(Debug)]
pub struct Halves<S: Duplex = TcpStream> {
    pub reader: Reader<S>,
    pub writer: Writer<S>,
}

enum ReadState {
    Open(Vec<u8>, usize),
    Closed,
}

pub struct Reader<S: Duplex = TcpStream> {
    stream: S,
    transport: Arc<snow::StatelessTransportState>,
    nonce: u64,
    state: ReadState,
}

pub struct Writer<S: Duplex = TcpStream> {
    stream: S,
    transport: Arc<snow::StatelessTransportState>,
    nonce: u64,
    closed: bool,
}

impl<S: Duplex> std::fmt::Debug for Reader<S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Reader")
            .field("nonce", &self.nonce)
            .finish_non_exhaustive()
    }
}

impl<S: Duplex> std::fmt::Debug for Writer<S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Writer")
            .field("nonce", &self.nonce)
            .finish_non_exhaustive()
    }
}

fn other(message: &'static str) -> std::io::Error {
    std::io::Error::other(message)
}

fn next_nonce(nonce: &mut u64) -> std::io::Result<u64> {
    let current = *nonce;
    *nonce = nonce
        .checked_add(1)
        .ok_or_else(|| other("the session ran out of nonces"))?;
    Ok(current)
}

impl<S: Duplex> Reader<S> {
    fn fill(&mut self) -> std::io::Result<Option<Vec<u8>>> {
        let frame = match receive_frame(&mut self.stream) {
            Ok(frame) => frame,
            Err(SecureError::Io(error))
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::UnexpectedEof
                        | std::io::ErrorKind::ConnectionReset
                        | std::io::ErrorKind::ConnectionAborted
                ) =>
            {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "the connection was cut before it was closed",
                ));
            }
            Err(error) => return Err(std::io::Error::other(error.to_string())),
        };
        let mut plain = vec![0u8; frame.len()];
        let nonce = next_nonce(&mut self.nonce)?;
        let len = self
            .transport
            .read_message(nonce, &frame, &mut plain)
            .map_err(|_forged| other("a frame failed authentication"))?;
        plain.truncate(len);
        match plain.split_first() {
            Some((&DATA, body)) => Ok(Some(body.to_vec())),
            Some((&CLOSE, [])) => Ok(None),
            Some(_) | None => Err(other("a frame had an unknown kind")),
        }
    }
}

impl<S: Duplex> Read for Reader<S> {
    fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
        loop {
            match &mut self.state {
                ReadState::Closed => return Ok(0),
                ReadState::Open(buffer, offset) if *offset < buffer.len() => {
                    let available = buffer.get(*offset..).unwrap_or(&[]);
                    let n = available.len().min(out.len());
                    if let (Some(target), Some(source)) = (out.get_mut(..n), available.get(..n)) {
                        target.copy_from_slice(source);
                    }
                    *offset = offset.saturating_add(n);
                    return Ok(n);
                }
                ReadState::Open(..) => {
                    self.state = match self.fill()? {
                        Some(buffer) => ReadState::Open(buffer, 0),
                        None => ReadState::Closed,
                    };
                }
            }
        }
    }
}

impl<S: Duplex> Writer<S> {
    fn seal(&mut self, kind: u8, chunk: &[u8]) -> std::io::Result<()> {
        if self.closed {
            return Err(other("the channel is already closed"));
        }
        let mut plain = Vec::with_capacity(chunk.len().saturating_add(1));
        plain.push(kind);
        plain.extend_from_slice(chunk);
        let mut sealed = vec![0u8; plain.len().saturating_add(TAG)];
        let nonce = next_nonce(&mut self.nonce)?;
        let len = self
            .transport
            .write_message(nonce, &plain, &mut sealed)
            .map_err(|_failed| other("encryption failed"))?;
        send_frame(&mut self.stream, sealed.get(..len).unwrap_or(&[]))
            .map_err(|e| std::io::Error::other(e.to_string()))
    }

    pub fn close(&mut self) -> std::io::Result<()> {
        self.seal(CLOSE, &[])?;
        self.closed = true;
        self.stream.close_sending()
    }
}

impl<S: Duplex> Write for Writer<S> {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        for chunk in bytes.chunks(MAX_PAYLOAD) {
            self.seal(DATA, chunk)?;
        }
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.stream.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::paths::Dirs;

    fn identity(root: &std::path::Path, name: &str) -> Identity {
        let dirs = Dirs {
            home: root.into(),
            state: root.join(name),
            config: root.join("c"),
            cache: root.join("k"),
            keys: crate::keystore::KeyStore::OwnerOnlyFile,
        };
        Identity::load_or_create(&dirs).unwrap()
    }

    fn parties() -> (tempfile::TempDir, std::path::PathBuf, Identity, PublicKey) {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_path_buf();
        let client = identity(&root, "a");
        let server_key = *identity(&root, "b").public();
        (tmp, root, client, server_key)
    }

    fn serve_one<T: Send + 'static>(
        root: std::path::PathBuf,
        work: impl FnOnce(TcpStream, &Identity) -> T + Send + 'static,
    ) -> (std::net::SocketAddr, std::thread::JoinHandle<T>) {
        let (listener, address) = bound();
        let serving = std::thread::spawn(move || {
            let server = identity(&root, "b");
            let (stream, _) = listener.accept().unwrap();
            work(stream, &server)
        });
        (address, serving)
    }

    fn accepted(mut stream: TcpStream, server: &Identity) -> Channel<TcpStream> {
        read_route(&mut stream).unwrap();
        accept(stream, server, |_| Some(())).unwrap().0
    }

    fn accept_known(mut stream: TcpStream, server: &Identity) -> Result<(), SecureError> {
        read_route(&mut stream)?;
        accept(stream, server, |_| Some(())).map(drop)
    }

    fn read_all(stream: TcpStream, server: &Identity) -> std::io::Result<Vec<u8>> {
        let mut channel = accepted(stream, server);
        let mut received = Vec::new();
        channel.reader.read_to_end(&mut received).map(|_| received)
    }

    fn bound() -> (std::net::TcpListener, std::net::SocketAddr) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        (listener, address)
    }

    #[test]
    fn connections_authenticate_both_ends_and_detect_truncation() {
        let (_tmp, root, client, server_key) = parties();
        let client_key = *client.public();
        let (address, serving) = serve_one(root.clone(), move |mut stream, server| {
            assert_eq!(read_route(&mut stream).unwrap(), Some(Purpose::Connect));
            let (mut accepted, who) = accept(stream, server, |key| {
                (key == &client_key).then_some("known")
            })
            .unwrap();
            assert_eq!(who, "known");
            let mut received = Vec::new();
            accepted.reader.read_to_end(&mut received).unwrap();
            received.len()
        });
        let mut channel =
            connect(TcpStream::connect(address).unwrap(), &client, &server_key).unwrap();
        assert_eq!(channel.remote(), &server_key);
        channel.writer.write_all(&vec![7u8; 200_000]).unwrap();
        channel.writer.close().unwrap();
        assert_eq!(serving.join().unwrap(), 200_000);

        let (cut_listener, cut_address) = bound();
        let cutter = std::thread::spawn(move || {
            let server = identity(&root, "b");
            let (mut stream, _) = cut_listener.accept().unwrap();
            read_route(&mut stream).unwrap();
            let (mut cut_channel, ()) = accept(stream, &server, |_| Some(())).unwrap();
            cut_channel.writer.write_all(b"partial").unwrap();
            drop(cut_channel);
        });
        let mut victim = connect(
            TcpStream::connect(cut_address).unwrap(),
            &client,
            &server_key,
        )
        .unwrap();
        cutter.join().unwrap();
        let mut received = Vec::new();
        let error = victim.reader.read_to_end(&mut received).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::UnexpectedEof);
    }

    struct Closer([TcpStream; 2]);

    impl Drop for Closer {
        fn drop(&mut self) {
            for side in &self.0 {
                match side.shutdown(std::net::Shutdown::Both) {
                    Ok(()) | Err(_) => {}
                }
            }
        }
    }

    fn tamper(mut from_client: TcpStream, mut to_server: TcpStream) {
        let mut length = [0u8; 2];
        to_server.read_exact(&mut length).unwrap();
        let mut ciphertext = vec![0u8; usize::from(u16::from_be_bytes(length))];
        to_server.read_exact(&mut ciphertext).unwrap();
        *ciphertext.first_mut().unwrap() ^= 1;
        from_client.write_all(&length).unwrap();
        from_client.write_all(&ciphertext).unwrap();
        match std::io::copy(&mut to_server, &mut from_client) {
            Ok(_) | Err(_) => {}
        }
    }

    #[test]
    fn tampering_with_the_post_quantum_exchange_breaks_the_handshake() {
        let (_tmp, root, client, server_key) = parties();
        let client_key = *client.public();
        let (address, serving) = serve_one(root, move |mut stream, server| {
            read_route(&mut stream).unwrap();
            accept(stream, server, |key| (key == &client_key).then_some(())).map(|_| ())
        });
        let (relay, relay_address) = bound();
        let relaying = std::thread::spawn(move || {
            let (from_client, _) = relay.accept().unwrap();
            let to_server = TcpStream::connect(address).unwrap();
            let closer = Closer([
                from_client.try_clone().unwrap(),
                to_server.try_clone().unwrap(),
            ]);
            let mut upstream = from_client.try_clone().unwrap();
            let mut server_side = to_server.try_clone().unwrap();
            let forward =
                std::thread::spawn(
                    move || match std::io::copy(&mut upstream, &mut server_side) {
                        Ok(_) | Err(_) => {}
                    },
                );
            tamper(from_client, to_server);
            drop(closer);
            forward.join().unwrap();
        });
        let attempt = connect(
            TcpStream::connect(relay_address).unwrap(),
            &client,
            &server_key,
        );
        attempt.unwrap_err();
        assert!(matches!(
            serving.join().unwrap(),
            Err(SecureError::Handshake)
        ));
        relaying.join().unwrap();
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Toward {
        Client,
        Server,
    }

    struct Pump {
        from: TcpStream,
        to: TcpStream,
        limit: Option<usize>,
        closer: Closer,
    }

    fn pump(pumped: Pump) -> Vec<u8> {
        let Pump {
            mut from,
            mut to,
            limit,
            closer,
        } = pumped;
        let mut seen = Vec::new();
        let mut buffer = [0u8; 4096];
        loop {
            let room = limit.map_or(buffer.len(), |l| {
                l.saturating_sub(seen.len()).min(buffer.len())
            });
            if room == 0 {
                break;
            }
            let Ok(read) = from.read(buffer.get_mut(..room).unwrap()) else {
                break;
            };
            let chunk = buffer.get(..read).unwrap();
            if chunk.is_empty() {
                break;
            }
            seen.extend_from_slice(chunk);
            if to.write_all(chunk).is_err() {
                break;
            }
        }
        drop(closer);
        seen
    }

    type Relaying = std::thread::JoinHandle<(Vec<u8>, Vec<u8>)>;

    fn relay(
        target: std::net::SocketAddr,
        cut: Option<(Toward, usize)>,
    ) -> (std::net::SocketAddr, Relaying) {
        let (listener, address) = bound();
        let handle = std::thread::spawn(move || {
            let (client, _) = listener.accept().unwrap();
            let server = TcpStream::connect(target).unwrap();
            let limit = |toward| match cut {
                Some((side, at)) if side == toward => Some(at),
                Some(_) | None => None,
            };
            let closer = || Closer([client.try_clone().unwrap(), server.try_clone().unwrap()]);
            let up = Pump {
                from: client.try_clone().unwrap(),
                to: server.try_clone().unwrap(),
                limit: limit(Toward::Server),
                closer: closer(),
            };
            let down = Pump {
                from: server.try_clone().unwrap(),
                to: client.try_clone().unwrap(),
                limit: limit(Toward::Client),
                closer: closer(),
            };
            let upward = std::thread::spawn(move || pump(up));
            let downward = pump(down);
            (upward.join().unwrap(), downward)
        });
        (address, handle)
    }

    fn frames(mut bytes: &[u8]) -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        while let [high, low, rest @ ..] = bytes {
            let len = usize::from(u16::from_be_bytes([*high, *low]));
            let Some(frame) = rest.get(..len) else {
                break;
            };
            out.push(frame.to_vec());
            bytes = rest.get(len..).unwrap_or(&[]);
        }
        out
    }

    #[test]
    fn every_frame_on_the_wire_is_sealed_under_a_fresh_nonce() {
        let (_tmp, root, client, server_key) = parties();
        let (address, serving) = serve_one(root, read_all);
        let (via, relaying) = relay(address, None);
        let mut channel = connect(TcpStream::connect(via).unwrap(), &client, &server_key).unwrap();
        channel.writer.write_all(b"same words").unwrap();
        channel.writer.write_all(b"same words").unwrap();
        channel.writer.close().unwrap();
        channel.writer.write_all(b"after close").unwrap_err();
        assert_eq!(serving.join().unwrap().unwrap(), b"same wordssame words");
        let (upward, _) = relaying.join().unwrap();
        let sent = frames(upward.get(1..).unwrap());
        let [_, _, first, second, _] = sent.as_slice() else {
            panic!("expected two handshake frames, two data frames, and a close");
        };
        assert_ne!(first, second);
    }

    #[test]
    fn a_handshake_cut_anywhere_fails_cleanly_on_both_sides() {
        let (_tmp, root, client, server_key) = parties();
        let cuts = [
            0usize, 1, 2, 3, 700, 1087, 1088, 1089, 1090, 1091, 1092, 1093, 1100, 1186, 1187, 1188,
            1189, 1190, 1200, 1283,
        ];
        for toward in [Toward::Client, Toward::Server] {
            for at in cuts {
                let (address, serving) = serve_one(root.clone(), move |mut stream, server| {
                    match read_route(&mut stream) {
                        Ok(Some(Purpose::Connect)) => {
                            accept(stream, server, |_| Some(())).map(drop)
                        }
                        Ok(Some(Purpose::Pair) | None) => Err(SecureError::Handshake),
                        Err(error) => Err(error),
                    }
                });
                let (via, relaying) = relay(address, Some((toward, at)));
                let attempt =
                    connect(TcpStream::connect(via).unwrap(), &client, &server_key).map(drop);
                let served = serving.join().unwrap();
                relaying.join().unwrap();
                if at < 1100 {
                    assert!(
                        attempt.is_err(),
                        "the client finished a handshake cut at {at} toward {toward:?}"
                    );
                }
                if at < 1186 && toward == Toward::Server {
                    assert!(
                        served.is_err(),
                        "the server finished a handshake cut at {at}"
                    );
                }
            }
        }
    }

    #[test]
    fn a_pairing_cut_anywhere_fails_cleanly_on_both_sides() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_path_buf();
        let words = crate::words::WORDS;
        let code_text = format!("{}-{}-{}-{}", words[0], words[1], words[2], words[3]);
        for toward in [Toward::Client, Toward::Server] {
            for at in [0usize, 1, 2, 20, 35, 36, 37, 38, 60, 1100, 1200, 1300] {
                let server_code = PairingCode::parse(&code_text).unwrap();
                let (address, serving) = serve_one(root.clone(), move |mut stream, server| {
                    read_route(&mut stream)?;
                    pair(
                        stream,
                        server,
                        Attempt {
                            code: &server_code,
                            side: Side::Responder,
                        },
                    )
                    .map(drop)
                });
                let (via, relaying) = relay(address, Some((toward, at)));
                let code = PairingCode::parse(&code_text).unwrap();
                let attempt = pair(
                    TcpStream::connect(via).unwrap(),
                    &identity(&root, "a"),
                    Attempt {
                        code: &code,
                        side: Side::Initiator,
                    },
                )
                .map(drop);
                let served = serving.join().unwrap();
                relaying.join().unwrap();
                assert!(attempt.is_err() || served.is_err() || at >= 1300);
                assert!(
                    attempt.is_err() && served.is_err() || at >= 1300 || toward == Toward::Client
                );
            }
        }
    }

    #[derive(Debug)]
    struct Faulty {
        stream: TcpStream,
        left: Arc<std::sync::atomic::AtomicUsize>,
        unsplittable: Arc<std::sync::atomic::AtomicBool>,
        cut: Arc<std::sync::Mutex<Option<std::io::ErrorKind>>>,
    }

    impl Faulty {
        fn new(stream: TcpStream, left: usize) -> Self {
            Self {
                stream,
                left: Arc::new(std::sync::atomic::AtomicUsize::new(left)),
                unsplittable: Arc::new(std::sync::atomic::AtomicBool::new(false)),
                cut: Arc::new(std::sync::Mutex::new(None)),
            }
        }

        fn refuse_splits(&self) {
            self.unsplittable
                .store(true, std::sync::atomic::Ordering::SeqCst);
        }

        fn spend(&self) -> std::io::Result<()> {
            use std::sync::atomic::Ordering;
            match self
                .left
                .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
            {
                Ok(_) => Ok(()),
                Err(_) => {
                    let cut = *self.cut.lock().map_err(|_poisoned| {
                        std::io::Error::other("the fault lock was poisoned")
                    })?;
                    Err(match cut {
                        Some(kind) => kind.into(),
                        None => std::io::Error::other("an injected fault"),
                    })
                }
            }
        }

        fn spent(&self, from: usize) -> usize {
            from.saturating_sub(self.left.load(std::sync::atomic::Ordering::SeqCst))
        }
    }

    type Reading = std::thread::JoinHandle<std::io::Result<Vec<u8>>>;

    fn connected_faulty(
        root: std::path::PathBuf,
        client: &Identity,
        server_key: &PublicKey,
    ) -> (Channel<Faulty>, Faulty, Reading) {
        let (address, reading) = serve_one(root, read_all);
        let wrapped = Faulty::new(TcpStream::connect(address).unwrap(), usize::MAX);
        let probe = wrapped.split().unwrap();
        let channel = connect(wrapped, client, server_key).unwrap();
        (channel, probe, reading)
    }

    impl Read for Faulty {
        fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
            self.spend()?;
            self.stream.read(out)
        }
    }

    impl Write for Faulty {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.spend()?;
            self.stream.write(bytes)
        }

        fn flush(&mut self) -> std::io::Result<()> {
            self.spend()?;
            self.stream.flush()
        }
    }

    impl Duplex for Faulty {
        fn split(&self) -> std::io::Result<Self> {
            if self.unsplittable.load(std::sync::atomic::Ordering::SeqCst) {
                return Err(std::io::Error::other("an injected split failure"));
            }
            Ok(Self {
                stream: self.stream.try_clone()?,
                left: Arc::clone(&self.left),
                unsplittable: Arc::clone(&self.unsplittable),
                cut: Arc::clone(&self.cut),
            })
        }

        fn close_sending(&self) -> std::io::Result<()> {
            self.stream.shutdown(std::net::Shutdown::Write)
        }
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Faults {
        Client(usize),
        Server(usize),
    }

    fn budget(faults: Faults, server: bool) -> usize {
        match (faults, server) {
            (Faults::Client(n), false) | (Faults::Server(n), true) => n,
            (Faults::Client(_), true) | (Faults::Server(_), false) => usize::MAX,
        }
    }

    fn faulty_connection(address: std::net::SocketAddr, faults: Faults) -> (Faulty, Faulty, usize) {
        let budget = budget(faults, false);
        let wrapped = Faulty::new(TcpStream::connect(address).unwrap(), budget);
        let probe = wrapped.split().unwrap();
        (wrapped, probe, budget)
    }

    type FaultOutcome = (Result<usize, SecureError>, Result<usize, SecureError>);

    fn every_fault_fails(
        root: &std::path::Path,
        limit: usize,
        run: fn(Faults, &std::path::Path) -> FaultOutcome,
        roles: (&str, &str),
    ) {
        let (client_name, server_name) = roles;
        let (client_ops, server_ops) = {
            let (client, server) = run(Faults::Client(usize::MAX), root);
            (client.unwrap(), server.unwrap())
        };
        for at in 0..client_ops.min(limit) {
            let (client, _) = run(Faults::Client(at), root);
            assert!(
                client.is_err(),
                "the {client_name} survived a fault at operation {at}"
            );
        }
        for at in 0..server_ops.min(limit) {
            let (_, server) = run(Faults::Server(at), root);
            assert!(
                server.is_err(),
                "the {server_name} survived a fault at operation {at}"
            );
        }
    }

    fn connect_with(
        faults: Faults,
        root: &std::path::Path,
    ) -> (Result<usize, SecureError>, Result<usize, SecureError>) {
        let client = identity(root, "a");
        let server_key = *identity(root, "b").public();
        let (address, serving) = serve_one(root.to_path_buf(), move |stream, server| {
            let budget = budget(faults, true);
            let mut wrapped = Faulty::new(stream, budget);
            read_route(&mut wrapped).and_then(|_| {
                let probe = wrapped.split()?;
                accept(wrapped, server, |_| Some(())).map(|(mut channel, ())| {
                    let spent = probe.spent(budget);
                    match channel.writer.close() {
                        Ok(()) | Err(_) => {}
                    }
                    spent
                })
            })
        });
        let (wrapped, probe, budget) = faulty_connection(address, faults);
        let connected = connect(wrapped, &client, &server_key).map(|mut channel| {
            let spent = probe.spent(budget);
            match channel.writer.close() {
                Ok(()) | Err(_) => {}
            }
            spent
        });
        drop(probe);
        (connected, serving.join().unwrap())
    }

    #[test]
    fn a_failing_random_source_stops_a_connection_and_a_pairing_code() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_path_buf();
        let (connected, served) =
            crate::pq::exhausted(|| connect_with(Faults::Client(usize::MAX), &root));
        connected.unwrap_err();
        served.unwrap_err();
        assert!(matches!(
            crate::pq::exhausted(PairingCode::generate),
            Err(Invalid::Random(_))
        ));
        PairingCode::generate().unwrap();
    }

    #[test]
    fn a_fault_at_any_step_of_a_connection_is_an_error_never_a_panic() {
        let tmp = tempfile::tempdir().unwrap();
        every_fault_fails(tmp.path(), 64, connect_with, ("client", "server"));
    }

    fn pair_with(
        faults: Faults,
        root: &std::path::Path,
    ) -> (Result<usize, SecureError>, Result<usize, SecureError>) {
        let words = crate::words::WORDS;
        let code_text = format!("{}-{}-{}-{}", words[0], words[1], words[2], words[3]);
        let server_code = PairingCode::parse(&code_text).unwrap();
        let (address, serving) = serve_one(root.to_path_buf(), move |stream, server| {
            let budget = budget(faults, true);
            let mut wrapped = Faulty::new(stream, budget);
            let probe = wrapped.split().unwrap();
            let attempt = Attempt {
                code: &server_code,
                side: Side::Responder,
            };
            read_route(&mut wrapped)
                .and_then(|_| pair(wrapped, server, attempt))
                .map(|_| probe.spent(budget))
        });
        let (wrapped, probe, budget) = faulty_connection(address, faults);
        let code = PairingCode::parse(&code_text).unwrap();
        let attempt = Attempt {
            code: &code,
            side: Side::Initiator,
        };
        let paired = pair(wrapped, &identity(root, "a"), attempt).map(|_| probe.spent(budget));
        drop(probe);
        (paired, serving.join().unwrap())
    }

    #[test]
    fn a_fault_at_any_step_of_a_pairing_is_an_error_never_a_panic() {
        let tmp = tempfile::tempdir().unwrap();
        every_fault_fails(tmp.path(), 96, pair_with, ("initiator", "responder"));
    }

    #[test]
    fn a_fault_while_talking_is_reported_on_both_ends() {
        let (_tmp, root, client, server_key) = parties();
        for extra in 0..9usize {
            let (address, serving) = serve_one(root.clone(), read_all);
            let wrapped = Faulty::new(TcpStream::connect(address).unwrap(), usize::MAX);
            let probe = wrapped.split().unwrap();
            let mut channel = connect(wrapped, &client, &server_key).unwrap();
            probe.left.store(extra, std::sync::atomic::Ordering::SeqCst);
            let sent = channel
                .writer
                .write_all(b"first")
                .and_then(|()| channel.writer.write_all(b"second"))
                .and_then(|()| channel.writer.close());
            assert!(
                sent.is_err(),
                "writing survived a fault after {extra} operations"
            );
            drop(channel);
            drop(probe);
            let received = serving.join().unwrap();
            let only_the_last_flush_failed = extra == 8;
            assert!(
                received.is_err() || only_the_last_flush_failed,
                "the reader took a cut stream after {extra} operations as a clean close"
            );
        }
    }

    #[test]
    fn unknown_clients_and_impostor_servers_are_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_path_buf();
        let client = identity(&root, "a");
        let expected = *identity(&root, "b").public();
        let (address, serving) = serve_one(root.clone(), move |mut stream, server| {
            read_route(&mut stream).unwrap();
            accept(stream, server, |_| None::<()>).map(|_| ())
        });
        connect(TcpStream::connect(address).unwrap(), &client, &expected).unwrap_err();
        assert!(matches!(serving.join().unwrap(), Err(SecureError::Unknown)));

        let (impostor_listener, impostor_address) = bound();
        std::thread::spawn(move || {
            let impostor = identity(&root, "c");
            let (mut stream, _) = impostor_listener.accept().unwrap();
            read_route(&mut stream).unwrap();
            accept(stream, &impostor, |_| Some(())).map(|_| ())
        });
        connect(
            TcpStream::connect(impostor_address).unwrap(),
            &client,
            &expected,
        )
        .unwrap_err();
    }

    fn run_pairing(
        root: &std::path::Path,
        client_code: &str,
        server_code: &str,
    ) -> (Result<Pending, SecureError>, Result<Pending, SecureError>) {
        let client = identity(root, "a");
        let server_root = root.to_path_buf();
        let (listener, address) = bound();
        let server_code = PairingCode::parse(server_code).unwrap();
        let serving = std::thread::spawn(move || {
            let server = identity(&server_root, "b");
            let (mut stream, _) = listener.accept().unwrap();
            read_route(&mut stream).unwrap();
            pair(
                stream,
                &server,
                Attempt {
                    code: &server_code,
                    side: Side::Responder,
                },
            )
        });
        let code = PairingCode::parse(client_code).unwrap();
        let ours = pair(
            TcpStream::connect(address).unwrap(),
            &client,
            Attempt {
                code: &code,
                side: Side::Initiator,
            },
        );
        (ours, serving.join().unwrap())
    }

    #[test]
    fn pairing_needs_the_same_code_and_yields_the_same_confirmation() {
        let tmp = tempfile::tempdir().unwrap();
        let words = crate::words::WORDS;
        let right = format!("{}-{}-{}-{}", words[0], words[1], words[2], words[3]);
        let wrong = format!("{}-{}-{}-{}", words[0], words[1], words[2], words[4]);
        let (ours, theirs) = run_pairing(tmp.path(), &right, &right);
        let (ours, theirs) = (ours.unwrap(), theirs.unwrap());
        assert_eq!(ours.sas, theirs.sas);
        assert_eq!(ours.sas.as_str().split(' ').count(), 5);
        let (wrong_ours, wrong_theirs) = run_pairing(tmp.path(), &right, &wrong);
        assert!(wrong_ours.is_err() && wrong_theirs.is_err());
        PairingCode::parse("not-a-real-code").unwrap_err();
        assert_eq!(
            PairingCode::generate().unwrap().as_str().split('-').count(),
            4
        );
    }

    fn pairing_code() -> PairingCode {
        let words = crate::words::WORDS;
        PairingCode::parse(&format!(
            "{}-{}-{}-{}",
            words[0], words[1], words[2], words[3]
        ))
        .unwrap()
    }

    fn respond_to_pairing(mut stream: TcpStream, server: &Identity) -> Result<(), SecureError> {
        read_route(&mut stream)?;
        let code = pairing_code();
        pair(
            stream,
            server,
            Attempt {
                code: &code,
                side: Side::Responder,
            },
        )
        .map(drop)
    }

    fn send_short(address: std::net::SocketAddr, purpose: Purpose) {
        let mut raw = TcpStream::connect(address).unwrap();
        start_route(&mut raw, purpose).unwrap();
        send_frame(&mut raw, b"short").unwrap();
    }

    #[test]
    fn a_malformed_post_quantum_key_is_an_error_never_a_panic() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_path_buf();
        let (address, serving) = serve_one(root, accept_known);
        send_short(address, Purpose::Connect);
        serving.join().unwrap().unwrap_err();
    }

    #[test]
    fn a_malformed_post_quantum_ciphertext_is_an_error_never_a_panic() {
        let (_tmp, _root, client, server_key) = parties();
        let (listener, address) = bound();
        let faking = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            read_route(&mut stream).unwrap();
            receive_frame(&mut stream).unwrap();
            send_frame(&mut stream, b"short").unwrap();
            stream
        });
        let attempt = connect(TcpStream::connect(address).unwrap(), &client, &server_key);
        drop(faking.join().unwrap());
        attempt.map(drop).unwrap_err();
    }

    #[test]
    fn a_malformed_pairing_message_is_an_error_never_a_panic() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_path_buf();
        let (address, serving) = serve_one(root, respond_to_pairing);
        send_short(address, Purpose::Pair);
        serving.join().unwrap().unwrap_err();
    }

    #[test]
    fn a_forged_frame_or_a_failing_read_is_an_error_never_a_clean_close() {
        let (_tmp, root, client, server_key) = parties();
        for (forged, cut, expected) in [
            (true, None, std::io::ErrorKind::Other),
            (false, None, std::io::ErrorKind::Other),
            (
                false,
                Some(std::io::ErrorKind::ConnectionReset),
                std::io::ErrorKind::UnexpectedEof,
            ),
            (
                false,
                Some(std::io::ErrorKind::ConnectionAborted),
                std::io::ErrorKind::UnexpectedEof,
            ),
        ] {
            let (address, serving) = serve_one(root.clone(), move |stream, server| {
                let mut wrapped = Faulty::new(stream, usize::MAX);
                let probe = wrapped.split().unwrap();
                read_route(&mut wrapped).unwrap();
                let (mut channel, ()) = accept(wrapped, server, |_| Some(())).unwrap();
                if !forged {
                    *probe.cut.lock().unwrap() = cut;
                    probe.left.store(0, std::sync::atomic::Ordering::SeqCst);
                }
                let mut received = Vec::new();
                channel.reader.read_to_end(&mut received).map(|_| received)
            });
            let stream = TcpStream::connect(address).unwrap();
            let mut raw = stream.try_clone().unwrap();
            let channel = connect(stream, &client, &server_key).unwrap();
            if forged {
                send_frame(&mut raw, &[0u8; 40]).unwrap();
            }
            let error = serving.join().unwrap().unwrap_err();
            assert_eq!(error.kind(), expected);
            drop(channel);
        }
    }

    #[test]
    fn closing_a_tcp_stream_ends_its_sending() {
        let (listener, address) = bound();
        let accepting = std::thread::spawn(move || listener.accept().unwrap().0);
        let stream = TcpStream::connect(address).unwrap();
        let peer = accepting.join().unwrap();
        Duplex::close_sending(&stream).unwrap();
        (&stream).write(b"x").unwrap_err();
        drop(peer);
    }

    #[test]
    fn closing_stops_the_writer_and_half_closes_the_stream() {
        let (_tmp, root, client, server_key) = parties();
        let (mut channel, probe, serving) = connected_faulty(root, &client, &server_key);
        channel.writer.close().unwrap();
        let before = probe.spent(usize::MAX);
        channel.writer.write_all(b"after close").unwrap_err();
        assert_eq!(
            probe.spent(usize::MAX),
            before,
            "a closed writer still reached the stream"
        );
        (&probe.stream).write(b"x").unwrap_err();
        assert!(serving.join().unwrap().unwrap().is_empty());
    }

    #[test]
    fn a_peer_that_echoes_the_wrong_confirmation_is_refused() {
        let (_tmp, root, client, server_key) = parties();
        let (address, serving) = serve_one(root, move |stream, server| {
            let mut channel = accepted(stream, server);
            let mut mark = [0u8; 16];
            channel.reader.read_exact(&mut mark).unwrap();
            channel.writer.write_all(b"domyjob/2 wrong!").unwrap();
            channel
        });
        let mut channel =
            connect(TcpStream::connect(address).unwrap(), &client, &server_key).unwrap();
        assert!(matches!(
            confirm_keys(&mut channel, Side::Initiator),
            Err(SecureError::Handshake)
        ));
        drop(serving.join().unwrap());
    }

    #[test]
    fn a_stream_that_cannot_be_split_fails_every_handshake() {
        let (_tmp, root, client, server_key) = parties();
        for refusing in [Side::Initiator, Side::Responder] {
            let (address, serving) = serve_one(root.clone(), move |stream, server| {
                let mut wrapped = Faulty::new(stream, usize::MAX);
                if refusing == Side::Responder {
                    wrapped.refuse_splits();
                }
                read_route(&mut wrapped).unwrap();
                accept(wrapped, server, |_| Some(())).map(drop)
            });
            let wrapped = Faulty::new(TcpStream::connect(address).unwrap(), usize::MAX);
            if refusing == Side::Initiator {
                wrapped.refuse_splits();
            }
            let connected = connect(wrapped, &client, &server_key).map(drop);
            let served = serving.join().unwrap();
            match refusing {
                Side::Initiator => assert!(connected.is_err()),
                Side::Responder => assert!(served.is_err()),
            }
        }

        let (address, serving) = serve_one(root.clone(), respond_to_pairing);
        let wrapped = Faulty::new(TcpStream::connect(address).unwrap(), usize::MAX);
        wrapped.refuse_splits();
        let code = pairing_code();
        let attempt = Attempt {
            code: &code,
            side: Side::Initiator,
        };
        pair(wrapped, &identity(&root, "a"), attempt)
            .map(drop)
            .unwrap_err();
        assert!(serving.join().unwrap().is_err());
    }

    #[test]
    fn a_session_that_runs_out_of_nonces_stops_instead_of_reusing_one() {
        let (_tmp, root, client, server_key) = parties();
        let (address, serving) = serve_one(root, move |stream, server| {
            let mut channel = accepted(stream, server);
            channel.reader.nonce = u64::MAX;
            let mut byte = [0u8; 1];
            channel.reader.read(&mut byte).unwrap_err().kind()
        });
        let mut channel =
            connect(TcpStream::connect(address).unwrap(), &client, &server_key).unwrap();
        channel.writer.write_all(b"x").unwrap();
        assert_eq!(serving.join().unwrap(), std::io::ErrorKind::Other);
        channel.writer.nonce = u64::MAX;
        channel.writer.write_all(b"y").unwrap_err();
    }

    #[test]
    fn a_sealed_frame_of_an_unknown_kind_is_refused_and_flushing_reaches_the_stream() {
        let (_tmp, root, client, server_key) = parties();
        let (mut channel, probe, serving) = connected_faulty(root, &client, &server_key);
        channel.writer.seal(7, b"").unwrap();
        assert_eq!(
            serving.join().unwrap().unwrap_err().to_string(),
            "a frame had an unknown kind"
        );
        probe.left.store(0, std::sync::atomic::Ordering::SeqCst);
        channel.writer.flush().unwrap_err();
    }

    fn what_the_initiator_sends_after_both_exchanges(purpose: Purpose) -> Vec<u8> {
        let (_tmp, _root, client, server_key) = parties();
        let (listener, address) = bound();
        let listening = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            assert_eq!(read_route(&mut stream).unwrap(), Some(purpose));
            if purpose == Purpose::Pair {
                pake_psk(&mut stream, &pairing_code()).unwrap();
            }
            agree(&mut stream, Side::Responder).unwrap();
            stream.shutdown(std::net::Shutdown::Write).unwrap();
            let mut rest = Vec::new();
            stream.read_to_end(&mut rest).unwrap();
            rest
        });
        let stream = TcpStream::connect(address).unwrap();
        let attempt = match purpose {
            Purpose::Connect => connect(stream, &client, &server_key).map(drop),
            Purpose::Pair => {
                let code = pairing_code();
                let attempt = Attempt {
                    code: &code,
                    side: Side::Initiator,
                };
                pair(stream, &client, attempt).map(drop)
            }
        };
        attempt.unwrap_err();
        listening.join().unwrap()
    }

    #[test]
    fn the_initiator_names_its_route_and_speaks_first_once_the_exchanges_are_done() {
        for purpose in [Purpose::Connect, Purpose::Pair] {
            let rest = what_the_initiator_sends_after_both_exchanges(purpose);
            assert_eq!(
                frames(&rest).len(),
                1,
                "the {purpose:?} initiator sent {} bytes instead of its first handshake message",
                rest.len()
            );
        }
    }

    #[test]
    fn a_pairing_responder_answers_the_route_with_its_code_message_alone() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_path_buf();
        let (address, responding) = serve_one(root, respond_to_pairing);
        let mut raw = TcpStream::connect(address).unwrap();
        start_route(&mut raw, Purpose::Pair).unwrap();
        let mut length = [0u8; 2];
        raw.read_exact(&mut length).unwrap();
        assert_eq!(u16::from_be_bytes(length), 33);
        drop(raw);
        responding.join().unwrap().unwrap_err();
    }
}
