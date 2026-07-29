//! JSON Schema to TypeScript type expressions.
//!
//! Backing MCP servers describe their tool parameters with JSON Schema, but the
//! agent writes JavaScript against those tools. Models read TypeScript
//! declarations far more reliably than raw JSON Schema, so schemas are rendered
//! as `.d.ts`-style type expressions.
//!
//! Two rules govern everything here:
//!
//! * **Never fail.** Schemas arrive from arbitrary third-party servers across
//!   several JSON Schema dialects. A construct we do not recognize renders as
//!   `unknown` and generation continues.
//! * **`unknown`, not `any`.** `unknown` forces the model to narrow the value
//!   rather than silently assuming a shape that may not hold.

use serde_json::{Map, Value};

/// Rendered when a schema is missing, unrecognized, or unconstrained.
const UNKNOWN: &str = "unknown";

/// Render a JSON Schema as a TypeScript type expression.
pub fn render_type(schema: &Value) -> String {
    render_at(schema, 0)
}

fn render_at(schema: &Value, indent: usize) -> String {
    match schema {
        // A schema may be a bare boolean: `true` accepts anything, `false`
        // accepts nothing.
        Value::Bool(true) => UNKNOWN.to_string(),
        Value::Bool(false) => "never".to_string(),
        Value::Object(map) => render_object_schema(map, indent),
        // Anything else is malformed; degrade rather than fail.
        _ => UNKNOWN.to_string(),
    }
}

fn render_object_schema(schema: &Map<String, Value>, indent: usize) -> String {
    if schema.is_empty() {
        return UNKNOWN.to_string();
    }

    let base = render_constrained(schema, indent);

    // `nullable` is an OpenAPI 3.0 extension rather than JSON Schema, but the
    // schemars 0.8 line emits it for optional fields and those servers are
    // deployed today.
    if schema.get("nullable") == Some(&Value::Bool(true)) && base != "null" {
        return join_union(vec![base, "null".to_string()]);
    }
    base
}

/// Dispatch on whichever constraint keyword the schema uses, narrowest first.
fn render_constrained(schema: &Map<String, Value>, indent: usize) -> String {
    // A single permitted value is narrower than anything else present.
    if let Some(value) = schema.get("const") {
        return render_literal(value);
    }

    // `enum` constrains the value regardless of any declared `type`.
    if let Some(Value::Array(values)) = schema.get("enum") {
        return render_enum(values);
    }

    // Composition keywords. `allOf` combines constraints; `anyOf`/`oneOf`
    // offer alternatives. A single-element `allOf` — the shape generators emit
    // to attach a description to a reference — collapses to its one member.
    if let Some(Value::Array(members)) = schema.get("allOf") {
        return render_intersection(members, indent);
    }
    for key in ["anyOf", "oneOf"] {
        if let Some(Value::Array(members)) = schema.get(key) {
            return render_union(members, indent);
        }
    }

    match schema.get("type") {
        Some(Value::String(ty)) => render_with_type(schema, ty, indent),
        // A type may be a list of alternatives, e.g. `["string", "null"]`.
        Some(Value::Array(types)) => {
            let parts = types
                .iter()
                .filter_map(Value::as_str)
                .map(|ty| render_with_type(schema, ty, indent))
                .collect();
            join_union(parts)
        }
        _ => UNKNOWN.to_string(),
    }
}

fn render_with_type(schema: &Map<String, Value>, ty: &str, indent: usize) -> String {
    match ty {
        "string" => "string".to_string(),
        // JSON Schema separates integers from other numbers; TypeScript does not.
        "number" | "integer" => "number".to_string(),
        "boolean" => "boolean".to_string(),
        "null" => "null".to_string(),
        "array" => render_array(schema, indent),
        "object" => render_struct(schema, indent),
        _ => UNKNOWN.to_string(),
    }
}

fn render_union(members: &[Value], indent: usize) -> String {
    let parts = members.iter().map(|m| render_at(m, indent)).collect();
    join_union(parts)
}

fn render_intersection(members: &[Value], indent: usize) -> String {
    let mut parts: Vec<String> = Vec::new();
    for member in members {
        let rendered = parenthesize_if_composite(render_at(member, indent));
        if !parts.contains(&rendered) {
            parts.push(rendered);
        }
    }
    match parts.len() {
        0 => UNKNOWN.to_string(),
        1 => parts.pop().unwrap_or_default(),
        _ => parts.join(" & "),
    }
}

/// Join alternatives into a union, dropping duplicates and flattening the
/// `unknown` case — `T | unknown` is just `unknown`.
fn join_union(parts: Vec<String>) -> String {
    let mut unique: Vec<String> = Vec::new();
    for part in parts {
        if part == UNKNOWN {
            return UNKNOWN.to_string();
        }
        if !unique.contains(&part) {
            unique.push(part);
        }
    }
    match unique.len() {
        0 => UNKNOWN.to_string(),
        1 => unique.pop().unwrap_or_default(),
        _ => unique.join(" | "),
    }
}

/// Parenthesize a union so it binds correctly inside a larger type.
fn parenthesize_if_composite(rendered: String) -> String {
    if rendered.contains(" | ") && !rendered.starts_with('(') {
        format!("({rendered})")
    } else {
        rendered
    }
}

fn render_array(schema: &Map<String, Value>, indent: usize) -> String {
    let Some(items) = schema.get("items") else {
        return format!("{UNKNOWN}[]");
    };
    // `A | B[]` would parse as `A | (B[])`, so a union element needs parens.
    let inner = parenthesize_if_composite(render_at(items, indent));
    format!("{inner}[]")
}

fn render_struct(schema: &Map<String, Value>, indent: usize) -> String {
    let Some(Value::Object(properties)) = schema.get("properties") else {
        // No declared properties: the schema describes a map, and
        // `additionalProperties` types its values.
        //
        // `propertyNames` is deliberately ignored — it constrains keys, which
        // TypeScript index signatures cannot express.
        return match schema.get("additionalProperties") {
            Some(Value::Bool(false)) => "{}".to_string(),
            Some(value @ Value::Object(_)) => {
                format!("Record<string, {}>", render_at(value, indent))
            }
            _ => format!("Record<string, {UNKNOWN}>"),
        };
    };

    if properties.is_empty() {
        return "{}".to_string();
    }

    let required: Vec<&str> = match schema.get("required") {
        Some(Value::Array(names)) => names.iter().filter_map(Value::as_str).collect(),
        _ => Vec::new(),
    };

    let pad = "  ".repeat(indent + 1);
    let close_pad = "  ".repeat(indent);
    let mut out = String::from("{\n");

    for (name, subschema) in properties {
        if let Some(doc) = doc_comment(subschema) {
            out.push_str(&format!("{pad}/** {doc} */\n"));
        }
        let optional = if required.contains(&name.as_str()) {
            ""
        } else {
            "?"
        };
        let rendered = render_at(subschema, indent + 1);
        out.push_str(&format!(
            "{pad}{}{optional}: {rendered};\n",
            property_key(name)
        ));
    }

    out.push_str(&close_pad);
    out.push('}');
    out
}

fn render_enum(values: &[Value]) -> String {
    if values.is_empty() {
        return "never".to_string();
    }
    join_union(values.iter().map(render_literal).collect())
}

/// Render a JSON value as a TypeScript literal type.
fn render_literal(value: &Value) -> String {
    match value {
        Value::String(s) => format!("{s:?}"),
        Value::Number(n) => n.to_string(),
        Value::Bool(b) => b.to_string(),
        Value::Null => "null".to_string(),
        // An object or array literal has no TypeScript literal-type spelling.
        _ => UNKNOWN.to_string(),
    }
}

/// A property's `description`, collapsed to one line and safe inside a JSDoc
/// block.
fn doc_comment(schema: &Value) -> Option<String> {
    let text = schema.get("description")?.as_str()?.trim();
    if text.is_empty() {
        return None;
    }
    // `*/` inside a JSDoc comment would end it early.
    let collapsed = text.split_whitespace().collect::<Vec<_>>().join(" ");
    Some(collapsed.replace("*/", "* /"))
}

/// Quote a property name unless it is a valid JavaScript identifier.
///
/// Tool schemas routinely use names that are not identifiers (`content-type`,
/// `2fa`), and those are legal as quoted property keys.
fn property_key(name: &str) -> String {
    if is_js_identifier(name) {
        name.to_string()
    } else {
        format!("{name:?}")
    }
}

fn is_js_identifier(name: &str) -> bool {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !(first.is_ascii_alphabetic() || first == '_' || first == '$') {
        return false;
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '$')
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn render(schema: Value) -> String {
        render_type(&schema)
    }

    // -- scalars --

    #[test]
    fn renders_scalar_types() {
        assert_eq!(render(json!({"type": "string"})), "string");
        assert_eq!(render(json!({"type": "boolean"})), "boolean");
        assert_eq!(render(json!({"type": "null"})), "null");
    }

    /// JSON Schema distinguishes integers from other numbers; TypeScript has
    /// only `number`.
    #[test]
    fn renders_integer_as_number() {
        assert_eq!(render(json!({"type": "number"})), "number");
        assert_eq!(render(json!({"type": "integer"})), "number");
    }

    // -- degradation --

    #[test]
    fn renders_unconstrained_schemas_as_unknown() {
        assert_eq!(render(json!({})), "unknown");
        assert_eq!(render(json!(true)), "unknown");
        assert_eq!(render(json!({"type": "not-a-type"})), "unknown");
        assert_eq!(render(json!({"minimum": 3})), "unknown");
    }

    #[test]
    fn renders_uninhabited_schemas_as_never() {
        assert_eq!(render(json!(false)), "never");
        assert_eq!(render(json!({"enum": []})), "never");
    }

    // -- arrays --

    #[test]
    fn renders_arrays() {
        assert_eq!(
            render(json!({"type": "array", "items": {"type": "string"}})),
            "string[]"
        );
        assert_eq!(render(json!({"type": "array"})), "unknown[]");
    }

    /// `A | B[]` parses as `A | (B[])`, so a union element must be parenthesized.
    #[test]
    fn parenthesizes_union_array_elements() {
        let out = render(json!({
            "type": "array",
            "items": {"enum": ["a", "b"]},
        }));
        assert_eq!(out, r#"("a" | "b")[]"#);
    }

    // -- enums --

    #[test]
    fn renders_string_enum_as_literal_union() {
        assert_eq!(render(json!({"enum": ["a", "b"]})), r#""a" | "b""#);
    }

    /// Enum members are not always strings, and a declared `type` must not
    /// override the narrower `enum` constraint.
    #[test]
    fn renders_mixed_enum_and_ignores_declared_type() {
        assert_eq!(render(json!({"enum": [1, true, null]})), "1 | true | null");
        assert_eq!(
            render(json!({"type": "boolean", "enum": [true]})),
            "true",
            "enum should win over type"
        );
    }

    // -- unions and composition --

    /// The dominant Python generator emits this for every optional field, and
    /// it is the single most common construct the naive mapping would drop.
    #[test]
    fn renders_nullable_anyof_as_union() {
        let out = render(json!({
            "anyOf": [{"type": "string"}, {"type": "null"}],
            "default": null,
        }));
        assert_eq!(out, "string | null");
    }

    /// Optionality is decided by `required`. A nullable field that *is*
    /// required stays non-optional; only its type gains `null`.
    #[test]
    fn nullable_union_does_not_imply_optional() {
        let out = render(json!({
            "type": "object",
            "properties": {
                "a": {"anyOf": [{"type": "string"}, {"type": "null"}]},
            },
            "required": ["a"],
        }));
        assert_eq!(out, "{\n  a: string | null;\n}");
    }

    #[test]
    fn renders_oneof_as_union() {
        let out = render(json!({
            "oneOf": [{"type": "string"}, {"type": "number"}],
        }));
        assert_eq!(out, "string | number");
    }

    #[test]
    fn renders_allof_as_intersection() {
        let out = render(json!({
            "allOf": [
                {"type": "object", "properties": {"a": {"type": "string"}}},
                {"type": "object", "properties": {"b": {"type": "number"}}},
            ],
        }));
        assert_eq!(out, "{\n  a?: string;\n} & {\n  b?: number;\n}");
    }

    /// Generators wrap a single member in `allOf` to attach a sibling
    /// description. That must collapse rather than render a one-sided `&`.
    #[test]
    fn collapses_single_member_allof() {
        let out = render(json!({
            "allOf": [{"type": "string"}],
            "description": "wrapped",
        }));
        assert_eq!(out, "string");
    }

    /// `T | unknown` is just `unknown`, and repeated alternatives collapse.
    #[test]
    fn union_absorbs_unknown_and_drops_duplicates() {
        assert_eq!(
            render(json!({"anyOf": [{"type": "string"}, {}]})),
            "unknown"
        );
        assert_eq!(
            render(json!({"anyOf": [{"type": "string"}, {"type": "string"}]})),
            "string"
        );
    }

    // -- type as a list --

    #[test]
    fn renders_array_valued_type_as_union() {
        assert_eq!(render(json!({"type": ["string", "null"]})), "string | null");
        assert_eq!(
            render(json!({"type": ["string", "number", "boolean"]})),
            "string | number | boolean"
        );
    }

    // -- const --

    #[test]
    fn renders_const_as_literal() {
        assert_eq!(render(json!({"const": "resource"})), r#""resource""#);
        assert_eq!(render(json!({"const": 3})), "3");
    }

    // -- nullable (OpenAPI spelling) --

    /// One deployed Rust generator marks optional fields with `nullable`
    /// rather than a union or a null-typed alternative.
    #[test]
    fn renders_openapi_nullable_as_union() {
        let out = render(json!({
            "type": "array",
            "items": {"type": "string"},
            "nullable": true,
        }));
        assert_eq!(out, "string[] | null");
    }

    #[test]
    fn nullable_null_type_does_not_repeat_null() {
        assert_eq!(render(json!({"type": "null", "nullable": true})), "null");
    }

    // -- records --

    #[test]
    fn renders_typed_additional_properties_as_record() {
        let out = render(json!({
            "type": "object",
            "additionalProperties": {"type": "string"},
        }));
        assert_eq!(out, "Record<string, string>");
    }

    /// `propertyNames` constrains keys, which a TypeScript index signature
    /// cannot express, so it is ignored rather than degrading the type.
    #[test]
    fn ignores_property_names_constraint() {
        let out = render(json!({
            "type": "object",
            "propertyNames": {"type": "string"},
            "additionalProperties": {"type": "string"},
        }));
        assert_eq!(out, "Record<string, string>");
    }

    #[test]
    fn renders_closed_empty_object_as_empty_type() {
        let out = render(json!({"type": "object", "additionalProperties": false}));
        assert_eq!(out, "{}");
    }

    // -- objects --

    #[test]
    fn renders_object_with_required_and_optional_properties() {
        let out = render(json!({
            "type": "object",
            "properties": {
                "sql": {"type": "string"},
                "limit": {"type": "integer"},
            },
            "required": ["sql"],
        }));
        assert_eq!(out, "{\n  limit?: number;\n  sql: string;\n}");
    }

    /// Optionality comes from `required` alone. Nothing else marks a property
    /// optional.
    #[test]
    fn properties_are_optional_when_required_is_absent() {
        let out = render(json!({
            "type": "object",
            "properties": {"a": {"type": "string"}},
        }));
        assert_eq!(out, "{\n  a?: string;\n}");
    }

    /// `additionalProperties: false` is the single most common construct in
    /// real tool schemas. Degrading on it would turn every ordinary object into
    /// `unknown`.
    #[test]
    fn additional_properties_false_does_not_degrade_the_object() {
        let out = render(json!({
            "type": "object",
            "properties": {"a": {"type": "string"}},
            "required": ["a"],
            "additionalProperties": false,
        }));
        assert_eq!(out, "{\n  a: string;\n}");
    }

    #[test]
    fn renders_object_without_properties_as_open_map() {
        assert_eq!(render(json!({"type": "object"})), "Record<string, unknown>");
        assert_eq!(render(json!({"type": "object", "properties": {}})), "{}");
    }

    #[test]
    fn renders_nested_objects_with_indentation() {
        let out = render(json!({
            "type": "object",
            "properties": {
                "outer": {
                    "type": "object",
                    "properties": {"inner": {"type": "string"}},
                    "required": ["inner"],
                },
            },
            "required": ["outer"],
        }));
        assert_eq!(
            out, "{\n  outer: {\n    inner: string;\n  };\n}",
            "nested braces should indent, got:\n{out}"
        );
    }

    // -- property names --

    /// Tool schemas use property names that are not JavaScript identifiers.
    #[test]
    fn quotes_property_names_that_are_not_identifiers() {
        let out = render(json!({
            "type": "object",
            "properties": {
                "content-type": {"type": "string"},
                "2fa": {"type": "boolean"},
                "ok_name": {"type": "string"},
            },
        }));
        assert!(out.contains(r#""content-type"?: string;"#), "got:\n{out}");
        assert!(out.contains(r#""2fa"?: boolean;"#), "got:\n{out}");
        assert!(
            out.contains("ok_name?: string;"),
            "valid identifiers stay unquoted, got:\n{out}"
        );
    }

    // -- documentation --

    #[test]
    fn renders_description_as_jsdoc() {
        let out = render(json!({
            "type": "object",
            "properties": {
                "sql": {"type": "string", "description": "The query to run"},
            },
        }));
        assert_eq!(out, "{\n  /** The query to run */\n  sql?: string;\n}");
    }

    /// A description containing `*/` would close the JSDoc block early.
    #[test]
    fn escapes_comment_terminator_in_description() {
        let out = render(json!({
            "type": "object",
            "properties": {
                "a": {"type": "string", "description": "ends the block */ here"},
            },
        }));
        assert!(!out.contains("*/ here"), "got:\n{out}");
        assert!(out.contains("* / here"), "got:\n{out}");
    }

    #[test]
    fn collapses_multiline_descriptions() {
        let out = render(json!({
            "type": "object",
            "properties": {
                "a": {"type": "string", "description": "first line\n  second line"},
            },
        }));
        assert!(out.contains("/** first line second line */"), "got:\n{out}");
    }
}
