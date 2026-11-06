//! Small helpers for poking at `plist::Dictionary` values without panicking.

use plist::{Dictionary, Value};

pub fn get_str<'a>(d: &'a Dictionary, key: &str) -> Option<&'a str> {
    d.get(key).and_then(Value::as_string)
}

pub fn get_u64(d: &Dictionary, key: &str) -> Option<u64> {
    value_u64(d.get(key)?)
}

pub fn value_u64(v: &Value) -> Option<u64> {
    match v {
        Value::Integer(i) => i.as_unsigned().or_else(|| i.as_signed().and_then(|s| u64::try_from(s).ok())),
        Value::Real(r) if *r >= 0.0 && r.fract() == 0.0 && *r < u64::MAX as f64 => Some(*r as u64),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

pub fn get_bool(d: &Dictionary, key: &str) -> Option<bool> {
    match d.get(key)? {
        Value::Boolean(b) => Some(*b),
        Value::Integer(i) => i.as_signed().map(|v| v != 0),
        Value::String(s) => match s.as_str() {
            "true" | "YES" | "1" => Some(true),
            "false" | "NO" | "0" => Some(false),
            _ => None,
        },
        _ => None,
    }
}

/// Data value; also accepts a string (some tools store PEM as a string).
pub fn get_data(d: &Dictionary, key: &str) -> Option<Vec<u8>> {
    match d.get(key)? {
        Value::Data(b) => Some(b.clone()),
        Value::String(s) => Some(s.as_bytes().to_vec()),
        _ => None,
    }
}

pub fn get_dict<'a>(d: &'a Dictionary, key: &str) -> Option<&'a Dictionary> {
    d.get(key).and_then(Value::as_dictionary)
}

/// Serialize a dictionary as an XML plist.
pub fn to_xml(d: &Dictionary) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    Value::Dictionary(d.clone()).to_writer_xml(&mut out).map_err(|e| e.to_string())?;
    Ok(out)
}

/// Parse either an XML or a binary plist whose root is a dictionary.
pub fn from_bytes(b: &[u8]) -> Result<Dictionary, String> {
    let v: Value = plist::from_bytes(b).map_err(|e| e.to_string())?;
    match v {
        Value::Dictionary(d) => Ok(d),
        _ => Err("plist root is not a dictionary".into()),
    }
}

/// Build a `plist::Dictionary` literal: `dict! { "Key" => "value", "N" => 1u64 }`.
#[macro_export]
macro_rules! dict {
    ($($k:expr => $v:expr),* $(,)?) => {{
        #[allow(unused_mut)]
        let mut d = ::plist::Dictionary::new();
        $( d.insert(String::from($k), ::plist::Value::from($v)); )*
        d
    }};
}
