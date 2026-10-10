//! hemslab devices against the `eebus` crate's own energy-manager actors, over loopback.

use std::time::Duration;

use eebus::cert::{self, CertParams};
use eebus::model::{DeviceType, EntityType};
use eebus::runtime::{Hub, HubEvent, Node, TrustStore};
use eebus::spine::{Engine, LocalDevice, LocalEntity};
use eebus::tls::ShipTls;
use eebus::usecases::limitation::{self, EnergyGuardActor, GuardEvent, LimitWrite};
use eebus::usecases::monitoring::{self, Measurand, MonitoringApplianceActor, MonitoringEvent};
use eebus::usecases::{lpc, mpc};
use hemslab::device::{self, Spec, Started};
use hemslab::profile::Profile;
use tokio::sync::{oneshot, watch};

async fn start(profile: &str, port: u16, dir: &std::path::Path) -> (Started, watch::Sender<bool>) {
    let (tx, rx) = oneshot::channel();
    let (stop, stop_rx) = watch::channel(false);
    let spec = Spec {
        profile: Profile::load(profile).unwrap(),
        instance: format!("{profile}-test"),
        port,
        state_dir: dir.to_path_buf(),
        mdns: false,
        announce_ip: None,
        accept_all: true,
        trust: Vec::new(),
        dial: Vec::new(),
    };
    tokio::spawn(device::run(spec, tx, stop_rx));
    (rx.await.unwrap().unwrap(), stop)
}

fn energy_manager() -> (Hub, EnergyGuardActor, MonitoringApplianceActor) {
    let trust = TrustStore::new();
    let node = Node::new(
        "hemslab-test-cem",
        ShipTls::new(cert::self_signed(CertParams::new("hemslab-test-cem")).unwrap()),
        trust,
    );
    let mut device =
        LocalDevice::new("n:test", "CEM-1", DeviceType::EnergyManagementSystem).unwrap();
    device
        .add_entity(
            LocalEntity::new([1], EntityType::CEM)
                .with_feature(limitation::client_feature(1))
                .with_feature(limitation::device_diagnosis_feature(2)),
        )
        .unwrap();
    let client = device.address_of(&[1], 1);
    let diagnosis = device.address_of(&[1], 2);
    let mut engine = Engine::new(device);
    engine.add_use_case([1], 1, &lpc::ENERGY_GUARD);
    engine.add_use_case([1], 1, &mpc::MONITORING_APPLIANCE);
    let hub = Hub::new(node, engine);
    let guard = EnergyGuardActor::new(lpc::DIRECTION, client.clone(), diagnosis, hub.now());
    (hub, guard, MonitoringApplianceActor::new(client))
}

/// Dials the device, writes `limit`, and returns what came back: whether the limit was
/// accepted, and the first total power reading.
async fn exercise(started: &Started, limit: f64) -> (bool, Option<f64>, Vec<String>) {
    let (mut hub, mut guard, mut monitor) = energy_manager();
    hub.node().trust_store().trust(started.ski);
    hub.dial(([127, 0, 0, 1], started.port).into());
    let mut accepted = false;
    let mut power = None;
    let mut use_cases = Vec::new();
    for _ in 0..600 {
        let now = hub.now();
        hub.wake_at(guard.poll_timeout());
        hub.wake_at(now + Duration::from_millis(100));
        let reports = match hub.next().await.unwrap() {
            HubEvent::PeerDiscovered { device, .. } => {
                let remote = hub.engine().peer(&device).unwrap();
                use_cases = remote.use_cases.iter().map(|u| u.name.as_str().to_string()).collect();
                guard.require(&device, Some(LimitWrite::active(limit)), now);
                if let Some(unit) = monitoring::locate(remote, mpc::MONITORED_UNIT.name, mpc::MONITORED_UNIT.actor) {
                    monitor.attach(hub.engine_mut(), unit, now);
                }
                Vec::new()
            }
            HubEvent::Spine(event) => {
                if let Some(MonitoringEvent::Measured { readings, .. }) = monitor.handle_event(&event) {
                    for reading in readings {
                        if reading.measurand == Measurand::total_power() && reading.value.is_some() {
                            power = reading.value;
                        }
                    }
                }
                guard.handle_event(hub.engine_mut(), &event, now).into_iter().collect()
            }
            HubEvent::Tick => guard.handle_timeout(hub.engine_mut(), now),
            HubEvent::HandshakeFailed { error, .. } => panic!("handshake failed: {error}"),
            _ => Vec::new(),
        };
        for report in reports {
            match report {
                GuardEvent::LimitAccepted { limit: l, .. } if l.watts == limit => accepted = true,
                GuardEvent::LimitRefused { error, .. } => panic!("refused: {error}"),
                _ => {}
            }
        }
        if accepted && power.is_some() {
            break;
        }
    }
    hub.shutdown(eebus::ship::ConnectionCloseReason::Unspecific).await;
    (accepted, power, use_cases)
}

#[tokio::test(flavor = "multi_thread")]
async fn a_heat_pump_follows_a_limit_and_reports_its_power() {
    let dir = tempfile::tempdir().unwrap();
    let (started, stop) = start("heat-pump", 47_811, dir.path()).await;
    let (accepted, power, use_cases) = exercise(&started, 3_000.0).await;
    assert!(accepted, "the limit was accepted");
    assert!(use_cases.contains(&"limitationOfPowerConsumption".to_string()));
    let power = power.expect("MPC reported power");
    assert!((0.0..=6_200.0).contains(&power), "plausible power: {power}");
    let _ = stop.send(true);
}

#[tokio::test(flavor = "multi_thread")]
async fn the_replayed_elli_looks_like_an_elli_and_takes_a_limit() {
    let dir = tempfile::tempdir().unwrap();
    let (started, stop) = start("elli-charger-2", 47_812, dir.path()).await;
    let (accepted, power, use_cases) = exercise(&started, 4_200.0).await;
    assert!(use_cases.contains(&"evseCommissioningAndConfiguration".to_string()), "{use_cases:?}");
    assert!(accepted, "the replayed LoadControl accepted the write");
    assert!(power.is_some(), "recorded measurements were served");
    let _ = stop.send(true);
}

#[tokio::test(flavor = "multi_thread")]
async fn every_built_in_profile_starts() {
    let dir = tempfile::tempdir().unwrap();
    for (index, (name, _)) in hemslab::profile::BUILTIN.iter().enumerate() {
        let (tx, rx) = oneshot::channel();
        let (stop, stop_rx) = watch::channel(false);
        let spec = Spec {
            profile: Profile::load(name).unwrap(),
            instance: format!("{name}-start"),
            port: 47_830 + index as u16,
            state_dir: dir.path().to_path_buf(),
            mdns: false,
            announce_ip: None,
            accept_all: true,
            trust: Vec::new(),
            dial: Vec::new(),
        };
        let task = tokio::spawn(device::run(spec, tx, stop_rx));
        rx.await.unwrap().unwrap_or_else(|e| panic!("{name}: {e}"));
        let _ = stop.send(true);
        task.await.unwrap();
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn the_ui_sets_a_limit_through_the_test_energy_manager() {
    use std::sync::Arc;
    let dir = tempfile::tempdir().unwrap();
    let (_stop_tx, stop_rx) = watch::channel(false);
    let registry = Arc::new(hemslab::view::Registry::default());
    let cem = hemslab::cem::start(registry.clone(), stop_rx).unwrap();
    let (started, stop) = start("wallbox", 47_813, dir.path()).await;
    registry.add(started.handle.clone());
    let ui = hemslab::ui::serve(([127, 0, 0, 1], 0).into(), registry, cem).await.unwrap();

    // Plain HTTP, as the page does it.
    async fn http(ui: std::net::SocketAddr, request: String) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let mut socket = tokio::net::TcpStream::connect(ui).await.unwrap();
        socket.write_all(request.as_bytes()).await.unwrap();
        let mut out = String::new();
        socket.read_to_string(&mut out).await.unwrap();
        out
    }
    let body = r#"{"watts":3000}"#;
    let response = http(ui, format!(
        "POST /api/devices/wallbox-test/limit HTTP/1.1\r\nHost: x\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )).await;
    assert!(response.starts_with("HTTP/1.1 204"), "{response}");

    for _ in 0..100 {
        let state = http(ui, "GET /api/state HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n".into()).await;
        // 3 kW is below 6 A on three phases: the wallbox pauses.
        if state.contains(r#""limit_watts":3000.0"#)
            && state.contains(r#""state":"limited""#)
            && state.contains(r#""status":"paused by limit""#)
        {
            let _ = stop.send(true);
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("the limit never reached the wallbox");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_recorded_device_imports_as_the_same_device() {
    let dir = tempfile::tempdir().unwrap();
    let (started, stop) = start("elli-charger-2", 47_814, dir.path()).await;
    let capture = dir.path().join("elli.jsonl");
    let (_stop_rec, stop_rx) = watch::channel(false);
    let written = hemslab::record::record(
        hemslab::record::Options {
            ski: started.ski,
            address: Some(([127, 0, 0, 1], started.port).into()),
            state_dir: dir.path().join("recorder"),
            port: 47_815,
            output: capture.clone(),
            duration: Some(Duration::from_secs(5)),
        },
        stop_rx,
    )
    .await
    .unwrap();
    assert!(written > 10, "recorded {written} messages");

    let options = hemslab::capture::ImportOptions {
        name: "elli-again".into(),
        description: None,
        side: None,
        redact: true,
        max_timeline_s: 60.0,
    };
    let (profile, report) = hemslab::capture::import(&[capture.as_path()], &options).unwrap();
    assert_eq!(profile.brand.as_deref(), Some("Elli"));
    assert_eq!(report.use_cases, 4, "LPC, MPC, EVSECC and EV charging summary");
    assert_eq!(report.features, 10);
    let _ = stop.send(true);
}
