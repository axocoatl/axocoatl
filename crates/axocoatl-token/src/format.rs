/// Supported serialization formats for agent-to-agent communication.
#[derive(Debug, Clone, PartialEq)]
pub enum TokenFormat {
    /// Standard JSON (minified). Default. Universal LLM compatibility.
    JsonMinified,
    /// Pretty-printed JSON. Never use in LLM context — 40-50% more tokens.
    /// Only for human-readable logging.
    JsonPretty,
    /// TOON (Token-Oriented Object Notation). Use for uniform arrays of objects.
    /// 40-60% fewer tokens than pretty JSON, 20-35% fewer than minified JSON.
    Toon,
    /// CSV-style for pure tabular data (no nested objects).
    Csv,
}

/// Hints about data shape to guide format selection.
#[derive(Debug, Clone)]
pub enum FormatHint {
    /// An array where all objects have the same keys.
    UniformArray,
    /// A 2D table with no nested objects.
    PureTabular,
    /// Deeply nested objects.
    Nested,
    /// Unknown — let the serializer decide.
    Unknown,
}

/// Serialize a value to the most token-efficient format for its shape.
pub fn adaptive_serialize(value: &serde_json::Value, hint: FormatHint) -> (String, TokenFormat) {
    match hint {
        FormatHint::UniformArray => {
            if let Some(toon) = try_serialize_toon(value) {
                return (toon, TokenFormat::Toon);
            }
            (
                serde_json::to_string(value).unwrap_or_default(),
                TokenFormat::JsonMinified,
            )
        }
        FormatHint::PureTabular => {
            if let Some(csv) = try_serialize_csv(value) {
                return (csv, TokenFormat::Csv);
            }
            (
                serde_json::to_string(value).unwrap_or_default(),
                TokenFormat::JsonMinified,
            )
        }
        FormatHint::Nested | FormatHint::Unknown => (
            serde_json::to_string(value).unwrap_or_default(),
            TokenFormat::JsonMinified,
        ),
    }
}

/// TOON serialization: tab-separated header + rows for uniform arrays.
///
/// Input:  `[{"name": "Alice", "age": 30}, {"name": "Bob", "age": 25}]`
/// Output: `name\tage\nAlice\t30\nBob\t25`
pub fn try_serialize_toon(value: &serde_json::Value) -> Option<String> {
    let arr = value.as_array()?;
    if arr.is_empty() {
        return None;
    }

    // All items must be objects with identical keys
    let first = arr[0].as_object()?;
    let keys: Vec<&str> = first.keys().map(|s| s.as_str()).collect();

    if keys.is_empty() {
        return None;
    }

    // Verify all rows have same keys in same order
    for item in arr.iter().skip(1) {
        let obj = item.as_object()?;
        let item_keys: Vec<&str> = obj.keys().map(|s| s.as_str()).collect();
        if item_keys != keys {
            return None; // Non-uniform — fall back to JSON
        }
    }

    // Build TOON output
    let mut lines = Vec::with_capacity(arr.len() + 1);
    lines.push(keys.join("\t")); // Header row

    for item in arr {
        let obj = item.as_object()?;
        let row: Vec<String> = keys
            .iter()
            .map(|k| value_to_toon_cell(obj.get(*k)?))
            .collect::<Option<Vec<_>>>()?;
        lines.push(row.join("\t"));
    }

    Some(lines.join("\n"))
}

/// CSV serialization for pure tabular data.
fn try_serialize_csv(value: &serde_json::Value) -> Option<String> {
    // Same as TOON but comma-separated
    let arr = value.as_array()?;
    if arr.is_empty() {
        return None;
    }

    let first = arr[0].as_object()?;
    let keys: Vec<&str> = first.keys().map(|s| s.as_str()).collect();

    if keys.is_empty() {
        return None;
    }

    for item in arr.iter().skip(1) {
        let obj = item.as_object()?;
        let item_keys: Vec<&str> = obj.keys().map(|s| s.as_str()).collect();
        if item_keys != keys {
            return None;
        }
    }

    let mut lines = Vec::with_capacity(arr.len() + 1);
    lines.push(keys.join(","));

    for item in arr {
        let obj = item.as_object()?;
        let row: Vec<String> = keys
            .iter()
            .map(|k| value_to_csv_cell(obj.get(*k)?))
            .collect::<Option<Vec<_>>>()?;
        lines.push(row.join(","));
    }

    Some(lines.join("\n"))
}

fn value_to_toon_cell(v: &serde_json::Value) -> Option<String> {
    match v {
        serde_json::Value::String(s) => Some(s.clone()),
        serde_json::Value::Number(n) => Some(n.to_string()),
        serde_json::Value::Bool(b) => Some(b.to_string()),
        serde_json::Value::Null => Some("null".to_string()),
        // Objects/arrays in cells = not suitable for TOON
        _ => None,
    }
}

fn value_to_csv_cell(v: &serde_json::Value) -> Option<String> {
    match v {
        serde_json::Value::String(s) => {
            // Escape commas in CSV
            if s.contains(',') || s.contains('"') || s.contains('\n') {
                Some(format!("\"{}\"", s.replace('"', "\"\"")))
            } else {
                Some(s.clone())
            }
        }
        serde_json::Value::Number(n) => Some(n.to_string()),
        serde_json::Value::Bool(b) => Some(b.to_string()),
        serde_json::Value::Null => Some("".to_string()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn toon_uniform_array() {
        let value = serde_json::json!([
            {"name": "Alice", "age": 30},
            {"name": "Bob", "age": 25}
        ]);
        let (result, format) = adaptive_serialize(&value, FormatHint::UniformArray);
        assert_eq!(format, TokenFormat::Toon);
        // JSON object key order isn't guaranteed, so check both possible orderings
        let lines: Vec<&str> = result.lines().collect();
        assert_eq!(lines.len(), 3); // header + 2 rows
                                    // Header must contain both keys
        assert!(lines[0].contains("name"));
        assert!(lines[0].contains("age"));
        // Rows must contain values
        assert!(lines[1].contains("Alice"));
        assert!(lines[1].contains("30"));
        assert!(lines[2].contains("Bob"));
        assert!(lines[2].contains("25"));
    }

    #[test]
    fn toon_non_uniform_falls_back_to_json() {
        let value = serde_json::json!([
            {"name": "Alice", "age": 30},
            {"name": "Bob", "score": 95}
        ]);
        let (_, format) = adaptive_serialize(&value, FormatHint::UniformArray);
        assert_eq!(format, TokenFormat::JsonMinified);
    }

    #[test]
    fn toon_nested_objects_fall_back() {
        let value = serde_json::json!([
            {"name": "Alice", "addr": {"city": "NYC"}},
            {"name": "Bob", "addr": {"city": "LA"}}
        ]);
        let (_, format) = adaptive_serialize(&value, FormatHint::UniformArray);
        assert_eq!(format, TokenFormat::JsonMinified);
    }

    #[test]
    fn toon_empty_array_falls_back() {
        let value = serde_json::json!([]);
        let (_, format) = adaptive_serialize(&value, FormatHint::UniformArray);
        assert_eq!(format, TokenFormat::JsonMinified);
    }

    #[test]
    fn toon_single_row() {
        let value = serde_json::json!([{"x": 1, "y": 2}]);
        let (result, format) = adaptive_serialize(&value, FormatHint::UniformArray);
        assert_eq!(format, TokenFormat::Toon);
        assert!(result.contains("x\ty"));
        assert!(result.contains("1\t2"));
    }

    #[test]
    fn toon_with_null_and_bool() {
        let value = serde_json::json!([
            {"name": "Alice", "active": true, "score": null},
            {"name": "Bob", "active": false, "score": null}
        ]);
        let (result, format) = adaptive_serialize(&value, FormatHint::UniformArray);
        assert_eq!(format, TokenFormat::Toon);
        assert!(result.contains("true"));
        assert!(result.contains("null"));
    }

    #[test]
    fn csv_pure_tabular() {
        let value = serde_json::json!([
            {"id": 1, "name": "Alice"},
            {"id": 2, "name": "Bob"}
        ]);
        let (result, format) = adaptive_serialize(&value, FormatHint::PureTabular);
        assert_eq!(format, TokenFormat::Csv);
        assert!(result.contains("id,name"));
        assert!(result.contains("1,Alice"));
    }

    #[test]
    fn csv_escapes_commas() {
        let value = serde_json::json!([
            {"name": "Smith, John", "age": 30}
        ]);
        let (result, format) = adaptive_serialize(&value, FormatHint::PureTabular);
        assert_eq!(format, TokenFormat::Csv);
        assert!(result.contains("\"Smith, John\""));
    }

    #[test]
    fn nested_hint_returns_json() {
        let value = serde_json::json!({"nested": {"deep": true}});
        let (_, format) = adaptive_serialize(&value, FormatHint::Nested);
        assert_eq!(format, TokenFormat::JsonMinified);
    }

    #[test]
    fn unknown_hint_returns_json() {
        let value = serde_json::json!(42);
        let (_, format) = adaptive_serialize(&value, FormatHint::Unknown);
        assert_eq!(format, TokenFormat::JsonMinified);
    }

    #[test]
    fn toon_fewer_tokens_than_json() {
        // This is the key efficiency claim: TOON should use fewer characters
        // (and thus fewer tokens) than minified JSON for uniform arrays
        let value: serde_json::Value = serde_json::json!((0..20)
            .map(|i| serde_json::json!({
                "id": i,
                "name": format!("item_{}", i),
                "status": "active",
                "score": i * 10,
            }))
            .collect::<Vec<_>>());

        let json_str = serde_json::to_string(&value).unwrap();
        let (toon_str, format) = adaptive_serialize(&value, FormatHint::UniformArray);
        assert_eq!(format, TokenFormat::Toon);
        // TOON should be shorter than minified JSON
        assert!(
            toon_str.len() < json_str.len(),
            "TOON ({} chars) should be shorter than JSON ({} chars)",
            toon_str.len(),
            json_str.len()
        );
    }
}
