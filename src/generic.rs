//! Generic devices: the use cases from the `eebus` crate, numbers from a behaviour model.

use std::time::Duration;

use chrono::Timelike;
use eebus::model::{
    CmdData, DeviceClassificationManufacturerData, DeviceType, ElectricalConnectionPhaseName as Phase,
    EntityType, FeatureAddress, FeatureType, Function, Role,
};
use eebus::spine::{Engine, LocalDevice, LocalEntity, LocalFeature, Operations};
use eebus::usecases::limitation::{
    self, ControllableSystem, ControllableSystemActor, ControllableSystemBuilder, CsConfig,
    CsFeatures, EffectiveLimit,
};
use eebus::usecases::monitoring::{Measurand, MonitoredUnit, Quantity};
use eebus::usecases::{lpc, lpp, mpc};

use crate::profile::{GenericProfile, LimitUseCase, Model};

const VOLTS: f64 = 230.0;

const F_LOAD_CONTROL: u32 = 1;
const F_CONFIGURATION: u32 = 2;
const F_DIAGNOSIS: u32 = 3;
const F_ELECTRICAL: u32 = 4;
const F_DIAGNOSIS_CLIENT: u32 = 5;
const F_MEASUREMENT: u32 = 6;
const F_CLASSIFICATION: u32 = 7;

pub struct Generic {
    pub engine: Engine,
    pub actor: ControllableSystemBuilder,
    pub meter: Option<Meter>,
    pub behaviour: Behaviour,
}

/// The MPC side: what is published, and where.
pub struct Meter {
    pub unit: MonitoredUnit,
    pub measurement: FeatureAddress,
}

fn parse_enum<T: serde::de::DeserializeOwned>(text: &str, what: &str) -> Result<T, String> {
    serde_json::from_value(serde_json::Value::String(text.to_string()))
        .map_err(|_| format!("unknown {what} `{text}`"))
}

pub fn build(profile: &GenericProfile, instance: &str, serial: &str) -> Result<Generic, String> {
    let device_type: DeviceType = parse_enum(&profile.device.device_type, "device_type")?;
    let entity_type: EntityType = parse_enum(&profile.device.entity_type, "entity_type")?;
    let (use_case, direction) = match profile.limit.use_case {
        LimitUseCase::Lpc => (&lpc::CONTROLLABLE_SYSTEM, lpc::DIRECTION),
        LimitUseCase::Lpp => (&lpp::CONTROLLABLE_SYSTEM, lpp::DIRECTION),
    };
    if !use_case.permits_entity(&entity_type) {
        return Err(format!(
            "entity type {} cannot play the {} Controllable System",
            profile.device.entity_type, use_case.name
        ));
    }
    if profile.mpc && !mpc::MONITORED_UNIT.permits_entity(&entity_type) {
        return Err(format!(
            "entity type {} cannot play the MPC Monitored Unit",
            profile.device.entity_type
        ));
    }

    let mut device = LocalDevice::new("n:hemslab", instance, device_type)
        .map_err(|e| format!("device address: {e}"))?;

    let unit = profile.mpc.then(|| {
        let mut unit = MonitoredUnit::new(1);
        unit.add(Measurand::total_power());
        for phase in [Phase::A, Phase::B, Phase::C] {
            unit.add(Measurand::on(Quantity::Power, phase.clone()));
            unit.add(Measurand::on(Quantity::Current, phase));
        }
        unit.add(Measurand::total(Quantity::EnergyConsumed));
        unit.add(Measurand::total(Quantity::EnergyProduced));
        unit
    });

    // One ElectricalConnection feature for both use cases.
    let mut electrical = LocalFeature::new(F_ELECTRICAL, FeatureType::ElectricalConnection, Role::Server)
        .with_function(
            Function::ElectricalConnectionCharacteristicListData,
            Operations::read(),
        );
    if unit.is_some() {
        electrical = electrical
            .with_function(Function::ElectricalConnectionDescriptionListData, Operations::read())
            .with_function(
                Function::ElectricalConnectionParameterDescriptionListData,
                Operations::read(),
            );
    }
    let mut entity = LocalEntity::new([1], entity_type)
        .with_feature(limitation::load_control_feature(F_LOAD_CONTROL))
        .with_feature(limitation::device_configuration_feature(F_CONFIGURATION))
        .with_feature(limitation::device_diagnosis_feature(F_DIAGNOSIS))
        .with_feature(limitation::device_diagnosis_client_feature(F_DIAGNOSIS_CLIENT))
        .with_feature(electrical)
        .with_feature(
            LocalFeature::new(F_CLASSIFICATION, FeatureType::DeviceClassification, Role::Server)
                .with_function(Function::DeviceClassificationManufacturerData, Operations::read()),
        );
    if let Some(unit) = &unit {
        entity = entity.with_feature(unit.measurement_feature(F_MEASUREMENT));
    }
    device.add_entity(entity).map_err(|e| e.to_string())?;

    let at = |feature| device.address_of(&[1], feature);
    let features = CsFeatures {
        load_control: at(F_LOAD_CONTROL),
        device_configuration: at(F_CONFIGURATION),
        device_diagnosis: at(F_DIAGNOSIS),
        device_diagnosis_client: at(F_DIAGNOSIS_CLIENT),
    };
    let electrical = at(F_ELECTRICAL);
    let measurement = at(F_MEASUREMENT);
    let classification = at(F_CLASSIFICATION);

    let mut engine = Engine::new(device);
    engine.add_use_case_scenarios([1], 1, use_case, &[1, 2, 3, 4]);
    if unit.is_some() {
        engine.add_use_case_scenarios([1], 1, &mpc::MONITORED_UNIT, &[1, 2, 3]);
    }
    if let Some(feature) = engine.device_mut().resolve_mut(&classification) {
        let _ = feature.set_data(CmdData::DeviceClassificationManufacturerData(
            DeviceClassificationManufacturerData {
                device_name: Some(profile.device.model.as_str().into()),
                device_code: Some(profile.name.as_str().into()),
                brand_name: Some(profile.device.brand.as_str().into()),
                vendor_name: Some(profile.device.brand.as_str().into()),
                serial_number: Some(serial.into()),
                ..Default::default()
            },
        ));
    }

    let config = CsConfig::new(
        profile.limit.failsafe_watts,
        Duration::from_secs(profile.limit.failsafe_duration_s),
    )
    .with_nominal_max(profile.limit.nominal_max_watts);
    let actor = ControllableSystemActor::builder(
        ControllableSystem::new(config, Duration::ZERO),
        direction,
        features,
    )
    .with_electrical_connection(electrical.clone());

    let meter = unit.map(|unit| {
        unit.publish(&mut engine, &electrical, &measurement);
        Meter { unit, measurement }
    });

    Ok(Generic {
        engine,
        actor,
        meter,
        behaviour: Behaviour::new(profile.model.clone()),
    })
}

/// The limit in force, in watts, or `None`.
pub fn limit_of(actor: &ControllableSystemActor) -> Option<f64> {
    match actor.system().effective_limit() {
        EffectiveLimit::Active(w) | EffectiveLimit::Failsafe(w) => Some(w),
        _ => None,
    }
}

// ---- behaviour ------------------------------------------------------------------------

/// One step of a device's physics. Load convention: consumption positive.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Reading {
    pub watts: f64,
    pub phases: u32,
    /// A short state for the log: "running", "paused by limit", "car away", …
    pub status: &'static str,
}

pub struct Behaviour {
    model: Model,
    elapsed: Duration,
    pub consumed_wh: f64,
    pub produced_wh: f64,
}

impl Behaviour {
    fn new(model: Model) -> Self {
        Self {
            model,
            elapsed: Duration::ZERO,
            consumed_wh: 0.0,
            produced_wh: 0.0,
        }
    }

    /// Advances by `dt` under `limit` (watts, `None` for no limit).
    pub fn step(&mut self, dt: Duration, limit: Option<f64>, hour_of_day: f64) -> Reading {
        self.elapsed += dt;
        let t = self.elapsed.as_secs_f64();
        let reading = match &self.model {
            Model::HeatPump {
                rated_watts,
                min_watts,
                on_s,
                off_s,
            } => {
                let cycle = (*on_s + *off_s).max(1) as f64;
                if t % cycle >= *on_s as f64 {
                    Reading { watts: 0.0, phases: 3, status: "idle (cycle off)" }
                } else {
                    // A compressor's draw wanders a little.
                    let wanted = rated_watts * (1.0 + 0.03 * (t / 37.0).sin());
                    match limit {
                        Some(l) if l < *min_watts => Reading { watts: 0.0, phases: 3, status: "stopped by limit" },
                        Some(l) if l < wanted => Reading { watts: l, phases: 3, status: "modulating to limit" },
                        _ => Reading { watts: wanted, phases: 3, status: "running" },
                    }
                }
            }
            Model::PvInverter {
                peak_watts,
                sunrise_h,
                sunset_h,
                clouds,
            } => {
                let day = (hour_of_day - sunrise_h) / (sunset_h - sunrise_h);
                let sun = if (0.0..=1.0).contains(&day) {
                    (std::f64::consts::PI * day).sin().max(0.0)
                } else {
                    0.0
                };
                let cloud = if *clouds {
                    let wave = 0.5 * (t / 173.0).sin() + 0.3 * (t / 41.0).sin() + 0.2 * (t / 11.0).sin();
                    (0.75 + 0.25 * wave).clamp(0.35, 1.0)
                } else {
                    1.0
                };
                let available = peak_watts * sun * cloud;
                match limit {
                    _ if available < 1.0 => Reading { watts: 0.0, phases: 3, status: "night" },
                    Some(l) if l < available => Reading { watts: -l, phases: 3, status: "curtailed by limit" },
                    _ => Reading { watts: -available, phases: 3, status: "producing" },
                }
            }
            Model::Wallbox {
                phases,
                min_amps,
                max_amps,
                plugged_s,
                away_s,
            } => {
                let cycle = (*plugged_s + *away_s).max(1) as f64;
                let per_amp = VOLTS * *phases as f64;
                if t % cycle >= *plugged_s as f64 {
                    Reading { watts: 0.0, phases: *phases, status: "car away" }
                } else {
                    let amps = match limit {
                        Some(l) => (l / per_amp).min(*max_amps),
                        None => *max_amps,
                    };
                    if amps < *min_amps {
                        Reading { watts: 0.0, phases: *phases, status: "paused by limit" }
                    } else {
                        // Cars charge in whole amps.
                        Reading { watts: amps.floor() * per_amp, phases: *phases, status: "charging" }
                    }
                }
            }
        };
        let hours = dt.as_secs_f64() / 3600.0;
        if reading.watts >= 0.0 {
            self.consumed_wh += reading.watts * hours;
        } else {
            self.produced_wh += -reading.watts * hours;
        }
        reading
    }
}

/// Publishes a reading on the MPC unit and notifies subscribers.
pub fn publish(meter: &mut Meter, behaviour: &Behaviour, reading: Reading, engine: &mut Engine, now: Duration) {
    let round = |v: f64| (v * 10.0).round() / 10.0;
    let unit = &mut meter.unit;
    unit.set(&Measurand::total_power(), round(reading.watts));
    let phases = [Phase::A, Phase::B, Phase::C];
    let active = reading.phases.clamp(1, 3) as usize;
    for (index, phase) in phases.into_iter().enumerate() {
        let watts = if index < active { reading.watts / active as f64 } else { 0.0 };
        unit.set(&Measurand::on(Quantity::Power, phase.clone()), round(watts));
        unit.set(&Measurand::on(Quantity::Current, phase), round(watts.abs() / VOLTS));
    }
    unit.set(&Measurand::total(Quantity::EnergyConsumed), behaviour.consumed_wh.round());
    unit.set(&Measurand::total(Quantity::EnergyProduced), behaviour.produced_wh.round());
    unit.notify(engine, &meter.measurement, now);
}

/// Local hour of day as a fraction, for the sun.
pub fn hour_now() -> f64 {
    let now = chrono::Local::now();
    now.hour() as f64 + now.minute() as f64 / 60.0 + now.second() as f64 / 3600.0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wallbox() -> Behaviour {
        Behaviour::new(Model::Wallbox { phases: 3, min_amps: 6.0, max_amps: 16.0, plugged_s: 100, away_s: 50 })
    }

    #[test]
    fn a_wallbox_pauses_below_its_minimum_and_charges_in_whole_amps() {
        let mut b = wallbox();
        let s = Duration::from_secs(1);
        assert_eq!(b.step(s, None, 12.0).watts, 16.0 * 690.0);
        assert_eq!(b.step(s, Some(4_200.0), 12.0).watts, 6.0 * 690.0);
        assert_eq!(b.step(s, Some(4_000.0), 12.0).status, "paused by limit");
        assert_eq!(b.step(Duration::from_secs(100), None, 12.0).status, "car away");
    }

    #[test]
    fn a_pv_inverter_produces_by_day_and_obeys_a_limit() {
        let mut b = Behaviour::new(Model::PvInverter { peak_watts: 10_000.0, sunrise_h: 6.0, sunset_h: 20.0, clouds: false });
        let s = Duration::from_secs(1);
        assert_eq!(b.step(s, None, 2.0).watts, 0.0);
        assert!((b.step(s, None, 13.0).watts + 10_000.0).abs() < 1.0);
        assert_eq!(b.step(s, Some(3_000.0), 13.0).watts, -3_000.0);
        assert!(b.produced_wh > 0.0);
    }

    #[test]
    fn a_heat_pump_stops_below_its_minimum() {
        let mut b = Behaviour::new(Model::HeatPump { rated_watts: 6000.0, min_watts: 1500.0, on_s: 100, off_s: 50 });
        let s = Duration::from_secs(1);
        assert_eq!(b.step(s, Some(1_000.0), 12.0).watts, 0.0);
        assert_eq!(b.step(s, Some(3_000.0), 12.0).watts, 3_000.0);
    }
}
