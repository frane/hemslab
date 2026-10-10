//! Recording a real Modbus device: every register of a map, read in blocks at an
//! interval, written to a capture and turned into a replay profile.

use std::collections::BTreeSet;
use std::io::Write;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::sync::watch;
use tokio_modbus::client::{Context, Reader, tcp};
use tokio_modbus::Slave;
use tokio_modbus::slave::SlaveContext;
use tracing::{info, warn};

use super::map::{Map, Table, Value as Reading, decode};
use crate::profile::{ModbusReplayProfile, Snapshot};

pub struct Options {
    pub address: SocketAddr,
    pub map: String,
    pub output: PathBuf,
    pub interval: Duration,
    pub duration: Option<Duration>,
}

/// A contiguous run of registers read in one request.
#[derive(Clone, Debug)]
struct Block {
    unit: u8,
    table: Table,
    address: u16,
    count: u16,
}

/// Groups a map's readable registers into requests of at most `max` words, bridging gaps
/// of up to `gap` words.
fn blocks(map: &Map, max: u16, gap: u16) -> Vec<Block> {
    let mut registers: Vec<_> = map
        .registers
        .iter()
        .filter(|r| r.access != "wo")
        .map(|r| (map.unit_of(r), r.table, r.address, r.count))
        .collect();
    registers.sort();
    registers.dedup();
    let mut out: Vec<Block> = Vec::new();
    for (unit, table, address, count) in registers {
        if let Some(last) = out.last_mut()
            && last.unit == unit
            && last.table == table
            && address >= last.address
            && address <= last.address + last.count + gap
            && address + count - last.address <= max
        {
            last.count = last.count.max(address + count - last.address);
            continue;
        }
        out.push(Block { unit, table, address, count });
    }
    out
}

async fn read(ctx: &mut Context, block: &Block) -> Result<Vec<u16>, String> {
    ctx.set_slave(Slave(block.unit));
    let result = match block.table {
        Table::Input => ctx.read_input_registers(block.address, block.count).await,
        Table::Holding => ctx.read_holding_registers(block.address, block.count).await,
    };
    match result {
        Ok(Ok(words)) => Ok(words),
        Ok(Err(exception)) => Err(format!("{exception:?}")),
        Err(error) => Err(error.to_string()),
    }
}

pub async fn record(options: Options, mut stop: watch::Receiver<bool>) -> Result<usize, String> {
    let map = Map::load(&options.map)?;
    let mut ctx = tcp::connect(options.address)
        .await
        .map_err(|e| format!("cannot connect to {}: {e}", options.address))?;
    info!("connected to {}", options.address);

    // A block that spans a gap the device does not serve fails as a whole; it is split
    // into its registers once, and those that still fail are left out from then on.
    let mut plan = blocks(&map, 100, 8);
    let mut checked = BTreeSet::new();
    let mut out = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&options.output)
        .map_err(|e| format!("{}: {e}", options.output.display()))?;
    writeln!(out, "{}", json!({ "protocol": "modbus", "map": map.name, "address": options.address.to_string() }))
        .map_err(|e| e.to_string())?;

    let started = tokio::time::Instant::now();
    let mut ticker = tokio::time::interval(options.interval);
    let mut written = 0;
    loop {
        tokio::select! {
            _ = ticker.tick() => {}
            _ = stop.changed() => break,
        }
        if *stop.borrow() || options.duration.is_some_and(|d| started.elapsed() >= d) {
            break;
        }
        let mut next = Vec::with_capacity(plan.len());
        let mut queue: std::collections::VecDeque<Block> = plan.clone().into();
        while let Some(block) = queue.pop_front() {
            match read(&mut ctx, &block).await {
                Ok(words) => {
                    let line = json!({
                        "t": chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
                        "unit": block.unit,
                        "table": block.table,
                        "address": block.address,
                        "words": words,
                    });
                    let _ = writeln!(out, "{line}");
                    written += 1;
                    next.push(block);
                }
                Err(error) if checked.insert((block.unit, block.table, block.address, block.count)) => {
                    // Split it into the map's registers and read those straight away, so
                    // the first round is complete.
                    let singles: Vec<Block> = map
                        .registers
                        .iter()
                        .filter(|r| {
                            map.unit_of(r) == block.unit
                                && r.table == block.table
                                && r.access != "wo"
                                && r.address >= block.address
                                && r.address < block.address + block.count
                        })
                        .map(|r| Block { unit: block.unit, table: r.table, address: r.address, count: r.count })
                        .collect();
                    if singles.len() > 1 {
                        tracing::debug!("block {}@{} failed ({error}); splitting", block.unit, block.address);
                        for single in singles.into_iter().rev() {
                            queue.push_front(single);
                        }
                    } else {
                        warn!("unit {} register {} is not served ({error}); skipped", block.unit, block.address);
                    }
                }
                Err(_) => {}
            }
        }
        plan = next;
        let _ = out.flush();
    }
    Ok(written)
}

/// Turns a Modbus capture into a replay profile.
pub fn import(path: &Path, name: &str, redact: bool) -> Result<(ModbusReplayProfile, usize), String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut map_name = None;
    let mut snapshots = Vec::new();
    let mut start: Option<chrono::DateTime<chrono::FixedOffset>> = None;
    for line in text.lines().filter(|l| !l.trim().is_empty()) {
        let value: Value = serde_json::from_str(line).map_err(|e| e.to_string())?;
        if let Some(map) = value.get("map").and_then(Value::as_str) {
            map_name = Some(map.to_string());
            continue;
        }
        let at = value
            .get("t")
            .and_then(Value::as_str)
            .and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok())
            .ok_or("a capture line without a time")?;
        let start = *start.get_or_insert(at);
        snapshots.push(Snapshot {
            at: (at - start).num_milliseconds() as f64 / 1000.0,
            unit: serde_json::from_value(value["unit"].clone()).map_err(|e| e.to_string())?,
            table: serde_json::from_value(value["table"].clone()).map_err(|e| e.to_string())?,
            address: serde_json::from_value(value["address"].clone()).map_err(|e| e.to_string())?,
            words: serde_json::from_value(value["words"].clone()).map_err(|e| e.to_string())?,
        });
    }
    let map_name = map_name.ok_or("not a Modbus capture: no map header")?;
    let map = Map::load(&map_name)?;

    // Name and identity from the registers that carry them.
    let mut bank = super::bank::Bank::new(std::sync::Arc::new(map.clone()));
    for s in &snapshots {
        bank.write_words(s.unit, s.table, s.address, &s.words);
    }
    let text_of = |name: &str| match bank.get(name) {
        Some(Reading::Text(t)) if !t.is_empty() => Some(t),
        _ => None,
    };
    let model = text_of("inverter_model_type");
    let serial = text_of("inverter_serial_number");

    if redact && let Some(serial) = serial.filter(|s| s.len() >= 4) {
        // Every register that holds the serial is blanked to X.
        for register in map.registers.iter().filter(|r| r.kind == super::map::Kind::String) {
            let unit = map.unit_of(register);
            for s in snapshots.iter_mut().filter(|s| {
                s.unit == unit && s.table == register.table && s.address <= register.address
                    && register.address + register.count <= s.address + s.words.len() as u16
            }) {
                let offset = (register.address - s.address) as usize;
                let slice = &mut s.words[offset..offset + register.count as usize];
                if let Reading::Text(t) = decode(register, slice)
                    && t.contains(&serial)
                {
                    let masked = t.replace(&serial, &"X".repeat(serial.len()));
                    let words = super::map::encode(register, &Reading::Text(masked));
                    slice.copy_from_slice(&words);
                }
            }
        }
    }
    let count = snapshots.len();
    Ok((
        ModbusReplayProfile {
            name: name.to_string(),
            description: format!(
                "{}, replayed from a Modbus recording",
                model.clone().unwrap_or_else(|| map.description.clone())
            ),
            source: format!("hemslab record-modbus, map {map_name}"),
            protocol: "modbus".into(),
            map: map_name,
            brand: Some(map.name.clone()),
            model,
            snapshots,
        },
        count,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_sigenergy_map_reads_in_few_requests() {
        let map = Map::load("sigenergy").unwrap();
        let plan = blocks(&map, 100, 8);
        assert!(plan.len() < 40, "{} requests", plan.len());
        assert!(plan.iter().all(|b| b.count <= 100));
        // Every readable register is covered by exactly one block.
        for r in map.registers.iter().filter(|r| r.access != "wo") {
            let covering = plan
                .iter()
                .filter(|b| b.unit == map.unit_of(r) && b.table == r.table && b.address <= r.address && r.address + r.count <= b.address + b.count)
                .count();
            assert_eq!(covering, 1, "{}", r.name);
        }
    }
}
