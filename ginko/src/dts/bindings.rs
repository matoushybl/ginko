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
/// patterns a child node's NAME matches, plus fixed-name node entries under
/// `properties` (classified as child nodes), with properties merged from
/// inline definitions and `$ref`s. `children` carries grandchildren —
/// recursion is depth-capped and cycle-guarded.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ChildSchema {
    /// Raw pattern keys (alternatives split out); fixed-name children use
    /// `^name$`.
    pub patterns: Vec<String>,
    /// Property name -> hint for the child node.
    pub properties: BTreeMap<String, PropertyHint>,
    /// Grandchildren: the child's own child-node schemas.
    pub children: Vec<ChildSchema>,
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
    /// The entry structurally denotes a child NODE (carries child schemas)
    /// rather than a leaf — rendered as a node-creation snippet.
    pub child_object: bool,
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
    /// The binding roots given to `from_paths` — the resolution base for
    /// dt-schema flavour absolute refs (`/schemas/graph.yaml#/…`).
    roots: Vec<PathBuf>,
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
        let mut bindings = Bindings {
            roots: paths.to_vec(),
            ..Default::default()
        };
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
            self.add_common_schema(binding, &file);
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
                self.add_common_schema(binding, &file);
            }
            return;
        }
        let properties = {
            let mut merged: BTreeMap<String, PropertyHint> = BTreeMap::new();
            for (view, _) in
                flat_views(binding, &file, &mut self.ref_cache, &self.roots)
            {
                for (name, hint) in extract_properties(&view) {
                    merge_hint(merged.entry(name.clone()).or_default(), &hint);
                }
            }
            merged
        };
        let children = extract_children(
            binding,
            &file,
            &self.roots,
            &mut self.ref_cache,
            0,
        );
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
    fn add_common_schema(&mut self, binding: &Value, file: &Path) {
        for (view, _) in flat_views(binding, file, &mut self.ref_cache, &self.roots) {
            for (name, hint) in extract_properties(&view) {
                merge_hint(self.common.entry(name.clone()).or_default(), &hint);
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

    /// The common-part hint for a property — the merge of all
    /// compatible-less binding files that mention it (e.g. `color` from
    /// leds/common.yaml).
    pub fn common_hint(&self, name: &str) -> Option<&PropertyHint> {
        self.common.get(name)
    }

    /// Property hints for a DESCENDANT node: walk the child schemas of every
    /// schema matching `compatibles` down `path` (one node name per level,
    /// the target node's own name last); the properties of the schemas
    /// matching the FINAL name merge out. A one-element path is the
    /// direct-child lookup; deeper paths light up grandchildren
    /// (ports → port@N → endpoint chains).
    pub fn child_properties_for(
        &self,
        compatibles: &[String],
        path: &[String],
    ) -> BTreeMap<String, PropertyHint> {
        let mut pool: Vec<&ChildSchema> = Vec::new();
        for compatible in compatibles {
            for idx in self.by_compatible.get(compatible).into_iter().flatten() {
                pool.extend(self.schemas[*idx].children.iter());
            }
        }
        let mut matched: Vec<&ChildSchema> = Vec::new();
        for name in path {
            matched = pool
                .iter()
                .copied()
                .filter(|schema| pattern_matches(&schema.patterns, name))
                .collect();
            if matched.is_empty() {
                return BTreeMap::new();
            }
            pool = matched
                .iter()
                .flat_map(|schema| schema.children.iter())
                .collect();
        }
        let mut merged = BTreeMap::new();
        for schema in matched {
            for (name, hint) in &schema.properties {
                merge_hint(merged.entry(name.clone()).or_default(), hint);
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

    /// Every indexed child schema, grandchildren included — audit metric.
    pub fn child_schema_count(&self) -> usize {
        fn count(children: &[ChildSchema]) -> usize {
            children.iter().map(|child| 1 + count(&child.children)).sum()
        }
        self.schemas
            .iter()
            .map(|schema| count(&schema.children))
            .sum()
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
                // dt-schema meta-constraints ($nodename, additional mapping
                // hints) are directives, not devicetree properties
                if !name.starts_with('$') {
                    properties.insert(
                        name.to_string(),
                        property_hint(sub, required.contains(name)),
                    );
                }
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
        child_object: node_like(sub),
    }
}

/// Bindings prose conventions that point completion at a header's defines.
/// Starred uppercase families ("Use one of the LED_COLOR_ID_* prefixed
/// definitions … include/dt-bindings/leds/common.h.") map each `X_*`
/// prefix to the first plain `…h` path mention. An angle-bracketed header
/// mention without a starred family ("… macro, which can be found in
/// <arch/…/imx8m[m,n,p,q]-pinfunc.h>.", fsl,imx8m-pinctrl prose) names a
/// whole-header family keyed by the empty prefix: every `#define` of that
/// header is a candidate. Descriptions without a header mention yield
/// nothing.
fn define_families_from(description: Option<&str>) -> BTreeMap<String, String> {
    let Some(description) = description else {
        return BTreeMap::new();
    };

    // starred uppercase family tokens: `LED_COLOR_ID_*`
    let bytes = description.as_bytes();
    let mut starred: Vec<String> = Vec::new();
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
                starred.push(token.to_string());
            }
        } else {
            i += 1;
        }
    }

    // header mentions: path-ish tokens ending `.h` (trailing punctuation
    // trimmed), split by whether the prose angle-brackets them
    let is_path_char = |c: char| {
        c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '/' | '-' | '[' | ']' | ',')
    };
    let mut plain_headers: Vec<String> = Vec::new();
    let mut angle_headers: Vec<String> = Vec::new();
    let mut start = None;
    for (idx, c) in description.char_indices() {
        if is_path_char(c) {
            if start.is_none() {
                start = Some(idx);
            }
        } else if let Some(begin) = start.take() {
            let token = description[begin..idx].trim_end_matches(['.', ',']);
            if token.ends_with(".h") && token.len() > 2 {
                if description[..begin].ends_with('<') {
                    angle_headers.push(token.to_string());
                } else {
                    plain_headers.push(token.to_string());
                }
            }
        }
    }
    if let Some(begin) = start {
        let token = description[begin..].trim_end_matches(['.', ',', '>']);
        if token.ends_with(".h") && token.len() > 2 {
            if description[..begin].ends_with('<') {
                angle_headers.push(token.to_string());
            } else {
                plain_headers.push(token.to_string());
            }
        }
    }

    let mut families = BTreeMap::new();
    if !starred.is_empty() {
        if let Some(header) = plain_headers.first() {
            for family in starred {
                families.insert(family, header.clone());
            }
        }
    } else if let Some(header) = angle_headers.first() {
        // whole-header family: no prefix filters its defines
        families.insert(String::new(), header.clone());
    }
    families
}

/// Fold `src` into `dst`: required/boolean/deprecated OR together, values
/// dedupe, and the first non-empty optional field wins.
pub fn merge_hint(dst: &mut PropertyHint, src: &PropertyHint) {
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
    dst.child_object |= src.child_object;
    for (prefix, header) in &src.define_families {
        dst.define_families
            .entry(prefix.clone())
            .or_insert_with(|| header.clone());
    }
}

/// The terminal views of a schema: dtschema applies `allOf` members
/// jointly and treats bare `$ref` wrappers transparently — chase both and
/// OVERLAY: every reached view's inline content (its own `properties`,
/// `patternProperties`, `required`, entries) describes the schema (the
/// leds-gpio pattern child $refs leds/common.yaml while adding inline
/// `gpios`; audio-graph-port's `definitions/endpoint-base` $refs the
/// graph alongside its own dai properties). Chased targets dedupe per
/// (file, fragment); deeper structural recursion stays depth-capped by
/// `MAX_DEPTH`.
fn flat_views(
    sub: &Value,
    origin_file: &Path,
    ref_cache: &mut HashMap<PathBuf, Value>,
    roots: &[PathBuf],
) -> Vec<(Value, PathBuf)> {
    let mut heads: Vec<(Value, PathBuf)> = Vec::new();
    let mut seen: Vec<(PathBuf, Vec<String>)> = Vec::new();
    let mut queue: Vec<(Value, PathBuf)> = vec![(sub.clone(), origin_file.to_path_buf())];
    while let Some((view, vorigin)) = queue.pop() {
        if let Some(reference) = view.get("$ref").and_then(Value::as_str) {
            if let Some((target, segments)) = split_ref(&vorigin, reference, roots) {
                let key = (target.clone(), segments.clone());
                if !seen.contains(&key) {
                    if let Some(next) = load_ref_value(target.clone(), &segments, ref_cache) {
                        seen.push(key);
                        queue.push((next.clone(), target));
                    }
                }
            }
        }
        if let Some(Value::Sequence(members)) = view.get("allOf") {
            for member in members {
                // members originate from the same file as their parent:
                // their `#/…` fragments resolve there
                queue.push((member.clone(), vorigin.clone()));
            }
        }
        heads.push((view, vorigin));
    }
    heads
}

/// Extract the child-node schemas of `schema`: `patternProperties` entries
/// (pattern children) and `properties` entries that structurally denote
/// child nodes (fixed-name children), collected from every flat view of
/// `schema` (inline + `allOf` members + `$ref` wrappers). Recursion is
/// depth-capped; ref chasing is cycle-guarded by `flat_views`.
fn extract_children(
    schema: &Value,
    schema_file: &Path,
    roots: &[PathBuf],
    ref_cache: &mut HashMap<PathBuf, Value>,
    depth: usize,
) -> Vec<ChildSchema> {
    const MAX_DEPTH: usize = 4;
    if depth > MAX_DEPTH {
        return Vec::new();
    }
    // collect catalog entries (patterns, subschema clone, origin file) from
    // every flat view; clones avoid holding ref-cache borrows across recursion
    let mut entries: Vec<(Vec<String>, Value, PathBuf)> = Vec::new();
    for (view, origin) in flat_views(schema, schema_file, ref_cache, roots) {
        collect_child_entries(&view, &origin, ref_cache, roots, &mut entries);
    }
    // build per entry, merging same-pattern (duplicated inline+ref) entries
    let mut children: Vec<ChildSchema> = Vec::new();
    for (patterns, subschema, origin_file) in entries {
        let built = child_schema_for(
            patterns,
            &subschema,
            &origin_file,
            roots,
            ref_cache,
            depth,
        );
        if let Some(existing) = children
            .iter_mut()
            .find(|child| child.patterns == built.patterns)
        {
            for (name, hint) in &built.properties {
                merge_hint(existing.properties.entry(name.clone()).or_default(), hint);
            }
            existing.children.extend(built.children);
        } else {
            children.push(built);
        }
    }
    children
}

/// One entry's full ChildSchema: leaf properties merge from every flat
/// view of the entry; its own children recurse likewise. `required:`
/// entries of bare stubs (graph.yaml's `^port@…$` requires `reg` without a
/// property body) synthesize required hints for names the views never
/// spell out.
fn child_schema_for(
    patterns: Vec<String>,
    subschema: &Value,
    origin_file: &Path,
    roots: &[PathBuf],
    ref_cache: &mut HashMap<PathBuf, Value>,
    depth: usize,
) -> ChildSchema {
    let heads = flat_views(subschema, origin_file, ref_cache, roots);
    let mut properties: BTreeMap<String, PropertyHint> = BTreeMap::new();
    for (view, _) in &heads {
        for (name, hint) in extract_properties(view) {
            merge_hint(properties.entry(name.clone()).or_default(), &hint);
        }
    }
    for (view, _) in &heads {
        for name in required_set(view) {
            properties.entry(name).or_default().required = true;
        }
    }
    let children = extract_children(subschema, origin_file, roots, ref_cache, depth + 1);
    ChildSchema {
        patterns,
        properties,
        children,
    }
}

/// Append the patternProperties entries and the child-node classified
/// `properties` entries of `view` to `entries`.
fn collect_child_entries(
    view: &Value,
    origin_file: &Path,
    ref_cache: &mut HashMap<PathBuf, Value>,
    roots: &[PathBuf],
    entries: &mut Vec<(Vec<String>, Value, PathBuf)>,
) {
    if let Some(Value::Mapping(patterns)) = view.get("patternProperties") {
        for (pattern, child) in patterns {
            let (Some(pattern), Value::Mapping(_)) = (pattern.as_str(), child) else {
                continue;
            };
            entries.push((
                split_patterns(pattern),
                child.clone(),
                origin_file.to_path_buf(),
            ));
        }
    }
    if let Some(Value::Mapping(props)) = view.get("properties") {
        for (name, sub) in props {
            let (Some(name), Value::Mapping(_)) = (name.as_str(), sub) else {
                continue;
            };
            if node_like_or_resolved(sub, origin_file, ref_cache, roots) {
                entries.push((
                    vec![format!("^{name}$")],
                    sub.clone(),
                    origin_file.to_path_buf(),
                ));
            }
        }
    }
}

/// devicetree nodes double as properties — dt-schema keeps no node/leaf
/// distinction, so completion invents one: a mapping entry is node-like
/// when it (or any of its flat views — inline, `allOf`, `$ref` chases)
/// carries non-empty nested `properties`/`patternProperties` (e.g. bare
/// `port@0: $ref: audio-graph-port.yaml#` entries).
fn node_like(value: &Value) -> bool {
    for key in ["properties", "patternProperties"] {
        if let Some(Value::Mapping(inner)) = value.get(key) {
            if !inner.is_empty() {
                return true;
            }
        }
    }
    false
}

fn node_like_or_resolved(
    sub: &Value,
    origin_file: &Path,
    ref_cache: &mut HashMap<PathBuf, Value>,
    roots: &[PathBuf],
) -> bool {
    if node_like(sub) {
        return true;
    }
    flat_views(sub, origin_file, ref_cache, roots)
        .iter()
        .any(|(view, _)| node_like(view))
}

/// Split a schema `$ref` into the target file path and the post-`#` key
/// segments. Handles three flavours: same-file `#/…` fragments, relative
/// neighbor files (`common.yaml#/…`), and dt-schema absolute refs
/// (`/schemas/graph.yaml#/…`) resolved against every registered binding
/// root — either verbatim or without the `schemas/` prefix (the dtschema
/// pip root IS the schemas dir). URLs stay unresolvable.
fn split_ref(schema_file: &Path, reference: &str, roots: &[PathBuf]) -> Option<(PathBuf, Vec<String>)> {
    let (file, fragment) = reference.split_once('#').unwrap_or((reference, ""));
    if file.contains("://") {
        return None;
    }
    let target = if file.is_empty() {
        fs::canonicalize(schema_file).ok()?
    } else if file.starts_with('/') {
        let rel = file.trim_start_matches('/');
        let stripped = rel.strip_prefix("schemas/").unwrap_or(rel);
        let mut found = None;
        'roots: for root in roots {
            for candidate in [root.join(rel), root.join(stripped)] {
                if candidate.exists() {
                    if let Ok(canonical) = fs::canonicalize(&candidate) {
                        found = Some(canonical);
                        break 'roots;
                    }
                }
            }
        }
        found?
    } else {
        let dir = schema_file.parent().unwrap_or(Path::new("."));
        fs::canonicalize(dir.join(file)).ok()?
    };
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
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '@')
}

/// The maximal runs of `text` that contain no regex metachars.
fn literal_runs(text: &str) -> Vec<String> {
    let metachar =
        |c: char| !(c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '@');
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
                                if let Some(scalar) = scalar_string(item) {
                                    out.push(scalar);
                                }
                            }
                        }
                    }
                    Some("const") => {
                        if let Some(scalar) = scalar_string(val) {
                            out.push(scalar);
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

/// Binding `enum:`/`const:` bare scalars — integers as well as strings
/// (video-interfaces' `bus-type: enum: [4, 5, 6]`).
fn scalar_string(value: &Value) -> Option<String> {
    match value {
        Value::String(string) => Some(string.clone()),
        Value::Number(number) => Some(number.to_string()),
        _ => None,
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
        assert!(define_families_from(Some("touches include/dt-bindings/x.h but no starred family")).is_empty());
        assert!(define_families_from(None).is_empty());

        // angle-bracketed header mentions without a starred family name a
        // whole-header family (fsl,imx8m-pinctrl prose shape)
        let pinfunc_desc = "each entry consists of 6 integers. The first 5 \
integers are specified using a PIN_FUNC_ID macro, which can be found in \
<arch/arm64/boot/dts/freescale/imx8m[m,n,p,q]-pinfunc.h>.";
        let families = define_families_from(Some(pinfunc_desc));
        assert_eq!(families.len(), 1);
        assert_eq!(
            families.get("").map(String::as_str),
            Some("arch/arm64/boot/dts/freescale/imx8m[m,n,p,q]-pinfunc.h")
        );
    }

    #[test]
    pub fn whole_header_families_reach_pinctrl_children() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("iomux.yaml"),
            r#"
properties:
  compatible:
    const: vendor,iomux
patternProperties:
  'grp$':
    type: object
    properties:
      fsl,pins:
        description: |
          each entry consists of 6 integers. The first 5 integers
          <mux_reg conf_reg input_reg mux_val input_val> are specified using
          a PIN_FUNC_ID macro, which can be found in
          <arch/arm64/boot/dts/freescale/imx8m[m,n,p,q]-pinfunc.h>.
        $ref: /schemas/types.yaml#/definitions/uint32-matrix
"#,
        )
        .unwrap();

        let bindings = Bindings::from_paths(&[dir.path().to_path_buf()]);
        let hints = bindings.child_properties_for(
            &["vendor,iomux".to_string()],
            &["uart1grp".to_string()],
        );
        let pins = hints.get("fsl,pins").expect("fsl,pins hinted in groups");
        assert_eq!(
            pins.define_families.get("").map(String::as_str),
            Some("arch/arm64/boot/dts/freescale/imx8m[m,n,p,q]-pinfunc.h")
        );
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
        // the unit-address @ joins the literal runs (graph.yaml chains)
        assert!(m("^port@[0-9a-f]+$", "port@0"));
        assert!(!m("^port@[0-9a-f]+$", "flash@0"));
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
        let hints = bindings.child_properties_for(
            &["vendor,led".to_string()],
            &["led-status".to_string()],
        );
        assert!(hints.contains_key("gpios"));
        assert!(hints.contains_key("color"));
        assert!(hints.contains_key("label"));
        assert!(hints["color"]
            .define_families
            .contains_key("LED_COLOR_ID_"));
        assert!(bindings
            .child_properties_for(
                &["vendor,led".to_string()],
                &["clock-controller".to_string()],
            )
            .is_empty());

        // inline-only child schema (imx pinctrl shape)
        let hints = bindings.child_properties_for(
            &["vendor,pctl".to_string()],
            &["uart1grp".to_string()],
        );
        assert!(hints.contains_key("fsl,pins"));
        assert!(bindings
            .child_properties_for(
                &["vendor,pctl".to_string()],
                &["somewhere-else".to_string()],
            )
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
        let hints =
            bindings.child_properties_for(&["vendor,a".to_string()], &["a-child".to_string()]);
        assert!(hints.contains_key("b-marker"));
    }

    #[test]
    pub fn classifier_marks_node_objects() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("classy.yaml"),
            r#"
$defs:
  status-def:
    type: string
    enum: [ okay, disabled, reserved ]
properties:
  compatible:
    const: vendor,classy
  ports:
    type: object
    properties:
      '#address-cells': true
  reg:
    type: array
  status: true
  port-count:
    $ref: '#/$defs/status-def'
"#,
        )
        .unwrap();
        let bindings = Bindings::from_paths(&[dir.path().to_path_buf()]);
        let hints = bindings.properties_for(&["vendor,classy".to_string()]);
        // node-like by INLINE markers only; scalars, booleans and
        // ref-only leaves stay property-shaped
        assert!(hints["ports"].child_object);
        assert!(!hints["reg"].child_object);
        assert!(!hints["status"].child_object);
        assert!(!hints["port-count"].child_object);
        // the classified fixed-name child walks; scalar properties do not
        let kids = bindings.child_properties_for(
            &["vendor,classy".to_string()],
            &["ports".to_string()],
        );
        assert!(kids.contains_key("#address-cells"));
        assert!(bindings
            .child_properties_for(&["vendor,classy".to_string()], &["reg".to_string()])
            .is_empty());
    }

    #[test]
    pub fn numeric_enum_values_complete_as_strings() {
        let mut bindings = Bindings::default();
        let yaml: Value = serde_yaml::from_str(
            r#"
properties:
  compatible:
    const: vendor,bus
  bus-type:
    enum: [4, 5, 6]
"#,
        )
        .unwrap();
        bindings.add_schema(PathBuf::from("bus.yaml"), &yaml);
        let hints = bindings.properties_for(&["vendor,bus".to_string()]);
        assert_eq!(
            hints["bus-type"].values,
            vec!["4".to_string(), "5".to_string(), "6".to_string()]
        );
    }

    #[test]
    pub fn grandchild_paths_walk_full_chains() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("graph.yaml"),
            r#"
$defs:
  endpoint-base:
    properties:
      reg: true
      remote-endpoint: true
  port-base:
    properties:
      '#address-cells':
        const: 1
      reg: true
      endpoint:
        type: object
        properties:
          dual-lane: true
    patternProperties:
      '^endpoint(@[0-9a-f]+)?$':
        $ref: '#/$defs/endpoint-base'
properties:
  ports:
    type: object
    properties:
      '#address-cells':
        const: 1
      '#size-cells':
        const: 0
    patternProperties:
      '^port@[0-9a-f]+$':
        $ref: '#/$defs/port-base'
"#,
        )
        .unwrap();
        fs::write(
            dir.path().join("sfc.yaml"),
            r#"
properties:
  compatible:
    const: vendor,sfc
  ports:
    $ref: graph.yaml#/properties/ports
    properties:
      port@0:
        $ref: graph.yaml#/$defs/port-base
"#,
        )
        .unwrap();
        let bindings = Bindings::from_paths(&[dir.path().to_path_buf()]);
        let compat = &["vendor,sfc".to_string()];

        // level 1: the ports child's own properties, not its children's
        let hints = bindings.child_properties_for(compat, &["ports".to_string()]);
        assert!(hints.contains_key("#address-cells"));
        assert!(hints.contains_key("#size-cells"));
        assert!(!hints.contains_key("reg"));

        // level 2: sfc's fixed-name port@0 and graph's ^port@…$ pattern
        // both reach port-base; the inline-marker endpoint flags node-like
        for port in ["port@0", "port@1"] {
            let hints = bindings.child_properties_for(
                compat,
                &["ports".to_string(), port.to_string()],
            );
            assert!(hints.contains_key("reg"));
            assert!(hints["endpoint"].child_object);
        }

        // level 3: endpoint matches BOTH the classified fixed-name
        // (^endpoint$) and the graph pattern (^endpoint(@…)?$) — both
        // final matches' properties merge
        let hints = bindings.child_properties_for(
            compat,
            &[
                "ports".to_string(),
                "port@0".to_string(),
                "endpoint".to_string(),
            ],
        );
        assert!(hints.contains_key("remote-endpoint"));
        assert!(hints.contains_key("dual-lane"));

        // dead ends: names matching nothing, walking below a leaf level
        assert!(bindings
            .child_properties_for(
                compat,
                &["ports".to_string(), "clock@0".to_string()],
            )
            .is_empty());
        assert!(bindings
            .child_properties_for(
                compat,
                &[
                    "ports".to_string(),
                    "port@0".to_string(),
                    "endpoint".to_string(),
                    "deeper".to_string(),
                ],
            )
            .is_empty());
    }

    #[test]
    pub fn recursive_ref_cycles_terminate_via_guard() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("cyc-a.yaml"),
            r#"
properties:
  compatible:
    const: vendor,cyc-a
  a-marker: true
patternProperties:
  '^a-':
    $ref: cyc-b.yaml#
"#,
        )
        .unwrap();
        fs::write(
            dir.path().join("cyc-b.yaml"),
            r#"
properties:
  compatible:
    const: vendor,cyc-b
  b-marker: true
patternProperties:
  '^b-':
    $ref: cyc-a.yaml#
"#,
        )
        .unwrap();

        // indexing the mutually-recursive pair terminates: `flat_views`
        // dedupes chased (file, fragment) targets per call and structural
        // recursion is depth-capped, so rings echo a bounded number of
        // levels and die at the MAX_DEPTH boundary
        let bindings = Bindings::from_paths(&[dir.path().to_path_buf()]);
        let compat = &["vendor,cyc-a".to_string()];
        let hints = bindings.child_properties_for(compat, &["a-x".to_string()]);
        assert!(hints.contains_key("b-marker"));
        let ring2 = bindings.child_properties_for(
            compat,
            &["a-x".to_string(), "b-y".to_string()],
        );
        assert!(ring2.contains_key("a-marker"));
        assert!(bindings
            .child_properties_for(
                compat,
                &["a-x".to_string(), "b-y".to_string(), "a-z".to_string()],
            )
            .contains_key("b-marker"));
        assert!(bindings
            .child_properties_for(
                compat,
                &[
                    "a-x".to_string(),
                    "b-y".to_string(),
                    "a-z".to_string(),
                    "b-w".to_string(),
                ],
            )
            .contains_key("a-marker"));
        assert!(bindings
            .child_properties_for(
                compat,
                &[
                    "a-x".to_string(),
                    "b-y".to_string(),
                    "a-z".to_string(),
                    "b-w".to_string(),
                    "a-v".to_string(),
                ],
            )
            .contains_key("b-marker"));
        assert!(bindings
            .child_properties_for(
                compat,
                &[
                    "a-x".to_string(),
                    "b-y".to_string(),
                    "a-z".to_string(),
                    "b-w".to_string(),
                    "a-v".to_string(),
                    "b-u".to_string(),
                ],
            )
            .is_empty());
    }

    #[test]
    pub fn all_of_and_refs_flatten_to_terminal_views() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("panel-common.yaml"),
            r#"
$defs:
  timing:
    properties:
      clock-frequency: true
      hactive: true
  timing-wrapper:
    $ref: '#/$defs/timing'
properties:
  label: true
  rotation: true
  panel-timing:
    $ref: '#/$defs/timing-wrapper'
"#,
        )
        .unwrap();
        fs::write(
            dir.path().join("panel.yaml"),
            r#"
allOf:
  - $ref: panel-common.yaml#
properties:
  compatible:
    const: vendor,panel
  panel-timing: true
"#,
        )
        .unwrap();

        let bindings = Bindings::from_paths(&[dir.path().to_path_buf()]);
        let compat = &["vendor,panel".to_string()];

        // allOf merge at the binding top: the common part's properties
        // light up alongside the own ones
        let top = bindings.properties_for(compat);
        assert!(top.contains_key("label"));
        assert!(top.contains_key("rotation"));
        assert!(top["panel-timing"].boolean);

        // the common part's panel-timing classifies as a child and its
        // bare-wrapper chain ($ref -> $ref -> $defs/timing) chases to the
        // timing properties
        let timing = bindings.child_properties_for(compat, &["panel-timing".to_string()]);
        assert!(timing.contains_key("clock-frequency"));
        assert!(timing.contains_key("hactive"));
    }

    #[test]
    pub fn nodename_meta_keys_stay_unsuggested() {
        let mut bindings = Bindings::default();
        let yaml: Value = serde_yaml::from_str(
            r#"
properties:
  compatible:
    const: vendor,named
  $nodename:
    pattern: "^thing@.*$"
  reg: true
"#,
        )
        .unwrap();
        bindings.add_schema(PathBuf::from("named.yaml"), &yaml);
        let hints = bindings.properties_for(&["vendor,named".to_string()]);
        assert!(hints.contains_key("reg"));
        assert!(!hints.contains_key("$nodename"));
    }

    #[test]
    pub fn required_only_stubs_synthesize_hints() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("stubbed.yaml"),
            r#"
properties:
  compatible:
    const: vendor,stub
  ports:
    type: object
    patternProperties:
      '^port@[0-9a-f]+$':
        type: object
        required:
          - reg
          - stub-clock
"#,
        )
        .unwrap();

        let bindings = Bindings::from_paths(&[dir.path().to_path_buf()]);
        let hints = bindings.child_properties_for(
            &["vendor,stub".to_string()],
            &["ports".to_string(), "port@0".to_string()],
        );
        // bodies of the pattern entry spell nothing out; the required
        // list names members the final node must carry
        assert!(hints["reg"].required);
        assert!(hints["stub-clock"].required);
    }

    #[test]
    pub fn root_relative_refs_resolve_across_binding_roots() {
        let dir = tempfile::tempdir().unwrap();
        let schemas = dir.path().join("schemas");
        fs::create_dir_all(&schemas).unwrap();
        fs::write(
            schemas.join("graph-root.yaml"),
            r#"
$defs:
  endpoint-base:
    properties:
      reg: true
      remote-endpoint: true
  port-base:
    properties:
      reg: true
    patternProperties:
      '^endpoint@[0-9a-f]+$':
        $ref: '#/$defs/endpoint-base'
properties:
  ports:
    type: object
    patternProperties:
      '^port@[0-9a-f]+$':
        $ref: '#/$defs/port-base'
"#,
        )
        .unwrap();
        fs::write(
            dir.path().join("sfc-root.yaml"),
            r#"
properties:
  compatible:
    const: vendor,sfcroot
  ports:
    $ref: /schemas/graph-root.yaml#/properties/ports
"#,
        )
        .unwrap();

        // in-tree layout: `$ref: /schemas/…` resolves under the binding
        // root carrying a schemas/ subdir
        let bindings = Bindings::from_paths(&[dir.path().to_path_buf()]);
        let compat = &["vendor,sfcroot".to_string()];
        let port = bindings.child_properties_for(
            compat,
            &["ports".to_string(), "port@1".to_string()],
        );
        assert!(port.contains_key("reg"));
        let endpoint = bindings.child_properties_for(
            compat,
            &[
                "ports".to_string(),
                "port@1".to_string(),
                "endpoint@0".to_string(),
            ],
        );
        assert!(endpoint.contains_key("remote-endpoint"));

        // dtschema pip layout: the registered root IS the schemas dir —
        // the schemas/ prefix strips before joining
        fs::write(
            schemas.join("sfc-pip.yaml"),
            r#"
properties:
  compatible:
    const: vendor,sfcpip
  ports:
    $ref: /schemas/graph-root.yaml#/properties/ports
"#,
        )
        .unwrap();
        let pip = Bindings::from_paths(&[schemas]);
        let port = pip.child_properties_for(
            &["vendor,sfcpip".to_string()],
            &["ports".to_string(), "port@0".to_string()],
        );
        assert!(port.contains_key("reg"));
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
    #[ignore = "machine-local audit: times indexing the real kernel + pip roots"]
    pub fn bindings_index_perf_canary() {
        let start = std::time::Instant::now();
        let bindings = Bindings::from_paths(&[
            PathBuf::from(
                "/home/matous/Development/HatiResearch/Unipi/kernelspace/unipi-kernel/linux/Documentation/devicetree/bindings",
            ),
            PathBuf::from(
                "/home/matous/.local/share/pipx/venvs/dtschema/lib/python3.14/site-packages/dtschema/schemas",
            ),
        ]);
        eprintln!(
            "perf canary: indexed {} schemas, {} child schemas in {:?}",
            bindings.schema_count(),
            bindings.child_schema_count(),
            start.elapsed()
        );
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
