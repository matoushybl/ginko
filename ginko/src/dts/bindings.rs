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

use std::collections::{BTreeMap, BTreeSet, HashMap};
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
    /// Child-node schemas keyed by `patternProperties` patterns.
    pub children: Vec<ChildSchema>,
}

/// A child-node schema: the `patternProperties` entries of a binding whose
/// patterns a child node's NAME matches, with properties merged from inline
/// definitions and relative `$ref`s.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ChildSchema {
    /// Raw `patternProperties` pattern keys (alternatives split out).
    pub patterns: Vec<String>,
    /// Property name -> hint for the child node.
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
    /// Define-prefix families referenced by the description ("Use one of
    /// the LED_COLOR_ID_* prefixed definitions …"), mapped to the relative
    /// header path that defines them.
    pub define_families: BTreeMap<String, String>,
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
    /// Neighbor schema files pulled in by patternProperties `$ref`s,
    /// parsed once per absolute path.
    ref_cache: HashMap<PathBuf, Value>,
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
                    merge_hint(merged.entry(name.clone()).or_default(), hint);
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
            // dt-schema `select: true` schemas (its `dt-core.yaml`) apply to
            // every node despite a pattern-only `compatible`: their property
            // enums (status, ...) are global value candidates. Pattern-only
            // schemas without `select: true` only match on their pattern —
            // they stay unused, as before.
            if matches!(binding.get("select"), Some(Value::Bool(true))) {
                self.add_common_schema(binding);
            }
            return;
        }
        let properties = extract_properties(binding);
        let children = extract_children(binding, &file, &mut self.ref_cache);
        self.schemas.push(CompletionSchema {
            file,
            properties,
            children,
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
            merge_hint(self.common.entry(name).or_default(), &hint);
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

    /// The common-part hint for a property — the merge of all
    /// compatible-less binding files that mention it (e.g. `color` from
    /// leds/common.yaml).
    pub fn common_hint(&self, name: &str) -> Option<&PropertyHint> {
        self.common.get(name)
    }

    /// Property hints for a CHILD node keyed by its parent's binding:
    /// every child schema of the schemas matching `parent_compatibles`
    /// whose `patternProperties` keys match `child_name`.
    pub fn child_properties_for(
        &self,
        parent_compatibles: &[String],
        child_name: &str,
    ) -> BTreeMap<String, PropertyHint> {
        let mut merged: BTreeMap<String, PropertyHint> = BTreeMap::new();
        for compatible in parent_compatibles {
            for idx in self.by_compatible.get(compatible).into_iter().flatten() {
                for child in &self.schemas[*idx].children {
                    if pattern_matches(&child.patterns, child_name) {
                        for (name, hint) in &child.properties {
                            merge_hint(merged.entry(name.clone()).or_default(), hint);
                        }
                    }
                }
            }
        }
        merged
    }

    /// The number of distinct property names contributed by common-part
    /// and `select: true` schemas — the audit metric for which shared files
    /// feed value completion.
    pub fn common_prop_count(&self) -> usize {
        self.common.len()
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
    let define_families = define_families_from(description.as_deref());
    let mut values = Vec::new();
    collect_value_strings(sub, &mut values);
    PropertyHint {
        required,
        boolean: false,
        type_hint,
        description,
        deprecated,
        values,
        define_families,
    }
}

/// Bindings prose convention: "Use one of the LED_COLOR_ID_* prefixed
/// definitions from the header include/dt-bindings/leds/common.h." Extract
/// the uppercase `_`-suffixed token(s) starred via `*` in such descriptions
/// and map each family prefix to the first `include/…h` path mention;
/// descriptions without a header mention yield nothing.
fn define_families_from(description: Option<&str>) -> BTreeMap<String, String> {
    let Some(description) = description else {
        return BTreeMap::new();
    };
    let Some(start) = description.find("include/") else {
        return BTreeMap::new();
    };
    let rest = &description[start..];
    let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
    let header = rest[..end]
        .trim_end_matches(|c| c == '.' || c == ',' || c == ';')
        .to_string();
    if !header.ends_with(".h") || header == ".h" {
        return BTreeMap::new();
    }
    let bytes = description.as_bytes();
    let mut families = BTreeMap::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i].is_ascii_uppercase() {
            let start = i;
            i += 1;
            while i < bytes.len()
                && (bytes[i].is_ascii_uppercase()
                    || bytes[i].is_ascii_digit()
                    || bytes[i] == b'_')
            {
                i += 1;
            }
            let token = &description[start..i];
            if i < bytes.len() && bytes[i] == b'*' && token.ends_with('_') {
                families.insert(token.to_string(), header.clone());
            }
        } else {
            i += 1;
        }
    }
    families
}

/// Fold `src` into `dst`: required/boolean/deprecated OR together, values
/// dedupe, and the first non-empty optional field wins.
fn merge_hint(dst: &mut PropertyHint, src: &PropertyHint) {
    dst.required |= src.required;
    dst.boolean |= src.boolean;
    dst.deprecated |= src.deprecated;
    for value in &src.values {
        if !dst.values.contains(value) {
            dst.values.push(value.clone());
        }
    }
    if dst.type_hint.is_none() {
        dst.type_hint = src.type_hint.clone();
    }
    if dst.description.is_none() {
        dst.description = src.description.clone();
    }
    for (prefix, header) in &src.define_families {
        dst.define_families
            .entry(prefix.clone())
            .or_insert_with(|| header.clone());
    }
}

/// Extract the binding's `patternProperties` child schemas: per pattern,
/// the child's INLINE properties merged with a relative `$ref`'s leaf
/// properties (dtschema merges both, e.g. leds-gpio children $ref
/// leds/common.yaml while adding `gpios`). Ref expansion is depth-one by
/// design — grandchildren are a follow-up — so refs cannot cycle.
fn extract_children(
    binding: &Value,
    schema_file: &Path,
    ref_cache: &mut HashMap<PathBuf, Value>,
) -> Vec<ChildSchema> {
    let Some(Value::Mapping(patterns)) = binding.get("patternProperties") else {
        return Vec::new();
    };
    let dir = schema_file.parent().unwrap_or(Path::new("."));
    let mut children = Vec::new();
    for (pattern, value) in patterns {
        let (Some(pattern), Value::Mapping(child)) = (pattern.as_str(), value) else {
            continue;
        };
        let mut properties: BTreeMap<String, PropertyHint> = BTreeMap::new();
        if let Some(reference) = child.get("$ref").and_then(Value::as_str) {
            if let Some((target, segments)) = split_ref(dir, reference) {
                if let Some(referenced) = load_ref_value(target, &segments, ref_cache) {
                    for (name, hint) in extract_properties(referenced) {
                        merge_hint(properties.entry(name).or_default(), &hint);
                    }
                }
            }
        }
        for (name, hint) in extract_properties(value) {
            merge_hint(properties.entry(name).or_default(), &hint);
        }
        children.push(ChildSchema {
            patterns: split_patterns(pattern),
            properties,
        });
    }
    children
}

/// Split a schema-local `$ref` into the target file path (resolved against
/// the referencing schema's directory) and the post-`#` key segments.
/// External refs (`/schemas/…`, absolute paths, URLs) resolve to None.
fn split_ref(dir: &Path, reference: &str) -> Option<(PathBuf, Vec<String>)> {
    let (file, fragment) = reference.split_once('#').unwrap_or((reference, ""));
    if file.contains("://") || file.starts_with('/') || file.is_empty() {
        return None;
    }
    let target = fs::canonicalize(dir.join(file)).ok()?;
    let segments = fragment
        .split('/')
        .filter(|segment| !segment.is_empty())
        .map(str::to_string)
        .collect();
    Some((target, segments))
}

/// The (cached) yaml value a `$ref` points at: load and parse the file on
/// first use, then descend the `#/a/b` fragment keys.
fn load_ref_value<'v>(
    target: PathBuf,
    segments: &[String],
    ref_cache: &'v mut HashMap<PathBuf, Value>,
) -> Option<&'v Value> {
    if !ref_cache.contains_key(&target) {
        let Ok(text) = fs::read_to_string(&target) else {
            return None;
        };
        let Ok(value) = serde_yaml::from_str::<Value>(&text) else {
            return None;
        };
        ref_cache.insert(target.clone(), value);
    }
    let mut current = ref_cache.get(&target)?;
    for segment in segments {
        current = current.get(segment)?;
    }
    Some(current)
}

/// The alternatives of a `patternProperties` key: strip one surrounding
/// paren pair and split on `|` — "(^led-[0-9a-f]$|led)" becomes
/// ["^led-[0-9a-f]$", "led"].
fn split_patterns(pattern: &str) -> Vec<String> {
    let trimmed = pattern.trim();
    let inner = trimmed
        .strip_prefix('(')
        .and_then(|rest| rest.strip_suffix(')'))
        .unwrap_or(trimmed);
    inner
        .split('|')
        .map(str::trim)
        .filter(|alt| !alt.is_empty())
        .map(str::to_string)
        .collect()
}

/// Whether the child node NAME matches the binding pattern keys.
/// Alternatives test in order; plain literals bind via contains/prefix/
/// suffix/equality per anchors, regex-y alternatives (character classes,
/// `.*`) fall back to literal-run tests — deliberate OVER-match: this
/// suggests, it does not validate.
fn pattern_matches(patterns: &[String], node_name: &str) -> bool {
    patterns
        .iter()
        .any(|pattern| alternative_matches(pattern, node_name))
}

fn alternative_matches(pattern: &str, name: &str) -> bool {
    let mut anchored_start = false;
    let mut anchored_end = false;
    let mut rest = pattern;
    if let Some(stripped) = rest.strip_prefix('^') {
        anchored_start = true;
        rest = stripped;
    }
    if let Some(stripped) = rest.strip_suffix('$') {
        anchored_end = true;
        rest = stripped;
    }
    // Leading/trailing wildcard runs void their anchors: `^.*-pins$` anchors
    // nothing at the start, it only says the name ends in `-pins`.
    if let Some(stripped) = rest.strip_prefix(".*").or_else(|| rest.strip_prefix(".+")) {
        anchored_start = false;
        rest = stripped;
    }
    if let Some(stripped) = rest.strip_suffix(".*").or_else(|| rest.strip_suffix(".+")) {
        anchored_end = false;
        rest = stripped;
    }
    if is_plain_literal(rest) {
        return if anchored_start && anchored_end {
            name == rest
        } else if anchored_start {
            name.starts_with(rest)
        } else if anchored_end {
            name.ends_with(rest)
        } else {
            name.contains(rest)
        };
    }
    // regex metachars present: require the first/last literal runs per
    // anchors, over the exact class contents (completion-friendly)
    let runs = literal_runs(rest);
    let Some(first) = runs.first() else {
        return false;
    };
    if anchored_start {
        if !name.starts_with(first.as_str()) {
            return false;
        }
        return if runs.len() == 1 && anchored_end {
            name.ends_with(first.as_str())
        } else {
            true
        };
    }
    if let Some(last) = runs.last() {
        if anchored_end {
            return name.ends_with(last.as_str());
        }
    }
    runs.iter().all(|run| name.contains(run.as_str()))
}

fn is_plain_literal(text: &str) -> bool {
    !text.is_empty()
        && text
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// The maximal runs of `text` that contain no regex metachars.
fn literal_runs(text: &str) -> Vec<String> {
    let metachar = |c: char| !(c.is_ascii_alphanumeric() || c == '-' || c == '_');
    let mut runs = Vec::new();
    let mut current = String::new();
    for c in text.chars() {
        if metachar(c) {
            if !current.is_empty() {
                runs.push(current);
                current = String::new();
            }
        } else {
            current.push(c);
        }
    }
    if !current.is_empty() {
        runs.push(current);
    }
    runs
}

/// The tail of a types.yaml `$ref`, e.g. `uint32` from
/// `/schemas/types.yaml#/definitions/uint32`.
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
    pub fn define_families_bind_prefixes_to_header_mentions() {
        let color_desc = "Color of the LED. Use one of the LED_COLOR_ID_* \
prefixed definitions from the header include/dt-bindings/leds/common.h. If \
there is no matching LED_COLOR_ID available, add a new one.";

        let families = define_families_from(Some(color_desc));
        // `LED_COLOR_ID` without the `_*` star is not a family
        assert_eq!(families.len(), 1);
        assert_eq!(
            families.get("LED_COLOR_ID_").map(String::as_str),
            Some("include/dt-bindings/leds/common.h")
        );

        assert!(define_families_from(Some("a plain description")).is_empty());
        assert!(define_families_from(Some("mentions LED_COLOR_ID_* but no header")).is_empty());
        assert!(define_families_from(None).is_empty());
    }

    #[test]
    pub fn child_patterns_match_binding_node_names() {
        let m = |pattern: &str, name: &str| pattern_matches(&split_patterns(pattern), name);
        // anchored literal: suffix / prefix / equality
        assert!(m("grp$", "uart1grp"));
        assert!(!m("grp$", "grpfoo"));
        assert!(m("^pins-", "pins-led"));
        assert!(!m("^pins-", "gpios-1"));
        assert!(m("^grp$", "grp"));
        assert!(!m("^grp$", "grp2"));
        // unanchored literal binds as substring (dtschema uses it to cast
        // a wide net, e.g. leds' "just 'led' anywhere")
        assert!(m("led", "led-status"));
        assert!(!m("led", "backlight"));
        // wildcard-run patterns: anchors fall away behind `.*`
        assert!(m("^.*-pins$", "host-pins"));
        assert!(!m("^.*-pins$", "host-clocks"));
        // parenthesized alternatives with a character class: the class
        // content is deliberately over-matched
        assert!(m("(^led-[0-9a-f]$|led)", "led-1"));
        assert!(m("(^led-[0-9a-f]$|led)", "led-status"));
        assert!(!m("(^led-[0-9a-f]$|led)", "backlight"));
    }

    #[test]
    pub fn child_properties_merge_inline_and_refs() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("common2.yaml"),
            r#"
properties:
  color:
    description: Use one of the LED_COLOR_ID_* prefixed definitions from the header include/dt-bindings/leds/common.h.
  label: true
"#,
        )
        .unwrap();
        fs::write(
            dir.path().join("leds.yaml"),
            r#"
properties:
  compatible:
    const: vendor,led
patternProperties:
  "(^led-[0-9a-f]$|led)":
    type: object
    $ref: common2.yaml#
    properties:
      gpios: true
"#,
        )
        .unwrap();
        fs::write(
            dir.path().join("pinctrl2.yaml"),
            r#"
properties:
  compatible:
    const: vendor,pctl
patternProperties:
  'grp$':
    type: object
    properties:
      fsl,pins: true
"#,
        )
        .unwrap();

        let bindings = Bindings::from_paths(&[dir.path().to_path_buf()]);

        // inline + ref-merged LED child hints, only for matching child names
        let hints = bindings.child_properties_for(&["vendor,led".to_string()], "led-status");
        assert!(hints.contains_key("gpios"));
        assert!(hints.contains_key("color"));
        assert!(hints.contains_key("label"));
        assert!(hints["color"]
            .define_families
            .contains_key("LED_COLOR_ID_"));
        assert!(bindings
            .child_properties_for(&["vendor,led".to_string()], "clock-controller")
            .is_empty());

        // inline-only child schema (imx pinctrl shape)
        let hints = bindings.child_properties_for(&["vendor,pctl".to_string()], "uart1grp");
        assert!(hints.contains_key("fsl,pins"));
        assert!(bindings
            .child_properties_for(&["vendor,pctl".to_string()], "somewhere-else")
            .is_empty());
    }

    #[test]
    pub fn mutual_child_refs_do_not_cycle() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("a.yaml"),
            r#"
properties:
  compatible:
    const: vendor,a
patternProperties:
  '^a-':
    $ref: b.yaml#
"#,
        )
        .unwrap();
        fs::write(
            dir.path().join("b.yaml"),
            r#"
properties:
  b-marker: true
  compatible:
    const: vendor,b
patternProperties:
  '^b-':
    $ref: a.yaml#
"#,
        )
        .unwrap();

        // depth-one ref expansion cannot loop; indexing terminates
        let bindings = Bindings::from_paths(&[dir.path().to_path_buf()]);
        let hints = bindings.child_properties_for(&["vendor,a".to_string()], "a-child");
        assert!(hints.contains_key("b-marker"));
    }

    #[test]
    pub fn common_and_matched_hints_carry_define_families() {
        const MATCHED_LED: &str = r#"
properties:
  compatible:
    const: vendor,led
  color:
    description: Use one of the LED_COLOR_ID_* prefixed definitions from the header include/dt-bindings/leds/common.h.
"#;
        const COMMON_LED: &str = r#"
properties:
  color:
    description: Use one of the LED_COLOR_ID_* prefixed definitions from the header include/dt-bindings/leds/common.h.
  function:
    description: Use one of the LED_FUNCTION_* prefixed definitions from the header include/dt-bindings/leds/common.h.
"#;
        let mut bindings = Bindings::default();
        let matched_yaml: Value = serde_yaml::from_str(MATCHED_LED).unwrap();
        bindings.add_schema(PathBuf::from("leds/vendor-led.yaml"), &matched_yaml);
        let common_yaml: Value = serde_yaml::from_str(COMMON_LED).unwrap();
        bindings.add_schema(PathBuf::from("leds/common.yaml"), &common_yaml);

        let matched = bindings.properties_for(&["vendor,led".to_string()]);
        assert_eq!(
            matched["color"]
                .define_families
                .get("LED_COLOR_ID_")
                .map(String::as_str),
            Some("include/dt-bindings/leds/common.h")
        );

        let function = bindings.common_hint("function").unwrap();
        assert!(function.define_families.contains_key("LED_FUNCTION_"));
        assert!(bindings.common_hint("color").is_some());
    }

    #[test]
    pub fn select_true_pattern_only_schemas_are_common() {
        const DT_CORE: &str = r#"
select: true
properties:
  compatible:
    $ref: types.yaml#/definitions/string-array
  status:
    oneOf:
      - type: object
      - $ref: types.yaml#/definitions/string
        enum: [ okay, disabled, reserved, fail, fail-needs-probe ]
"#;
        const PATTERN_ONLY: &str = r#"
properties:
  compatible:
    $ref: types.yaml#/definitions/string-array
  status:
    enum: [ okay, disabled ]
"#;

        let mut bindings = Bindings::default();
        let core: Value = serde_yaml::from_str(DT_CORE).unwrap();
        bindings.add_schema(PathBuf::from("dt-core.yaml"), &core);
        assert_eq!(bindings.schema_count(), 0);
        assert_eq!(
            bindings.common_values("status").to_vec(),
            vec![
                "okay".to_string(),
                "disabled".to_string(),
                "reserved".to_string(),
                "fail".to_string(),
                "fail-needs-probe".to_string(),
            ]
        );

        // pattern-only without `select: true` matches on its pattern only —
        // it must not contribute common candidates (as before)
        let mut bindings = Bindings::default();
        let pattern_only: Value = serde_yaml::from_str(PATTERN_ONLY).unwrap();
        bindings.add_schema(PathBuf::from("serial.yaml"), &pattern_only);
        assert_eq!(bindings.schema_count(), 0);
        assert!(bindings.common_values("status").is_empty());
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
