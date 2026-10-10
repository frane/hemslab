//! Captures → replay profiles.
//!
//! Two inputs are understood:
//!
//! * `traffic.jsonl` from `elli-eebus-proxy` (and later `hemslab record`): one JSON object
//!   per line, `{"t", "side", "ski", "dir", "msg": {"datagram": …}}`. Everything the
//!   device said on its side of the proxy becomes the profile.
//! * Single datagrams in either JSON form, e.g. the `enbility/devices` corpus: a
//!   discovery reply and, optionally, a use-case reply.

use std::collections::BTreeMap;
use std::path::Path;

use chrono::{DateTime, FixedOffset};
use eebus::model::{Cmd, CmdClassifier, CmdData, Datagram};
use serde_json::Value;

use crate::profile::{FeatureData, ReplayProfile, TimedData};
use crate::wire;

pub struct ImportOptions {
    pub name: String,
    pub description: Option<String>,
    /// Which side of the proxy is the device. `None`: the side whose discovery is not an
    /// energy manager's.
    pub side: Option<String>,
    /// Replace serial numbers with `X`.
    pub redact: bool,
    /// Keep at most this many seconds of timeline.
    pub max_timeline_s: f64,
}

/// One message from the device.
struct Seen {
    at: Option<DateTime<FixedOffset>>,
    datagram: Datagram,
}

pub fn import(paths: &[&Path], options: &ImportOptions) -> Result<(ReplayProfile, Report), String> {
    let mut messages = Vec::new();
    let mut source = Vec::new();
    for path in paths {
        let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
        let trimmed = text.trim_start();
        if is_jsonl(trimmed) {
            messages.extend(read_proxy_capture(&text, options.side.as_deref())?);
            source.push(format!("proxy capture {}", file_name(path)));
        } else {
            let value: Value =
                serde_json::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))?;
            let datagram = wire::datagram(&value).map_err(|e| format!("{}: {e}", path.display()))?;
            messages.push(Seen { at: None, datagram });
            source.push(file_name(path));
        }
    }
    build(messages, options, source.join(", "))
}

fn file_name(path: &Path) -> String {
    path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
}

fn is_jsonl(text: &str) -> bool {
    text.lines()
        .next()
        .and_then(|line| serde_json::from_str::<Value>(line).ok())
        .is_some_and(|v| v.get("side").is_some() && v.get("msg").is_some())
}

fn read_proxy_capture(text: &str, side: Option<&str>) -> Result<Vec<Seen>, String> {
    // Every incoming datagram, per side.
    let mut by_side: BTreeMap<String, Vec<Seen>> = BTreeMap::new();
    for (number, line) in text.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let Ok(entry) = serde_json::from_str::<Value>(line) else {
            return Err(format!("line {}: not JSON", number + 1));
        };
        if entry.get("dir").and_then(Value::as_str) != Some("in") {
            continue;
        }
        let Some(name) = entry.get("side").and_then(Value::as_str) else {
            continue;
        };
        let Some(msg) = entry.get("msg") else { continue };
        let Ok(datagram) = wire::datagram(msg) else {
            continue;
        };
        let at = entry
            .get("t")
            .and_then(Value::as_str)
            .and_then(|t| DateTime::parse_from_rfc3339(t).ok());
        by_side.entry(name.to_string()).or_default().push(Seen { at, datagram });
    }
    if let Some(side) = side {
        return by_side
            .remove(side)
            .ok_or_else(|| format!("the capture has no side `{side}`"));
    }
    // The device is the side whose discovery is not an energy manager's.
    let device_side = by_side.iter().find_map(|(name, seen)| {
        seen.iter()
            .find_map(|s| discovery_of(&s.datagram))
            .filter(|d| {
                d.device_information
                    .as_ref()
                    .and_then(|i| i.description.as_ref())
                    .and_then(|d| d.device_type.as_ref())
                    .is_none_or(|t| t.as_str() != "EnergyManagementSystem")
            })
            .map(|_| name.clone())
    });
    let side = device_side.ok_or("no device discovery found in the capture; pass --side")?;
    Ok(by_side.remove(&side).unwrap_or_default())
}

fn first_cmd(datagram: &Datagram) -> Option<&Cmd> {
    datagram.payload.as_ref()?.cmd.as_ref()?.first()
}

fn discovery_of(datagram: &Datagram) -> Option<&eebus::model::NodeManagementDetailedDiscoveryData> {
    match first_cmd(datagram)?.data.as_ref()? {
        CmdData::NodeManagementDetailedDiscoveryData(d) if d.device_information.is_some() => Some(d),
        _ => None,
    }
}

/// What an import found, for the CLI to print.
#[derive(Debug, Default)]
pub struct Report {
    pub entities: usize,
    pub features: usize,
    pub use_cases: usize,
    pub data: usize,
    pub timeline: usize,
    pub timeline_s: f64,
    pub redacted: Vec<String>,
}

fn build(
    messages: Vec<Seen>,
    options: &ImportOptions,
    source: String,
) -> Result<(ReplayProfile, Report), String> {
    let mut discovery: Option<(String, Value)> = None;
    let mut use_cases = None;
    let mut data: BTreeMap<(Vec<u32>, u32, String), Value> = BTreeMap::new();
    let mut timeline = Vec::new();
    let mut start: Option<DateTime<FixedOffset>> = None;
    let mut report = Report::default();

    for seen in &messages {
        let datagram = &seen.datagram;
        let Some(header) = datagram.header.as_ref() else { continue };
        let classifier = header.cmd_classifier;
        if !matches!(classifier, Some(CmdClassifier::Reply) | Some(CmdClassifier::Notify)) {
            continue;
        }
        let Some(cmd) = first_cmd(datagram) else { continue };
        let Some(payload) = cmd.data.as_ref() else { continue };
        let Some(source_address) = header.address_source.as_ref() else { continue };
        let entity: Vec<u32> = source_address
            .entity
            .as_ref()
            .map(|e| e.iter().map(|a| a.get()).collect())
            .unwrap_or_default();
        let feature = source_address.feature.map(|f| f.get()).unwrap_or(0);

        match payload {
            CmdData::NodeManagementDetailedDiscoveryData(d) if d.device_information.is_some() => {
                {
                    // The latest wins here too: a later discovery is the more complete one.
                    let address = source_address
                        .device
                        .as_ref()
                        .map(|d| d.as_str().to_string())
                        .or_else(|| {
                            d.device_information
                                .as_ref()?
                                .description
                                .as_ref()?
                                .device_address
                                .as_ref()?
                                .device
                                .as_ref()
                                .map(|a| a.as_str().to_string())
                        })
                        .ok_or("the discovery names no device address")?;
                    discovery = Some((address, wire::cmd_to_value(cmd)));
                }
                continue;
            }
            CmdData::NodeManagementUseCaseData(_) => {
                // The latest wins: a recorder writes these as they fill in.
                if matches!(payload, CmdData::NodeManagementUseCaseData(d) if d.use_case_information.as_ref().is_some_and(|i| !i.is_empty())) {
                    use_cases = Some(wire::cmd_to_value(cmd));
                }
                continue;
            }
            // Bindings, subscriptions and the like describe this capture's peers, not the
            // device.
            _ if entity == [0] && feature == 0 => continue,
            _ => {}
        }

        let key = (entity.clone(), feature, payload.key().to_string());
        let partial = cmd.filter.is_some();
        if !partial {
            data.entry(key).or_insert_with(|| wire::cmd_to_value(cmd));
        }
        if matches!(classifier, Some(CmdClassifier::Notify))
            && let Some(at) = seen.at
        {
            let start = *start.get_or_insert(at);
            let offset = (at - start).num_milliseconds() as f64 / 1000.0;
            if offset <= options.max_timeline_s {
                timeline.push(TimedData {
                    at: offset,
                    entity,
                    feature,
                    cmd: wire::cmd_to_value(cmd),
                });
            }
        }
    }

    let (device_address, discovery) =
        discovery.ok_or("the capture contains no discovery reply from the device")?;

    // Identification, for the mDNS record and the listing.
    // A device may carry several DeviceClassification features, some of them empty: the
    // first one that names a brand is the one that identifies it.
    let (brand, model, serial) = data
        .iter()
        .filter(|((_, _, function), _)| function == "deviceClassificationManufacturerData")
        .filter_map(|(_, value)| wire::cmd_from_value(value).ok())
        .find_map(|cmd| match cmd.data {
            Some(CmdData::DeviceClassificationManufacturerData(m))
                if m.brand_name.is_some() || m.vendor_name.is_some() =>
            {
                Some((
                    m.brand_name.or(m.vendor_name).map(|s| s.as_str().to_string()),
                    m.device_name.or(m.device_code).map(|s| s.as_str().to_string()),
                    m.serial_number.map(|s| s.as_str().to_string()),
                ))
            }
            _ => None,
        })
        .unwrap_or((None, None, None));

    let mut profile = ReplayProfile {
        name: options.name.clone(),
        description: options.description.clone().unwrap_or_else(|| {
            format!(
                "{} {}, replayed from a capture",
                brand.as_deref().unwrap_or("Unknown"),
                model.as_deref().unwrap_or("device")
            )
        }),
        source,
        brand,
        model,
        device_address: device_address.clone(),
        discovery,
        use_cases,
        data: data
            .into_iter()
            .map(|((entity, feature, _), cmd)| FeatureData { entity, feature, cmd })
            .collect(),
        timeline,
    };

    {
        let model = crate::replay::Structure::of(&profile)?;
        report.entities = model.entities;
        report.features = model.features;
        report.use_cases = model.use_cases;
    }
    report.data = profile.data.len();
    report.timeline = profile.timeline.len();
    report.timeline_s = profile.timeline.last().map(|t| t.at).unwrap_or(0.0);

    if options.redact {
        let mut secrets = Vec::new();
        if let Some(serial) = serial.filter(|s| s.len() >= 4) {
            secrets.push(serial);
        }
        // The unique part of the device address usually carries the serial, too:
        // `d:_i:3210_Elli-00015087`.
        if let Some(unique) = device_address.rsplit('_').next() {
            for run in digit_runs(unique) {
                secrets.push(run);
            }
        }
        secrets.sort();
        secrets.dedup();
        let mut json = serde_json::to_string(&profile).map_err(|e| e.to_string())?;
        for secret in &secrets {
            json = json.replace(secret.as_str(), &"X".repeat(secret.chars().count()));
        }
        profile = serde_json::from_str(&json).map_err(|e| e.to_string())?;
        report.redacted = secrets;
    }
    Ok((profile, report))
}

/// Runs of five or more digits: what serial numbers look like inside an address.
fn digit_runs(text: &str) -> Vec<String> {
    let mut runs = Vec::new();
    let mut current = String::new();
    for ch in text.chars().chain(std::iter::once(' ')) {
        if ch.is_ascii_digit() {
            current.push(ch);
        } else {
            if current.len() >= 5 {
                runs.push(current.clone());
            }
            current.clear();
        }
    }
    runs
}
