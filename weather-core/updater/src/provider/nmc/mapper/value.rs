use anyhow::{Context, Result, bail};
use reqwest::Url;
use serde::de::DeserializeOwned;
use serde_json::{Map, Value};

use super::MappingContext;

enum Candidate<T> {
    Missing,
    Invalid,
    Value(T),
}

pub(super) fn is_missing_value(value: &Value) -> bool {
    match value {
        Value::Null => true,
        Value::String(value) => {
            let value = value.trim();
            value.is_empty() || is_sentinel_number(value.parse::<f64>().ok())
        }
        Value::Number(value) => is_sentinel_number(value.as_f64()),
        Value::Object(map) => map.is_empty() || map.values().all(is_missing_value),
        Value::Array(values) => values.is_empty() || values.iter().all(is_missing_value),
        Value::Bool(_) => false,
    }
}

pub(super) fn decode_section<T: DeserializeOwned>(
    value: Option<Value>,
    path: &str,
    context: &mut MappingContext<'_>,
) -> Option<T> {
    let value = value?;
    if is_missing_value(&value) {
        return None;
    }
    if !value.is_object() {
        context.warn(format!("{path}: malformed object; ignored"));
        return None;
    }
    match serde_json::from_value(value) {
        Ok(value) => Some(value),
        Err(_) => {
            context.warn(format!("{path}: malformed object; ignored"));
            None
        }
    }
}

pub(super) fn section_object(
    value: Option<Value>,
    path: &str,
    context: &mut MappingContext<'_>,
) -> Option<Map<String, Value>> {
    let value = value?;
    if is_missing_value(&value) {
        return None;
    }
    match value {
        Value::Object(map) => Some(map),
        _ => {
            context.warn(format!("{path}: expected object; ignored"));
            None
        }
    }
}

pub(super) fn sequence(
    value: Option<Value>,
    path: &str,
    context: &mut MappingContext<'_>,
) -> Vec<(usize, Value)> {
    let Some(value) = value else {
        return Vec::new();
    };
    sequence_candidate(value, path, context)
}

pub(super) fn alias_sequence(
    map: &Map<String, Value>,
    aliases: &[&str],
    path: &str,
    context: &mut MappingContext<'_>,
) -> Vec<(usize, Value)> {
    let mut saw_invalid = false;
    for alias in aliases {
        let Some(value) = map.get(*alias) else {
            continue;
        };
        if is_missing_value(value) {
            continue;
        }
        match value {
            Value::Array(values) => {
                return values
                    .iter()
                    .cloned()
                    .enumerate()
                    .filter(|(_, value)| !is_missing_value(value))
                    .collect();
            }
            Value::Object(_) => return vec![(0, value.clone())],
            _ => saw_invalid = true,
        }
    }
    if saw_invalid {
        context.warn(format!(
            "{path}: no alias contained an array or object; ignored"
        ));
    }
    Vec::new()
}

pub(super) fn fixed_string(
    value: Option<&Value>,
    path: &str,
    context: &mut MappingContext<'_>,
) -> Option<String> {
    fixed(value, path, "string", string_candidate, context)
}

pub(super) fn fixed_f64(
    value: Option<&Value>,
    path: &str,
    context: &mut MappingContext<'_>,
) -> Option<f64> {
    fixed(value, path, "number", f64_candidate, context)
}

pub(super) fn alias_string(
    map: &Map<String, Value>,
    aliases: &[&str],
    path: &str,
    context: &mut MappingContext<'_>,
) -> Option<String> {
    alias(map, aliases, path, "string", string_candidate, context)
}

pub(super) fn alias_f64(
    map: &Map<String, Value>,
    aliases: &[&str],
    path: &str,
    context: &mut MappingContext<'_>,
) -> Option<f64> {
    alias(map, aliases, path, "number", f64_candidate, context)
}

pub(super) fn alias_u32(
    map: &Map<String, Value>,
    aliases: &[&str],
    path: &str,
    context: &mut MappingContext<'_>,
) -> Option<u32> {
    alias(
        map,
        aliases,
        path,
        "non-negative integer",
        u32_candidate,
        context,
    )
}

pub(super) fn resource_url(
    value: Option<&Value>,
    path: &str,
    context: &mut MappingContext<'_>,
) -> Option<String> {
    let reference = fixed(
        value,
        path,
        "resource URL string",
        resource_reference_candidate,
        context,
    )?;
    join_resource_url(context.base_url, &reference, path, context)
}

pub(super) fn alias_resource_url(
    map: &Map<String, Value>,
    aliases: &[&str],
    path: &str,
    context: &mut MappingContext<'_>,
) -> Option<String> {
    let mut saw_invalid = false;
    for alias in aliases {
        let Some(value) = map.get(*alias) else {
            continue;
        };
        match resource_reference_candidate(value) {
            Candidate::Missing => {}
            Candidate::Invalid => saw_invalid = true,
            Candidate::Value(reference) => match context.base_url.join(&reference) {
                Ok(url) => return Some(url.to_string()),
                Err(_) => saw_invalid = true,
            },
        }
    }
    if saw_invalid {
        context.warn(format!(
            "{path}: no alias contained a usable resource URL; field ignored"
        ));
    }
    None
}

pub(super) fn required_resource_url(base_url: &Url, reference: &str) -> Result<String> {
    let reference = reference.trim();
    if reference.is_empty() || is_sentinel_number(reference.parse::<f64>().ok()) {
        bail!("resource URL is empty or sentinel");
    }
    Ok(base_url
        .join(reference)
        .with_context(|| format!("failed to join resource URL `{reference}`"))?
        .to_string())
}

pub(super) fn has_non_missing_values(map: &Map<String, Value>) -> bool {
    map.values().any(|value| !is_missing_value(value))
}

fn fixed<T>(
    value: Option<&Value>,
    path: &str,
    expected: &str,
    convert: impl Fn(&Value) -> Candidate<T>,
    context: &mut MappingContext<'_>,
) -> Option<T> {
    match value.map(convert) {
        None | Some(Candidate::Missing) => None,
        Some(Candidate::Value(value)) => Some(value),
        Some(Candidate::Invalid) => {
            context.warn(format!("{path}: expected {expected}; field ignored"));
            None
        }
    }
}

fn alias<T>(
    map: &Map<String, Value>,
    aliases: &[&str],
    path: &str,
    expected: &str,
    convert: impl Fn(&Value) -> Candidate<T>,
    context: &mut MappingContext<'_>,
) -> Option<T> {
    let mut saw_invalid = false;
    for alias in aliases {
        let Some(value) = map.get(*alias) else {
            continue;
        };
        match convert(value) {
            Candidate::Missing => {}
            Candidate::Invalid => saw_invalid = true,
            Candidate::Value(value) => return Some(value),
        }
    }
    if saw_invalid {
        context.warn(format!(
            "{path}: no alias contained a usable {expected}; field ignored"
        ));
    }
    None
}

fn sequence_candidate(
    value: Value,
    path: &str,
    context: &mut MappingContext<'_>,
) -> Vec<(usize, Value)> {
    if is_missing_value(&value) {
        return Vec::new();
    }
    match value {
        Value::Array(values) => values
            .into_iter()
            .enumerate()
            .filter(|(_, value)| !is_missing_value(value))
            .collect(),
        Value::Object(_) => vec![(0, value)],
        _ => {
            context.warn(format!("{path}: expected array or object; ignored"));
            Vec::new()
        }
    }
}

fn string_candidate(value: &Value) -> Candidate<String> {
    if is_missing_value(value) {
        return Candidate::Missing;
    }
    match value {
        Value::String(value) => Candidate::Value(value.trim().to_string()),
        Value::Number(value) => Candidate::Value(value.to_string()),
        _ => Candidate::Invalid,
    }
}

fn resource_reference_candidate(value: &Value) -> Candidate<String> {
    if is_missing_value(value) {
        return Candidate::Missing;
    }
    match value {
        Value::String(value) => Candidate::Value(value.trim().to_string()),
        _ => Candidate::Invalid,
    }
}

fn f64_candidate(value: &Value) -> Candidate<f64> {
    if is_missing_value(value) {
        return Candidate::Missing;
    }
    let parsed = match value {
        Value::Number(value) => value.as_f64(),
        Value::String(value) => value.trim().parse::<f64>().ok(),
        _ => None,
    };
    match parsed {
        Some(value) if value.is_finite() => Candidate::Value(value),
        _ => Candidate::Invalid,
    }
}

fn u32_candidate(value: &Value) -> Candidate<u32> {
    if is_missing_value(value) {
        return Candidate::Missing;
    }
    let parsed = match value {
        Value::Number(value) => value.as_u64().and_then(|value| u32::try_from(value).ok()),
        Value::String(value) => value.trim().parse::<u32>().ok(),
        _ => None,
    };
    parsed.map_or(Candidate::Invalid, Candidate::Value)
}

fn join_resource_url(
    base_url: &Url,
    reference: &str,
    path: &str,
    context: &mut MappingContext<'_>,
) -> Option<String> {
    match base_url.join(reference) {
        Ok(url) => Some(url.to_string()),
        Err(_) => {
            context.warn(format!("{path}: invalid resource URL; field ignored"));
            None
        }
    }
}

fn is_sentinel_number(value: Option<f64>) -> bool {
    value.is_some_and(|value| value == 9999.0)
}
