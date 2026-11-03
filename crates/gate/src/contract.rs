//! Strict response contracts (Track E §1.1–§1.3): a closed subset of
//! JSON Schema, validated by fl-gate itself, plus finite response bounds.

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::{Map, Value};

use crate::GateError;

const ALLOWED_KEYS: &[&str] = &[
    "type",
    "required",
    "properties",
    "additionalProperties",
    "items",
    "enum",
    "const",
    "minimum",
    "maximum",
    "minLength",
    "maxLength",
    "minItems",
    "maxItems",
];
const ALLOWED_TYPES: &[&str] = &[
    "object", "array", "string", "number", "integer", "boolean", "null",
];

fn unsupported(construct: impl Into<String>) -> GateError {
    GateError::ContractUnsupportedConstruct {
        construct: construct.into(),
    }
}

/// A response schema in the strict, closed subset. Stored as canonical
/// JSON (serde_json object keys are ordered, never `preserve_order`).
#[derive(Debug, Clone, PartialEq)]
pub struct ResponseSchema(Value);

impl ResponseSchema {
    pub fn from_value(value: Value) -> Result<Self, GateError> {
        validate_schema(&value)?;
        Ok(Self(value))
    }

    pub fn as_value(&self) -> &Value {
        &self.0
    }

    pub fn value(&self) -> &Value {
        &self.0
    }

    pub fn into_value(self) -> Value {
        self.0
    }
}

impl Serialize for ResponseSchema {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.0.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for ResponseSchema {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = Value::deserialize(deserializer)?;
        Self::from_value(value).map_err(serde::de::Error::custom)
    }
}

fn validate_schema(value: &Value) -> Result<(), GateError> {
    let obj: &Map<String, Value> = value
        .as_object()
        .ok_or_else(|| unsupported("schema_not_object"))?;
    for key in obj.keys() {
        if !ALLOWED_KEYS.contains(&key.as_str()) {
            return Err(unsupported(key.clone()));
        }
    }
    let ty: Option<&str> = match obj.get("type") {
        Some(v) => {
            let s = v.as_str().ok_or_else(|| unsupported("type"))?;
            if !ALLOWED_TYPES.contains(&s) {
                return Err(unsupported("type"));
            }
            Some(s)
        }
        None => None,
    };
    if ty.is_none() && !obj.contains_key("enum") && !obj.contains_key("const") {
        return Err(unsupported("type"));
    }
    if let Some(ap) = obj.get("additionalProperties")
        && ap.as_bool() != Some(false)
    {
        return Err(unsupported("additionalProperties"));
    }
    let has_object_keys = obj.contains_key("properties")
        || obj.contains_key("required")
        || obj.contains_key("additionalProperties");
    if has_object_keys && ty != Some("object") {
        return Err(unsupported("type"));
    }
    if ty == Some("array") {
        match obj.get("items") {
            Some(items) if items.is_object() => {}
            _ => return Err(unsupported("items")),
        }
    }
    let is_object_schema = ty == Some("object") || has_object_keys;
    if is_object_schema && obj.get("additionalProperties").and_then(Value::as_bool) != Some(false) {
        return Err(unsupported("additionalProperties"));
    }
    if let Some(req) = obj.get("required") {
        let arr = req.as_array().ok_or_else(|| unsupported("required"))?;
        for item in arr {
            if !item.is_string() {
                return Err(unsupported("required"));
            }
        }
    }
    if let Some(props) = obj.get("properties") {
        let props = props.as_object().ok_or_else(|| unsupported("properties"))?;
        for sub in props.values() {
            validate_schema(sub)?;
        }
    }
    if let Some(items) = obj.get("items") {
        validate_schema(items)?;
    }
    if let Some(e) = obj.get("enum") {
        let arr = e.as_array().ok_or_else(|| unsupported("enum"))?;
        if arr.is_empty() {
            return Err(unsupported("enum"));
        }
    }
    // `const` accepts any JSON value.
    for key in ["minimum", "maximum"] {
        if let Some(v) = obj.get(key)
            && !v.is_number()
        {
            return Err(unsupported(key));
        }
    }
    for key in ["minLength", "maxLength", "minItems", "maxItems"] {
        if let Some(v) = obj.get(key)
            && !v.is_u64()
        {
            return Err(unsupported(key));
        }
    }
    if let (Some(a), Some(b)) = (obj.get("minimum"), obj.get("maximum"))
        && a.as_f64().zip(b.as_f64()).is_some_and(|(a, b)| a > b)
    {
        return Err(unsupported("minimum"));
    }
    for (lo, hi) in [("minLength", "maxLength"), ("minItems", "maxItems")] {
        if let (Some(a), Some(b)) = (obj.get(lo), obj.get(hi))
            && a.as_u64().zip(b.as_u64()).is_some_and(|(a, b)| a > b)
        {
            return Err(unsupported(lo));
        }
    }
    Ok(())
}

/// A response contract bound to a tool: strict schema plus finite bounds.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ResponseContract {
    pub schema: ResponseSchema,
    pub max_response_bytes: u64,
    pub max_response_tokens: Option<u64>,
    pub max_depth: u32,
    pub max_array_len: u64,
    pub max_string_len: u64,
}

impl ResponseContract {
    pub fn validate_load(&self) -> Result<(), GateError> {
        if self.max_response_bytes == 0 {
            return Err(GateError::ContractUnenforceable);
        }
        if self.max_response_tokens.is_some() && fl_core::token_count::count_unit().is_none() {
            return Err(GateError::ContractUnenforceable);
        }
        Ok(())
    }

    pub fn validate_value(&self, value: &Value) -> Result<(), ContractViolation> {
        validate_value(self, value)
    }
}

/// A single contract violation. Display names the dotted path and rule.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ContractViolation {
    #[error("contract_violation at {path}: type_mismatch expected {expected}, found {actual}")]
    TypeMismatch {
        path: String,
        expected: String,
        actual: String,
    },
    #[error("contract_violation at {path}: required_missing property `{property}`")]
    MissingRequired { path: String, property: String },
    #[error("contract_violation at {path}: additional_property_refused property `{property}`")]
    AdditionalProperty { path: String, property: String },
    #[error("contract_violation at {path}: enum_mismatch")]
    EnumMismatch { path: String },
    #[error("contract_violation at {path}: const_mismatch")]
    ConstMismatch { path: String },
    #[error("contract_violation at {path}: minimum_violation below minimum {minimum}")]
    Minimum { path: String, minimum: String },
    #[error("contract_violation at {path}: maximum_violation above maximum {maximum}")]
    Maximum { path: String, maximum: String },
    #[error(
        "contract_violation at {path}: min_length_violation length {actual} below minLength {min}"
    )]
    MinLength { path: String, min: u64, actual: u64 },
    #[error(
        "contract_violation at {path}: max_length_violation length {actual} above maxLength {max}"
    )]
    MaxLength { path: String, max: u64, actual: u64 },
    #[error(
        "contract_violation at {path}: min_items_violation length {actual} below minItems {min}"
    )]
    MinItems { path: String, min: u64, actual: u64 },
    #[error(
        "contract_violation at {path}: max_items_violation length {actual} above maxItems {max}"
    )]
    MaxItems { path: String, max: u64, actual: u64 },
    #[error(
        "contract_violation at {path}: max_depth_exceeded depth {actual} above max_depth {max}"
    )]
    MaxDepth { path: String, max: u32, actual: u32 },
    #[error(
        "contract_violation at {path}: max_array_len_exceeded length {actual} above max_array_len {max}"
    )]
    MaxArrayLen { path: String, max: u64, actual: u64 },
    #[error(
        "contract_violation at {path}: max_string_len_exceeded length {actual} above max_string_len {max}"
    )]
    MaxStringLen { path: String, max: u64, actual: u64 },
    #[error(
        "contract_violation at {path}: max_response_tokens_exceeded tokens {actual} above max_response_tokens {max}"
    )]
    TokenLimitExceeded { path: String, max: u64, actual: u64 },
    #[error(
        "contract_violation at {path}: token_counter_unavailable max_response_tokens cannot be counted in this build"
    )]
    TokenCounterUnavailable { path: String },
}

fn type_name(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

fn join_path(path: &str, key: &str) -> String {
    format!("{path}.{key}")
}

pub fn validate_value(contract: &ResponseContract, value: &Value) -> Result<(), ContractViolation> {
    validate_node(contract, contract.schema.as_value(), value, "$", 1)
}

fn validate_node(
    contract: &ResponseContract,
    schema: &Value,
    value: &Value,
    path: &str,
    depth: u32,
) -> Result<(), ContractViolation> {
    if depth > contract.max_depth {
        return Err(ContractViolation::MaxDepth {
            path: path.to_string(),
            max: contract.max_depth,
            actual: depth,
        });
    }
    match value {
        Value::Array(a) if a.len() as u64 > contract.max_array_len => {
            return Err(ContractViolation::MaxArrayLen {
                path: path.to_string(),
                max: contract.max_array_len,
                actual: a.len() as u64,
            });
        }
        Value::String(s) if s.chars().count() as u64 > contract.max_string_len => {
            return Err(ContractViolation::MaxStringLen {
                path: path.to_string(),
                max: contract.max_string_len,
                actual: s.chars().count() as u64,
            });
        }
        _ => {}
    }

    let obj = schema.as_object();
    let get = |k: &str| obj.and_then(|o| o.get(k));

    if let Some(vals) = get("enum").and_then(Value::as_array)
        && !vals.iter().any(|v| v == value)
    {
        return Err(ContractViolation::EnumMismatch {
            path: path.to_string(),
        });
    }
    if let Some(c) = get("const")
        && c != value
    {
        return Err(ContractViolation::ConstMismatch {
            path: path.to_string(),
        });
    }

    if let Some(ty) = get("type").and_then(Value::as_str) {
        let ok = match ty {
            "object" => value.is_object(),
            "array" => value.is_array(),
            "string" => value.is_string(),
            "number" => value.is_number(),
            "integer" => value.as_number().is_some_and(|n| n.is_i64() || n.is_u64()),
            "boolean" => value.is_boolean(),
            "null" => value.is_null(),
            _ => false,
        };
        if !ok {
            return Err(ContractViolation::TypeMismatch {
                path: path.to_string(),
                expected: ty.to_string(),
                actual: type_name(value).to_string(),
            });
        }
    }

    match value {
        Value::Object(map) => {
            if let Some(req) = get("required").and_then(Value::as_array) {
                for r in req.iter().filter_map(Value::as_str) {
                    if !map.contains_key(r) {
                        return Err(ContractViolation::MissingRequired {
                            path: join_path(path, r),
                            property: r.to_string(),
                        });
                    }
                }
            }
            let props = get("properties").and_then(Value::as_object);
            let matched_solely_by_enum_or_const = get("type").is_none()
                && get("properties").is_none()
                && get("required").is_none()
                && get("additionalProperties").is_none()
                && (get("enum").is_some() || get("const").is_some());
            if !matched_solely_by_enum_or_const {
                // Closed object: no declared properties means every key
                // is additional.
                for key in map.keys() {
                    if !props.is_some_and(|p| p.contains_key(key)) {
                        return Err(ContractViolation::AdditionalProperty {
                            path: join_path(path, key),
                            property: key.clone(),
                        });
                    }
                }
            }
            for (key, v) in map {
                let child_path = join_path(path, key);
                if let Some(sub) = props.and_then(|p| p.get(key)) {
                    validate_node(contract, sub, v, &child_path, depth + 1)?;
                } else {
                    // No subschema: contract bounds still apply at every node.
                    validate_bounds_only(contract, v, &child_path, depth + 1)?;
                }
            }
        }
        Value::Array(arr) => {
            if let Some(min) = get("minItems").and_then(Value::as_u64)
                && (arr.len() as u64) < min
            {
                return Err(ContractViolation::MinItems {
                    path: path.to_string(),
                    min,
                    actual: arr.len() as u64,
                });
            }
            if let Some(max) = get("maxItems").and_then(Value::as_u64)
                && (arr.len() as u64) > max
            {
                return Err(ContractViolation::MaxItems {
                    path: path.to_string(),
                    max,
                    actual: arr.len() as u64,
                });
            }
            for (i, item) in arr.iter().enumerate() {
                let child_path = format!("{path}[{i}]");
                if let Some(items) = get("items") {
                    validate_node(contract, items, item, &child_path, depth + 1)?;
                } else {
                    validate_bounds_only(contract, item, &child_path, depth + 1)?;
                }
            }
        }
        Value::String(s) => {
            let len = s.chars().count() as u64;
            if let Some(min) = get("minLength").and_then(Value::as_u64)
                && len < min
            {
                return Err(ContractViolation::MinLength {
                    path: path.to_string(),
                    min,
                    actual: len,
                });
            }
            if let Some(max) = get("maxLength").and_then(Value::as_u64)
                && len > max
            {
                return Err(ContractViolation::MaxLength {
                    path: path.to_string(),
                    max,
                    actual: len,
                });
            }
        }
        Value::Number(n) => {
            if let Some(min) = get("minimum")
                && n.as_f64().zip(min.as_f64()).is_some_and(|(v, m)| v < m)
            {
                return Err(ContractViolation::Minimum {
                    path: path.to_string(),
                    minimum: min.to_string(),
                });
            }
            if let Some(max) = get("maximum")
                && n.as_f64().zip(max.as_f64()).is_some_and(|(v, m)| v > m)
            {
                return Err(ContractViolation::Maximum {
                    path: path.to_string(),
                    maximum: max.to_string(),
                });
            }
        }
        _ => {}
    }
    Ok(())
}

fn validate_bounds_only(
    contract: &ResponseContract,
    value: &Value,
    path: &str,
    depth: u32,
) -> Result<(), ContractViolation> {
    // No schema: only contract bounds, applied recursively. This is not
    // a schema node, so closed-object checking does not apply here.
    if depth > contract.max_depth {
        return Err(ContractViolation::MaxDepth {
            path: path.to_string(),
            max: contract.max_depth,
            actual: depth,
        });
    }
    match value {
        Value::Array(a) if a.len() as u64 > contract.max_array_len => {
            return Err(ContractViolation::MaxArrayLen {
                path: path.to_string(),
                max: contract.max_array_len,
                actual: a.len() as u64,
            });
        }
        Value::String(s) if s.chars().count() as u64 > contract.max_string_len => {
            return Err(ContractViolation::MaxStringLen {
                path: path.to_string(),
                max: contract.max_string_len,
                actual: s.chars().count() as u64,
            });
        }
        _ => {}
    }
    match value {
        Value::Object(map) => {
            for (key, v) in map {
                validate_bounds_only(contract, v, &join_path(path, key), depth + 1)?;
            }
        }
        Value::Array(arr) => {
            for (i, item) in arr.iter().enumerate() {
                validate_bounds_only(contract, item, &format!("{path}[{i}]"), depth + 1)?;
            }
        }
        _ => {}
    }
    Ok(())
}

/// Iterative, string/escape-aware bracket-depth scan run before any JSON
/// parse. Returns the maximum nesting depth of `{`/`[` in `text`.
pub fn bracket_depth(text: &str) -> u32 {
    let mut depth: u32 = 0;
    let mut max_seen: u32 = 0;
    let mut in_string = false;
    let mut escaped = false;
    for c in text.chars() {
        if in_string {
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                in_string = false;
            }
            continue;
        }
        match c {
            '"' => in_string = true,
            '{' | '[' => {
                depth = depth.saturating_add(1);
                max_seen = max_seen.max(depth);
            }
            '}' | ']' => depth = depth.saturating_sub(1),
            _ => {}
        }
    }
    max_seen
}

/// Refuse `text` before parsing when its bracket depth exceeds the bound.
pub fn check_pre_parse_depth(
    contract: &ResponseContract,
    text: &str,
) -> Result<(), ContractViolation> {
    let actual = bracket_depth(text);
    // A bracketed root is depth 1, matching `validate_value`'s root depth.
    if actual > contract.max_depth {
        return Err(ContractViolation::MaxDepth {
            path: "$".to_string(),
            max: contract.max_depth,
            actual,
        });
    }
    Ok(())
}

/// The token count unit as its serialized string (`o200k_base`), if any.
pub fn count_unit_string() -> Option<String> {
    fl_core::token_count::count_unit().map(|u| match u {
        fl_core::token_count::CountUnit::O200kBase => "o200k_base".to_string(),
    })
}

/// Count `text` in the contract's token bound. `Ok(None)` when the
/// contract carries no token bound.
pub fn count_tokens_bounded(
    contract: &ResponseContract,
    text: &str,
) -> Result<Option<u64>, ContractViolation> {
    let Some(max) = contract.max_response_tokens else {
        return Ok(None);
    };
    let count = fl_core::token_count::counted_tokens(text).ok_or_else(|| {
        ContractViolation::TokenCounterUnavailable {
            path: "$".to_string(),
        }
    })?;
    if count > max {
        return Err(ContractViolation::TokenLimitExceeded {
            path: "$".to_string(),
            max,
            actual: count,
        });
    }
    Ok(Some(count))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn contract(schema: Value) -> ResponseContract {
        ResponseContract {
            schema: ResponseSchema::from_value(schema).unwrap(),
            max_response_bytes: 1_000_000,
            max_response_tokens: None,
            max_depth: 32,
            max_array_len: 1000,
            max_string_len: 100_000,
        }
    }

    fn nested_schema() -> Value {
        json!({
            "type": "object", "additionalProperties": false,
            "required": ["user"],
            "properties": {
                "user": {
                    "type": "object", "additionalProperties": false,
                    "required": ["name", "tags"],
                    "properties": {
                        "name": {"type": "string", "minLength": 1},
                        "tags": {"type": "array", "items": {"type": "string"}, "maxItems": 3},
                        "score": {"type": "integer", "minimum": 0, "maximum": 10}
                    }
                }
            }
        })
    }

    #[test]
    fn additional_property_refused() {
        let c = contract(json!({
            "type": "object", "additionalProperties": false,
            "required": ["id"],
            "properties": {"id": {"type": "string"}}
        }));
        assert!(validate_value(&c, &json!({"id": "x"})).is_ok());
        let err = validate_value(&c, &json!({"id": "x", "extra": 1})).unwrap_err();
        assert!(
            matches!(err, ContractViolation::AdditionalProperty { .. }),
            "{err}"
        );
        assert!(err.to_string().contains("$.extra"), "{err}");
    }

    #[test]
    fn closed_object_without_properties_refuses_all_keys() {
        let c = contract(json!({"type": "object", "additionalProperties": false}));
        assert!(validate_value(&c, &json!({})).is_ok());
        let err = validate_value(&c, &json!({"SECRET_SENTINEL": 1})).unwrap_err();
        assert!(
            matches!(err, ContractViolation::AdditionalProperty { .. }),
            "{err}"
        );
        assert!(err.to_string().contains("$.SECRET_SENTINEL"), "{err}");
    }

    #[test]
    fn minimum_maximum_carry_bound_and_path_only() {
        // The offending body value (424242) must not be stored or displayed.
        let c = contract(json!({"type": "integer", "minimum": 999999}));
        let err = validate_value(&c, &json!(424242)).unwrap_err();
        assert!(
            matches!(&err, ContractViolation::Minimum { path, minimum } if path == "$" && minimum == "999999"),
            "{err}"
        );
        assert_eq!(
            err.to_string(),
            "contract_violation at $: minimum_violation below minimum 999999"
        );
        assert!(!err.to_string().contains("424242"), "{err}");
        let c = contract(json!({"type": "integer", "maximum": 10}));
        let err = validate_value(&c, &json!(424242)).unwrap_err();
        assert!(
            matches!(&err, ContractViolation::Maximum { path, maximum } if path == "$" && maximum == "10"),
            "{err}"
        );
        assert_eq!(
            err.to_string(),
            "contract_violation at $: maximum_violation above maximum 10"
        );
        assert!(!err.to_string().contains("424242"), "{err}");
    }

    #[test]
    fn pre_parse_depth_scan_is_string_aware() {
        assert_eq!(bracket_depth(r#"{"a":"[[[["}"#), 1);
        assert_eq!(bracket_depth(r#"{"a":"\"{"}"#), 1);
        assert_eq!(bracket_depth("[[[1]]]"), 3);
        let mut c = contract(json!({"type": "array", "items": {"type": "string"}}));
        c.max_depth = 2;
        assert!(check_pre_parse_depth(&c, "[[[1]]]").is_err());
        assert!(check_pre_parse_depth(&c, "[[1]]").is_ok());
    }

    #[test]
    fn unsupported_construct_refused_at_load() {
        let err = ResponseSchema::from_value(json!({
            "type": "object", "additionalProperties": false,
            "anyOf": [{"type": "string"}]
        }))
        .unwrap_err();
        assert!(
            matches!(err, GateError::ContractUnsupportedConstruct { .. }),
            "{err}"
        );
        assert!(
            err.to_string().contains("contract_unsupported_construct"),
            "{err}"
        );
        // Open object: object type without additionalProperties:false.
        let err = ResponseSchema::from_value(json!({
            "type": "object", "properties": {"id": {"type": "string"}}
        }))
        .unwrap_err();
        assert!(
            matches!(err, GateError::ContractUnsupportedConstruct { .. }),
            "{err}"
        );
        // Nested unsupported construct is also refused.
        assert!(
            ResponseSchema::from_value(json!({
                "type": "object", "additionalProperties": false,
                "properties": {"x": {"type": "string", "pattern": "^a"}}
            }))
            .is_err()
        );
    }

    #[test]
    fn typeless_and_itemless_schemas_refused_at_load() {
        for schema in [
            json!({}),
            json!({"required": ["id"]}),
            json!({"type": "array"}),
        ] {
            let err = ResponseSchema::from_value(schema.clone()).unwrap_err();
            assert!(
                matches!(err, GateError::ContractUnsupportedConstruct { .. }),
                "{schema}: {err}"
            );
        }
        // A valid closed object schema still loads and validates.
        let c = contract(json!({
            "type": "object", "additionalProperties": false,
            "required": ["id"],
            "properties": {"id": {"type": "string"}}
        }));
        assert!(validate_value(&c, &json!({"id": "x"})).is_ok());
        assert!(validate_value(&c, &json!({"id": "x", "extra": 1})).is_err());
        // Typeless enum/const schemas still work, including object values.
        let enum_c = contract(json!({"enum": [{"id": "x"}, "a"]}));
        assert!(validate_value(&enum_c, &json!({"id": "x"})).is_ok());
        assert!(validate_value(&enum_c, &json!("a")).is_ok());
        assert!(validate_value(&enum_c, &json!("c")).is_err());
        let const_c = contract(json!({"const": {"id": "x"}}));
        assert!(validate_value(&const_c, &json!({"id": "x"})).is_ok());
        assert!(validate_value(&const_c, &json!({"id": "y"})).is_err());
    }

    #[test]
    fn nested_object_and_array_contract_enforced() {
        let c = contract(nested_schema());
        let ok = json!({"user": {"name": "ada", "tags": ["a", "b"], "score": 5}});
        assert!(
            validate_value(&c, &ok).is_ok(),
            "{:?}",
            validate_value(&c, &ok)
        );
        let deep_bad = json!({"user": {"name": "ada", "tags": ["a", 42], "score": 5}});
        let err = validate_value(&c, &deep_bad).unwrap_err();
        assert!(err.to_string().contains("$.user.tags[1]"), "{err}");
        let bad_score = json!({"user": {"name": "ada", "tags": [], "score": 99}});
        assert!(validate_value(&c, &bad_score).is_err());
        let missing = json!({"user": {"tags": []}});
        assert!(validate_value(&c, &missing).is_err());
    }

    #[test]
    fn depth_array_string_bounds_refused() {
        let mut c = contract(json!({"type": "object", "additionalProperties": false,
            "properties": {"a": {"type": "object", "additionalProperties": false,
                "properties": {"b": {"type": "string"}}}}}));
        c.max_depth = 2;
        assert!(validate_value(&c, &json!({"a": {"b": "x"}})).is_err());
        c.max_depth = 32;
        c.max_array_len = 2;
        let arr_c = ResponseContract {
            max_array_len: 2,
            ..contract(json!({"type": "array", "items": {"type": "integer"}}))
        };
        assert!(validate_value(&arr_c, &json!([1, 2, 3])).is_err());
        let mut str_c = contract(json!({"type": "string"}));
        str_c.max_string_len = 3;
        assert!(validate_value(&str_c, &json!("abcd")).is_err());
        assert!(validate_value(&str_c, &json!("abc")).is_ok());
        let _ = c;
    }

    #[test]
    fn token_bound_without_counter_unenforceable_at_load() {
        let mut c = contract(json!({"type": "string"}));
        c.max_response_tokens = Some(10);
        if fl_core::token_count::count_unit().is_none() {
            let err = c.validate_load().unwrap_err();
            assert!(matches!(err, GateError::ContractUnenforceable), "{err}");
            assert!(err.to_string().contains("contract_unenforceable"), "{err}");
            assert!(count_tokens_bounded(&c, "hello").is_err());
        } else {
            assert!(c.validate_load().is_ok());
            assert!(count_tokens_bounded(&c, "hello").unwrap().is_some());
        }
        // No token bound always loads and counts as None.
        let plain = contract(json!({"type": "string"}));
        assert_eq!(count_tokens_bounded(&plain, "hello").unwrap(), None);
    }
}
