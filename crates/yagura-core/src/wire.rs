//! Reading back the few `&'static str` fields of the readings: each name is
//! kept once for good (see [`intern`]). Used by `#[serde(deserialize_with)]`.

use std::sync::Mutex;

use serde::{Deserialize, Deserializer};

/// Names kept for good (each `&'static str` an agent sends: a source, a
/// vendor, a bus). They are a handful of words; past this many, or past
/// [`NAME_MAX`] bytes, an agent is sending something else and gets `?`.
const NAMES_MAX: usize = 256;
pub const NAME_MAX: usize = 64;

static NAMES: Mutex<Vec<&'static str>> = Mutex::new(Vec::new());

/// `s` as a `&'static str`, leaking each new name once.
pub fn intern(s: &str) -> &'static str {
    let mut names = NAMES.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(n) = names.iter().find(|n| **n == s) {
        return n;
    }
    if s.len() > NAME_MAX || names.len() >= NAMES_MAX {
        return "?";
    }
    let n: &'static str = Box::leak(s.to_string().into_boxed_str());
    names.push(n);
    n
}

/// For `#[serde(deserialize_with)]` on a `&'static str` field.
pub fn name<'de, D: Deserializer<'de>>(d: D) -> Result<&'static str, D::Error> {
    Ok(intern(&String::deserialize(d)?))
}

/// For `#[serde(deserialize_with)]` on an `Option<&'static str>` field.
pub fn opt_name<'de, D: Deserializer<'de>>(d: D) -> Result<Option<&'static str>, D::Error> {
    Ok(Option::<String>::deserialize(d)?.map(|s| intern(&s)))
}
