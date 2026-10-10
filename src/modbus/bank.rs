//! The registers a simulated Modbus device serves.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use tokio_modbus::ExceptionCode;

use super::map::{Map, Table, Value, decode, encode};

#[derive(Default, Clone)]
struct Unit {
    input: BTreeMap<u16, u16>,
    holding: BTreeMap<u16, u16>,
    /// Holding addresses a client may write.
    writable: std::collections::BTreeSet<u16>,
}

/// Every register of every unit, with the map that names them.
pub struct Bank {
    pub map: Arc<Map>,
    units: HashMap<u8, Unit>,
}

impl Bank {
    /// All registers of the map present and zero, so a client reading a block the
    /// device documents never meets a gap.
    pub fn new(map: Arc<Map>) -> Self {
        let mut units: HashMap<u8, Unit> = HashMap::new();
        for register in &map.registers {
            let unit = units.entry(map.unit_of(register)).or_default();
            let table = match register.table {
                Table::Input => &mut unit.input,
                Table::Holding => &mut unit.holding,
            };
            for offset in 0..register.count {
                table.insert(register.address + offset, 0);
                if register.table == Table::Holding && register.writable() {
                    unit.writable.insert(register.address + offset);
                }
            }
        }
        Self { map, units }
    }

    pub fn set(&mut self, name: &str, value: impl Into<Value>) {
        let Some(register) = self.map.register(name) else {
            return;
        };
        let words = encode(register, &value.into());
        let unit_id = self.map.unit_of(register);
        let (address, table) = (register.address, register.table);
        self.write_words(unit_id, table, address, &words);
    }

    pub fn get(&self, name: &str) -> Option<Value> {
        let register = self.map.register(name)?;
        let unit = self.units.get(&self.map.unit_of(register))?;
        let table = match register.table {
            Table::Input => &unit.input,
            Table::Holding => &unit.holding,
        };
        let words: Option<Vec<u16>> = (0..register.count)
            .map(|o| table.get(&(register.address + o)).copied())
            .collect();
        Some(decode(register, &words?))
    }

    pub fn number(&self, name: &str) -> f64 {
        self.get(name).and_then(|v| v.number()).unwrap_or(0.0)
    }

    /// Raw words, for replaying a recording.
    pub fn write_words(&mut self, unit: u8, table: Table, address: u16, words: &[u16]) {
        let unit = self.units.entry(unit).or_default();
        let target = match table {
            Table::Input => &mut unit.input,
            Table::Holding => &mut unit.holding,
        };
        for (offset, word) in words.iter().enumerate() {
            target.insert(address + offset as u16, *word);
        }
    }

    pub fn read(&self, unit: u8, table: Table, address: u16, quantity: u16) -> Result<Vec<u16>, ExceptionCode> {
        let unit = self.units.get(&unit).ok_or(ExceptionCode::GatewayTargetDevice)?;
        let source = match table {
            Table::Input => &unit.input,
            Table::Holding => &unit.holding,
        };
        (0..quantity)
            .map(|o| {
                address
                    .checked_add(o)
                    .and_then(|a| source.get(&a).copied())
                    .ok_or(ExceptionCode::IllegalDataAddress)
            })
            .collect()
    }

    /// A client's write. Returns the names of the registers it touched.
    pub fn write(&mut self, unit_id: u8, address: u16, words: &[u16]) -> Result<Vec<String>, ExceptionCode> {
        let unit = self.units.get_mut(&unit_id).ok_or(ExceptionCode::GatewayTargetDevice)?;
        for offset in 0..words.len() as u16 {
            if !unit.writable.contains(&(address + offset)) {
                return Err(ExceptionCode::IllegalDataAddress);
            }
        }
        for (offset, word) in words.iter().enumerate() {
            unit.holding.insert(address + offset as u16, *word);
        }
        let end = address + words.len() as u16;
        Ok(self
            .map
            .registers
            .iter()
            .filter(|r| {
                r.table == Table::Holding
                    && self.map.unit_of(r) == unit_id
                    && r.address < end
                    && address < r.address + r.count
            })
            .map(|r| r.name.clone())
            .collect())
    }
}

impl From<f64> for Value {
    fn from(n: f64) -> Self {
        Value::Number(n)
    }
}

impl From<&str> for Value {
    fn from(s: &str) -> Self {
        Value::Text(s.to_string())
    }
}
