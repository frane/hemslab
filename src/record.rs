//! Recording a real device: hemslab connects to it as an energy manager, reads everything
//! it serves, subscribes to every feature, and writes what comes back to a capture file.
//!
//! The capture has the same shape as `elli-eebus-proxy`'s `traffic.jsonl`, so
//! [`crate::capture::import`] turns either into a replay profile.

use std::collections::HashMap;
use std::io::Write;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use eebus::cert::{self, CertParams};
use eebus::model::{
    AddressDevice, Cmd, CmdClassifier, CmdData, Datagram, DeviceType, EntityType, FeatureAddress,
    FeatureType, Function, Header, Payload, Role,
};
use eebus::runtime::{Hub, HubEvent, Node, TrustStore, TrustedPeer};
use eebus::ship::{ShipId, ShipTxtRecord, Ski};
use eebus::spine::{Engine, LocalDevice, LocalEntity, LocalFeature, SpineEvent};
use eebus::tls::ShipTls;
use eebus::usecases::descriptor::UseCaseDescriptor;
use eebus::usecases::emobility::{evcc, evcem, evcs, evsecc, evsoc, opev, oscev};
use eebus::usecases::hvac::{cdsf, cdt, crcsf, crct, crhsf, crht, mdsf, mdt, mot, mrcsf, mrhsf, mrt};
use eebus::usecases::{cob, lpc, lpp, mgcp, mob, moi, mpc, mps, ohpcf};
use tokio::sync::watch;
use tracing::{info, warn};

/// Every client actor the crate knows. Some devices only serve a use case to an energy
/// manager that announces its counterpart, so the recorder announces all of them.
const CLIENT_USE_CASES: &[&UseCaseDescriptor] = &[
    &lpc::ENERGY_GUARD,
    &lpp::ENERGY_GUARD,
    &mpc::MONITORING_APPLIANCE,
    &mgcp::MONITORING_APPLIANCE,
    &moi::MONITORING_APPLIANCE,
    &mob::MONITORING_APPLIANCE,
    &mps::MONITORING_APPLIANCE,
    &cob::CEM,
    &ohpcf::CEM,
    &evsecc::CEM,
    &evcc::CEM,
    &evcem::ENERGY_GUARD,
    &evcs::CEM,
    &evsoc::MONITORING_APPLIANCE,
    &opev::ENERGY_GUARD,
    &oscev::CEM,
    &cdsf::CONFIGURATION_APPLIANCE,
    &cdt::CONFIGURATION_APPLIANCE,
    &crcsf::CONFIGURATION_APPLIANCE,
    &crct::CONFIGURATION_APPLIANCE,
    &crhsf::CONFIGURATION_APPLIANCE,
    &crht::CONFIGURATION_APPLIANCE,
    &mdsf::MONITORING_APPLIANCE,
    &mdt::MONITORING_APPLIANCE,
    &mot::MONITORING_APPLIANCE,
    &mrcsf::MONITORING_APPLIANCE,
    &mrhsf::MONITORING_APPLIANCE,
    &mrt::MONITORING_APPLIANCE,
];

/// A client feature per feature type a device may serve: SPINE matches subscriptions
/// and bindings by type, so reading a `Measurement` server wants a `Measurement` client.
const CLIENT_TYPES: &[&str] = &[
    "ActuatorLevel",
    "ActuatorSwitch",
    "Alarm",
    "Bill",
    "DataTunneling",
    "DeviceClassification",
    "DeviceConfiguration",
    "DeviceDiagnosis",
    "DirectControl",
    "ElectricalConnection",
    "HVAC",
    "Identification",
    "IncentiveTable",
    "LoadControl",
    "Measurement",
    "Messaging",
    "OperatingConstraints",
    "PowerSequences",
    "Sensing",
    "Setpoint",
    "SmartEnergyManagementPs",
    "StateInformation",
    "SupplyCondition",
    "TariffInformation",
    "TaskManagement",
    "Threshold",
    "TimeInformation",
    "TimeSeries",
    "TimeTable",
];

pub struct Options {
    pub ski: Ski,
    /// Where the device is; `None` finds it via mDNS.
    pub address: Option<SocketAddr>,
    pub state_dir: PathBuf,
    pub port: u16,
    pub output: PathBuf,
    /// Stop after this long; `None` until Ctrl-C.
    pub duration: Option<Duration>,
}

/// Records until the duration is up or `stop` turns true. Returns how many messages
/// were written.
pub async fn record(options: Options, mut stop: watch::Receiver<bool>) -> Result<usize, String> {
    std::fs::create_dir_all(&options.state_dir).map_err(|e| e.to_string())?;
    let identity = load_identity(&options.state_dir)?;
    let our_ski = identity.ski;
    let trust = TrustStore::new();
    trust.remember(TrustedPeer::new(options.ski));
    let ship_id: ShipId = "hemslab-recorder".parse().map_err(|e| format!("{e}"))?;
    let node = Node::new(ship_id.as_str(), ShipTls::new(identity), trust.clone());

    let mut device = LocalDevice::new("n:hemslab", "recorder", DeviceType::EnergyManagementSystem)
        .map_err(|e| e.to_string())?;
    let mut entity = LocalEntity::new([1], EntityType::CEM);
    let mut clients = HashMap::new();
    for (index, name) in CLIENT_TYPES.iter().enumerate() {
        let feature_type: FeatureType = serde_json::from_value(serde_json::Value::String((*name).into()))
            .map_err(|_| format!("feature type {name}"))?;
        let address = index as u32 + 1;
        entity = entity.with_feature(LocalFeature::new(address, feature_type, Role::Client));
        clients.insert(name.to_string(), address);
    }
    let generic = CLIENT_TYPES.len() as u32 + 1;
    entity = entity.with_feature(LocalFeature::new(generic, FeatureType::Generic, Role::Client));
    device.add_entity(entity).map_err(|e| e.to_string())?;
    let mut engine = Engine::new(device);
    for descriptor in CLIENT_USE_CASES {
        engine.add_use_case([1], generic, descriptor);
    }

    let mut hub = Hub::new(node, engine);
    let bound = hub
        .listen(("0.0.0.0", options.port))
        .await
        .map_err(|e| format!("cannot listen on port {}: {e}", options.port))?;

    // Announced, so the device can show it for pairing; browsed, to find the device.
    let mut mdns = eebus::mdns::Mdns::new().map_err(|e| format!("mDNS: {e}"))?;
    let record = ShipTxtRecord::new(ship_id.clone(), our_ski)
        .with_brand("hemslab")
        .with_model("recorder")
        .with_device_type("EnergyManagementSystem");
    let _ = mdns.announce(ship_id.as_str(), &record, bound.port(), &crate::device::announce_addresses(None));
    match options.address {
        Some(address) => {
            hub.remember(options.ski, address);
        }
        None => {
            hub.browse(&mdns).map_err(|e| format!("mDNS: {e}"))?;
        }
    }

    println!("hemslab recorder, SKI {our_ski}");
    println!(
        "Pair \"hemslab-recorder\" in the device's EEBUS settings if you have not yet. Waiting for {}…",
        options.ski
    );

    let mut out = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&options.output)
        .map_err(|e| format!("{}: {e}", options.output.display()))?;
    let mut written = 0usize;
    let started = hub.now();
    let mut device_address: Option<AddressDevice> = None;
    let mut asked = false;

    loop {
        if *stop.borrow_and_update() {
            break;
        }
        if let (Some(limit), Some(_)) = (options.duration, &device_address)
            && hub.now().saturating_sub(started) >= limit
        {
            break;
        }
        hub.wake_at(hub.now() + Duration::from_millis(500));
        let now = hub.now();
        let event = match hub.next().await {
            Ok(event) => event,
            Err(error) => return Err(format!("the hub stopped: {error}")),
        };
        match event {
            HubEvent::Connected { ski, .. } if ski == options.ski => {
                info!("connected to {ski}");
            }
            HubEvent::TrustRequested { peer, .. } if peer.ski == options.ski => hub.approve(peer.ski),
            HubEvent::TrustRequested { peer, .. } => hub.refuse(peer.ski),
            HubEvent::HandshakeFailed { error, .. } => {
                warn!("connection failed: {error} (has the device paired hemslab-recorder?)")
            }
            HubEvent::Disconnected { ski, reason } if ski == options.ski => {
                warn!("the device went away: {reason}");
                device_address = None;
                asked = false;
            }
            HubEvent::PeerDiscovered { device, ski } if ski == options.ski => {
                info!("{ski} is {}", device.as_str());
                device_address = Some(device);
            }
            HubEvent::Spine(event) => {
                let ski = options.ski.to_string();
                let mut line = |feature: &FeatureAddress, data: &CmdData, classifier: CmdClassifier| {
                    if write_line(&mut out, &ski, feature, data, classifier).is_ok() {
                        written += 1;
                    }
                };
                match &event {
                    SpineEvent::ReplyReceived { feature, resolved, .. } => {
                        line(feature, resolved, CmdClassifier::Reply)
                    }
                    SpineEvent::DataNotified { feature, resolved, .. } => {
                        line(feature, resolved, CmdClassifier::Notify)
                    }
                    SpineEvent::UseCasesUpdated { device } | SpineEvent::DiscoveryUpdated { device } => {
                        // The two node-management answers are kept merged by the engine;
                        // they go to the capture whole, as a reply from feature 0.
                        let node = FeatureAddress {
                            device: Some(device.clone()),
                            entity: Some(vec![0.into()]),
                            feature: Some(0.into()),
                        };
                        for function in [
                            Function::NodeManagementDetailedDiscoveryData,
                            Function::NodeManagementUseCaseData,
                        ] {
                            if let Some(data) = hub.engine().remote_data(&node, &function).cloned() {
                                line(&node, &data, CmdClassifier::Reply);
                            }
                        }
                    }
                    _ => {}
                }
            }
            _ => {}
        }

        // Once the device has described itself, read everything and subscribe to it.
        if !asked
            && let Some(device) = device_address.clone()
            && hub
                .engine()
                .peer(&device)
                .is_some_and(|p| !p.use_cases.is_empty() || p.entities.len() > 1)
        {
            asked = true;
            let requests = read_everything(&mut hub, &device, &clients, generic, now);
            info!("reading {requests} functions and subscribing to every feature; Ctrl-C stops");
        }
        if written > 0 && written.is_multiple_of(100) {
            let _ = out.flush();
        }
    }
    let _ = out.flush();
    hub.shutdown(eebus::ship::ConnectionCloseReason::Unspecific).await;
    Ok(written)
}

fn read_everything(
    hub: &mut Hub,
    device: &AddressDevice,
    clients: &HashMap<String, u32>,
    generic: u32,
    now: Duration,
) -> usize {
    let Some(remote) = hub.engine().peer(device).cloned() else {
        return 0;
    };
    let mut count = 0;
    for entity in &remote.entities {
        for feature in &entity.features {
            if feature.role != Role::Server {
                continue;
            }
            let client_feature = clients
                .get(feature.feature_type.as_str())
                .copied()
                .unwrap_or(generic);
            let client = hub.engine().device().address_of(&[1], client_feature);
            let _ = hub.engine_mut().request_subscription(&client, &feature.address, now);
            for property in &feature.functions {
                let readable = property
                    .possible_operations
                    .as_ref()
                    .is_some_and(|o| o.read.is_some());
                if let (true, Some(function)) = (readable, property.function.clone()) {
                    hub.engine_mut().read(&feature.address, &client, function, now);
                    count += 1;
                }
            }
        }
    }
    count
}

fn write_line(
    out: &mut std::fs::File,
    ski: &str,
    feature: &FeatureAddress,
    data: &CmdData,
    classifier: CmdClassifier,
) -> std::io::Result<()> {
    let datagram = Datagram {
        header: Some(Header {
            address_source: Some(feature.clone()),
            cmd_classifier: Some(classifier),
            ..Default::default()
        }),
        payload: Some(Payload {
            cmd: Some(vec![Cmd::with_data(data.clone())]),
        }),
    };
    let msg = eebus::model::to_json_value(&datagram).map_err(std::io::Error::other)?;
    let line = serde_json::json!({
        "t": chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
        "side": "device",
        "ski": ski,
        "dir": "in",
        "msg": msg,
    });
    writeln!(out, "{line}")
}

fn load_identity(dir: &Path) -> Result<eebus::cert::Identity, String> {
    let path = dir.join("recorder.key.pem");
    if let Ok(pem) = std::fs::read_to_string(&path) {
        let key = cert::key_from_pem(&pem).map_err(|e| e.to_string())?;
        return cert::self_signed_with(CertParams::new("hemslab-recorder"), key).map_err(|e| e.to_string());
    }
    let identity = cert::self_signed(CertParams::new("hemslab-recorder")).map_err(|e| e.to_string())?;
    std::fs::write(&path, identity.key_pem()).map_err(|e| e.to_string())?;
    Ok(identity)
}

/// What `hemslab discover` prints: every SHIP node announcing itself.
pub fn discover(seconds: u64) -> Result<Vec<eebus::mdns::Discovered>, String> {
    let mdns = eebus::mdns::Mdns::new().map_err(|e| format!("mDNS: {e}"))?;
    let browse = mdns.browse().map_err(|e| format!("mDNS: {e}"))?;
    let deadline = std::time::Instant::now() + Duration::from_secs(seconds);
    let mut found: Vec<eebus::mdns::Discovered> = Vec::new();
    while let Some(left) = deadline.checked_duration_since(std::time::Instant::now()) {
        if let Some(eebus::mdns::BrowseEvent::Found(node)) = browse.recv_timeout(left) {
            found.retain(|n| n.instance != node.instance);
            found.push(node);
        }
    }
    Ok(found)
}
