//! A test energy manager inside hemslab, so the UI can set limits without a real HEMS.
//!
//! It is the `eebus` crate's Energy Guard (LPC) and, for producers, the same actor in the
//! LPP direction, on a CEM entity of its own. It connects to a device the first time
//! a limit is set for it, like any other energy manager would, over SHIP.
//!
//! The guard does the protocol: bindings, the heartbeat, learning the peer's `limitId`.
//! It also keeps to LPC §2.10's one write per five minutes, which is right for a grid
//! operator and wrong for someone trying things out, so the limit itself is written
//! directly as soon as the guard is ready — and handed to the guard as well, which will
//! repeat the same value later.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use eebus::cert::{self, CertParams};
use eebus::model::{
    AddressDevice, CmdData, DeviceType, EntityType, FeatureAddress, Filter, LoadControlLimitData,
    LoadControlLimitListData, MsgCounter, ScaledNumber,
};
use eebus::runtime::{Hub, HubEvent, Node, TrustStore};
use eebus::ship::Ski;
use eebus::spine::{Engine, LocalDevice, LocalEntity};
use eebus::tls::ShipTls;
use eebus::usecases::limitation::{self, EnergyGuardActor, GuardEvent, LimitWrite};
use eebus::usecases::{lpc, lpp};
use tokio::sync::{mpsc, watch};

use crate::view::{Handle, Registry};

pub enum Command {
    /// Set (`Some`) or release (`None`) a device's limit.
    Limit { instance: String, watts: Option<f64> },
}

/// What the UI shows about the test energy manager.
#[derive(Clone, Debug, Default, serde::Serialize)]
pub struct Status {
    pub ski: String,
    pub connected: Vec<String>,
}

pub struct Cem {
    pub commands: mpsc::UnboundedSender<Command>,
    pub status: Arc<Mutex<Status>>,
}

struct Peer {
    handle: Arc<Handle>,
    ski: Ski,
    device: Option<AddressDevice>,
    production: bool,
    ready: bool,
    pending: Option<Option<f64>>,
}

pub fn start(registry: Arc<Registry>, mut shutdown: watch::Receiver<bool>) -> Result<Cem, String> {
    let trust = TrustStore::new();
    let identity = cert::self_signed(CertParams::new("hemslab-test-cem")).map_err(|e| e.to_string())?;
    let node = Node::new("hemslab-test-cem", ShipTls::new(identity), trust.clone());
    let ski = node.ski();

    let mut device = LocalDevice::new("n:hemslab", "test-cem", DeviceType::EnergyManagementSystem)
        .map_err(|e| e.to_string())?;
    device
        .add_entity(
            LocalEntity::new([1], EntityType::CEM)
                .with_feature(limitation::client_feature(1))
                .with_feature(limitation::device_diagnosis_feature(2)),
        )
        .map_err(|e| e.to_string())?;
    let client = device.address_of(&[1], 1);
    let diagnosis = device.address_of(&[1], 2);
    let mut engine = Engine::new(device);
    engine.add_use_case([1], 1, &lpc::ENERGY_GUARD);
    engine.add_use_case([1], 1, &lpp::ENERGY_GUARD);

    let (commands, mut rx) = mpsc::unbounded_channel();
    let status = Arc::new(Mutex::new(Status {
        ski: ski.to_string(),
        connected: Vec::new(),
    }));
    let shared = status.clone();

    tokio::spawn(async move {
        let mut hub = Hub::new(node, engine);
        let now = hub.now();
        let mut consumption = EnergyGuardActor::new(lpc::DIRECTION, client.clone(), diagnosis.clone(), now);
        let mut production = EnergyGuardActor::new(lpp::DIRECTION, client.clone(), diagnosis, now);
        let mut peers: HashMap<String, Peer> = HashMap::new();
        let mut writes: HashMap<MsgCounter, (String, Option<f64>)> = HashMap::new();
        let client_address = client.clone();

        loop {
            if *shutdown.borrow_and_update() {
                break;
            }
            while let Ok(Command::Limit { instance, watts }) = rx.try_recv() {
                let Some(handle) = registry.get(&instance) else { continue };
                let Ok(device_ski) = handle.ski.parse::<Ski>() else { continue };
                let production_device = handle
                    .view
                    .lock()
                    .map(|v| v.direction == Some("production"))
                    .unwrap_or(false);
                let peer = peers.entry(instance.clone()).or_insert_with(|| {
                    trust.trust(device_ski);
                    hub.remember(device_ski, ([127, 0, 0, 1], handle.port).into());
                    handle.log("test energy manager: connecting");
                    Peer {
                        handle: handle.clone(),
                        ski: device_ski,
                        device: None,
                        production: production_device,
                        ready: false,
                        pending: None,
                    }
                });
                if peer.ready {
                    let guard = if peer.production { &mut production } else { &mut consumption };
                    if let Some(counter) = send(&mut hub, guard, &client_address, peer, watts) {
                        writes.insert(counter, (instance.clone(), watts));
                    }
                } else {
                    peer.pending = Some(watts);
                }
            }

            hub.wake_at(consumption.poll_timeout().min(production.poll_timeout()));
            hub.wake_at(hub.now() + Duration::from_millis(250));
            let now = hub.now();
            let event = match hub.next().await {
                Ok(event) => event,
                Err(_) => break,
            };
            let mut reports = Vec::new();
            if !matches!(event, HubEvent::Spine(_) | HubEvent::Tick) {
                tracing::debug!("test energy manager: {event:?}");
            }
            match event {
                HubEvent::PeerDiscovered { device, ski } => {
                    if let Some(peer) = peers.values_mut().find(|p| p.ski == ski) {
                        peer.device = Some(device.clone());
                        peer.ready = false;
                    }
                }
                HubEvent::Connected { ski, .. } => {
                    if let Some((name, _)) = peers.iter().find(|(_, p)| p.ski == ski)
                        && let Ok(mut status) = shared.lock()
                    {
                        status.connected.push(name.clone());
                    }
                }
                HubEvent::Disconnected { ski, .. } => {
                    if let Some((name, _)) = peers.iter().find(|(_, p)| p.ski == ski)
                        && let Ok(mut status) = shared.lock()
                    {
                        status.connected.retain(|c| c != name);
                    }
                }
                HubEvent::Spine(event) => {
                    if let eebus::spine::SpineEvent::ResultReceived { request, error } = &event
                        && let Some((instance, watts)) = writes.remove(request)
                        && let Some(peer) = peers.get(&instance)
                    {
                        let what = match watts {
                            Some(w) => format!("{w:.0} W"),
                            None => "release".to_string(),
                        };
                        if *error == eebus::model::ErrorNumber::None {
                            peer.handle.log(format!("test energy manager: {what} accepted"));
                        } else {
                            peer.handle.log(format!("test energy manager: {what} refused ({error:?})"));
                        }
                    }
                    reports.extend(consumption.handle_event(hub.engine_mut(), &event, now));
                    reports.extend(production.handle_event(hub.engine_mut(), &event, now));
                }
                HubEvent::Tick => {
                    reports.extend(consumption.handle_timeout(hub.engine_mut(), now));
                    reports.extend(production.handle_timeout(hub.engine_mut(), now));
                }
                _ => {}
            }
            for report in reports {
                tracing::debug!("test energy manager: {report:?}");
                match report {
                    GuardEvent::Ready { device } => {
                        let Some((instance, peer)) =
                            peers.iter_mut().find(|(_, p)| p.device.as_ref() == Some(&device))
                        else {
                            continue;
                        };
                        peer.ready = true;
                        if let Some(watts) = peer.pending.take() {
                            let guard = if peer.production { &mut production } else { &mut consumption };
                            if let Some(counter) = send(&mut hub, guard, &client_address, peer, watts) {
                                writes.insert(counter, (instance.clone(), watts));
                            }
                        }
                    }
                    GuardEvent::LimitRefused { device, limit, error, .. } => {
                        if let Some(peer) = peers.values().find(|p| p.device.as_ref() == Some(&device)) {
                            peer.handle.log(format!(
                                "test energy manager: {:.0} W refused ({error})",
                                limit.watts
                            ));
                        }
                    }
                    _ => {}
                }
            }
        }
        hub.shutdown(eebus::ship::ConnectionCloseReason::Unspecific).await;
    });

    Ok(Cem { commands, status })
}

/// Writes the limit now, and tells the guard it is what is required from here on.
fn send(
    hub: &mut Hub,
    guard: &mut EnergyGuardActor,
    client: &FeatureAddress,
    peer: &Peer,
    watts: Option<f64>,
) -> Option<MsgCounter> {
    let device = peer.device.as_ref()?;
    let now = hub.now();
    guard.require(device, Some(write(watts)), now);
    let limit_id = guard.peer_ids(device)?.limit?;
    let target = guard.peers().find(|p| &p.device == device)?.load_control.clone();
    let limit = write(watts);
    let data = CmdData::LoadControlLimitListData(LoadControlLimitListData {
        load_control_limit_data: Some(vec![LoadControlLimitData {
            limit_id: Some(limit_id),
            is_limit_active: Some(limit.is_active),
            value: Some(ScaledNumber::from_f64(limit.watts, 0)),
            ..Default::default()
        }]),
    });
    peer.handle.log(describe("test energy manager sends", watts));
    Some(hub.engine_mut().write_filtered(&target, client, data, vec![Filter::partial()], now))
}

fn write(watts: Option<f64>) -> LimitWrite {
    match watts {
        Some(w) => LimitWrite::active(w),
        None => LimitWrite::deactivated(),
    }
}

fn describe(prefix: &str, watts: Option<f64>) -> String {
    match watts {
        Some(w) => format!("{prefix} limit {w:.0} W"),
        None => format!("{prefix} release"),
    }
}
