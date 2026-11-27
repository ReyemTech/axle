//! Byte-preserving JSON / JSONC edits for harness configs, on `jsonc-parser`'s CST:
//! comments, key order, indentation and trailing commas outside the edited spot survive.

use std::path::Path;

use jsonc_parser::ParseOptions;
use jsonc_parser::cst::{CstInputValue, CstObject, CstRootNode};
use serde_json::Value;

use super::EditError;

/// Parses `text` (blank text counts as `{}`), requiring an object at the root. A file that
/// does not parse, or is not an object, is a manual step: `snippet` is what to add by hand.
pub fn parse_object(
    file: &Path,
    text: &str,
    snippet: &str,
) -> Result<(CstRootNode, CstObject), EditError> {
    let manual = |reason: String| EditError::NeedsManual {
        file: file.to_path_buf(),
        reason,
        snippet: snippet.to_string(),
    };
    let source = if text.trim().is_empty() { "{}\n" } else { text };
    let root = CstRootNode::parse(source, &ParseOptions::default()).map_err(|e| {
        manual(format!(
            "it is not valid JSON ({e}); axl will not rewrite it"
        ))
    })?;
    let obj = root
        .object_value()
        .ok_or_else(|| manual("its top level is not a JSON object".to_string()))?;
    Ok((root, obj))
}

/// `{ ... }` as an ordered input value.
pub fn object(props: Vec<(&str, CstInputValue)>) -> CstInputValue {
    CstInputValue::Object(props.into_iter().map(|(k, v)| (k.to_string(), v)).collect())
}

pub fn string(s: &str) -> CstInputValue {
    CstInputValue::String(s.to_string())
}

pub fn strings(items: &[&str]) -> CstInputValue {
    CstInputValue::Array(items.iter().map(|s| string(s)).collect())
}

pub fn number(n: u32) -> CstInputValue {
    CstInputValue::Number(n.to_string())
}

/// The plain value an input describes, for comparing with what is on disk.
pub fn to_value(v: &CstInputValue) -> Value {
    match v {
        CstInputValue::Null => Value::Null,
        CstInputValue::Bool(b) => Value::Bool(*b),
        CstInputValue::Number(n) => serde_json::from_str(n).unwrap_or(Value::Null),
        CstInputValue::String(s) => Value::String(s.clone()),
        CstInputValue::Array(a) => Value::Array(a.iter().map(to_value).collect()),
        CstInputValue::Object(o) => {
            Value::Object(o.iter().map(|(k, v)| (k.clone(), to_value(v))).collect())
        }
    }
}

/// A new pretty-printed JSON document holding `value`, ending in a newline.
pub fn render(value: CstInputValue) -> String {
    let root = CstRootNode::parse("{}\n", &ParseOptions::default()).expect("literal parses");
    root.set_value(value);
    root.to_string()
}
