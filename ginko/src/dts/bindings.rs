//! Devicetree binding files (`Documentation/devicetree/bindings/**.yaml`) as
//! a completion vocabulary.
//!
//! Linux describes the allowed devicetree content per device class in YAML
//! files using a json-schema flavored vocabulary. This module distills those
//! files into completion data: which properties a node with a given
//! `compatible` accepts, which of them are required, and what the binding
//! says about them.
//!
//! v1 is deliberately lean: it neither resolves `$ref`s nor implements
//! json-schema semantics — a `types.yaml` `$ref` is only mined for its tail as
//! a type hint, `select:`-driven bindings do not match, and schema validation
//! is out of scope.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use serde_yaml::Value;

use crate::dts::ast::PropertyValue;
use crate::dts::resolved::ResolvedNode;

/// A binding file distilled to completion data.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompletionSchema {
    /// The binding file this schema was read from.
    pub file: PathBuf,
    /// Property name -> hint.
    pub properties: BTreeMap<String, PropertyHint>,
}

/// Per-property completion hints from a binding.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PropertyHint {
    /// Listed under `required:`.
    pub required: bool,
    /// Set with `name: true` — a boolean property, completed as `name;`.
    pub boolean: bool,
    /// The tail of a `types.yaml` `$ref`, e.g. `uint32` or `string-array`.
    pub type_hint: Option<String>,
    /// The binding's `description` for this property.
    pub description: Option<String>,
    /// The binding marked this property `deprecated: true`.
    pub deprecated: bool,
    /// The property's candidate values: strings collected from `enum:`
    /// lists and `const:` entries of the property's sub-schema. A natural
    /// special case: a node's own `compatible` candidates are the values of
    /// its `compatible` property.
    pub values: Vec<String>,
}

/// Indexed data distilled from a set of binding roots.
///
/// `compatible`-driven: a node's compatible strings map to the schemas whose
/// `properties.compatible` mentions them.
#[derive(Debug, Default)]
pub struct Bindings {
    /// compatible string -> indices into `schemas`
    by_compatible: BTreeMap<String, Vec<usize>>,
    schemas: Vec<CompletionSchema>,
    /// Property hints from compatible-less common-part schemas ($ref'ed
    /// property sets like leds/common.yaml or dt-schema's status.yaml).
    /// Consulted for VALUE completion only — their property names are
    /// deliberately not suggested universally.
    common: BTreeMap<String, PropertyHint>,
}

impl Bindings {
    /// Build an index from binding roots, e.g. a kernel checkout's
    /// `Documentation/devicetree/bindings` directory. Unreadable or invalid
    /// files are skipped; empty input yields an empty index.
    pub fn from_paths(paths: &[PathBuf]) -> Bindings {
        let mut files = Vec::new();
        for path in paths {
            collect_yaml_files(path, &mut files);
        }
        let mut bindings = Bindings::default();
        for file in files {
            bindings.add_file(&file);
        }
        bindings
    }

    pub fn schema_count(&self) -> usize {
        self.schemas.len()
    }

    pub fn compatible_count(&self) -> usize {
        self.by_compatible.len()
    }

    /// Merge the property hints of all schemas matching any of the given
    /// compatible strings. A property required by any schema stays required;
    /// the first non-empty hint wins for the optional fields.
    pub fn properties_for(&self, compatibles: &[String]) -> BTreeMap<String, PropertyHint> {
        let mut merged: BTreeMap<String, PropertyHint> = BTreeMap::new();
        for compatible in compatibles {
            for idx in self.by_compatible.get(compatible).into_iter().flatten() {
                for (name, hint) in &self.schemas[*idx].properties {
                    let entry = merged.entry(name.clone()).or_default();
                    entry.required |= hint.required;
                    entry.boolean |= hint.boolean;
                    entry.deprecated |= hint.deprecated;
                    for value in &hint.values {
                        if !entry.values.contains(value) {
                            entry.values.push(value.clone());
                        }
                    }
                    if entry.type_hint.is_none() {
                        entry.type_hint = hint.type_hint.clone();
                    }
                    if entry.description.is_none() {
                        entry.description = hint.description.clone();
                    }
                }
            }
        }
        merged
    }

    fn add_file(&mut self, path: &Path) {
        let Ok(text) = fs::read_to_string(path) else {
            return;
        };
        let Ok(binding) = serde_yaml::from_str::<Value>(&text) else {
            return;
        };
        self.add_schema(path.to_path_buf(), &binding);
    }

    fn add_schema(&mut self, file: PathBuf, binding: &Value) {
        let Some(compatible) = binding.get("properties").and_then(|props| props.get("compatible"))
        else {
            // Schemas without a `compatible` are common-part schemas; they
            // contribute value candidates instead of matching nodes.
            self.add_common_schema(binding);
            return;
        };
        let mut compatibles = Vec::new();
        collect_compatible_strings(compatible, &mut compatibles);
        compatibles.sort();
        compatibles.dedup();
        if compatibles.is_empty() {
            return;
        }
        let properties = extract_properties(binding);
        self.schemas.push(CompletionSchema {
            file,
            properties,
        });
        let idx = self.schemas.len() - 1;
        for compatible in compatibles {
            self.by_compatible
                .entry(compatible)
                .or_default()
                .push(idx);
        }
    }

    /// Hints from common-part schemas merge by property name; their value
    /// candidates dedupe across files. See `common`.
    fn add_common_schema(&mut self, binding: &Value) {
        for (name, hint) in extract_properties(binding) {
            let entry = self.common.entry(name).or_default();
            entry.required |= hint.required;
            entry.boolean |= hint.boolean;
            entry.deprecated |= hint.deprecated;
            for value in hint.values {
                if !entry.values.contains(&value) {
                    entry.values.push(value);
                }
            }
            if entry.type_hint.is_none() {
                entry.type_hint = hint.type_hint;
            }
            if entry.description.is_none() {
                entry.description = hint.description;
            }
        }
    }

    /// Value candidates for a property from common-part schemas — matched by
    /// property name, e.g. `status` from dt-schema's status.yaml or `color`
    /// from leds/common.yaml. Called when the node's own bindings have no
    /// candidates for the property.
    pub fn common_values(&self, name: &str) -> &[String] {
        self.common
            .get(name)
            .map(|hint| hint.values.as_slice())
            .unwrap_or(&[])
    }
}

/// The `compatible` strings of a resolved node: all string values of its
/// `compatible` property, overlay- and include-merged — the base dtsi's
/// compatibles are visible when completing inside `&node { }` overlays.
pub fn node_compatibles(rnode: &ResolvedNode) -> Vec<String> {
    rnode
        .get_property("compatible")
        .into_iter()
        .flat_map(|prop| prop.property().values.iter())
        .filter_map(|value| match value {
            PropertyValue::String(string) => Some(string.item().clone()),
            _ => None,
        })
        .collect()
}

/// The binding's `required:` list.
fn required_set(binding: &Value) -> BTreeSet<String> {
    let mut required = BTreeSet::new();
    if let Some(Value::Sequence(items)) = binding.get("required") {
        for item in items {
            if let Some(name) = item.as_str() {
                required.insert(name.to_string());
            }
        }
    }
    required
}

/// The binding's `properties:` map distilled to hinted entries. `properties`
/// carries nested node schemas as well; every key is a devicetree name in
/// the end, so none are skipped.
fn extract_properties(binding: &Value) -> BTreeMap<String, PropertyHint> {
    let required = required_set(binding);
    let mut properties = BTreeMap::new();
    if let Some(Value::Mapping(props)) = binding.get("properties") {
        for (name, sub) in props.iter() {
            if let Some(name) = name.as_str() {
                properties.insert(
                    name.to_string(),
                    property_hint(sub, required.contains(name)),
                );
            }
        }
    }
    properties
}

fn collect_compatible_strings(value: &Value, out: &mut Vec<String>) {
    match value {
        Value::Mapping(map) => {
            for (key, val) in map.iter() {
                match key.as_str() {
                    Some("enum") => {
                        if let Value::Sequence(items) = val {
                            for item in items {
                                if let Some(string) = item.as_str() {
                                    out.push(string.to_string());
                                }
                            }
                        }
                    }
                    Some("const") => {
                        if let Some(string) = val.as_str() {
                            out.push(string.to_string());
                        }
                    }
                    // `oneOf`, `items`, `contains`, …
                    _ => collect_compatible_strings(val, out),
                }
            }
        }
        Value::Sequence(items) => {
            for item in items {
                collect_compatible_strings(item, out);
            }
        }
        _ => {}
    }
}

fn property_hint(sub: &Value, required: bool) -> PropertyHint {
    if let Value::Bool(boolean) = sub {
        return PropertyHint {
            required,
            boolean: *boolean,
            ..PropertyHint::default()
        };
    }
    let Value::Mapping(_) = sub else {
        return PropertyHint {
            required,
            ..PropertyHint::default()
        };
    };
    let type_hint = sub.get("$ref").and_then(Value::as_str).and_then(type_ref_tail);
    let description = sub.get("description").and_then(Value::as_str).map(str::to_string);
    let deprecated = matches!(sub.get("deprecated"), Some(Value::Bool(true)));
    let mut values = Vec::new();
    collect_value_strings(sub, &mut values);
    PropertyHint {
        required,
        boolean: false,
        type_hint,
        description,
        deprecated,
        values,
    }
}

fn type_ref_tail(reference: &str) -> Option<String> {
    reference
        .rsplit_once("types.yaml#/definitions/")
        .map(|(_, tail)| tail.to_string())
}

/// Collect the candidate values of a property sub-schema: strings under
/// `enum:` lists and `const:` entries. Nested child-node property maps
/// (`properties`, `patternProperties`) and `examples` are not descended
/// into — those enums belong to other properties.
fn collect_value_strings(value: &Value, out: &mut Vec<String>) {
    match value {
        Value::Mapping(map) => {
            for (key, val) in map.iter() {
                match key.as_str() {
                    Some("enum") => {
                        if let Value::Sequence(items) = val {
                            for item in items {
                                if let Some(string) = item.as_str() {
                                    out.push(string.to_string());
                                }
                            }
                        }
                    }
                    Some("const") => {
                        if let Some(string) = val.as_str() {
                            out.push(string.to_string());
                        }
                    }
                    Some("properties" | "patternProperties" | "examples") => {}
                    _ => collect_value_strings(val, out),
                }
            }
        }
        Value::Sequence(items) => {
            for item in items {
                collect_value_strings(item, out);
            }
        }
        _ => {}
    }
}

/// Recursively collect `*.yaml` files below `path`. A file passed directly is
/// accepted; anything unreadable contributes nothing.
fn collect_yaml_files(path: &Path, out: &mut Vec<PathBuf>) {
    match fs::read_dir(path) {
        Ok(entries) => {
            for entry in entries.flatten() {
                let file = entry.path();
                if file.is_dir() {
                    collect_yaml_files(&file, out);
                } else if is_binding_yaml(&file) {
                    out.push(file);
                }
            }
        }
        Err(_) => {
            if is_binding_yaml(path) {
                out.push(path.to_path_buf());
            }
        }
    }
}

/// The legacy documentation that still exists next to the YAML bindings is
/// plain text — only YAML is parseable.
fn is_binding_yaml(path: &Path) -> bool {
    path.extension().and_then(|ext| ext.to_str()) == Some("yaml")
}

#[cfg(test)]
mod tests {
    use super::*;

    const UART_BINDING: &str = r#"
%YAML 1.2
---
$id: http://devicetree.org/schemas/serial/fsl-imx-uart.yaml#
$schema: http://devicetree.org/meta-schemas/core.yaml#

title: Freescale i.MX UART

maintainers:
  - Someone <someone@example.com>

properties:
  compatible:
    oneOf:
      - const: fsl,imx1-uart
      - items:
          - enum:
              - fsl,imx8mm-uart
              - fsl,imx6q-uart
          - const: fsl,imx6q-uart

  reg:
    maxItems: 1
  status:
    description: indicates the operational status
    enum:
      - okay
      - disabled
  wakeup-source: true
  interrupts:
    $ref: /schemas/types.yaml#/definitions/uint32-array
    deprecated: true

required:
  - compatible
  - reg
  - interrupts
"#;

    const GPIO_BINDING: &str = r#"
properties:
  compatible:
    const: vendor,gpio
  gpio-controller: true
required:
  - compatible
  - gpio-controller
"#;

    const I2C_BINDING: &str = r#"
properties:
  compatible:
    const: vendor,i2c
  reg:
    $ref: /schemas/types.yaml#/definitions/uint32
required:
  - compatible
  - reg
"#;

    const COMMON_PART_BINDING: &str = r#"
properties:
  '#address-cells': true
  '#size-cells': true
"#;

    #[test]
    pub fn compatible_extraction_walks_the_oneof_soup() {
        let binding: Value = serde_yaml::from_str(UART_BINDING).unwrap();
        let mut bindings = Bindings::default();
        bindings.add_schema(PathBuf::from("serial/fsl-imx-uart.yaml"), &binding);

        assert_eq!(bindings.schema_count(), 1);
        // the `fsl,imx6q-uart` const duplicates the enum entry and is deduped
        assert_eq!(bindings.compatible_count(), 3);
        for compatible in ["fsl,imx1-uart", "fsl,imx8mm-uart", "fsl,imx6q-uart"] {
            assert!(
                bindings.by_compatible.contains_key(compatible),
                "missing {compatible}"
            );
        }
    }

    #[test]
    pub fn property_hints_carry_flags_docs_and_types() {
        let binding: Value = serde_yaml::from_str(UART_BINDING).unwrap();
        let mut bindings = Bindings::default();
        bindings.add_schema(PathBuf::from("serial/fsl-imx-uart.yaml"), &binding);

        let properties = bindings.properties_for(&["fsl,imx8mm-uart".to_string()]);

        let reg = &properties["reg"];
        assert!(reg.required);

        let status = &properties["status"];
        assert!(!status.required);
        assert_eq!(
            status.description.as_deref(),
            Some("indicates the operational status")
        );

        let wakeup = &properties["wakeup-source"];
        assert!(wakeup.boolean);
        assert!(!wakeup.required);

        let interrupts = &properties["interrupts"];
        assert!(interrupts.required);
        assert!(interrupts.deprecated);
        assert_eq!(interrupts.type_hint.as_deref(), Some("uint32-array"));
    }

    #[test]
    pub fn hints_merge_across_compatibles_with_required_union() {
        let mut bindings = Bindings::default();
        let gpio: Value = serde_yaml::from_str(GPIO_BINDING).unwrap();
        bindings.add_schema(PathBuf::from("gpio/vendor-gpio.yaml"), &gpio);
        let i2c: Value = serde_yaml::from_str(I2C_BINDING).unwrap();
        bindings.add_schema(PathBuf::from("i2c/vendor-i2c.yaml"), &i2c);

        let merged = bindings.properties_for(&["vendor,gpio".to_string(), "vendor,i2c".to_string()]);

        assert!(merged["gpio-controller"].required);
        assert!(merged["reg"].required);
        assert_eq!(merged["reg"].type_hint.as_deref(), Some("uint32"));
    }

    #[test]
    pub fn bindings_without_compatible_do_not_match() {
        let mut bindings = Bindings::default();
        let common: Value = serde_yaml::from_str(COMMON_PART_BINDING).unwrap();
        bindings.add_schema(PathBuf::from("common.yaml"), &common);

        assert_eq!(bindings.schema_count(), 0);
        assert!(bindings.properties_for(&["whatever".to_string()]).is_empty());
    }

    const VALUES_BINDING: &str = r#"
properties:
  compatible:
    oneOf:
      - const: vendor,dev
      - items:
          - enum:
              - vendor,dev2
          - const: vendor,dev
  mode:
    description: the device mode
    enum:
      - host
      - peripheral
      - otg
  clock-names:
    items:
      - const: ipg
      - const: per
  ports:
    description: demonstrates child-subschema skipping
    properties:
      port:
        type: object
        properties:
          endpoint:
            enum: [ child-value ]
"#;

    #[test]
    pub fn property_values_collect_enums_and_consts() {
        let binding: Value = serde_yaml::from_str(VALUES_BINDING).unwrap();
        let mut bindings = Bindings::default();
        bindings.add_schema(PathBuf::from("vendor/dev.yaml"), &binding);

        let properties = bindings.properties_for(&["vendor,dev".to_string()]);

        let mode = &properties["mode"];
        assert_eq!(
            mode.values,
            vec![
                "host".to_string(),
                "peripheral".to_string(),
                "otg".to_string(),
            ]
        );

        let clock_names = &properties["clock-names"];
        assert_eq!(clock_names.values, vec!["ipg".to_string(), "per".to_string()]);

        // the compatible property's values are the schema's candidate strings
        let compatible = &properties["compatible"];
        assert!(compatible.values.contains(&"vendor,dev".to_string()));
        assert!(compatible.values.contains(&"vendor,dev2".to_string()));

        // child-node property schemas are not descended into
        let ports = &properties["ports"];
        assert!(ports.values.is_empty());
    }

    const COMMON_BINDING: &str = r#"
properties:
  status:
    description: indicates the operational status
    enum:
      - okay
      - disabled
  shared-thing: true
"#;

    #[test]
    pub fn common_schemas_contribute_only_value_candidates() {
        let mut bindings = Bindings::default();
        let common: Value = serde_yaml::from_str(COMMON_BINDING).unwrap();
        bindings.add_schema(PathBuf::from("status.yaml"), &common);

        // no compatible => no schema => property-name completion unaffected
        assert_eq!(bindings.schema_count(), 0);
        assert!(bindings.properties_for(&["whatever".to_string()]).is_empty());

        // value candidates come from the common set, matched by name
        assert_eq!(
            bindings.common_values("status").to_vec(),
            vec!["okay".to_string(), "disabled".to_string()]
        );
        assert!(bindings.common_values("shared-thing").is_empty());
    }

    #[test]
    pub fn from_paths_walks_directories_and_skips_legacy_txt() {
        let dir = tempfile::tempdir().unwrap();
        let gpio_dir = dir.path().join("gpio");
        fs::create_dir_all(&gpio_dir).unwrap();
        fs::write(gpio_dir.join("vendor-gpio.yaml"), GPIO_BINDING).unwrap();
        fs::write(dir.path().join("i2c.yaml"), I2C_BINDING).unwrap();
        fs::write(dir.path().join("legacy.txt"), "old-style binding").unwrap();

        let bindings = Bindings::from_paths(&[dir.path().to_path_buf()]);

        assert_eq!(bindings.schema_count(), 2);
        assert_eq!(bindings.compatible_count(), 2);
    }
}
