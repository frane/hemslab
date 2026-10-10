//! Device profiles: what a simulated device is and how it behaves.
//!
//! Two kinds:
//!
//! * **Generic** (`.toml`): a device described by its role and a behaviour model — a heat
//!   pump, a PV inverter, a wallbox. The use cases come from the `eebus` crate and are
//!   implemented by the book; the behaviour makes the numbers move like a real device.
//! * **Replay** (`.json`): a real device as it was recorded — its SPINE structure, its
//!   use cases, every answer it gave, and its measurements over time. Made by
//!   `hemslab import` from a capture.

use std::path::Path;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// A profile of either kind.
#[derive(Clone, Debug)]
pub enum Profile {
    Generic(GenericProfile),
    Replay(ReplayProfile),
    /// A simulated Modbus device with behaviour.
    Modbus(ModbusProfile),
    /// A recorded Modbus device.
    ModbusReplay(ModbusReplayProfile),
}

impl Profile {
    pub fn name(&self) -> &str {
        match self {
            Profile::Generic(p) => &p.name,
            Profile::Replay(p) => &p.name,
            Profile::Modbus(p) => &p.name,
            Profile::ModbusReplay(p) => &p.name,
        }
    }

    pub fn description(&self) -> &str {
        match self {
            Profile::Generic(p) => &p.description,
            Profile::Replay(p) => &p.description,
            Profile::Modbus(p) => &p.description,
            Profile::ModbusReplay(p) => &p.description,
        }
    }

    /// A built-in profile by name, or a profile file by path.
    pub fn load(spec: &str) -> Result<Self, String> {
        if let Some(profile) = builtin(spec) {
            return Ok(profile);
        }
        let path = Path::new(spec);
        let text = std::fs::read_to_string(path).map_err(|e| {
            format!(
                "`{spec}` is neither a built-in profile ({}) nor a readable file: {e}",
                BUILTIN.iter().map(|(n, _)| *n).collect::<Vec<_>>().join(", ")
            )
        })?;
        Self::parse(&text, path.extension().and_then(|e| e.to_str()).unwrap_or(""))
            .map_err(|e| format!("{spec}: {e}"))
    }

    pub fn parse(text: &str, extension: &str) -> Result<Self, String> {
        match extension {
            "toml" => {
                let value: toml::Value = toml::from_str(text).map_err(|e| e.to_string())?;
                if value.get("protocol").and_then(|p| p.as_str()) == Some("modbus") {
                    toml::from_str(text).map(Profile::Modbus).map_err(|e| e.to_string())
                } else {
                    toml::from_str(text).map(Profile::Generic).map_err(|e| e.to_string())
                }
            }
            _ => {
                let value: serde_json::Value = serde_json::from_str(text).map_err(|e| e.to_string())?;
                if value.get("protocol").and_then(|p| p.as_str()) == Some("modbus") {
                    serde_json::from_value(value).map(Profile::ModbusReplay).map_err(|e| e.to_string())
                } else {
                    serde_json::from_value(value).map(Profile::Replay).map_err(|e| e.to_string())
                }
            }
        }
    }
}

// ---- generic --------------------------------------------------------------------------

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct GenericProfile {
    pub name: String,
    #[serde(default)]
    pub description: String,
    pub device: Identification,
    pub limit: LimitSettings,
    #[serde(default = "yes")]
    pub mpc: bool,
    pub model: Model,
}

fn yes() -> bool {
    true
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Identification {
    pub brand: String,
    pub model: String,
    /// SPINE device type, e.g. `HeatGenerationSystem`.
    pub device_type: String,
    /// SPINE entity type, e.g. `HeatPumpAppliance`.
    pub entity_type: String,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum LimitUseCase {
    /// Limitation of Power Consumption.
    Lpc,
    /// Limitation of Power Production.
    Lpp,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct LimitSettings {
    pub use_case: LimitUseCase,
    pub failsafe_watts: f64,
    #[serde(default = "two_hours")]
    pub failsafe_duration_s: u64,
    pub nominal_max_watts: f64,
}

fn two_hours() -> u64 {
    2 * 3600
}

/// How the device's power moves. Load convention throughout: consumption positive.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Model {
    /// A compressor that cycles: runs at `rated_watts` for `on_s`, rests for `off_s`.
    /// Below `min_watts` it cannot run and stops; between, it modulates down.
    HeatPump {
        rated_watts: f64,
        min_watts: f64,
        on_s: u64,
        off_s: u64,
    },
    /// PV production following the sun between `sunrise_h` and `sunset_h` local time,
    /// peaking at `peak_watts`, with passing clouds.
    PvInverter {
        peak_watts: f64,
        #[serde(default = "sunrise")]
        sunrise_h: f64,
        #[serde(default = "sunset")]
        sunset_h: f64,
        #[serde(default)]
        clouds: bool,
    },
    /// An EV comes and goes; while plugged in it charges between `min_amps` and
    /// `max_amps` on `phases` phases. A limit below the minimum pauses charging,
    /// as real wallboxes do.
    Wallbox {
        phases: u32,
        min_amps: f64,
        max_amps: f64,
        plugged_s: u64,
        away_s: u64,
    },
}

fn sunrise() -> f64 {
    6.0
}

fn sunset() -> f64 {
    20.0
}

// ---- replay ---------------------------------------------------------------------------

/// A recorded device. All SPINE content is kept in the EEBUS JSON-UTF8 projection
/// (SHIP §11.4), the form the `eebus` crate reads.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ReplayProfile {
    pub name: String,
    #[serde(default)]
    pub description: String,
    /// Where it came from, e.g. "elli-eebus-proxy capture, 2026-10-09".
    #[serde(default)]
    pub source: String,
    pub brand: Option<String>,
    pub model: Option<String>,
    /// The device's SPINE address, e.g. `d:_i:3210_Elli-XXXXXXXX`.
    pub device_address: String,
    /// `nodeManagementDetailedDiscoveryData`, as the `cmd` element.
    pub discovery: Value,
    /// `nodeManagementUseCaseData`, as the `cmd` element, if it was recorded.
    #[serde(default)]
    pub use_cases: Option<Value>,
    /// The last answer per feature and function: `cmd` elements addressed by
    /// entity and feature.
    #[serde(default)]
    pub data: Vec<FeatureData>,
    /// Notifications as they arrived, replayed in a loop to keep values moving.
    #[serde(default)]
    pub timeline: Vec<TimedData>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct FeatureData {
    pub entity: Vec<u32>,
    pub feature: u32,
    pub cmd: Value,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct TimedData {
    /// Seconds since the start of the recording.
    pub at: f64,
    pub entity: Vec<u32>,
    pub feature: u32,
    pub cmd: Value,
}

// ---- modbus ---------------------------------------------------------------------------

/// A simulated Modbus device: a register map and a behaviour model behind it.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ModbusProfile {
    pub name: String,
    #[serde(default)]
    pub description: String,
    /// Always `modbus`.
    pub protocol: String,
    /// A built-in register map name or a map file.
    pub map: String,
    /// The Sigenergy hybrid plant; further models will sit beside it.
    pub sigenergy: crate::modbus::sigenergy::Params,
}

/// A recorded Modbus device: register contents over time.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ModbusReplayProfile {
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub source: String,
    /// Always `modbus`.
    pub protocol: String,
    /// The register map it was recorded with, which names what the numbers mean.
    pub map: String,
    #[serde(default)]
    pub brand: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    /// Register blocks as they were read; replayed in a loop.
    pub snapshots: Vec<Snapshot>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Snapshot {
    /// Seconds since the start of the recording.
    pub at: f64,
    pub unit: u8,
    pub table: crate::modbus::map::Table,
    pub address: u16,
    pub words: Vec<u16>,
}

// ---- built-ins ------------------------------------------------------------------------

pub const BUILTIN: &[(&str, &str)] = &[
    ("heat-pump", include_str!("../profiles/heat-pump.toml")),
    ("pv-inverter", include_str!("../profiles/pv-inverter.toml")),
    ("wallbox", include_str!("../profiles/wallbox.toml")),
    ("sigenergy", include_str!("../profiles/sigenergy.toml")),
    ("elli-charger-2", include_str!("../profiles/elli-charger-2.json")),
    ("elli-charger-connect-pro", include_str!("../profiles/real/elli-charger-connect-pro.json")),
    ("kostal-smart-energy-meter", include_str!("../profiles/real/kostal-smart-energy-meter.json")),
    ("porsche-mobile-charger-connect", include_str!("../profiles/real/porsche-mobile-charger-connect.json")),
    ("sma-home-manager-2", include_str!("../profiles/real/sma-home-manager-2.json")),
    ("spelsberg-wallbox-smart-pro", include_str!("../profiles/real/spelsberg-wallbox-smart-pro.json")),
    ("vaillant-arotherm", include_str!("../profiles/real/vaillant-arotherm.json")),
    ("viessmann-vitoconnect", include_str!("../profiles/real/viessmann-vitoconnect.json")),
];

pub fn builtin(name: &str) -> Option<Profile> {
    let (_, text) = BUILTIN.iter().find(|(n, _)| *n == name)?;
    let extension = if text.trim_start().starts_with('{') { "json" } else { "toml" };
    Some(Profile::parse(text, extension).expect("built-in profiles parse"))
}
