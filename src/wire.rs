//! Between plain JSON and the EEBUS JSON-UTF8 projection (SHIP §11.4).
//!
//! Captures from `elli-eebus-proxy` (pyeebus) and `eebus-go` are ordinary JSON objects;
//! SHIP puts SPINE on the wire as arrays of single-key objects, and that is what the
//! `eebus` crate reads. The conversion is mechanical; this is the same rule as the
//! crate's own `xtask devices` converter.

use eebus::model::{Cmd, Datagram};
use serde_json::{Map, Value};

/// Plain JSON → wire form.
pub fn to_wire(value: &Value) -> Value {
    match value {
        Value::Object(fields) => Value::Array(
            fields
                .iter()
                .map(|(key, val)| single(key, to_wire(val)))
                .collect(),
        ),
        // A repeated complex element is an array of sequences; a repeated scalar one
        // stays a plain array (`{"entity":[0]}`).
        Value::Array(items) if !items.is_empty() && items.iter().all(Value::is_object) => {
            Value::Array(items.iter().map(to_wire).collect())
        }
        other => other.clone(),
    }
}

fn single(key: &str, value: Value) -> Value {
    let mut object = Map::new();
    object.insert(key.to_string(), value);
    Value::Object(object)
}

/// A datagram from either form: `{"datagram": …}` with plain objects or wire arrays.
pub fn datagram(value: &Value) -> Result<Datagram, String> {
    let inner = value.get("datagram").ok_or("no `datagram` element")?;
    let wire = if inner.is_array() {
        inner.clone()
    } else {
        to_wire(inner)
    };
    let mut root = Map::new();
    root.insert("datagram".into(), wire);
    eebus::model::from_json_value(Value::Object(root)).map_err(|e| e.to_string())
}

/// A `cmd` element as stored in a replay profile.
pub fn cmd_to_value(cmd: &Cmd) -> Value {
    serde_json::to_value(cmd).expect("a parsed command re-encodes")
}

pub fn cmd_from_value(value: &Value) -> Result<Cmd, String> {
    serde_json::from_value(value.clone()).map_err(|e| e.to_string())
}

/// Wire form → plain JSON, for logs and people. The inverse of [`to_wire`] where the
/// shapes allow it.
pub fn to_plain(value: &Value) -> Value {
    match value {
        Value::Array(items) if !items.is_empty() && items.iter().all(is_single) => {
            let mut object = Map::new();
            for item in items {
                if let Value::Object(map) = item {
                    for (key, val) in map {
                        object.insert(key.clone(), to_plain(val));
                    }
                }
            }
            Value::Object(object)
        }
        Value::Array(items) => Value::Array(items.iter().map(to_plain).collect()),
        Value::Object(map) => Value::Object(
            map.iter().map(|(k, v)| (k.clone(), to_plain(v))).collect(),
        ),
        other => other.clone(),
    }
}

fn is_single(value: &Value) -> bool {
    matches!(value, Value::Object(map) if map.len() == 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_and_wire_forms_meet() {
        let plain = serde_json::json!({"datagram": {
            "header": {"specificationVersion": "1.3.0", "addressSource": {"device": "d:_i:1_X", "entity": [1], "feature": 2},
                       "msgCounter": 5, "cmdClassifier": "notify"},
            "payload": {"cmd": [{"measurementListData": {"measurementData": [
                {"measurementId": 1, "valueType": "value", "value": {"number": 1234, "scale": 0}}]}}]}}});
        let datagram = datagram(&plain).unwrap();
        let cmd = &datagram.payload.as_ref().unwrap().cmd.as_ref().unwrap()[0];
        let stored = cmd_to_value(cmd);
        assert_eq!(&cmd_from_value(&stored).unwrap(), cmd);
        assert!(stored.is_array(), "stored in wire form: {stored}");
    }
}
