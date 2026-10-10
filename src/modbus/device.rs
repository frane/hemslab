//! One simulated Modbus device on the network: a register bank, a TCP server, its loop.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::{mpsc, oneshot, watch};

use super::bank::Bank;
use super::map::Map;
use super::server::{self, Write};
use super::sigenergy::{self, Plant};
use crate::profile::{ModbusReplayProfile, Profile, Snapshot};
use crate::view::{Control, Handle, View};

pub struct Spec {
    pub profile: Profile,
    pub instance: String,
    pub port: u16,
}

pub struct Started {
    pub instance: String,
    pub port: u16,
    pub handle: Arc<Handle>,
}

enum Sim {
    Plant(Plant),
    Replay(Replay),
}

const TICK: Duration = Duration::from_secs(1);

pub async fn run(spec: Spec, started: oneshot::Sender<Result<Started, String>>, mut shutdown: watch::Receiver<bool>) {
    let name = spec.instance.clone();
    let (bank, mut sim) = match build(&spec) {
        Ok(parts) => parts,
        Err(error) => {
            let _ = started.send(Err(format!("{name}: {error}")));
            return;
        }
    };
    let bank = Arc::new(Mutex::new(bank));
    let (writes_tx, mut writes) = mpsc::unbounded_channel();
    let bound = match server::serve(([0, 0, 0, 0], spec.port).into(), bank.clone(), writes_tx).await {
        Ok(bound) => bound,
        Err(error) => {
            let _ = started.send(Err(format!("{name}: {error}")));
            return;
        }
    };
    let (control, mut controls) = mpsc::unbounded_channel();
    let (kind, modes, status) = match &sim {
        Sim::Plant(_) => ("modbus", Plant::MODES.to_vec(), "starting"),
        Sim::Replay(_) => ("modbus-replay", Vec::new(), "replaying"),
    };
    let handle = Arc::new(Handle {
        instance: spec.instance.clone(),
        profile: spec.profile.name().to_string(),
        description: spec.profile.description().to_string(),
        kind,
        ski: String::new(),
        port: bound.port(),
        view: Mutex::new(View {
            status: status.into(),
            mode: "auto".into(),
            modes,
            direction: Some("production"),
            ..View::default()
        }),
        control,
    });
    let _ = started.send(Ok(Started {
        instance: spec.instance.clone(),
        port: bound.port(),
        handle: handle.clone(),
    }));

    let start = tokio::time::Instant::now();
    let mut ticker = tokio::time::interval(TICK);
    let mut last_mode: Option<Option<u16>> = None;
    loop {
        tokio::select! {
            _ = ticker.tick() => {}
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() { break; }
            }
            Some(write) = writes.recv() => {
                log_write(&handle, &bank, &write);
                continue;
            }
            Some(Control::Mode(mode)) = controls.recv() => {
                if let Sim::Plant(plant) = &mut sim && plant.set_sky(&mode) {
                    handle.log(format!("sky set to {mode}"));
                    handle.update(|v| v.mode = mode.clone());
                }
                continue;
            }
        }
        let Ok(mut bank) = bank.lock() else { break };
        match &mut sim {
            Sim::Plant(plant) => {
                let flows = plant.step(TICK, crate::generic::hour_now(), &mut bank);
                if last_mode != Some(flows.remote) {
                    match flows.remote {
                        Some(mode) => handle.log(format!("Remote EMS: {}", sigenergy::mode_name(mode))),
                        None => handle.log("local EMS: maximum self-consumption"),
                    }
                    last_mode = Some(flows.remote);
                }
                let pv_limit = bank.number("plant_pv_max_power_limit") * 1000.0;
                let limit = (pv_limit < bank.number("plant_max_active_power") * 1000.0).then_some(pv_limit);
                let battery = if flows.battery >= 0.0 {
                    format!("battery charging {:.1} kW", flows.battery / 1000.0)
                } else {
                    format!("battery discharging {:.1} kW", -flows.battery / 1000.0)
                };
                let grid = if flows.grid >= 0.0 {
                    format!("grid import {:.1} kW", flows.grid / 1000.0)
                } else {
                    format!("grid export {:.1} kW", -flows.grid / 1000.0)
                };
                let status = format!(
                    "PV {:.1} kW, house {:.1} kW, {battery}, {grid}, SOC {:.0} %",
                    flows.pv / 1000.0,
                    flows.load / 1000.0,
                    flows.soc * 100.0
                );
                handle.update(|v| {
                    v.status = status;
                    v.limit_watts = limit;
                    v.state = Some(match flows.remote {
                        Some(_) => "remote EMS".to_string(),
                        None => "local EMS".to_string(),
                    });
                });
                handle.sample(-flows.pv, limit);
            }
            Sim::Replay(replay) => {
                replay.advance(start.elapsed(), &mut bank);
                if let Some(pv) = bank.get("plant_sigen_photovoltaic_power").and_then(|v| v.number()) {
                    handle.sample(-pv * 1000.0, None);
                }
            }
        }
    }
    tracing::info!("{name}: stopped");
}

fn build(spec: &Spec) -> Result<(Bank, Sim), String> {
    match &spec.profile {
        Profile::Modbus(p) => {
            let map = Arc::new(Map::load(&p.map)?);
            let mut bank = Bank::new(map);
            let plant = Plant::new(p.sigenergy.clone());
            let serial = format!("HL{:08X}", fnv(&spec.instance));
            plant.initialise(&mut bank, &serial);
            Ok((bank, Sim::Plant(plant)))
        }
        Profile::ModbusReplay(p) => {
            let map = Arc::new(Map::load(&p.map)?);
            let mut bank = Bank::new(map);
            // Everything that was ever read is there from the start; the timeline then
            // moves the values as they moved.
            for snapshot in &p.snapshots {
                bank.write_words(snapshot.unit, snapshot.table, snapshot.address, &snapshot.words);
            }
            let mut replay = Replay::new(p);
            replay.advance(Duration::ZERO, &mut bank);
            Ok((bank, Sim::Replay(replay)))
        }
        _ => Err("not a Modbus profile".into()),
    }
}

fn fnv(text: &str) -> u32 {
    text.bytes().fold(0x811c_9dc5u32, |h, b| (h ^ b as u32).wrapping_mul(0x0100_0193))
}

fn log_write(handle: &Handle, bank: &Mutex<Bank>, write: &Write) {
    let Ok(bank) = bank.lock() else { return };
    let values: Vec<String> = write
        .registers
        .iter()
        .map(|name| match bank.get(name) {
            Some(super::map::Value::Number(n)) => {
                let unit = bank.map.register(name).and_then(|r| r.unit.clone()).unwrap_or_default();
                format!("{name} = {n}{}{unit}", if unit.is_empty() { "" } else { " " })
            }
            Some(super::map::Value::Text(t)) => format!("{name} = \"{t}\""),
            None => name.clone(),
        })
        .collect();
    handle.log(format!(
        "write unit {} register {}: {}",
        write.unit,
        write.address,
        if values.is_empty() { format!("{:?}", write.words) } else { values.join(", ") }
    ));
}

/// Recorded register blocks, played back in a loop.
pub struct Replay {
    snapshots: Vec<Snapshot>,
    period: f64,
    next: usize,
    lap: u64,
}

impl Replay {
    pub fn new(profile: &ModbusReplayProfile) -> Self {
        let mut snapshots = profile.snapshots.clone();
        snapshots.sort_by(|a, b| a.at.total_cmp(&b.at));
        let period = snapshots.last().map(|s| s.at).unwrap_or(0.0) + 5.0;
        Self { snapshots, period, next: 0, lap: 0 }
    }

    pub fn advance(&mut self, elapsed: Duration, bank: &mut Bank) {
        if self.snapshots.is_empty() {
            return;
        }
        let now = elapsed.as_secs_f64();
        let mut applied = 0;
        loop {
            if self.next == self.snapshots.len() {
                self.next = 0;
                self.lap += 1;
            }
            let s = &self.snapshots[self.next];
            if self.lap as f64 * self.period + s.at > now || applied > self.snapshots.len() {
                break;
            }
            bank.write_words(s.unit, s.table, s.address, &s.words);
            self.next += 1;
            applied += 1;
        }
    }
}
