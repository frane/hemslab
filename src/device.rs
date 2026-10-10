//! One simulated device on the network: identity, listener, mDNS, and its loop.

use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use eebus::cert::{self, CertParams, Identity};
use eebus::model::CmdData;
use eebus::runtime::{Hub, HubEvent, Node, TrustStore, TrustedPeer};
use eebus::ship::{ShipId, ShipTxtRecord, Ski};
use eebus::spine::SpineEvent;
use eebus::tls::ShipTls;
use eebus::usecases::limitation::{ControllableSystemActor, LimitationState};
use std::sync::{Arc, Mutex};

use tokio::sync::{mpsc, watch};
use tracing::warn;

use crate::generic::{self, Behaviour, Meter};
use crate::profile::Profile;
use crate::replay::{self, Timeline};
use crate::view::{Control, Handle, View};
use crate::wire;

/// Everything a device needs to start.
pub struct Spec {
    pub profile: Profile,
    /// Unique per instance, e.g. `heat-pump-1`. Used for the state directory, the SPINE
    /// address of generic devices and the log.
    pub instance: String,
    pub port: u16,
    pub state_dir: PathBuf,
    pub mdns: bool,
    pub announce_ip: Option<IpAddr>,
    /// Pair with anyone who asks. A developer tool's default; `false` admits only
    /// `trust`.
    pub accept_all: bool,
    pub trust: Vec<Ski>,
    /// Energy managers to dial instead of waiting to be found.
    pub dial: Vec<Dial>,
}

/// An energy manager to dial.
#[derive(Clone, Copy, Debug)]
pub enum Dial {
    /// Known SKI: the hub keeps the connection up and redials when it drops.
    Remembered(Ski, std::net::SocketAddr),
    /// Address only: dialled again every ten seconds while nothing is connected.
    Address(std::net::SocketAddr),
}

impl std::str::FromStr for Dial {
    type Err = String;

    /// `host:port` or `SKI@host:port`.
    fn from_str(text: &str) -> Result<Self, String> {
        use std::net::ToSocketAddrs;
        let (ski, address) = match text.split_once('@') {
            Some((ski, address)) => (Some(ski), address),
            None => (None, text),
        };
        let address = address
            .to_socket_addrs()
            .ok()
            .and_then(|mut found| found.next())
            .ok_or_else(|| format!("`{address}` is not host:port"))?;
        match ski {
            Some(ski) => Ok(Dial::Remembered(
                ski.parse().map_err(|e| format!("`{ski}`: {e}"))?,
                address,
            )),
            None => Ok(Dial::Address(address)),
        }
    }
}

/// What a running device reports to whoever started it.
#[derive(Clone, Debug)]
pub struct Started {
    pub instance: String,
    pub ski: Ski,
    pub port: u16,
    pub ship_id: String,
    pub handle: Arc<Handle>,
}

#[allow(clippy::large_enum_variant)] // one per device, never moved in a hot path
enum Sim {
    Generic {
        actor: ControllableSystemActor,
        meter: Option<Meter>,
        behaviour: Behaviour,
        last_limit: Option<(LimitationState, Option<f64>)>,
        last_status: &'static str,
    },
    Replay {
        timeline: Timeline,
    },
}

const TICK: Duration = Duration::from_secs(1);
const REDIAL: Duration = Duration::from_secs(10);

pub async fn run(
    spec: Spec,
    started: tokio::sync::oneshot::Sender<Result<Started, String>>,
    mut shutdown: watch::Receiver<bool>,
) {
    let name = spec.instance.clone();
    let result = prepare(spec).await;
    let (mut hub, mut sim, trust, store, info, _mdns, dial, mut controls) = match result {
        Ok(parts) => parts,
        Err(error) => {
            let _ = started.send(Err(format!("{name}: {error}")));
            return;
        }
    };
    let handle = info.handle.clone();
    let _ = started.send(Ok(info));
    let log = |text: String| handle.log(text);
    let accept_all = store.accept_all;

    let redial: Vec<_> = dial
        .iter()
        .filter_map(|d| match d {
            Dial::Address(address) => Some(*address),
            Dial::Remembered(..) => None,
        })
        .collect();
    let mut connected = 0usize;
    let mut next_redial = hub.now() + REDIAL;
    let mut next_tick = hub.now();
    loop {
        if *shutdown.borrow_and_update() {
            break;
        }
        while let Ok(control) = controls.try_recv() {
            match control {
                Control::Mode(mode) => {
                    if let Sim::Generic { behaviour, .. } = &mut sim
                        && behaviour.set_mode(&mode)
                    {
                        log(format!("mode set to {mode}"));
                        handle.update(|v| v.mode = mode.clone());
                    }
                }
            }
        }
        let now = hub.now();
        // The physics and the replay run on a one-second tick.
        if now >= next_tick {
            next_tick = now + TICK;
            step(&handle, &mut hub, &mut sim, now);
        }
        if connected == 0 && !redial.is_empty() && now >= next_redial {
            next_redial = now + REDIAL;
            for address in &redial {
                hub.dial(*address);
            }
        }
        match &sim {
            Sim::Generic { actor, .. } => hub.wake_at(actor.poll_timeout()),
            Sim::Replay { timeline } => {
                if let Some(due) = timeline.next_due() {
                    hub.wake_at(due);
                }
            }
        }
        // `Hub::next` is not cancel-safe: a deadline, not a select, keeps shutdown snappy.
        hub.wake_at(next_tick.min(now + Duration::from_millis(250)));

        let event = match hub.next().await {
            Ok(event) => event,
            Err(error) => {
                warn!("{name}: the hub stopped: {error}");
                break;
            }
        };
        let now = hub.now();
        match event {
            HubEvent::TrustRequested { peer, origin } => {
                let ski = peer.ski;
                if accept_all {
                    log(format!("paired with {ski} ({origin})"));
                    hub.approve(ski);
                    trust.remember(TrustedPeer::new(ski));
                    store.save_trust(&trust);
                } else {
                    log(format!("refused {ski} ({origin}); start with --trust {ski} to admit it"));
                    hub.refuse(ski);
                }
            }
            HubEvent::Connected { ski, .. } => {
                connected += 1;
                log(format!("{ski} connected"));
                handle.update(|v| v.peers.push(ski.to_string()));
                store.save_trust(&trust);
            }
            HubEvent::Disconnected { ski, reason } => {
                connected = connected.saturating_sub(1);
                log(format!("{ski} disconnected ({reason})"));
                let gone = ski.to_string();
                handle.update(|v| {
                    if let Some(i) = v.peers.iter().position(|p| *p == gone) {
                        v.peers.remove(i);
                    }
                });
            }
            HubEvent::HandshakeFailed { origin, error, .. } => {
                tracing::debug!("{name}: connection {origin} failed: {error}");
            }
            HubEvent::PeerDiscovered { device, ski } => {
                log(format!("{ski} is {}", device.as_str()));
            }
            HubEvent::PeerKeysUpdated { .. } => store.save_trust(&trust),
            HubEvent::Spine(event) => {
                log_write(&handle, &event, hub.engine(), matches!(sim, Sim::Replay { .. }));
                if let Sim::Generic { actor, .. } = &mut sim {
                    actor.handle_event(hub.engine_mut(), &event, now);
                }
            }
            HubEvent::Tick => {
                if let Sim::Generic { actor, .. } = &mut sim {
                    actor.handle_timeout(hub.engine_mut(), now);
                }
            }
            _ => {}
        }
        if let Sim::Generic { actor, last_limit, .. } = &mut sim {
            report_limit(&handle, actor, last_limit);
        }
    }
    hub.shutdown(eebus::ship::ConnectionCloseReason::Unspecific).await;
    tracing::info!("{name}: stopped");
}

type Prepared = (
    Hub,
    Sim,
    TrustStore,
    Store,
    Started,
    Option<eebus::mdns::Mdns>,
    Vec<Dial>,
    mpsc::UnboundedReceiver<Control>,
);

async fn prepare(spec: Spec) -> Result<Prepared, String> {
    if matches!(spec.profile, Profile::Modbus(_) | Profile::ModbusReplay(_)) {
        return Err("a Modbus profile runs through hemslab::modbus::device".into());
    }
    let store = Store::open(&spec.state_dir.join(&spec.instance), spec.accept_all)?;
    let ship_id: ShipId = format!("hemslab-{}", spec.instance)
        .parse()
        .map_err(|e| format!("SHIP ID: {e}"))?;
    let identity = store.identity(ship_id.as_str())?;
    let ski = identity.ski;
    let trust = store.trust();
    for peer in &spec.trust {
        trust.remember(TrustedPeer::new(*peer));
    }

    let serial = format!("HL{}", ski.to_string()[..8].to_uppercase());
    let (engine, pending, brand, model, device_type) = match &spec.profile {
        Profile::Generic(profile) => {
            let built = generic::build(profile, &spec.instance, &serial)?;
            (
                built.engine,
                Pending::Generic(built.actor, built.meter, built.behaviour),
                profile.device.brand.clone(),
                profile.device.model.clone(),
                profile.device.device_type.clone(),
            )
        }
        Profile::Modbus(_) | Profile::ModbusReplay(_) => unreachable!("checked above"),
        Profile::Replay(profile) => {
            let built = replay::build(profile)?;
            let device_type = built.engine.device().device_type().as_str().to_string();
            (
                built.engine,
                Pending::Replay(built.timeline),
                profile.brand.clone().unwrap_or_else(|| "hemslab".into()),
                profile.model.clone().unwrap_or_else(|| profile.name.clone()),
                device_type,
            )
        }
    };

    let node = Node::new(ship_id.as_str(), ShipTls::new(identity), trust.clone());
    let mut hub = Hub::new(node, engine);
    let bound = hub
        .listen(("0.0.0.0", spec.port))
        .await
        .map_err(|e| format!("cannot listen on port {}: {e}", spec.port))?;

    let mdns = if spec.mdns {
        let record = ShipTxtRecord::new(ship_id.clone(), ski)
            .with_brand(&txt_value(&brand))
            .with_model(&txt_value(&model))
            .with_device_type(&txt_value(&device_type))
            .with_serial(&serial);
        let mut mdns = eebus::mdns::Mdns::new().map_err(|e| format!("mDNS: {e}"))?;
        mdns.announce(
            ship_id.as_str(),
            &record,
            bound.port(),
            &announce_addresses(spec.announce_ip),
        )
        .map_err(|e| format!("mDNS: {e}"))?;
        let _ = hub.browse(&mdns);
        Some(mdns)
    } else {
        None
    };

    for dial in &spec.dial {
        match *dial {
            Dial::Remembered(ski, address) => {
                trust.remember(TrustedPeer::new(ski));
                hub.remember(ski, address);
            }
            Dial::Address(address) => hub.dial(address),
        }
    }

    let now = hub.now();
    let sim = match pending {
        Pending::Generic(actor, meter, behaviour) => Sim::Generic {
            actor: actor.install(hub.engine_mut(), now),
            meter,
            behaviour,
            last_limit: None,
            last_status: "",
        },
        Pending::Replay(timeline) => Sim::Replay { timeline },
    };
    let (control, controls) = mpsc::unbounded_channel();
    let mut initial = View {
        status: "starting".into(),
        mode: "auto".into(),
        ..View::default()
    };
    if let Sim::Generic { behaviour, .. } = &sim
        && let Profile::Generic(p) = &spec.profile
    {
        initial.modes = behaviour.modes().to_vec();
        initial.direction = Some(match p.limit.use_case {
            crate::profile::LimitUseCase::Lpc => "consumption",
            crate::profile::LimitUseCase::Lpp => "production",
        });
        initial.failsafe_watts = Some(p.limit.failsafe_watts);
        initial.nominal_max_watts = Some(p.limit.nominal_max_watts);
    } else {
        initial.status = "replaying".into();
    }
    let handle = Arc::new(Handle {
        instance: spec.instance.clone(),
        profile: spec.profile.name().to_string(),
        description: spec.profile.description().to_string(),
        kind: match spec.profile {
            Profile::Generic(_) => "generic",
            _ => "replay",
        },
        ski: ski.to_string(),
        port: bound.port(),
        view: Mutex::new(initial),
        control,
    });
    let started = Started {
        instance: spec.instance.clone(),
        ski,
        port: bound.port(),
        ship_id: ship_id.as_str().to_string(),
        handle,
    };
    Ok((hub, sim, trust, store, started, mdns, spec.dial, controls))
}

#[allow(clippy::large_enum_variant)]
enum Pending {
    Generic(
        eebus::usecases::limitation::ControllableSystemBuilder,
        Option<Meter>,
        Behaviour,
    ),
    Replay(Timeline),
}

/// A TXT value as SHIP allows it: no whitespace, `;` or control characters, ≤ 32 chars.
fn txt_value(text: &str) -> String {
    text.chars()
        .map(|ch| if ch == ';' || ch.is_control() || ch.is_whitespace() { '-' } else { ch })
        .take(32)
        .collect()
}

/// The configured address, or the one the default route leaves by.
pub fn announce_addresses(configured: Option<IpAddr>) -> Vec<IpAddr> {
    if let Some(ip) = configured {
        return vec![ip];
    }
    for target in ["224.0.0.251:5353", "192.0.2.1:9"] {
        if let Ok(socket) = std::net::UdpSocket::bind("0.0.0.0:0")
            && socket.connect(target).is_ok()
            && let Ok(local) = socket.local_addr()
            && !local.ip().is_unspecified()
        {
            return vec![local.ip()];
        }
    }
    vec![IpAddr::from([127, 0, 0, 1])]
}

/// Persisted identity and trust, one directory per device instance.
struct Store {
    dir: PathBuf,
    accept_all: bool,
}

impl Store {
    fn open(dir: &Path, accept_all: bool) -> Result<Self, String> {
        std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        Ok(Self {
            dir: dir.to_path_buf(),
            accept_all,
        })
    }

    fn identity(&self, ship_id: &str) -> Result<Identity, String> {
        let path = self.dir.join("identity.key.pem");
        if let Ok(pem) = std::fs::read_to_string(&path) {
            let key = cert::key_from_pem(&pem).map_err(|e| format!("{}: {e}", path.display()))?;
            return cert::self_signed_with(CertParams::new(ship_id), key).map_err(|e| e.to_string());
        }
        let identity = cert::self_signed(CertParams::new(ship_id)).map_err(|e| e.to_string())?;
        std::fs::write(&path, identity.key_pem()).map_err(|e| format!("{}: {e}", path.display()))?;
        Ok(identity)
    }

    fn trust(&self) -> TrustStore {
        std::fs::read_to_string(self.dir.join("trust.json"))
            .ok()
            .and_then(|json| TrustStore::from_json(&json).ok())
            .unwrap_or_default()
    }

    fn save_trust(&self, trust: &TrustStore) {
        if let Ok(json) = trust.to_json() {
            let _ = std::fs::write(self.dir.join("trust.json"), json);
        }
    }
}

fn step(handle: &Handle, hub: &mut Hub, sim: &mut Sim, now: Duration) {
    match sim {
        Sim::Generic {
            actor,
            meter,
            behaviour,
            last_status,
            ..
        } => {
            let limit = generic::limit_of(actor);
            let reading = behaviour.step(TICK, limit, generic::hour_now());
            if let Some(meter) = meter {
                generic::publish(meter, behaviour, reading, hub.engine_mut(), now);
            }
            if *last_status != reading.status {
                handle.log(format!("{} ({:.0} W)", reading.status, reading.watts));
                *last_status = reading.status;
                handle.update(|v| v.status = reading.status.to_string());
            }
            handle.sample(reading.watts, limit);
        }
        Sim::Replay { timeline } => {
            timeline.advance(hub.engine_mut(), now);
            if let Some(watts) = replay::total_power(hub.engine()) {
                handle.sample(watts, None);
            }
        }
    }
}

fn report_limit(
    handle: &Handle,
    actor: &ControllableSystemActor,
    last: &mut Option<(LimitationState, Option<f64>)>,
) {
    let state = actor.system().state();
    let limit = generic::limit_of(actor);
    if *last != Some((state, limit)) {
        match limit {
            Some(w) => handle.log(format!("{} — limit {w:.0} W", state.as_str())),
            None => handle.log(format!("{} — no limit", state.as_str())),
        }
        handle.update(|v| {
            v.state = Some(state.as_str().to_string());
            v.limit_watts = limit;
            v.failsafe_watts = Some(actor.system().config().failsafe_watts);
        });
        *last = Some((state, limit));
    }
}

/// What an energy manager wrote, in plain JSON, for the developer watching.
///
/// Generic devices decide on their writes (`WriteRequested`, which carries the data);
/// replayed devices store them directly (`DataWritten`), so the data is read back.
fn log_write(handle: &Handle, event: &SpineEvent, engine: &eebus::spine::Engine, replay: bool) {
    match event {
        SpineEvent::WriteRequested(request) => {
            handle.log(format!("write to {}: {}", describe(&request.feature), plain(&request.data)));
        }
        SpineEvent::DataWritten { feature, function, .. } if replay => {
            let data = engine
                .device()
                .resolve(feature)
                .and_then(|f| f.data(function))
                .map(plain)
                .unwrap_or_default();
            handle.log(format!("write to {}: {data}", describe(feature)));
        }
        _ => {}
    }
}

fn describe(feature: &eebus::model::FeatureAddress) -> String {
    let entity: Vec<String> = feature
        .entity
        .iter()
        .flatten()
        .map(|e| e.get().to_string())
        .collect();
    format!(
        "[{}]/{}",
        entity.join(","),
        feature.feature.map(|f| f.get()).unwrap_or(0)
    )
}

fn plain(data: &CmdData) -> String {
    serde_json::to_value(data)
        .map(|v| wire::to_plain(&v).to_string())
        .unwrap_or_default()
}

