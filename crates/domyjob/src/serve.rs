use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::io::{BufReader, Read, Write};
use std::net::{IpAddr, SocketAddr, TcpListener, TcpStream};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};

use crate::audit::{AuditLog, Verdict};
use crate::authz::{Capability, Principal};
use crate::clock::Timestamp;
use crate::domain::{Invalid, MachineName};
use crate::paths::Dirs;
use crate::secure::{self, PairingCode, Purpose, SecureError, Side};
use crate::terminal::Display;
use crate::trust::{Grant, Identity, PublicKey, Server, Trust, TrustError};

pub const DEFAULT_PORT: u16 = 4747;
pub const SERVICE: &str = "_domyjob._tcp.local.";
const PAIRING_TRIES: u8 = 5;
const MAX_CONNECTIONS: usize = 64;
const MAX_PER_SOURCE: usize = 16;
const GREETING_LIMIT: u64 = 1024;

#[derive(Debug, thiserror::Error)]
pub enum ServeError {
    #[error("listening on {address}: {source}")]
    Listen {
        address: SocketAddr,
        source: std::io::Error,
    },
    #[error(transparent)]
    Trust(#[from] TrustError),
    #[error(transparent)]
    Secure(#[from] SecureError),
    #[error(transparent)]
    Node(#[from] crate::node::NodeError),
    #[error(transparent)]
    Audit(#[from] crate::audit::AuditError),
    #[error(transparent)]
    Invalid(#[from] Invalid),
    #[error("the pairing exchange was malformed")]
    Exchange,
    #[error("{0}")]
    Refused(&'static str),
    #[error(
        "the machine that answered did not accept that pairing code; check the code, or name the machine with --at"
    )]
    NoOneAnswered,
    #[error("discovery stopped before any machine announced a pairing: {0}")]
    Discovery(String),
    #[error("{machine} is not paired with this machine; run `domyjob pair` first")]
    NotPaired { machine: MachineName },
    #[error(
        "{machine} did not prove it holds the key it was paired with; something may be impersonating it"
    )]
    Impersonation { machine: MachineName },
    #[error(
        "{machine} is unreachable at {address}; if it moved, pair again with `domyjob pair --at NEW-ADDRESS`"
    )]
    Unreachable {
        machine: MachineName,
        address: String,
    },
    #[error(
        "no Tailscale address is configured on this machine; choose another exposure with --expose"
    )]
    NoTailnet,
    #[error("no private LAN address was found on this machine")]
    NoLan,
    #[error(
        "refusing to serve as root; run as an ordinary user, or pass --allow-root if you have isolated this account"
    )]
    Root,
    #[error("the pairing was declined on the other machine")]
    Declined,
    #[error("the connection failed: {0}")]
    Io(#[from] std::io::Error),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Exposure {
    Loopback,
    Tailnet,
    Lan,
    Explicit(SocketAddr),
}

impl Exposure {
    pub fn parse(text: &str) -> Result<Self, Invalid> {
        match text {
            "loopback" => Ok(Self::Loopback),
            "tailnet" => Ok(Self::Tailnet),
            "lan" => Ok(Self::Lan),
            other => other
                .parse::<SocketAddr>()
                .map(Self::Explicit)
                .map_err(|_bad| Invalid::Exposure(other.to_owned())),
        }
    }

    #[must_use]
    pub fn default_for_this_machine() -> Self {
        if tailnet_address().is_some() {
            Self::Tailnet
        } else {
            Self::Loopback
        }
    }

    pub fn resolve(self, port: u16) -> Result<SocketAddr, ServeError> {
        match self {
            Self::Loopback => Ok(SocketAddr::from(([127, 0, 0, 1], port))),
            Self::Tailnet => tailnet_address()
                .map(|ip| SocketAddr::new(ip, port))
                .ok_or(ServeError::NoTailnet),
            Self::Lan => lan_address()
                .map(|ip| SocketAddr::new(ip, port))
                .ok_or(ServeError::NoLan),
            Self::Explicit(address) => Ok(address),
        }
    }

    #[must_use]
    pub const fn announces(self) -> bool {
        match self {
            Self::Lan | Self::Explicit(_) => true,
            Self::Loopback | Self::Tailnet => false,
        }
    }
}

fn is_tailnet(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let [a, b, ..] = v4.octets();
            a == 100 && (64..128).contains(&b)
        }
        IpAddr::V6(v6) => v6.segments().starts_with(&[0xfd7a, 0x115c, 0xa1e0]),
    }
}

fn addresses() -> Vec<IpAddr> {
    match if_addrs::get_if_addrs() {
        Ok(interfaces) => interfaces
            .into_iter()
            .filter(|i| !i.is_loopback())
            .map(|i| i.ip())
            .collect(),
        Err(_unavailable) => Vec::new(),
    }
}

fn tailnet_address() -> Option<IpAddr> {
    addresses()
        .into_iter()
        .find(|ip| is_tailnet(*ip) && ip.is_ipv4())
}

fn lan_address() -> Option<IpAddr> {
    addresses().into_iter().find(|ip| match ip {
        IpAddr::V4(v4) => v4.is_private() && !is_tailnet(*ip),
        IpAddr::V6(_) => false,
    })
}

pub fn refuse_root(allowed: bool) -> Result<(), ServeError> {
    if crate::platform::elevated() && !allowed {
        Err(ServeError::Root)
    } else {
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Greeting {
    name: MachineName,
    os: String,
    arch: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "snake_case", tag = "decision")]
enum Answer {
    Accepted {
        name: MachineName,
        os: String,
        arch: String,
        capabilities: BTreeSet<Capability>,
    },
    Declined,
}

impl Greeting {
    fn here() -> Result<Self, Invalid> {
        Ok(Self {
            name: host_name().parse()?,
            os: crate::platform::OS.to_owned(),
            arch: crate::platform::ARCH.to_owned(),
        })
    }
}

#[must_use]
pub fn host_name() -> String {
    let raw = sysinfo::System::host_name().unwrap_or_default();
    let cleaned: String = raw
        .split('.')
        .next()
        .unwrap_or("")
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    let cleaned = cleaned.trim_matches('-').to_owned();
    if cleaned.is_empty() {
        "machine".to_owned()
    } else {
        cleaned
    }
}

fn send_line<T: Serialize>(writer: &mut dyn Write, value: &T) -> Result<(), ServeError> {
    let mut line = serde_json::to_vec(value).map_err(|_unencodable| ServeError::Exchange)?;
    line.push(b'\n');
    writer.write_all(&line)?;
    writer.flush()?;
    Ok(())
}

fn read_line<T: crate::ingress::Ingress>(reader: &mut dyn Read) -> Result<T, ServeError> {
    let mut line = Vec::new();
    let mut limited = reader.take(GREETING_LIMIT);
    let mut byte = [0u8; 1];
    while limited.read(&mut byte)? == 1 && byte != *b"\n" {
        line.extend_from_slice(&byte);
    }
    crate::ingress::json(&line).map_err(|_malformed| ServeError::Exchange)
}

#[derive(Debug)]
enum PairingState {
    Closed,
    Open(Offer),
}

#[derive(Debug)]
struct Offer {
    code: PairingCode,
    tries_left: u8,
    in_flight: u8,
    capabilities: BTreeSet<Capability>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Standing {
    Open,
    Closed,
}

#[derive(Debug, Default)]
struct Load {
    total: usize,
    by_source: HashMap<IpAddr, usize>,
}

#[derive(Debug, Default)]
struct Permits(Mutex<Load>);

#[derive(Debug)]
struct Permit {
    shared: Arc<Shared>,
    source: IpAddr,
}

impl Permits {
    fn admit(&self, source: IpAddr) -> Result<(), ServeError> {
        let mut load = self
            .0
            .lock()
            .map_err(|_poisoned| ServeError::Refused("the connection count is poisoned"))?;
        let from_source = load.by_source.get(&source).copied().unwrap_or(0);
        if load.total >= MAX_CONNECTIONS || from_source >= MAX_PER_SOURCE {
            return Err(ServeError::Refused("too many connections"));
        }
        load.total = load.total.saturating_add(1);
        load.by_source.insert(source, from_source.saturating_add(1));
        drop(load);
        Ok(())
    }

    fn release(&self, source: IpAddr) {
        let Ok(mut load) = self.0.lock() else {
            return;
        };
        load.total = load.total.saturating_sub(1);
        let left = load
            .by_source
            .get(&source)
            .copied()
            .unwrap_or(0)
            .saturating_sub(1);
        if left == 0 {
            load.by_source.remove(&source);
        } else {
            load.by_source.insert(source, left);
        }
    }
}

impl Permit {
    fn take(shared: &Arc<Shared>, source: IpAddr) -> Result<Self, ServeError> {
        shared.permits.admit(source)?;
        Ok(Self {
            shared: Arc::clone(shared),
            source,
        })
    }
}

impl Drop for Permit {
    fn drop(&mut self) {
        self.shared.permits.release(self.source);
    }
}

struct Shared {
    dirs: Dirs,
    identity: Identity,
    pairing: Mutex<PairingState>,
    confirming: Mutex<()>,
    permits: Permits,
    audit: AuditLog,
    advertiser: Option<Advertiser>,
}

impl std::fmt::Debug for Shared {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Shared")
            .field("dirs", &self.dirs)
            .finish_non_exhaustive()
    }
}

struct Advertiser {
    daemon: mdns_sd::ServiceDaemon,
    fullname: String,
}

impl Advertiser {
    fn start(port: u16, key: &PublicKey) -> Option<Self> {
        let daemon = match mdns_sd::ServiceDaemon::new() {
            Ok(daemon) => daemon,
            Err(error) => {
                eprintln!("domyjob: discovery is unavailable: {error}");
                return None;
            }
        };
        let instance = key.fingerprint().replace('-', "");
        let properties = HashMap::from([("id".to_owned(), key.fingerprint())]);
        let registered = mdns_sd::ServiceInfo::new(
            SERVICE,
            &instance,
            &format!("{instance}.local."),
            "",
            port,
            properties,
        )
        .map(mdns_sd::ServiceInfo::enable_addr_auto)
        .and_then(|info| {
            let fullname = info.get_fullname().to_owned();
            daemon.register(info).map(|()| fullname)
        });
        match registered {
            Ok(fullname) => Some(Self { daemon, fullname }),
            Err(error) => {
                eprintln!("domyjob: announcing the pairing failed: {error}");
                None
            }
        }
    }

    fn stop(&self) {
        match self.daemon.unregister(&self.fullname) {
            Ok(_) | Err(_) => {}
        }
    }
}

#[derive(Debug, Clone)]
pub struct Options {
    pub exposure: Exposure,
    pub port: u16,
    pub pairing: Option<BTreeSet<Capability>>,
    pub allow_root: bool,
}

pub fn serve(dirs: Dirs, options: &Options) -> Result<(), ServeError> {
    refuse_root(options.allow_root)?;
    let audit = AuditLog::at(&dirs);
    let entries = audit.verify()?;
    let identity = Identity::load_or_create(&dirs)?;
    let address = options.exposure.resolve(options.port)?;
    let listener =
        TcpListener::bind(address).map_err(|source| ServeError::Listen { address, source })?;
    let bound = listener.local_addr()?;
    println!(
        "domyjob serve on {bound}  key {}  audit entries {entries}",
        identity.public().fingerprint()
    );
    let pairing = match &options.pairing {
        Some(capabilities) => open_pairing(capabilities.clone())?,
        None => PairingState::Closed,
    };
    let advertiser = match (&pairing, options.exposure.announces()) {
        (PairingState::Open(_), true) => Advertiser::start(bound.port(), identity.public()),
        (PairingState::Open(_) | PairingState::Closed, _) => None,
    };
    let shared = Arc::new(Shared {
        dirs,
        identity,
        pairing: Mutex::new(pairing),
        confirming: Mutex::new(()),
        permits: Permits::default(),
        audit,
        advertiser,
    });
    for stream in listener.incoming() {
        let stream = match stream {
            Ok(stream) => keep_alive(stream),
            Err(error) => {
                eprintln!("domyjob: accepting a connection: {error}");
                continue;
            }
        };
        let peer = match stream.peer_addr() {
            Ok(remote) => remote,
            Err(error) => {
                eprintln!("domyjob: a connection without a peer address: {error}");
                continue;
            }
        };
        let permit = match Permit::take(&shared, peer.ip()) {
            Ok(permit) => permit,
            Err(error) => {
                eprintln!("domyjob: {peer}: {error}");
                continue;
            }
        };
        let shared = Arc::clone(&shared);
        std::thread::spawn(move || {
            if let Err(error) = handle(stream, &shared) {
                eprintln!("domyjob: {peer}: {error}");
            }
            drop(permit);
        });
    }
    Ok(())
}

fn open_pairing(capabilities: BTreeSet<Capability>) -> Result<PairingState, ServeError> {
    let code = PairingCode::generate()?;
    let granted: Vec<&str> = capabilities.iter().map(|c| c.as_str()).collect();
    println!(
        "pairing is open until one machine pairs, {PAIRING_TRIES} attempts fail, or you stop this command; a paired machine will be allowed: {}",
        granted.join(", ")
    );
    println!("on the other machine run:  domyjob pair {}", code.as_str());
    Ok(PairingState::Open(Offer {
        code,
        tries_left: PAIRING_TRIES,
        in_flight: 0,
        capabilities,
    }))
}

fn handle(mut stream: TcpStream, shared: &Shared) -> Result<(), ServeError> {
    match secure::read_route(&mut stream)? {
        Some(Purpose::Connect) => serve_connection(stream, shared),
        Some(Purpose::Pair) => accept_pairing(stream, shared),
        None => Err(ServeError::Refused("unknown route")),
    }
}

fn serve_connection(stream: TcpStream, shared: &Shared) -> Result<(), ServeError> {
    let trust = Trust::load(&shared.dirs)?;
    let (channel, principal) = secure::accept(stream, &shared.identity, |key| {
        trust.grant_for(key).map(|grant| Principal::Peer {
            key: grant.public_key,
            label: grant.label.clone(),
            capabilities: grant.capabilities.clone(),
        })
    })?;
    let secure::Halves { reader, mut writer } = channel.into_halves();
    let node = crate::node::Node::open(shared.dirs.clone())?;
    node.serve(&principal, BufReader::new(reader), &mut writer)?;
    writer.close()?;
    Ok(())
}

fn lock_pairing(shared: &Shared) -> Result<std::sync::MutexGuard<'_, PairingState>, ServeError> {
    shared
        .pairing
        .lock()
        .map_err(|_poisoned| ServeError::Refused("pairing state is poisoned"))
}

fn claim(shared: &Shared) -> Result<(PairingCode, BTreeSet<Capability>), ServeError> {
    let mut guard = lock_pairing(shared)?;
    let claimed = match &mut *guard {
        PairingState::Open(offer) if offer.tries_left > 0 => {
            offer.tries_left = offer.tries_left.saturating_sub(1);
            offer.in_flight = offer.in_flight.saturating_add(1);
            Ok((
                PairingCode::parse(offer.code.as_str())?,
                offer.capabilities.clone(),
            ))
        }
        PairingState::Open(_) => Err(ServeError::Refused("no pairing attempts are left")),
        PairingState::Closed => Err(ServeError::Refused(
            "this machine is not accepting pairings",
        )),
    };
    drop(guard);
    claimed
}

fn standing(shared: &Shared) -> Result<Standing, ServeError> {
    Ok(match &*lock_pairing(shared)? {
        PairingState::Open(_) => Standing::Open,
        PairingState::Closed => Standing::Closed,
    })
}

fn settle(shared: &Shared, consumed: bool) -> Result<(), ServeError> {
    let mut guard = lock_pairing(shared)?;
    let state = std::mem::replace(&mut *guard, PairingState::Closed);
    let next = match state {
        PairingState::Open(mut offer) if !consumed => {
            offer.in_flight = offer.in_flight.saturating_sub(1);
            if offer.tries_left == 0 && offer.in_flight == 0 {
                PairingState::Closed
            } else {
                PairingState::Open(offer)
            }
        }
        PairingState::Open(_) | PairingState::Closed => PairingState::Closed,
    };
    let closed = matches!(next, PairingState::Closed);
    *guard = next;
    drop(guard);
    if closed {
        if let Some(advertiser) = &shared.advertiser {
            advertiser.stop();
        }
        println!("pairing is closed");
    }
    Ok(())
}

fn confirm_on_terminal(
    peer: &Greeting,
    sas: &str,
    capabilities: &BTreeSet<Capability>,
) -> Result<bool, ServeError> {
    use std::io::IsTerminal;
    if !std::io::stdin().is_terminal() {
        return Err(ServeError::Refused(
            "pairing needs a person at this machine's terminal to confirm it",
        ));
    }
    let granted: Vec<&str> = capabilities.iter().map(|c| c.as_str()).collect();
    println!();
    println!(
        "{} ({}/{}) wants to pair.",
        Display::of(peer.name.as_str()),
        Display::of(&peer.os),
        Display::of(&peer.arch)
    );
    println!("It will be allowed: {}", granted.join(", "));
    println!("Confirmation words: {sas}");
    print!("Do the same words appear on the other machine? Type yes to pair: ");
    std::io::stdout().flush()?;
    let mut answer = String::new();
    std::io::stdin().read_line(&mut answer)?;
    Ok(answer.trim() == "yes")
}

fn accept_pairing(stream: TcpStream, shared: &Shared) -> Result<(), ServeError> {
    let (code, capabilities) = claim(shared)?;
    match pair_with(stream, shared, &code, &capabilities) {
        Ok(true) => Ok(()),
        Ok(false) => settle(shared, false),
        Err(error) => {
            settle(shared, false)?;
            Err(error)
        }
    }
}

fn pair_with(
    stream: TcpStream,
    shared: &Shared,
    code: &PairingCode,
    capabilities: &BTreeSet<Capability>,
) -> Result<bool, ServeError> {
    let mut pending = secure::pair(
        stream,
        &shared.identity,
        secure::Attempt {
            code,
            side: Side::Responder,
        },
    )?;
    let peer: Greeting = read_line(&mut pending.channel.reader)?;
    let turn = shared
        .confirming
        .lock()
        .map_err(|_poisoned| ServeError::Refused("the confirmation prompt is poisoned"))?;
    let accepted = match standing(shared)? {
        Standing::Open => confirm_on_terminal(&peer, pending.sas.as_str(), capabilities)?,
        Standing::Closed => false,
    };
    let key = *pending.channel.remote();
    let granted_at = Timestamp::observe();
    let (verdict, answer) = if accepted {
        let here = Greeting::here()?;
        Trust::update(&shared.dirs, |trust| {
            trust.grants.retain(|grant| grant.public_key != key);
            trust.grants.push(Grant {
                label: peer.name.clone(),
                public_key: key,
                capabilities: capabilities.clone(),
                granted_at,
            });
        })?;
        settle(shared, true)?;
        (
            Verdict::Allowed,
            Answer::Accepted {
                name: here.name,
                os: here.os,
                arch: here.arch,
                capabilities: capabilities.clone(),
            },
        )
    } else {
        (Verdict::Denied, Answer::Declined)
    };
    let subject = Some(format!("{} {}", peer.name, key.fingerprint()));
    shared.audit.record(crate::audit::Event {
        principal: "owner",
        action: "pair",
        subject,
        verdict,
    })?;
    drop(turn);
    send_line(&mut pending.channel.writer, &answer)?;
    pending.channel.writer.close()?;
    println!("{}", if accepted { "paired" } else { "declined" });
    Ok(accepted)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Found {
    pub address: SocketAddr,
    pub id: String,
}

pub fn discover_first() -> Result<Found, ServeError> {
    let failed = |error: mdns_sd::Error| ServeError::Discovery(error.to_string());
    let daemon = mdns_sd::ServiceDaemon::new().map_err(failed)?;
    let events = daemon.browse(SERVICE).map_err(failed)?;
    let found = loop {
        let event = match events.recv() {
            Ok(event) => event,
            Err(error) => break Err(ServeError::Discovery(error.to_string())),
        };
        let mdns_sd::ServiceEvent::ServiceResolved(service) = event else {
            continue;
        };
        let id = service.get_property_val_str("id").unwrap_or("").to_owned();
        if let Some(ip) = service.get_addresses_v4().into_iter().next() {
            break Ok(Found {
                address: SocketAddr::new(ip.into(), service.get_port()),
                id,
            });
        }
    };
    match daemon.shutdown() {
        Ok(_) | Err(_) => {}
    }
    found
}

fn keep_alive(stream: TcpStream) -> TcpStream {
    match socket2::SockRef::from(&stream).set_keepalive(true) {
        Ok(()) | Err(_) => {}
    }
    stream
}

fn open(address: &str) -> Result<TcpStream, ServeError> {
    use std::net::ToSocketAddrs;
    let mut last = std::io::Error::other(format!("{address} did not resolve"));
    for candidate in address.to_socket_addrs()? {
        match TcpStream::connect(candidate) {
            Ok(stream) => return Ok(keep_alive(stream)),
            Err(error) => last = error,
        }
    }
    Err(ServeError::Io(last))
}

fn with_port(address: &str) -> String {
    let has_port = address.parse::<SocketAddr>().is_ok()
        || address
            .rsplit_once(':')
            .is_some_and(|(_, port)| port.parse::<u16>().is_ok());
    if has_port {
        address.to_owned()
    } else {
        format!("{address}:{DEFAULT_PORT}")
    }
}

#[derive(Debug, Clone)]
pub struct Paired {
    pub name: MachineName,
    pub address: String,
    pub key: PublicKey,
    pub os: String,
    pub arch: String,
    pub capabilities: BTreeSet<Capability>,
}

fn candidates(at: Option<&str>) -> Result<String, ServeError> {
    if let Some(address) = at {
        return Ok(with_port(address));
    }
    eprintln!("domyjob: waiting for a machine that is accepting pairings on this network");
    let found = discover_first()?;
    eprintln!("domyjob: found {} ({})", found.address, found.id);
    Ok(found.address.to_string())
}

pub fn pair(
    dirs: &Dirs,
    code: &PairingCode,
    at: Option<&str>,
    name: Option<&MachineName>,
) -> Result<Paired, ServeError> {
    let identity = Identity::load_or_create(dirs)?;
    let address = candidates(at)?;
    let stream = open(&address)?;
    let mut pending = secure::pair(
        stream,
        &identity,
        secure::Attempt {
            code,
            side: Side::Initiator,
        },
    )
    .map_err(|_failed| ServeError::NoOneAnswered)?;
    send_line(&mut pending.channel.writer, &Greeting::here()?)?;
    println!("Confirmation words: {}", pending.sas.as_str());
    println!("Check that the other machine shows the same words, and confirm there.");
    let answer: Answer = read_line(&mut pending.channel.reader)?;
    let (server_name, os, arch, capabilities) = match answer {
        Answer::Accepted {
            name: announced,
            os,
            arch,
            capabilities,
        } => (announced, os, arch, capabilities),
        Answer::Declined => return Err(ServeError::Declined),
    };
    let machine = name.cloned().unwrap_or(server_name);
    let key = *pending.channel.remote();
    let paired_at = Timestamp::observe();
    Trust::update(dirs, |trust| {
        trust.servers.insert(
            machine.clone(),
            Server {
                public_key: key,
                address: address.clone(),
                paired_at,
            },
        );
    })?;
    Ok(Paired {
        name: machine,
        address,
        key,
        os,
        arch,
        capabilities,
    })
}

fn verified_channel(
    dirs: &Dirs,
    machine: &MachineName,
    server: &Server,
) -> Result<secure::Channel, ServeError> {
    let identity = Identity::load_or_create(dirs)?;
    let stream = open(&server.address).map_err(|_unreachable| ServeError::Unreachable {
        machine: machine.clone(),
        address: server.address.clone(),
    })?;
    secure::connect(stream, &identity, &server.public_key).map_err(|error| match error {
        SecureError::Handshake => ServeError::Impersonation {
            machine: machine.clone(),
        },
        other @ (SecureError::Io(_)
        | SecureError::Unknown
        | SecureError::Frame(_)
        | SecureError::Invalid(_)
        | SecureError::PostQuantum(_)) => ServeError::Secure(other),
    })
}

pub fn tunnel(dirs: &Dirs, machine: &MachineName) -> Result<(), ServeError> {
    let trust = Trust::load(dirs)?;
    let server = trust
        .servers
        .get(machine)
        .ok_or_else(|| ServeError::NotPaired {
            machine: machine.clone(),
        })?;
    let channel = verified_channel(dirs, machine, server)?;
    let secure::Halves {
        mut reader,
        mut writer,
    } = channel.into_halves();
    std::thread::scope(|scope| {
        let upstream = scope.spawn(move || {
            let copied = std::io::copy(&mut std::io::stdin().lock(), &mut writer);
            copied.and_then(|_| writer.close())
        });
        let downstream = std::io::copy(&mut reader, &mut std::io::stdout().lock());
        match upstream.join() {
            Ok(Ok(())) => {}
            Ok(Err(error)) if error.kind() == std::io::ErrorKind::BrokenPipe => {}
            Ok(Err(error)) => return Err(ServeError::Io(error)),
            Err(_panicked) => return Err(ServeError::Refused("the sending thread panicked")),
        }
        downstream.map(drop).map_err(ServeError::Io)
    })
}

#[derive(Debug, Clone)]
pub struct Listing {
    pub servers: BTreeMap<MachineName, Server>,
    pub grants: Vec<Grant>,
}

pub fn listing(dirs: &Dirs) -> Result<Listing, ServeError> {
    let trust = Trust::load(dirs)?;
    Ok(Listing {
        servers: trust.servers,
        grants: trust.grants,
    })
}

pub fn revoke(dirs: &Dirs, who: &str) -> Result<usize, ServeError> {
    let removed = Trust::update(dirs, |trust| {
        let before = trust.grants.len().saturating_add(trust.servers.len());
        trust
            .grants
            .retain(|grant| grant.label.as_str() != who && grant.public_key.fingerprint() != who);
        trust
            .servers
            .retain(|name, server| name.as_str() != who && server.public_key.fingerprint() != who);
        before.saturating_sub(trust.grants.len().saturating_add(trust.servers.len()))
    })?;
    if removed == 0 {
        return Err(ServeError::Trust(TrustError::Unknown(who.to_owned())));
    }
    let event = crate::audit::Event {
        principal: "owner",
        action: "revoke",
        subject: Some(who.to_owned()),
        verdict: Verdict::Allowed,
    };
    AuditLog::at(dirs).record(event)?;
    Ok(removed)
}

impl crate::ingress::Ingress for Greeting {}
impl crate::ingress::Ingress for Answer {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exposures_are_explicit_and_tailnet_is_recognized() {
        assert_eq!(Exposure::parse("loopback").unwrap(), Exposure::Loopback);
        assert_eq!(
            Exposure::parse("10.0.0.2:4747").unwrap(),
            Exposure::Explicit("10.0.0.2:4747".parse().unwrap())
        );
        Exposure::parse("0.0.0.0").unwrap_err();
        assert!(is_tailnet("100.101.102.103".parse().unwrap()));
        assert!(!is_tailnet("100.128.0.1".parse().unwrap()));
        assert!(is_tailnet("fd7a:115c:a1e0::1".parse().unwrap()));
        assert_eq!(
            Exposure::Loopback.resolve(1).unwrap(),
            "127.0.0.1:1".parse().unwrap()
        );
        assert!(!Exposure::Tailnet.announces());
        assert_eq!(with_port("box"), format!("box:{DEFAULT_PORT}"));
        host_name().parse::<MachineName>().unwrap();
    }

    #[test]
    fn connections_are_bounded_overall_and_per_source() {
        let permits = Permits::default();
        let one: IpAddr = "10.0.0.1".parse().unwrap();
        for _ in 0..MAX_PER_SOURCE {
            permits.admit(one).unwrap();
        }
        permits.admit(one).unwrap_err();
        for n in 0..(MAX_CONNECTIONS - MAX_PER_SOURCE) {
            let other = IpAddr::from([10, 0, 1, u8::try_from(n % 250).unwrap()]);
            permits.admit(other).unwrap();
        }
        permits.admit("10.0.2.1".parse().unwrap()).unwrap_err();
        permits.release(one);
        permits.admit("10.0.2.1".parse().unwrap()).unwrap();
    }
}
