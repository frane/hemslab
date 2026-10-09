//! A recorded device, played back.
//!
//! The SPINE device is rebuilt from the recorded discovery: the same address, entities,
//! features and functions, the same use cases. Every recorded answer is loaded into its
//! feature, so a reader gets what the real device said. Recorded notifications are
//! played back in a loop, so values move and subscribers hear about it. Writes are
//! accepted into the data, as the real device announced them writable.

use std::collections::BTreeSet;
use std::time::Duration;

use eebus::model::{AddressDevice, CmdData, DeviceType, FeatureAddress, Function};
use eebus::spine::{Engine, LocalDevice, LocalEntity, LocalFeature, Operations, RemoteDevice};
use eebus::usecases::descriptor::{ActorRole, Scenario, Support, UseCaseDescriptor};

use crate::profile::{ReplayProfile, TimedData};
use crate::wire;

/// What a profile's structure amounts to.
pub struct Structure {
    pub remote: RemoteDevice,
    pub entities: usize,
    pub features: usize,
    pub use_cases: usize,
}

impl Structure {
    pub fn of(profile: &ReplayProfile) -> Result<Self, String> {
        let mut remote = RemoteDevice::default();
        match wire::cmd_from_value(&profile.discovery)?.data {
            Some(CmdData::NodeManagementDetailedDiscoveryData(d)) => remote.apply_detailed_discovery(&d),
            _ => return Err("`discovery` is not nodeManagementDetailedDiscoveryData".into()),
        }
        if let Some(value) = &profile.use_cases {
            match wire::cmd_from_value(value)?.data {
                Some(CmdData::NodeManagementUseCaseData(d)) => remote.apply_use_case_data(&d),
                _ => return Err("`use_cases` is not nodeManagementUseCaseData".into()),
            }
        }
        Ok(Self {
            entities: remote.entities.iter().filter(|e| e.address != [0]).count(),
            features: remote
                .entities
                .iter()
                .flat_map(|e| &e.features)
                .filter(|f| f.feature_type.as_str() != "NodeManagement")
                .count(),
            use_cases: remote.use_cases.len(),
            remote,
        })
    }
}

/// The engine for a replayed device, and the timeline to drive it with.
pub struct Replay {
    pub engine: Engine,
    pub timeline: Timeline,
}

pub fn build(profile: &ReplayProfile) -> Result<Replay, String> {
    let structure = Structure::of(profile)?;
    let remote = &structure.remote;
    let device_type = remote
        .device_type
        .clone()
        .unwrap_or_else(|| DeviceType::from("Generic"));
    let mut device =
        LocalDevice::from_address(AddressDevice::from(profile.device_address.as_str()), device_type);

    // Parents before children: `[1]` before `[1, 1]`.
    let mut entities: Vec<_> = remote.entities.iter().collect();
    entities.sort_by(|a, b| a.address.len().cmp(&b.address.len()).then(a.address.cmp(&b.address)));
    for entity in entities {
        let features: Vec<LocalFeature> = entity
            .features
            .iter()
            .filter(|f| f.feature_type.as_str() != "NodeManagement")
            .map(|f| {
                let mut local = LocalFeature::new(
                    f.address.feature.map(|a| a.get()).unwrap_or(0),
                    f.feature_type.clone(),
                    f.role,
                )
                // A replayed device answers writes from whoever is connected; it has no
                // use-case logic of its own that would ask for a binding first.
                .with_unbound_writes();
                for property in &f.functions {
                    let Some(function) = property.function.clone() else { continue };
                    let ops = property.possible_operations.as_ref();
                    let read = ops.and_then(|o| o.read.as_ref());
                    let write = ops.and_then(|o| o.write.as_ref());
                    let operations = match (read, write) {
                        (_, Some(w)) if w.partial.is_some() => Operations::read_write(),
                        (_, Some(_)) => Operations::read_write_full(),
                        (Some(r), None) if r.partial.is_some() => Operations::read(),
                        _ => Operations::read_full(),
                    };
                    local = local.with_function(function, operations);
                }
                local
            })
            .collect();
        if entity.address == [0] {
            let zero = device.entity_mut(&[0]).ok_or("no entity [0]")?;
            for feature in features {
                zero.add_feature(feature).map_err(|e| e.to_string())?;
            }
            continue;
        }
        let entity_type = entity
            .entity_type
            .clone()
            .unwrap_or_else(|| eebus::model::EntityType::from("Generic"));
        let mut local = LocalEntity::new(entity.address.clone(), entity_type);
        for feature in features {
            local = local.with_feature(feature);
        }
        device
            .add_entity(local)
            .map_err(|e| format!("entity {:?}: {e}", entity.address))?;
    }

    // The recorded answers.
    for item in &profile.data {
        let cmd = wire::cmd_from_value(&item.cmd)?;
        let Some(data) = cmd.data else { continue };
        let address = device.address_of(&item.entity, item.feature);
        if let Some(feature) = device.resolve_mut(&address) {
            let _ = feature.set_data(data);
        }
    }

    let mut engine = Engine::new(device);
    for use_case in &remote.use_cases {
        let (entity, feature) = match &use_case.address {
            Some(address) => (
                address
                    .entity
                    .as_ref()
                    .map(|e| e.iter().map(|a| a.get()).collect::<Vec<_>>())
                    .unwrap_or_else(|| vec![1]),
                address.feature.map(|f| f.get()).unwrap_or(0),
            ),
            None => (vec![1], 0),
        };
        let descriptor = recorded_descriptor(
            use_case.name.as_str(),
            use_case.actor.as_str(),
            use_case
                .version
                .as_ref()
                .map(|v| v.as_str())
                .unwrap_or("1.0.0"),
            &use_case.scenarios,
        );
        engine.add_use_case_scenarios(entity, feature, descriptor, &use_case.scenarios);
    }

    Ok(Replay {
        engine,
        timeline: Timeline::new(&profile.timeline)?,
    })
}

/// A use case exactly as the device announced it.
///
/// The engine announces use cases from descriptors with `'static` lifetimes, as a
/// device's use cases are fixed at build time; a replayed device's are fixed at load
/// time, once per process, so leaking them is the honest equivalent.
fn recorded_descriptor(
    name: &str,
    actor: &str,
    version: &str,
    scenarios: &[u32],
) -> &'static UseCaseDescriptor {
    let leak = |s: &str| -> &'static str { Box::leak(s.to_string().into_boxed_str()) };
    let scenarios: Vec<Scenario> = scenarios
        .iter()
        .map(|&number| Scenario {
            number,
            name: "",
            support: Support::Optional,
            functions: &[],
        })
        .collect();
    Box::leak(Box::new(UseCaseDescriptor {
        name: leak(name),
        actor: leak(actor),
        role: ActorRole::Server,
        version: leak(version),
        document_sub_revision: "release",
        entity_types: &[],
        counterpart: "",
        scenarios: Box::leak(scenarios.into_boxed_slice()),
    }))
}

/// Recorded notifications, played in a loop.
pub struct Timeline {
    events: Vec<(Duration, Vec<u32>, u32, CmdData, bool)>,
    period: Duration,
    next: usize,
    lap_start: Duration,
}

impl Timeline {
    fn new(timed: &[TimedData]) -> Result<Self, String> {
        let mut events = Vec::with_capacity(timed.len());
        for item in timed {
            let cmd = wire::cmd_from_value(&item.cmd)?;
            let partial = cmd.filter.is_some();
            if let Some(data) = cmd.data {
                events.push((
                    Duration::from_secs_f64(item.at.max(0.0)),
                    item.entity.clone(),
                    item.feature,
                    data,
                    partial,
                ));
            }
        }
        // One gap's worth after the last event, so the loop does not stutter.
        let last = events.last().map(|e| e.0).unwrap_or_default();
        let period = last + Duration::from_secs(1);
        Ok(Self {
            events,
            period,
            next: 0,
            lap_start: Duration::ZERO,
        })
    }

    pub fn is_empty(&self) -> bool {
        self.events.is_empty()
    }

    /// Applies what is due by `now` and notifies subscribers.
    pub fn advance(&mut self, engine: &mut Engine, now: Duration) -> usize {
        if self.events.is_empty() {
            return 0;
        }
        let mut applied = 0;
        let mut notified = BTreeSet::new();
        loop {
            if self.next == self.events.len() {
                self.next = 0;
                self.lap_start += self.period;
            }
            let (at, entity, feature, data, partial) = &self.events[self.next];
            if self.lap_start + *at > now {
                break;
            }
            let address: FeatureAddress = engine.device().address_of(entity, *feature);
            let function = Function::from(data.key());
            if let Some(local) = engine.device_mut().resolve_mut(&address) {
                let changed = if *partial {
                    local.apply(data.clone(), true).is_ok()
                } else {
                    local.set_data(data.clone()).unwrap_or(false)
                };
                if changed {
                    notified.insert((entity.clone(), *feature, data.key()));
                }
            }
            self.next += 1;
            applied += 1;
            // A capture with everything at one instant would otherwise spin forever.
            if applied > self.events.len() {
                break;
            }
            let _ = function;
        }
        for (entity, feature, key) in notified {
            let address = engine.device().address_of(&entity, feature);
            engine.notify(&address, &Function::from(key), now);
        }
        applied
    }

    /// When the next event is due.
    pub fn next_due(&self) -> Option<Duration> {
        let (at, ..) = self.events.get(self.next)?;
        Some(self.lap_start + *at)
    }
}

/// The total active power a replayed device currently reports, if it has a
/// `Measurement` server with an `acPowerTotal` power measurement.
pub fn total_power(engine: &Engine) -> Option<f64> {
    use eebus::model::FeatureType;
    for (_, feature) in engine.device().features() {
        if feature.feature_type() != &FeatureType::Measurement {
            continue;
        }
        let Some(CmdData::MeasurementDescriptionListData(descriptions)) =
            feature.data(&Function::MeasurementDescriptionListData)
        else {
            continue;
        };
        let id = descriptions.measurement_description_data.iter().flatten().find_map(|d| {
            let power = d.measurement_type.as_ref().is_some_and(|t| t.as_str() == "power");
            let total = d.scope_type.as_ref().is_some_and(|t| t.as_str() == "acPowerTotal");
            (power && total).then_some(d.measurement_id).flatten()
        })?;
        let Some(CmdData::MeasurementListData(list)) = feature.data(&Function::MeasurementListData)
        else {
            continue;
        };
        return list
            .measurement_data
            .iter()
            .flatten()
            .find(|m| m.measurement_id == Some(id))
            .and_then(|m| m.value.as_ref())
            .and_then(|v| v.to_f64());
    }
    None
}
