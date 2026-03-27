#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
ROOT_DIR=$(cd "$SCRIPT_DIR/.." && pwd)
SCHEMA_PATH="$ROOT_DIR/threadmill/protocol/threadmill-rpc.schema.json"
OUTPUT_PATH="$ROOT_DIR/src/protocol.rs"

python3 - "$OUTPUT_PATH" "$SCHEMA_PATH" <<'PY'
import json
import re
import subprocess
import sys

output_path = sys.argv[1]
schema_path = sys.argv[2]
schema = json.loads(subprocess.check_output(["jq", ".", schema_path], text=True))

defs = schema.get("$defs", {})
methods = schema.get("methods", {})

selected_defs = [
    "Project",
    "Thread",
    "ThreadStatus",
    "SourceType",
    "PresetStatus",
    "SystemStatsResult",
    "DirectoryEntry",
    "BinaryFrame",
]

rust_keywords = {
    "as", "break", "const", "continue", "crate", "else", "enum", "extern", "false", "fn",
    "for", "if", "impl", "in", "let", "loop", "match", "mod", "move", "mut", "pub", "ref",
    "return", "self", "Self", "static", "struct", "super", "trait", "true", "type", "unsafe",
    "use", "where", "while", "async", "await", "dyn", "abstract", "become", "box", "do", "final",
    "macro", "override", "priv", "typeof", "unsized", "virtual", "yield", "try",
}


def pascal_case(value: str) -> str:
    parts = [part for part in re.split(r"[^A-Za-z0-9]+", value) if part]
    if not parts:
        return "Generated"
    return "".join(part[0].upper() + part[1:] for part in parts)


def upper_snake(value: str) -> str:
    return re.sub(r"[^A-Za-z0-9]+", "_", value).strip("_").upper()


def ref_name(value: str) -> str:
    return value.rsplit("/", 1)[-1]


def integer_type(node: dict) -> str:
    minimum = node.get("minimum")
    maximum = node.get("maximum")
    if isinstance(minimum, (int, float)) and minimum >= 0:
        if isinstance(maximum, (int, float)) and maximum <= 65535:
            return "u16"
        return "u32"
    return "i64"


def rust_type(node: dict) -> str:
    if "$ref" in node:
        return ref_name(node["$ref"])

    if "const" in node:
        const_value = node["const"]
        if isinstance(const_value, str):
            return "String"
        if isinstance(const_value, bool):
            return "bool"
        if isinstance(const_value, int):
            return "i64"
        return "serde_json::Value"

    node_type = node.get("type")
    if isinstance(node_type, list):
        non_null = [entry for entry in node_type if entry != "null"]
        if len(non_null) == 1 and len(non_null) != len(node_type):
            nested = dict(node)
            nested["type"] = non_null[0]
            return f"Option<{rust_type(nested)}>"
        return "serde_json::Value"

    if node_type == "string":
        return "String"
    if node_type == "number":
        return "f64"
    if node_type == "integer":
        return integer_type(node)
    if node_type == "boolean":
        return "bool"
    if node_type == "array":
        return f"Vec<{rust_type(node.get('items', {}))}>"
    if node_type == "object":
        return "serde_json::Value"
    if node_type == "null":
        return "()"

    return "serde_json::Value"


def is_option_type(value: str) -> bool:
    return value.startswith("Option<")


def wrap_optional(value: str) -> str:
    if is_option_type(value):
        return value
    return f"Option<{value}>"


def field_ident(name: str) -> tuple[str, bool]:
    ident = re.sub(r"[^A-Za-z0-9_]", "_", name)
    if not ident:
        ident = "field"
    if ident[0].isdigit():
        ident = f"_{ident}"
    base = ident
    if base in rust_keywords:
        ident = f"r#{base}"
    needs_rename = name != base
    return ident, needs_rename


lines: list[str] = []
lines.append("// Generated from threadmill/protocol/threadmill-rpc.schema.json")
lines.append("// Do not edit manually. Run: ./codegen/generate.sh")
lines.append("")
lines.append("use serde::{Deserialize, Serialize};")
lines.append("")


def emit_struct(name: str, node: dict, description: str | None = None, unit_if_empty: bool = False) -> None:
    properties = node.get("properties", {})
    required = set(node.get("required", []))

    if description:
        for raw_line in description.splitlines():
            lines.append(f"/// {raw_line}")

    if unit_if_empty and not properties:
        lines.append("#[derive(Serialize, Deserialize, Debug, Clone, Default)]")
        lines.append(f"pub struct {name};")
        lines.append("")
        return

    lines.append("#[derive(Serialize, Deserialize, Debug, Clone)]")
    lines.append(f"pub struct {name} {{")
    for prop_name, prop_schema in properties.items():
        ident, needs_rename = field_ident(prop_name)
        field_type = rust_type(prop_schema)
        if prop_name not in required:
            field_type = wrap_optional(field_type)

        attrs: list[str] = []
        if prop_name not in required:
            attrs.append('#[serde(default, skip_serializing_if = "Option::is_none")]')
        if needs_rename:
            attrs.append(f'#[serde(rename = "{prop_name}")]')

        for attr in attrs:
            lines.append(f"    {attr}")
        lines.append(f"    pub {ident}: {field_type},")
    lines.append("}")
    lines.append("")


def emit_enum(name: str, node: dict, description: str | None = None) -> None:
    values = node.get("enum", [])
    if description:
        for raw_line in description.splitlines():
            lines.append(f"/// {raw_line}")
    lines.append("#[derive(Serialize, Deserialize, Debug, Clone)]")
    lines.append(f"pub enum {name} {{")
    used: set[str] = set()
    for raw_value in values:
        variant = pascal_case(str(raw_value))
        if not variant:
            variant = "Value"
        if variant[0].isdigit():
            variant = f"V{variant}"
        unique = variant
        suffix = 2
        while unique in used:
            unique = f"{variant}{suffix}"
            suffix += 1
        used.add(unique)
        lines.append(f"    #[serde(rename = \"{raw_value}\")]")
        lines.append(f"    {unique},")
    lines.append("}")
    lines.append("")


for def_name in selected_defs:
    if def_name not in defs:
        raise SystemExit(f"Missing $defs entry: {def_name}")

    node = defs[def_name]
    description = node.get("description")
    node_type = node.get("type")

    if node_type == "object":
        emit_struct(def_name, node, description=description, unit_if_empty=False)
    elif node_type == "string" and "enum" in node:
        emit_enum(def_name, node, description=description)
    else:
        lines.append(f"pub type {def_name} = {rust_type(node)};")
        lines.append("")

method_info: list[tuple[str, str, str, dict, dict]] = []

for method_name, method_schema in methods.items():
    method_base = pascal_case(method_name)
    params_name = f"{method_base}Params"
    method_info.append((method_name, method_base, params_name, method_schema.get("params"), method_schema.get("result")))

lines.append("// Request params")
lines.append("")
for method_name, method_base, params_name, params_schema, _ in method_info:
    if params_schema is None:
        emit_struct(params_name, {}, unit_if_empty=True)
        continue

    if isinstance(params_schema, dict) and params_schema.get("type") == "object":
        emit_struct(params_name, params_schema)
    else:
        lines.append("#[derive(Serialize, Deserialize, Debug, Clone)]")
        lines.append(f"pub struct {params_name}(pub {rust_type(params_schema or {})});")
        lines.append("")

lines.append("// Response results")
lines.append("")
for method_name, method_base, _, _, result_schema in method_info:
    result_name = f"{method_base}Result"
    if isinstance(result_schema, dict) and result_schema.get("type") == "object":
        emit_struct(result_name, result_schema)
    else:
        lines.append(f"pub type {result_name} = {rust_type(result_schema or {})};")
        lines.append("")

lines.append("// Method constants")
lines.append("")
for method_name, _, _, _, _ in method_info:
    const_name = f"METHOD_{upper_snake(method_name)}"
    lines.append(f"pub const {const_name}: &str = \"{method_name}\";")
lines.append("")

lines.append("// Method -> params dispatch")
lines.append("")
lines.append("#[derive(Serialize, Deserialize, Debug, Clone)]")
lines.append("#[serde(tag = \"method\", content = \"params\")]")
lines.append("pub enum RequestDispatch {")
for method_name, method_base, params_name, _, _ in method_info:
    lines.append(f"    #[serde(rename = \"{method_name}\")]")
    lines.append(f"    {method_base}({params_name}),")
lines.append("}")
lines.append("")

lines.append("pub fn parse_request_dispatch(method: &str, params: serde_json::Value) -> Result<RequestDispatch, String> {")
lines.append("    match method {")
for method_name, method_base, params_name, _, _ in method_info:
    const_name = f"METHOD_{upper_snake(method_name)}"
    lines.append(f"        {const_name} => serde_json::from_value::<{params_name}>(params)")
    lines.append(f"            .map(RequestDispatch::{method_base})")
    lines.append(f"            .map_err(|err| format!(\"invalid {method_name} params: {{err}}\")),")
lines.append("        _ => Err(format!(\"unknown method '{method}'\")),")
lines.append("    }")
lines.append("}")

with open(output_path, "w", encoding="utf-8") as file:
    file.write("\n".join(lines) + "\n")
PY
