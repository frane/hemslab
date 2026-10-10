//! The simulated Sigenergy over real Modbus TCP: read, steer, record, replay.

use std::time::Duration;

use hemslab::modbus::device::{self, Spec};
use hemslab::modbus::map::{Map, Value, decode};
use hemslab::profile::Profile;
use tokio::sync::{oneshot, watch};
use tokio_modbus::client::{Context, Reader, Writer, tcp};
use tokio_modbus::Slave;
use tokio_modbus::slave::SlaveContext;

async fn start(profile: Profile, port: u16) -> watch::Sender<bool> {
    let (tx, rx) = oneshot::channel();
    let (stop, stop_rx) = watch::channel(false);
    let spec = Spec { profile, instance: "sig-test".into(), port };
    tokio::spawn(device::run(spec, tx, stop_rx));
    rx.await.unwrap().unwrap();
    stop
}

async fn number(ctx: &mut Context, map: &Map, name: &str) -> f64 {
    let r = map.register(name).unwrap();
    ctx.set_slave(Slave(map.unit_of(r)));
    let words = match r.table {
        hemslab::modbus::map::Table::Input => ctx.read_input_registers(r.address, r.count).await,
        hemslab::modbus::map::Table::Holding => ctx.read_holding_registers(r.address, r.count).await,
    }
    .unwrap()
    .unwrap();
    decode(r, &words).number().unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn an_energy_manager_steers_the_simulated_sigenergy() {
    let stop = start(Profile::load("sigenergy").unwrap(), 47_820).await;
    let map = Map::load("sigenergy").unwrap();
    let mut ctx = tcp::connect(([127, 0, 0, 1], 47_820).into()).await.unwrap();

    let soc = number(&mut ctx, &map, "plant_ess_soc").await;
    assert!((40.0..=60.0).contains(&soc), "starts half full: {soc}");
    let model = map.register("inverter_model_type").unwrap();
    ctx.set_slave(Slave(1));
    let words = ctx.read_input_registers(model.address, model.count).await.unwrap().unwrap();
    assert_eq!(decode(model, &words), Value::Text("SigenStor (hemslab)".into()));

    // Remote EMS, charge from the grid at 2.5 kW — what an energy manager does at a
    // cheap hour.
    ctx.set_slave(Slave(247));
    ctx.write_single_register(40029, 1).await.unwrap().unwrap();
    ctx.write_single_register(40031, 3).await.unwrap().unwrap();
    ctx.write_multiple_registers(40032, &[0, 2500]).await.unwrap().unwrap();
    tokio::time::sleep(Duration::from_millis(2_200)).await;
    assert_eq!(number(&mut ctx, &map, "plant_ems_work_mode").await, 7.0);
    assert_eq!(number(&mut ctx, &map, "plant_ess_power").await, 2.5);

    // A read-only register cannot be written.
    let refused = ctx.write_single_register(30014, 1).await.unwrap();
    assert!(refused.is_err(), "input registers are read-only");
    let _ = stop.send(true);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_recorded_sigenergy_replays() {
    let stop = start(Profile::load("sigenergy").unwrap(), 47_821).await;
    let dir = tempfile::tempdir().unwrap();
    let capture = dir.path().join("sig.jsonl");
    let (_stop_rec, stop_rx) = watch::channel(false);
    let written = hemslab::modbus::record::record(
        hemslab::modbus::record::Options {
            address: ([127, 0, 0, 1], 47_821).into(),
            map: "sigenergy".into(),
            output: capture.clone(),
            interval: Duration::from_secs(1),
            duration: Some(Duration::from_secs(3)),
        },
        stop_rx,
    )
    .await
    .unwrap();
    assert!(written > 50, "{written} blocks");
    let _ = stop.send(true);

    let (profile, _) = hemslab::modbus::record::import(&capture, "sig-again", true).unwrap();
    assert_eq!(profile.model.as_deref(), Some("SigenStor (hemslab)"));
    let replay = start(Profile::ModbusReplay(profile), 47_822).await;
    let map = Map::load("sigenergy").unwrap();
    let mut ctx = tcp::connect(([127, 0, 0, 1], 47_822).into()).await.unwrap();
    let soc = number(&mut ctx, &map, "plant_ess_soc").await;
    assert!((40.0..=60.0).contains(&soc), "{soc}");
    let serial = map.register("inverter_serial_number").unwrap();
    ctx.set_slave(Slave(1));
    let words = ctx.read_input_registers(serial.address, serial.count).await.unwrap().unwrap();
    let Value::Text(serial) = decode(serial, &words) else { panic!() };
    assert!(serial.chars().all(|c| c == 'X'), "serial redacted: {serial}");
    let _ = replay.send(true);
}
