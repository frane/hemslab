//! Register maps: what a Modbus device keeps where, and how a value is encoded.

use std::collections::{BTreeMap, HashMap};

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[serde(rename_all = "lowercase")]
pub enum Table {
    /// Function code 04, read only.
    Input,
    /// Function code 03, read and written with 06/16.
    Holding,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    U16,
    S16,
    U32,
    S32,
    U64,
    String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Register {
    /// Which unit of the device this belongs to, e.g. `plant`; resolved through
    /// [`Map::units`].
    pub group: String,
    pub name: String,
    pub address: u16,
    pub count: u16,
    pub table: Table,
    /// `ro`, `rw` or `wo`.
    pub access: String,
    #[serde(rename = "type")]
    pub kind: Kind,
    /// Raw value = engineering value × gain.
    pub gain: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unit: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

impl Register {
    pub fn writable(&self) -> bool {
        self.access != "ro"
    }
}

/// A value as a person reads it: scaled, or text.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Value {
    Number(f64),
    Text(String),
}

impl Value {
    pub fn number(&self) -> Option<f64> {
        match self {
            Value::Number(n) => Some(*n),
            Value::Text(_) => None,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Map {
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub source: String,
    /// Modbus unit (slave) id per register group.
    pub units: BTreeMap<String, u8>,
    pub registers: Vec<Register>,
    #[serde(skip)]
    index: HashMap<String, usize>,
}

pub const BUILTIN_MAPS: &[(&str, &str)] = &[("sigenergy", include_str!("../../profiles/maps/sigenergy.json"))];

impl Map {
    pub fn load(spec: &str) -> Result<Self, String> {
        let text = match BUILTIN_MAPS.iter().find(|(n, _)| *n == spec) {
            Some((_, text)) => text.to_string(),
            None => std::fs::read_to_string(spec).map_err(|e| {
                format!(
                    "`{spec}` is neither a built-in register map ({}) nor a readable file: {e}",
                    BUILTIN_MAPS.iter().map(|(n, _)| *n).collect::<Vec<_>>().join(", ")
                )
            })?,
        };
        Self::parse(&text)
    }

    pub fn parse(text: &str) -> Result<Self, String> {
        let mut map: Map = serde_json::from_str(text).map_err(|e| e.to_string())?;
        map.index = map
            .registers
            .iter()
            .enumerate()
            .map(|(i, r)| (r.name.clone(), i))
            .collect();
        for register in &map.registers {
            if !map.units.contains_key(&register.group) {
                return Err(format!("register {} names unknown group {}", register.name, register.group));
            }
        }
        Ok(map)
    }

    pub fn register(&self, name: &str) -> Option<&Register> {
        self.index.get(name).map(|&i| &self.registers[i])
    }

    pub fn unit_of(&self, register: &Register) -> u8 {
        self.units[&register.group]
    }
}

/// Engineering value → register words, big-endian word order.
pub fn encode(register: &Register, value: &Value) -> Vec<u16> {
    let count = register.count as usize;
    match (register.kind, value) {
        (Kind::String, Value::Text(text)) => {
            let mut bytes: Vec<u8> = text.bytes().take(count * 2).collect();
            bytes.resize(count * 2, 0);
            bytes.chunks(2).map(|c| u16::from_be_bytes([c[0], c[1]])).collect()
        }
        (Kind::String, Value::Number(_)) => vec![0; count],
        (kind, value) => {
            let raw = (value.number().unwrap_or(0.0) * register.gain).round();
            let bits: u64 = match kind {
                Kind::U16 => (raw.clamp(0.0, u16::MAX as f64) as u64) & 0xffff,
                Kind::S16 => (raw.clamp(i16::MIN as f64, i16::MAX as f64) as i16 as u16) as u64,
                Kind::U32 => raw.clamp(0.0, u32::MAX as f64) as u64 & 0xffff_ffff,
                Kind::S32 => (raw.clamp(i32::MIN as f64, i32::MAX as f64) as i32 as u32) as u64,
                Kind::U64 => raw.max(0.0) as u64,
                Kind::String => unreachable!(),
            };
            let words = match kind {
                Kind::U16 | Kind::S16 => 1,
                Kind::U32 | Kind::S32 => 2,
                _ => 4,
            };
            let mut out: Vec<u16> = (0..words)
                .rev()
                .map(|i| ((bits >> (16 * i)) & 0xffff) as u16)
                .collect();
            out.resize(count.max(out.len()), 0);
            out.truncate(count);
            out
        }
    }
}

/// Register words → engineering value.
pub fn decode(register: &Register, words: &[u16]) -> Value {
    match register.kind {
        Kind::String => {
            let bytes: Vec<u8> = words.iter().flat_map(|w| w.to_be_bytes()).collect();
            Value::Text(
                String::from_utf8_lossy(&bytes)
                    .trim_end_matches(['\0', ' '])
                    .to_string(),
            )
        }
        kind => {
            let bits = words.iter().fold(0u64, |acc, w| (acc << 16) | *w as u64);
            let raw = match kind {
                Kind::U16 | Kind::U32 | Kind::U64 => bits as f64,
                Kind::S16 => bits as u16 as i16 as f64,
                Kind::S32 => bits as u32 as i32 as f64,
                Kind::String => unreachable!(),
            };
            Value::Number(if register.gain == 0.0 { raw } else { raw / register.gain })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reg(kind: Kind, count: u16, gain: f64) -> Register {
        Register {
            group: "plant".into(),
            name: "x".into(),
            address: 30000,
            count,
            table: Table::Input,
            access: "ro".into(),
            kind,
            gain,
            unit: None,
            description: None,
        }
    }

    #[test]
    fn numbers_round_trip_with_gain_and_sign() {
        let r = reg(Kind::S32, 2, 1000.0);
        let words = encode(&r, &Value::Number(-3.25));
        assert_eq!(words, vec![0xffff, 0xf34e]);
        assert_eq!(decode(&r, &words), Value::Number(-3.25));
        let soc = reg(Kind::U16, 1, 10.0);
        assert_eq!(decode(&soc, &encode(&soc, &Value::Number(63.4))), Value::Number(63.4));
        let energy = reg(Kind::U64, 4, 100.0);
        assert_eq!(decode(&energy, &encode(&energy, &Value::Number(12345.67))), Value::Number(12345.67));
    }

    #[test]
    fn strings_are_padded_and_trimmed() {
        let r = reg(Kind::String, 4, 1.0);
        let words = encode(&r, &Value::Text("ABC".into()));
        assert_eq!(words.len(), 4);
        assert_eq!(decode(&r, &words), Value::Text("ABC".into()));
    }

    #[test]
    fn the_sigenergy_map_loads() {
        let map = Map::load("sigenergy").unwrap();
        assert!(map.registers.len() > 300);
        let soc = map.register("plant_ess_soc").unwrap();
        assert_eq!((soc.address, map.unit_of(soc)), (30014, 247));
    }
}
