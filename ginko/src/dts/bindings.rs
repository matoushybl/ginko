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

/// One way a node's compatible list can satisfy a binding's
/// `compatible` constraint — the DISJUNCTIVE-branch model: each
/// `oneOf`/`anyOf` branch (or the single conjunctive shape, when
/// there is none) yields one alternative. A node satisfies the
/// binding when it satisfies one alternative: EVERY `const:` entry
/// present, plus at least one `linked` enum value when the
/// alternative has any (the `[enum-vendors, "syscon"]` positional
/// shape — or a pure enum/contains constraint, whose whole anchoring
/// IS the linked values).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CompatibleShape {
    /// Every `const:` entry — a matching node must carry ALL of them.
    pub consts: Vec<String>,
    /// `enum:` values — a matching node must carry one when the list
    /// is non-empty (positional items enums and free enums alike).
    pub linked: Vec<String>,
    /// Set when the consts came EXCLUSIVELY from a positional pure-const
    /// `items:` tuple: acceptance is then dt-schema TUPLE semantics, not
    /// member-pool semantics — the node's list must equal the tuple's
    /// prefix with a length of at least `min_prefix` (the psci.yaml
    /// `items: [const arm,psci-1.0, const arm,psci-0.2, consts p…]`
    /// with `minItems: 1`: a bare `[arm,psci-1.0]` IS a match).
    pub min_prefix: Option<usize>,
}

/// Whether a node's compatible list satisfies ONE alternative/shape:
/// every const present, plus one linked enum where the shape pins
/// any. Shared by `schema_accepts` and the if-then condition match —
/// the SAME machine judges binding selection and conditionals.
pub fn shape_satisfied(shape: &CompatibleShape, compatibles: &[String]) -> bool {
    if shape.consts.is_empty() && shape.linked.is_empty() {
        return false;
    }
    if let Some(min) = shape.min_prefix {
        // positional pure-const items-tuple (dt-schema `items:` array
        // semantics): the node's compatible list must be a prefix of the
        // tuple — positionally equal slot for slot — with a length of at
        // least the sibling `minItems:` (0 when unlisted). A longer list
        // than the tuple is tolerated (unconstrained beyond the array)
        return compatibles.len() >= min
            && compatibles.iter().zip(shape.consts.iter()).all(|(c, t)| c == t);
    }
    shape.consts.iter().all(|crit| compatibles.contains(crit))
        && (shape.linked.is_empty()
            || shape.linked.iter().any(|link| compatibles.contains(link)))
}

/// A compatible-controlled conditional requirement — the `allOf:
/// [{if:, then:}]` idiom: when a node's compatible list satisfies ANY
/// of the `if`-compatible `shapes`, every `required` name binds in
/// ADDITION to the binding's unconditional list (the
/// fsl,imx8mp-xcvr `resets` idiom; some 240 kernel bindings carry
/// one).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Conditional {
    /// The `if.properties.compatible` sub-schema parsed through the
    /// alternatives machine — a node matching any shape triggers.
    pub shapes: Vec<CompatibleShape>,
    /// The `then` sub-schema's comprises distilled: value candidates
    /// (the rpmcc per-SoC `clock-names` / blk-ctrl
    /// `power-domain-names` refinement) AND `then.required` flags —
    /// one hint map for both purposes. Overlaid on a matching node's
    /// pools with union semantics (merge_hint), so the refinement
    /// is additive and silence-gate safe. Property-only conditionals
    /// (no then.required) count — they refine values, not just bind.
    pub properties: BTreeMap<String, PropertyHint>,
}

/// One `oneOf`/`anyOf` requirement disjunction — every member is a
/// pure `required:` list (the `​interrupts` vs
/// `​interrupts-extended` idiom, `clocks` vs `power-domains`, `volid` vs
/// `volname`): a governed node must satisfy AT LEAST ONE branch
/// (anyOf semantics — carrying several stays silent, the
/// conservative reading). Branches with property constraints mixed
/// in opt the whole disjunction out: a pure-required reading there
/// would misjudge refinements as requirements.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RequirementAlternation {
    /// One branch per pure-required member — a branch is satisfied
    /// when the node carries every name (property OR child node).
    pub branches: Vec<Vec<String>>,
}

/// A binding file distilled to completion data.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompletionSchema {
    /// The binding file this schema was read from.
    pub file: PathBuf,
    /// Property name -> hint.
    pub properties: BTreeMap<String, PropertyHint>,
    /// Child-node schemas keyed by `patternProperties` patterns.
    pub children: Vec<ChildSchema>,
    /// The compatible constraint as a DISJUNCTION of
    /// `CompatibleShape` alternatives — one per `oneOf`/`anyOf`
    /// branch; the parse tree of an unbranched constraint collapses
    /// to a single alternative.
    pub alternatives: Vec<CompatibleShape>,
    /// Compatible-controlled requirement supplements (`if/then`
    /// members carrying a then-required list) — applied by the
    /// validator when the node's compatibles match the condition.
    pub conditionals: Vec<Conditional>,
    /// `oneOf`/`anyOf` pure-required disjunctions — the validator
    /// flags a governed node satisfying NONE of the branches.
    pub requirement_alternations: Vec<RequirementAlternation>,
    /// `dependencies:` pairs (JSON-schema's dependentRequired): the
    /// PRESENCE of the first property demands each dependent name
    /// (gpio-charger: charge-current-limit-gpios ⇒
    /// charge-current-limit-mapping). Dict-shaped values are
    /// dependentSchemas — out of scope, skipped.
    pub dependencies: Vec<(String, Vec<String>)>,
    /// The binding's `properties.$nodename` name constraint (pattern-
    /// or const-form) — a governed node's NAME must match, judged by
    /// the LENIENT literal-run matcher (fewer violations, never a
    /// false positive — the same matcher child-name completion uses).
    /// An overlay-merged node carries each spelling's pattern.
    pub nodename: Option<String>,
    /// All `enum:` values — index suggestion keys only; never a
    /// constraint (completion keeps the v1 over-collection).
    pub enums: Vec<String>,
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
    /// The child's OWN `$nodename` contract when its `$ref`'d schema
    /// carries one (enetc's literal `mdio` child -> the mdio binding's
    /// `^mdio(-(bus|external))?(@.+|-([0-9]+))?$`) — the same pattern
    /// validation judges EXACTLY; the completion-side sample set joins it
    /// so a suggestion can never contradict the validator.
    pub nodename: Option<String>,
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
    /// Deprecated VALUE spellings of this property — `deprecated: true`
    /// riding a NESTED constraint (a oneOf/anyOf branch, an items slot):
    /// the qcom,kpss-wdt class (`- const: qcom,kpss-wdt / deprecated:
    /// true`), the allwinner sram enum branch. The property's OWN
    /// `deprecated:` flag rides `deprecated` above instead — never both:
    /// a wholly-deprecated property marks the NAME, not each spelling.
    pub deprecated_values: Vec<String>,
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
    /// property sets like leds/common.yaml, rk3399-pcie-common style
    /// fragments). SUGGESTION-ONLY for completion — never normative for
    /// validation.
    common: BTreeMap<String, PropertyHint>,
    /// Property hints from `select: true` schemas — dtschema's dt-core,
    /// applicable to EVERY node. Normative: the validation counterpart of
    /// the suggestion pools.
    universal: BTreeMap<String, PropertyHint>,
    /// Neighbor schema files pulled in by patternProperties `$ref`s,
    /// parsed once per absolute path.
    ref_cache: HashMap<PathBuf, Value>,
    /// The binding roots given to `from_paths` — the resolution base for
    /// dt-schema flavour absolute refs (`/schemas/graph.yaml#/…`).
    roots: Vec<PathBuf>,
    /// EVERY compatible string the corpus documents ANYWHERE — top-level
    /// binding alternatives AND child-schema positions alike (the
    /// fsl,sec-v4.0 `jr` job rings, the snvs powerkey/rtc-lp children,
    /// `arm,idle-state` state nodes: child-held strings the corpus
    /// justifies even though no TOP-LEVEL binding accepts them). The
    /// bondless HINT consults this alone: a string documented in child
    /// position is corpus-governed, so a node citing it is not typeless.
    documented: BTreeSet<String>,
    /// compatible string -> number of binding-EXAMPLE assignments seen
    /// while indexing — the corpus maintainers' own usage signal, the
    /// basis of the staged common-compatibles tier.
    example_counts: HashMap<String, u32>,
    /// The canonical universal tier: documented strings used in at
    /// least `COMMON_COMPATIBLE_MIN_USES` example assignments, ordered
    /// by (count desc, lexical) — served for compatible VALUES when a
    /// node has no binding-derived candidates and little is typed yet.
    common_compatibles: Vec<String>,
}

/// A compatible string joins the canonical tier after this many
/// binding-example assignments — census on the Unipi kernel corpus:
/// at 8 uses the tier is ~34 strings (syscon, simple-mfd, simple-bus,
/// fixed-clock, arm,primecell, usb-c-connector, ...), and the
/// frequency curve flattens fast (6+ selects 47, 5+ selects 71,
/// 4+ selects 120).
const COMMON_COMPATIBLE_MIN_USES: u32 = 8;

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
        bindings.finalize_common_compatibles();
        bindings
    }

    /// Distill the canonical tier from the example census — called once,
    /// after indexing completes: a string earns a slot only when the
    /// corpus itself documents it AND its examples used it at least
    /// `COMMON_COMPATIBLE_MIN_USES` times. Deterministic order (count
    /// descending, then lexical) — batteries pin it.
    fn finalize_common_compatibles(&mut self) {
        let mut counted: Vec<(&String, u32)> = self
            .example_counts
            .iter()
            .filter(|(string, uses)| {
                **uses >= COMMON_COMPATIBLE_MIN_USES && self.documented.contains(*string)
            })
            .map(|(string, uses)| (string, *uses))
            .collect();
        counted.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(b.0)));
        self.common_compatibles = counted
            .into_iter()
            .map(|(string, _)| string.clone())
            .collect();
    }

    /// The canonical universal tier for compatible-value completion on
    /// candidate-less nodes: example-frequent documented strings in
    /// (count desc, lexical) order.
    pub fn common_compatibles(&self) -> &[String] {
        &self.common_compatibles
    }

    pub fn schema_count(&self) -> usize {
        self.schemas.len()
    }

    /// Every parsed compatible-controlled conditional (`if/then`
    /// members with a then-required list) — audit metric for the
    /// conditional-round parse (~240 kernel bindings carry one).
    pub fn conditional_count(&self) -> usize {
        self.schemas.iter().map(|s| s.conditionals.len()).sum()
    }

    /// Every parsed pure-required `oneOf`/`anyOf` disjunction — the
    /// audit metric for the either/or round (~60 kernel bindings).
    pub fn disjunction_count(&self) -> usize {
        self.schemas
            .iter()
            .map(|s| s.requirement_alternations.len())
            .sum()
    }

    /// Every parsed `dependencies:` pair (dependentRequired) — audit
    /// metric (63 kernel bindings carry one).
    pub fn dependency_count(&self) -> usize {
        self.schemas.iter().map(|s| s.dependencies.len()).sum()
    }

    /// Whether the corpus documents `compatible` ANYWHERE — top-level
    /// binding alternatives AND child-schema positions alike (the
    /// bondless HINT's sole extra oracle: a child-documented string
    /// is corpus-governed — job rings, snvs children, idle states
    /// — so a node citing it is NOT typeless even though no top-level
    /// view accepts it).
    pub fn documents(&self, compatible: &str) -> bool {
        self.documented.contains(compatible)
    }

    /// Every compatible string the corpus documents — audit metric.
    pub fn documented_count(&self) -> usize {
        self.documented.len()
    }

    /// Whether any binding data was indexed — completion hints and
    /// binding-driven validators no-op when false.
    pub fn is_empty(&self) -> bool {
        self.schemas.is_empty() && self.common.is_empty() && self.universal.is_empty()
    }

    pub fn compatible_count(&self) -> usize {
        self.by_compatible.len()
    }

    /// Property hints merged ONLY from schemas whose compatible
    /// constraint the node's full compatible list ACCEPTS — the
    /// VALIDATION counterpart of `properties_for`. Completion over-collects
    /// by any-shared-string (a `syscon` sibling binding suggests its
    /// properties); requirements must come from bindings that genuinely
    /// accept the node.
    /// Props from ACCEPTED schemas and their SHAPE-MATCHED conditionals
    /// merged into one pool — a node carrying the if-compatible sees the
    /// then-values as judgement candidates (rpmcc's conditional-ONLY
    /// clock-names pools fill; overlays are UNION, never narrowing).
    pub fn normative_properties_for(
        &self,
        compatibles: &[String],
    ) -> BTreeMap<String, PropertyHint> {
        let mut merged: BTreeMap<String, PropertyHint> = BTreeMap::new();
        for compatible in compatibles {
            for idx in self.by_compatible.get(compatible).into_iter().flatten() {
                if !self.schema_accepts(*idx, compatibles) {
                    continue;
                }
                for (name, hint) in &self.schemas[*idx].properties {
                    merge_hint(merged.entry(name.clone()).or_default(), hint);
                }
                for conditional in &self.schemas[*idx].conditionals {
                    if !conditional
                        .shapes
                        .iter()
                        .any(|shape| shape_satisfied(shape, compatibles))
                    {
                        continue;
                    }
                    for (name, hint) in &conditional.properties {
                        merge_hint(merged.entry(name.clone()).or_default(), hint);
                    }
                }
            }
        }
        merged
    }

    /// Whether a node's compatible list satisfies schema `idx`'s
    /// compatible constraint — the DISJUNCTIVE model: the node must
    /// satisfy ONE alternative (`shape_satisfied`) — all consts plus,
    /// where pinned, one linked enum. Const-ANCHORED alternatives
    /// govern exactly; enum/contains-anchored alternatives (the
    /// fsl,imx8-pcie-phy family — `compatible: enum:`) impose too:
    /// their whole anchoring is the linked values. Pattern-only
    /// shapes (regex) still impose nothing — matching them exceeds
    /// this round's approximation; `properties_for` still
    /// over-collects every schema's hints for completion.
    pub fn schema_accepts(&self, idx: usize, compatibles: &[String]) -> bool {
        self.schemas[idx]
            .alternatives
            .iter()
            .any(|alt| shape_satisfied(alt, compatibles))
    }

    /// The NORMATIVE schemas matching a node's compatible list — one
    /// entry per ACCEPTED binding, carrying the binding's identifier
    /// (its file stem) for diagnostic attribution and the schema
    /// itself (unconditional properties + conditionals) ..
    pub fn normative_views(
        &self,
        compatibles: &[String],
    ) -> Vec<(String, &CompletionSchema)> {
        let mut out = Vec::new();
        let mut seen: Vec<usize> = Vec::new();
        for compatible in compatibles {
            for &idx in self.by_compatible.get(compatible).into_iter().flatten() {
                if seen.contains(&idx) || !self.schema_accepts(idx, compatibles) {
                    continue;
                }
                seen.push(idx);
                let name = self.schemas[idx]
                    .file
                    .file_stem()
                    .and_then(|stem| stem.to_str())
                    .unwrap_or("unknown binding")
                    .to_string();
                out.push((name, &self.schemas[idx]));
            }
        }
        out
    }

    /// Merge the property hints of all schemas matching any of the given
    /// compatible strings. A property required by any schema stays required;
    /// the first non-empty hint wins for the optional fields. CONDITIONAL
    /// overlays apply per shape match: a node carrying the if-compatible
    /// gains the then-properties' values and requirements (completion
    /// narrows per-SoC without any acceptance gate).
    pub fn properties_for(&self, compatibles: &[String]) -> BTreeMap<String, PropertyHint> {
        let mut merged: BTreeMap<String, PropertyHint> = BTreeMap::new();
        for compatible in compatibles {
            for idx in self.by_compatible.get(compatible).into_iter().flatten() {
                for (name, hint) in &self.schemas[*idx].properties {
                    merge_hint(merged.entry(name.clone()).or_default(), hint);
                }
                for conditional in &self.schemas[*idx].conditionals {
                    if !conditional
                        .shapes
                        .iter()
                        .any(|shape| shape_satisfied(shape, compatibles))
                    {
                        continue;
                    }
                    for (name, hint) in &conditional.properties {
                        merge_hint(merged.entry(name.clone()).or_default(), hint);
                    }
                }
            }
        }
        merged
    }

    /// Typeable CHILD-NODE sample names for a node matched by
    /// `compatibles` — patternProperties children of every cited schema
    /// distilled through `pattern_samples` (the completion twin of
    /// `$nodename` validation: that round judges existing names exactly,
    /// this round proposes names to type). Literal-key children already
    /// complete via `properties_for`'s child_object hints; the server
    /// de-dupes by label.
    pub fn child_node_samples(&self, compatibles: &[String]) -> BTreeSet<String> {
        let mut out = BTreeSet::new();
        for compatible in compatibles {
            for idx in self.by_compatible.get(compatible).into_iter().flatten() {
                for child in &self.schemas[*idx].children {
                    for pattern in &child.patterns {
                        out.extend(pattern_samples(pattern));
                    }
                    // the child's own $nodename is the rest of the naming
                    // contract (the $ref'd binding's own view — enetc's
                    // mdio child, the adc/dac channel children). Its
                    // samples join ONLY when the child's KEY patterns
                    // leniently accept them — the landed over-match law
                    // is the completion-side arbiter: a suggestion and
                    // the matcher that lights up the typed node must
                    // agree, so fixed-name children stay fixed
                    // (`^mdio$` drops `mdio-bus`) while the optional-*
                    // address spellings the keys already over-match
                    // surface (`^channel@[0-9a-f]+$` gains bare
                    // `channel` — its $ref'd binding says `^channel(@…)?$`)
                    if let Some(nodename) = &child.nodename {
                        for sample in pattern_samples(nodename) {
                            if child
                                .patterns
                                .iter()
                                .any(|pattern| alternative_matches(pattern, &sample))
                            {
                                out.insert(sample);
                            }
                        }
                    }
                }
            }
        }
        out
    }

    /// The COMPLETION twin of the either/or (`requirement_alternations`)
    /// and dependentRequired (`dependencies`) validator checks: per-name
    /// contract phrases to surface in completion. Over-collects by
    /// compatibility exactly like `properties_for` — completion surfaces,
    /// it never judges. Returns (alternation members -> "at least one of:
    /// …", dependent names -> "required when `<trigger>` is present");
    /// the phrase is the FULL branch list so every member advertises the
    /// whole disjunction.
    pub fn requirement_stub_phrases(
        &self,
        compatibles: &[String],
    ) -> (BTreeMap<String, String>, BTreeMap<String, String>) {
        let mut alternations = BTreeMap::new();
        let mut dependencies = BTreeMap::new();
        for compatible in compatibles {
            for idx in self.by_compatible.get(compatible).into_iter().flatten() {
                for disjunction in &self.schemas[*idx].requirement_alternations {
                    let mut names: Vec<&str> = Vec::new();
                    for branch in &disjunction.branches {
                        for name in branch {
                            if !names.contains(&name.as_str()) {
                                names.push(name);
                            }
                        }
                    }
                    if names.is_empty() {
                        continue;
                    }
                    let joined = names
                        .iter()
                        .map(|name| format!("`{name}`"))
                        .collect::<Vec<_>>()
                        .join(", ");
                    let phrase = format!("at least one of: {joined}");
                    for name in names {
                        alternations.insert(name.to_string(), phrase.clone());
                    }
                }
                for (trigger, dependents) in &self.schemas[*idx].dependencies {
                    for dependent in dependents {
                        dependencies.insert(
                            dependent.clone(),
                            format!("required when `{trigger}` is present"),
                        );
                    }
                }
            }
        }
        (alternations, dependencies)
    }

    fn add_file(&mut self, path: &Path) {
        let Ok(text) = fs::read_to_string(path) else {
            return;
        };
        let Ok(binding) = serde_yaml::from_str::<Value>(&text) else {
            return;
        };
        {
            let mut example_counts = std::mem::take(&mut self.example_counts);
            count_example_compatibles(&binding, &mut example_counts);
            self.example_counts = example_counts;
        }
        self.add_schema(path.to_path_buf(), &binding);
    }

    fn add_schema(&mut self, file: PathBuf, binding: &Value) {
        let Some(compatible) = binding.get("properties").and_then(|props| props.get("compatible"))
        else {
            // Schemas without a `compatible` are common-part schemas; they
            // contribute value candidates instead of matching nodes. They
            // can still carry CHILD positions that document compatible
            // strings (idle-states.yaml's `arm,idle-state` state nodes) —
            // harvest those for the bondless oracle before returning.
            let mut documented = std::mem::take(&mut self.documented);
            extract_children(
                binding,
                &file,
                &self.roots,
                &mut self.ref_cache,
                0,
                &mut documented,
            );
            self.documented = documented;
            self.add_common_schema(binding, &file);
            return;
        };
        let mut keys = Vec::new();
        let alternatives = collect_compatible_shapes(compatible, &mut keys);
        let mut compatibles = keys.clone();
        for alt in &alternatives {
            compatibles.extend(alt.consts.iter().cloned());
            compatibles.extend(alt.linked.iter().cloned());
        }

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
        let mut conditionals = Vec::new();
        let mut alternations = Vec::new();
        let mut dependencies = Vec::new();
        let mut nodename = None;
        let properties = {
            let mut merged: BTreeMap<String, PropertyHint> = BTreeMap::new();
            for (view, _) in
                flat_views(binding, &file, &mut self.ref_cache, &self.roots)
            {
                if nodename.is_none() {
                    // the binding's own view comes first (flat_views pops
                    // the top before its allOf/refs) — $ref-inherited
                    // $nodenames only fill the gap
                    nodename = extract_nodename(&view);
                }
                for (name, hint) in extract_properties(&view) {
                    merge_hint(merged.entry(name.clone()).or_default(), &hint);
                }
                collect_conditionals(&view, &mut conditionals);
                collect_requirement_alternations(&view, &mut alternations);
                collect_dependencies(&view, &mut dependencies);
            }
            merged
        };
        let children = extract_children(
            binding,
            &file,
            &self.roots,
            &mut self.ref_cache,
            0,
            &mut self.documented,
        );
        self.schemas.push(CompletionSchema {
            file,
            properties,
            children,
            alternatives,
            conditionals,
            requirement_alternations: alternations,
            dependencies,
            nodename,
            enums: keys,
        });
        let idx = self.schemas.len() - 1;
        for compatible in &compatibles {
            self.by_compatible
                .entry(compatible.clone())
                .or_default()
                .push(idx);
        }
        self.documented.extend(compatibles);
    }

    /// Hints from common-part schemas merge by property name; `select:
    /// true` schemas (dtschema's dt-core) are UNIVERSAL — normative for
    /// every node — while compatible-less fragments stay suggestion-only.
    /// See `common` and `universal`.
    fn add_common_schema(&mut self, binding: &Value, file: &Path) {
        let universal = matches!(binding.get("select"), Some(Value::Bool(true)));
        for (view, _) in flat_views(binding, file, &mut self.ref_cache, &self.roots) {
            for (name, hint) in extract_properties(&view) {
                let target = if universal {
                    &mut self.universal
                } else {
                    &mut self.common
                };
                merge_hint(target.entry(name.clone()).or_default(), &hint);
            }
        }
    }

    /// Value candidates for a property from common-part schemas — matched by
    /// property name, e.g. `status` from dt-schema's dt-core or `color`
    /// from leds/common.yaml. Called when the node's own bindings have no
    /// candidates for the property.
    pub fn common_values(&self, name: &str) -> &[String] {
        self.common
            .get(name)
            .map(|hint| hint.values.as_slice())
            .filter(|values| !values.is_empty())
            .or_else(|| {
                self.universal
                    .get(name)
                    .map(|hint| hint.values.as_slice())
            })
            .unwrap_or(&[])
    }

    /// Value candidates from `select: true` schemas alone — authoritative
    /// for every node (dtschema applies dt-core regardless of binding); the
    /// VALIDATION counterpart of `common_values`.
    pub fn universal_values(&self, name: &str) -> &[String] {
        self.universal
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

/// The compatible constraint of a schema, parsed into a DISJUNCTION of
/// `CompatibleShape` alternatives plus free `enum:` suggestion keys:
/// - a `oneOf`/`anyOf` contributes one alternative per branch — a
///   node matching ANY branch matches the schema, so branch consts
///   must never pool (a branch is one match path, not a global
///   requirement);
/// - an `items:` position sequence pins its `const:` positions and
///   positionally links its `enum:` positions — const-less sequences
///   anchor purely through the linked enums; a PURE-CONST sequence
///   additionally switches the frame to positional TUPLE acceptance
///   (`min_prefix`, dt-schema `items:`-array semantics: prefixes of
///   length >= `minItems:` match, the psci bare `[arm,psci-1.0]`);
/// - free `enum:` and `contains:` constraints anchor the same way:
///   one linked value present (the fsl,imx8-pcie-phy /
///   `compatible: enum:` family; `contains: const: X` reduces to
///   consts=[X]).
///
/// A nested disjunction under `allOf` merges conjunctively — a
/// conservative approximation not seen in real bindings.
fn collect_compatible_shapes(
    value: &Value,
    keys: &mut Vec<String>,
) -> Vec<CompatibleShape> {
    match value {
        Value::Sequence(items) => {
            // a sequence of alternatives — treated as oneOf
            let mut alternatives = Vec::new();
            for item in items {
                alternatives.extend(collect_compatible_shapes(item, keys));
            }
            alternatives
        }
        Value::Mapping(map) => {
            let mut accum = CompatibleShape::default();
            let mut branches = Vec::new();
            // tuple-mode bookkeeping: a pure-const items tuple's extent
            // (start, len) within the accumulating consts, plus the
            // branch's declared `minItems:` (None until seen)
            let mut tuple_span: Option<(usize, usize)> = None;
            let mut tuple_min: Option<usize> = None;
            for (key, val) in map.iter() {
                match key.as_str() {
                    Some("const") => {
                        if let Some(string) = val.as_str() {
                            accum.consts.push(string.to_string());
                        }
                    }
                    Some("enum") => {
                        // a free enum anchors the alternative: one of
                        // the values must be present (enum-anchored
                        // bindings impose since this round)
                        if let Value::Sequence(items) = val {
                            for item in items {
                                if let Some(string) = item.as_str() {
                                    keys.push(string.to_string());
                                    accum.linked.push(string.to_string());
                                }
                            }
                        }
                    }
                    Some("minItems") => {
                        // the positional tuple's length floor — read as a
                        // SIBLING of `items:` in the same branch frame
                        tuple_min = val.as_u64().map(|min| min as usize);
                    }
                    Some("items") => {
                        if let Value::Sequence(items) = val {
                            let mut seq_consts = Vec::new();
                            let mut seq_enums = Vec::new();
                            for item in items {
                                collect_item_position(item, &mut seq_consts, &mut seq_enums);
                            }
                            // only a PURE-CONST sequence is positional —
                            // enum slots make per-position matching
                            // ambiguous, those frames stay member-pool
                            tuple_span =
                                (seq_enums.is_empty() && !seq_consts.is_empty())
                                    .then_some((accum.consts.len(), seq_consts.len()));
                            accum.consts.extend(seq_consts.iter().cloned());
                            if seq_consts.is_empty() {
                                // a const-less position sequence anchors
                                // purely through its enums
                                keys.extend(seq_enums.iter().cloned());
                                accum.linked.extend(seq_enums);
                            } else {
                                accum.linked.extend(seq_enums.iter().cloned());
                                keys.extend(seq_enums);
                            }
                        }
                    }
                    Some("oneOf" | "anyOf") => {
                        if let Value::Sequence(one_of) = val {
                            for one in one_of {
                                branches.extend(collect_compatible_shapes(one, keys));
                            }
                        }
                    }
                    Some("allOf") => {
                        // conjunctive: merge every member into every
                        // pending frame (nested disjunctions merge
                        // wholesale — see the doc comment above)
                        if let Value::Sequence(members) = val {
                            for member in members {
                                for alt in collect_compatible_shapes(member, keys) {
                                    tuple_span = None;
                                    if branches.is_empty() {
                                        accum.consts.extend(alt.consts);
                                        accum.linked.extend(alt.linked);
                                    } else {
                                        for frame in &mut branches {
                                            frame.consts.extend(alt.consts.iter().cloned());
                                            frame.linked.extend(alt.linked.iter().cloned());
                                            frame.min_prefix = None;
                                        }
                                    }
                                }
                            }
                        }
                    }
                    Some("contains") => {
                        // an "at least one member" constraint — the
                        // contained values anchor this frame (the
                        // if-then condition idiom and binding-level
                        // syscon selects both reduce here)
                        for alt in collect_compatible_shapes(val, keys) {
                            if branches.is_empty() {
                                accum.consts.extend(alt.consts);
                                accum.linked.extend(alt.linked);
                                tuple_span = None;
                            } else {
                                for frame in &mut branches {
                                    frame.consts.extend(alt.consts.iter().cloned());
                                    frame.linked.extend(alt.linked.iter().cloned());
                                    frame.min_prefix = None;
                                }
                            }
                        }
                    }
                    _ => {
                        // $ref descendants and other wrappers: recurse
                        // conjunctively into the shared frame
                        for alt in collect_compatible_shapes(val, keys) {
                            if branches.is_empty() {
                                accum.consts.extend(alt.consts);
                                accum.linked.extend(alt.linked);
                                tuple_span = None;
                            } else {
                                for frame in &mut branches {
                                    frame.consts.extend(alt.consts.iter().cloned());
                                    frame.linked.extend(alt.linked.iter().cloned());
                                    frame.min_prefix = None;
                                }
                            }
                        }
                    }
                }
            }
            if branches.is_empty() {
                if accum.consts.is_empty() && accum.linked.is_empty() {
                    return Vec::new();
                }
                // flip the pure-tuple frame to positional acceptance
                // — only when the tuple is the frame's SOLE const source
                if let Some((start, len)) = tuple_span {
                    if start == 0
                        && accum.consts.len() == len
                        && accum.linked.is_empty()
                    {
                        accum.min_prefix = Some(tuple_min.unwrap_or(0));
                    }
                }
                return vec![accum];
            }
            if !accum.consts.is_empty() || !accum.linked.is_empty() {
                // a const pinned beside a disjunction conjoins with
                // every branch — stricter (safe) direction
                for frame in &mut branches {
                    frame.consts.extend(accum.consts.iter().cloned());
                    frame.linked.extend(accum.linked.iter().cloned());
                    frame.min_prefix = None;
                }
            }
            branches
        }
        _ => Vec::new(),
    }
}

/// One `items:` position of a compatible sequence — either a pinned
/// `const:` or an `enum:` alternative slot.
fn collect_item_position(item: &Value, consts: &mut Vec<String>, enums: &mut Vec<String>) {
    let Value::Mapping(map) = item else {
        return;
    };
    for (key, val) in map.iter() {
        match key.as_str() {
            Some("const") => {
                if let Some(string) = val.as_str() {
                    consts.push(string.to_string());
                }
            }
            Some("enum") => {
                if let Value::Sequence(items) = val {
                    for item in items {
                        if let Some(string) = item.as_str() {
                            enums.push(string.to_string());
                        }
                    }
                }
            }
            _ => {}
        }
    }
}

/// Extract the `dependencies:` pairs of one flat view —
/// JSON-schema's dependentRequired: a node carrying the FIRST name
/// must also carry each of the dependent names (gpio-charger's
/// charge-current-limit-gpios ⇒ charge-current-limit-mapping).
/// Dict-shaped values are dependentSchemas (a nested constraint
/// object — 3 occurrences corpus-wide) and stay out of scope.
fn collect_dependencies(view: &Value, out: &mut Vec<(String, Vec<String>)>) {
    let Some(Value::Mapping(pairs)) = view.get("dependencies") else {
        return;
    };
    for (trigger, needed) in pairs.iter() {
        let Some(trigger) = trigger.as_str() else {
            continue;
        };
        let Value::Sequence(names) = needed else {
            continue; // dependentSchemas shape — skipped
        };
        let deps: Vec<String> = names
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect();
        if !deps.is_empty() {
            out.push((trigger.to_string(), deps));
        }
    }
}

/// The `properties.$nodename` constraint of one flat view — the
/// NAME shape of the nodes a binding governs (a sibling key of
/// `compatible`, which is why the property extractor skips `$`-meta
/// names). The `pattern:` spelling lands as-is; `const:` becomes
/// `^<const>$` (dt-schema's exact-name reading; the living corpus uses
/// it only for root-node schemas, which carry no compatible).
fn extract_nodename(view: &Value) -> Option<String> {
    let nodename = view
        .get("properties")?
        .get("$nodename")?;
    match nodename {
        Value::Mapping(map) => {
            if let Some(Value::String(pattern)) = map.get("pattern") {
                Some(pattern.clone())
            } else if let Some(Value::String(exact)) = map.get("const") {
                Some(format!("^{exact}$"))
            } else {
                None
            }
        }
        Value::String(pattern) => Some(pattern.clone()),
        _ => None,
    }
}

/// Extract a compatible-controlled conditional from one `{if:, then:}`
/// view of a binding (an `allOf` member flattened into a terminal
/// view). The `if.properties.compatible` sub-schema — collected
/// through the SAME alternatives machine that governs binding
/// selection — is the condition; the `then` sub-schema's properties
/// and required list distill to one hint overlay: value candidates
/// (rpmcc per-SoC clock-names) AND supplemental requirements in a
/// single map. Conditionals on other properties stay out of scope,
/// and then-shapes that neither bind requirements nor contribute
/// candidates (minItems/maxItems-only) are dropped.
fn collect_conditionals(view: &Value, out: &mut Vec<Conditional>) {
    let Some(Value::Mapping(if_block)) = view.get("if") else {
        return;
    };
    let Some(then_block) = view.get("then") else {
        return;
    };
    let Some(condition) = if_block
        .get("properties")
        .and_then(|props| props.get("compatible"))
    else {
        return;
    };
    let mut sink = Vec::new();
    let shapes = collect_compatible_shapes(condition, &mut sink);
    let mut overlay = extract_properties(then_block);
    // then.required names WITHOUT a property definition (the fsl,xcvr
    // `resets` spelling) still synthesize bare required hints
    if let Some(Value::Sequence(required)) = then_block.get("required") {
        for name in required.iter().filter_map(Value::as_str) {
            overlay.entry(name.to_string()).or_default().required = true;
        }
    }
    let enriching = overlay
        .values()
        .any(|hint| hint.required || !hint.values.is_empty());
    if shapes.is_empty() || !enriching {
        return;
    }
    out.push(Conditional {
        shapes,
        properties: overlay,
    });
}

/// Extract a `oneOf`/`anyOf` REQUIREMENT disjunction from one flat
/// view of a binding: every member must be a pure `required:` list
/// (a `deprecated: true` tag tolerates) and there must be at least
/// two members — a member carrying property constraints opts the
/// whole disjunction out, since a bare-required reading would
/// misjudge value refinements as requirements. Collected from every
/// flat view, so `allOf`-embedded disjunctions and $ref-inherited
/// ones land identically.
fn collect_requirement_alternations(
    view: &Value,
    out: &mut Vec<RequirementAlternation>,
) {
    for key in ["oneOf", "anyOf"] {
        let Some(Value::Sequence(members)) = view.get(key) else {
            continue;
        };
        if members.len() < 2 {
            continue;
        }
        let Some(branches) = pure_required_branches(members) else {
            continue;
        };
        out.push(RequirementAlternation { branches });
    }
}

/// The pure-required member set of one disjunction, or None when any
/// member carries property constraints, is empty or scalar — in those
/// shapes a bare-required reading would misjudge value refinements as
/// requirements, so the whole disjunction stays out.
fn pure_required_branches(members: &[Value]) -> Option<Vec<Vec<String>>> {
    let mut branches = Vec::new();
    for member in members {
        let Value::Mapping(map) = member else {
            return None;
        };
        if map.is_empty() {
            return None; // a `{}` member satisfies trivially
        }
        let Some(Value::Sequence(required)) = map.get("required") else {
            return None;
        };
        for k in map.keys() {
            if !matches!(k.as_str(), Some("required" | "deprecated")) {
                return None; // property-refining member — not pure
            }
        }
        let names: Vec<String> = required
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect();
        if names.is_empty() {
            return None;
        }
        branches.push(names);
    }
    Some(branches)
}

fn property_hint(sub: &Value, required: bool) -> PropertyHint {
    if matches!(sub, Value::Bool(_)) {
        // `name: true` means allowed-unconstrained in dtschema, NOT a
        // boolean property (fixed-regulator's `regulator-name: true`) —
        // booleans are spelled `{type: boolean}`
        return PropertyHint {
            required,
            ..PropertyHint::default()
        };
    }
    let Value::Mapping(_) = sub else {
        return PropertyHint {
            required,
            ..PropertyHint::default()
        };
    };
    // hints come from types.yaml $ref tails; a bare `type:` string fills the
    // gap LEFT open (the living corpus types every string via $ref — the
    // corpus audit for this round found ZERO bare `type: string` schemas —
    // but out-of-tree bindings may spell it directly). Only the two
    // string kinds quote; integer/array/object keep the raw hand and
    // boolean has its own flag above.
    let type_hint = sub
        .get("$ref")
        .and_then(Value::as_str)
        .and_then(type_ref_tail)
        .or_else(|| {
            sub.get("type")
                .and_then(Value::as_str)
                .filter(|kinded| matches!(*kinded, "string" | "string-array"))
                .map(str::to_string)
        });
    let description = sub.get("description").and_then(Value::as_str).map(str::to_string);
    let deprecated = matches!(sub.get("deprecated"), Some(Value::Bool(true)));
    // the kernel-idiomatic boolean spelling: `name: {type: boolean}`
    // (the bare `name: true` is handled above)
    let boolean = sub.get("type").and_then(Value::as_str) == Some("boolean");
    let define_families = define_families_from(description.as_deref());
    let mut values = Vec::new();
    collect_value_strings(sub, &mut values);
    let mut deprecated_values = Vec::new();
    collect_deprecated_values(sub, &mut deprecated_values);
    PropertyHint {
        required,
        boolean,
        type_hint,
        description,
        deprecated,
        values,
        deprecated_values,
        define_families,
        child_object: node_like(sub),
    }
}

/// Deprecated VALUE spellings of a property: a value whose EVERY
/// justification rides a `deprecated: true` constraint — the difference
/// of two sweeps over the sub-schema (the full spelling inventory minus
/// the spellings reachable without crossing a deprecated constraint).
/// Members justified by good constraints stay clean: fsl-imx-esdhc's
/// legacy fallback tuples (`items: [enum: imx8mm-usdhc..., const:
/// imx7d-usdhc] / deprecated: true`) mark the SHAPE, never its
/// spellings — imx7d-usdhc and imx8mm-usdhc also live in the good
/// single-string enums (the gate census proved the naive per-branch
/// reading with six stock leaks); qcom,kpss-wdt likewise rides good
/// modern tuples. The truly deprecated class: `{const:
/// hardkernel,odroid-xu3-audio, deprecated: true}` — a spelling whose
/// only justification IS the deprecated branch. The property's OWN
/// `deprecated:` flag is deliberately ignored at the TOP level (it
/// marks the NAME, feeding `hint.deprecated`; double-marking every
/// spelling would report both messages on a single use). Never descends
/// into `properties`/`patternProperties` — other properties' and
/// children's constraints, the same skip collect_value_strings keeps.
fn collect_deprecated_values(value: &Value, out: &mut Vec<String>) {
    fn local_spellings(node: &Value) -> Vec<String> {
        let Value::Mapping(map) = node else {
            return Vec::new();
        };
        let mut spellings = Vec::new();
        for (key, val) in map.iter() {
            match key.as_str() {
                Some("const") => {
                    if let Some(scalar) = scalar_string(val) {
                        spellings.push(scalar);
                    }
                }
                Some("enum") => {
                    if let Value::Sequence(items) = val {
                        for item in items {
                            if let Some(scalar) = scalar_string(item) {
                                spellings.push(scalar);
                            }
                        }
                    }
                }
                _ => {}
            }
        }
        spellings
    }
    const SKIP_KEYS: [&str; 3] = ["properties", "patternProperties", "examples"];

    // sweep one: the FULL spelling inventory, everywhere
    fn sweep_all(node: &Value, out: &mut Vec<String>) {
        match node {
            Value::Mapping(map) => {
                for (key, val) in map.iter() {
                    match key.as_str() {
                        Some("const") => {
                            if let Some(scalar) = scalar_string(val) {
                                out.push(scalar);
                            }
                        }
                        Some("enum") => {
                            if let Value::Sequence(items) = val {
                                for item in items {
                                    if let Some(scalar) = scalar_string(item) {
                                        out.push(scalar);
                                    }
                                }
                            }
                        }
                        Some(key) if SKIP_KEYS.contains(&key) => {}
                        _ => sweep_all(val, out),
                    }
                }
            }
            Value::Sequence(items) => {
                for item in items {
                    sweep_all(item, out);
                }
            }
            _ => {}
        }
    }

    // sweep two: spellings justified by NON-deprecated constraints —
    // a deprecated dict's own spellings AND everything below it are
    // excluded, but the TOP mapping's own flag is the NAME-level marker
    // (ignored here, honoured by `hint.deprecated`)
    fn sweep_justified(
        node: &Value,
        inherited: bool,
        top_level: bool,
        out: &mut Vec<String>,
    ) {
        match node {
            Value::Mapping(map) => match inherited {
                true => {}
                false => {
                    let self_deprecated = !top_level
                        && matches!(node.get("deprecated"), Some(Value::Bool(true)));
                    if !self_deprecated {
                        out.extend(local_spellings(node));
                    }
                    for (key, val) in map.iter() {
                        match key.as_str() {
                            Some("deprecated") => {}
                            Some(key) if SKIP_KEYS.contains(&key) => {}
                            _ => sweep_justified(val, self_deprecated, false, out),
                        }
                    }
                }
            },
            Value::Sequence(items) => {
                for item in items {
                    sweep_justified(item, inherited, false, out);
                }
            }
            _ => {}
        }
    }

    let mut all = Vec::new();
    sweep_all(value, &mut all);
    let mut justified = Vec::new();
    sweep_justified(value, false, true, &mut justified);
    for spelling in &all {
        if !justified.contains(spelling) && !out.contains(spelling) {
            out.push(spelling.clone());
        }
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
    for value in &src.deprecated_values {
        if !dst.deprecated_values.contains(value) {
            dst.deprecated_values.push(value.clone());
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
    documented: &mut BTreeSet<String>,
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
            documented,
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
    documented: &mut BTreeSet<String>,
) -> ChildSchema {
    let heads = flat_views(subschema, origin_file, ref_cache, roots);
    for (view, _) in &heads {
        // a child's own `compatible` documents its strings — even
        // though they never anchor a top-level view, the corpus
        // justifies them in child position (the jr job rings, the
        // snvs children, arm,idle-state state nodes)
        if let Some(compatible) = view.get("properties").and_then(|props| props.get("compatible")) {
            let mut keys = Vec::new();
            for shape in collect_compatible_shapes(compatible, &mut keys) {
                documented.extend(shape.consts.iter().cloned());
                documented.extend(shape.linked.iter().cloned());
            }
        }
    }
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
    let children = extract_children(subschema, origin_file, roots, ref_cache, depth + 1, documented);
    // the child's own $nodename: the subschema's own view first, $ref-
    // inherited ones only fill the gap (the binding-level policy)
    let mut nodename = None;
    for (view, _) in &heads {
        if nodename.is_none() {
            nodename = extract_nodename(view);
        }
    }
    ChildSchema {
        patterns,
        properties,
        nodename,
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
/// `port@0: $ref: audio-graph-port.yaml#` entries), or is spelled
/// `{type: object}` — the graph.yaml `endpoint: {type: object}` slot
/// class: minimal subschemas that are NODES nevertheless, previously
/// mislabeled as `= $0;` leaf properties.
fn node_like(value: &Value) -> bool {
    if matches!(value.get("type"), Some(Value::String(object)) if object == "object") {
        return true;
    }
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

/// Typeable CHILD-NODE sample names suggested for a patternProperties
/// key — the completion twin of `$nodename` validation (that round
/// JUDGES existing names with exact regexes; this round PROPOSES names
/// for new ones). Top-level alternation branches fork; literal walks
/// accumulate; a REQUIRED meta run (wildcard, class, quantifier —
/// `.*-supply$`, `^.*@[0-9a-f]+$`) is too generic to suggest and yields
/// nothing, except the classic `name@[0-9a-f]+$` unit-address class which
/// keeps the `@0` convention. An optional group stops the walk and emits
/// the head (`^led(-.*)?$` -> `led`; `^port(@.+)?$` -> `port`);
/// inner-anchored branches re-walk deanchored (`(^led-[0-9a-f]$|led)`
/// -> `led`).
pub fn pattern_samples(pattern: &str) -> Vec<String> {
    fn record(head_now: &str, out: &mut BTreeSet<String>) {
        if !head_now.is_empty() {
            out.insert(head_now.to_string());
        }
    }
    fn expand(mut prefix: String, rest: &str, out: &mut BTreeSet<String>) {
        let chars: Vec<char> = rest.chars().collect();
        let mut i = 0;
        while i < chars.len() {
            match chars[i] {
                '\\' if i + 1 < chars.len() => {
                    prefix.push(chars[i + 1]);
                    i += 2;
                }
                '^' | '$' => i += 1, // inner anchors are lexical no-ops
                '(' => {
                    // split the balanced group into (interior, after, quantifier)
                    let mut depth = 1;
                    let mut close = i + 1;
                    while close < chars.len() && depth > 0 {
                        match chars[close] {
                            '(' => depth += 1,
                            ')' => depth -= 1,
                            _ => {}
                        }
                        close += 1;
                    }
                    // close now sits one past ')'; the interior is between
                    let interior: String = chars[i + 1..close.saturating_sub(1)]
                        .iter()
                        .collect();
                    let quantified = matches!(chars.get(close), Some('*') | Some('?'));
                    let after: String = chars[close + usize::from(quantified)..]
                        .iter()
                        .collect();
                    if quantified {
                        // an optional tail never shapes a typeable
                        // suggestion on its own — freeze what we have
                        record(&prefix, out);
                        return;
                    }
                    // required group: fork every top-level alternative,
                    // branch + remainder re-walked as fresh text
                    for branch in split_patterns(&interior) {
                        let joined = format!("{branch}{after}");
                        expand(prefix.clone(), &joined, out);
                    }
                    return;
                }
                '[' | '.' | '+' | '{' | '|' => {
                    // required meta: too generic — UNLESS the head sits on a
                    // unit-address '@' (`^sdhci@[0-9a-f]+$` -> `sdhci@0`)
                    if prefix.ends_with('@') {
                        prefix.push('0');
                        record(&prefix, out);
                    }
                    return;
                }
                '?' | '*' => {
                    // a bare quantifier optionals the previous char: the
                    // head up to it is the concrete suggestion
                    record(&prefix, out);
                    return;
                }
                literal => {
                    prefix.push(literal);
                    i += 1;
                }
            }
        }
        record(&prefix, out);
    }
    let trimmed = pattern.trim();
    let mut unanchored = trimmed.strip_prefix('^').unwrap_or(trimmed);
    if let Some(stripped) = unanchored.strip_suffix('$') {
        unanchored = stripped;
    }
    let mut out = BTreeSet::new();
    expand(String::new(), unanchored, &mut out);
    out.into_iter().collect()
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

/// Whether a NODE NAME satisfies a `$nodename` pattern — EXACT regex
/// semantics (the corpus patterns are true regexes: alternatives,
/// classes, `{1,2}` quantifiers, tail groups — a naive `|` split
/// mangles the `(A|B)(@…)?$` wrapper). Compiled once per distinct
/// pattern; a pattern regex can't parse falls back to the LENIENT
/// child-name matcher. The sibling `alternative_matches` stays the
/// over-matching COMPLETION matcher and never judges validation.
pub fn name_matches(pattern: &str, name: &str) -> bool {
    static NODENAME_REGEXES: std::sync::LazyLock<
        std::sync::Mutex<HashMap<String, Option<regex::Regex>>>,
    > = std::sync::LazyLock::new(|| std::sync::Mutex::new(HashMap::new()));
    let mut cache = NODENAME_REGEXES
        .lock()
        .expect("nodename regex cache poisoned");
    let compiled = cache
        .entry(pattern.to_string())
        .or_insert_with(|| regex::Regex::new(pattern).ok());
    match compiled {
        Some(regex) => regex.is_match(name),
        None => split_patterns(pattern)
            .iter()
            .any(|alt| alternative_matches(alt, name)),
    }
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
                    Some("default") => {
                        // the binding's canonical spelling for this
                        // property — one more string candidate (snvs
                        // `clock-names: const: snvs-rtc +
                        // default: snvs-rtc` — deduped against the
                        // enum/const pool above). Only this level:
                        // nested CHILD defaults never leak into the
                        // parent's pool (the skipped keys above guard
                        // the deep paths)
                        if let Value::Sequence(items) = val {
                            for item in items {
                                if let Some(scalar) = scalar_string(item) {
                                    if !out.contains(&scalar) {
                                        out.push(scalar);
                                    }
                                }
                            }
                        } else if let Some(scalar) = scalar_string(val) {
                            if !out.contains(&scalar) {
                                out.push(scalar);
                            }
                        }
                    }
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

/// `compatible = "a", "b";` statements — a complete DTS assignment inside
/// a binding-example block; group 1 holds the quoted string list.
static EXAMPLE_COMPATIBLE_ASSIGNMENT: std::sync::LazyLock<regex::Regex> =
    std::sync::LazyLock::new(|| {
        regex::Regex::new(r#"compatible\s*=\s*((?:"[^"]+"\s*,?\s*)+);"#)
            .expect("compatible assignment pattern compiles")
    });

/// One quoted string inside an assignment's value list.
static EXAMPLE_QUOTED_STRING: std::sync::LazyLock<regex::Regex> =
    std::sync::LazyLock::new(|| {
        regex::Regex::new(r#""([^"]+)""#).expect("quoted string pattern compiles")
    });

/// Count compatible assignments inside a binding's `examples:` DTS blocks —
/// the popularity signal for the staged common-compatibles tier (the
/// corpus maintainers' own usage). Only COMPLETE assignments (ending in
/// `;`) count and only quoted strings do; `#`-prefixed dt-include defines
/// are skipped — they are not compatible strings.
fn count_example_compatibles(binding: &Value, out: &mut HashMap<String, u32>) {
    let Some(Value::Sequence(examples)) = binding.get("examples") else {
        return;
    };
    for example in examples {
        let Some(text) = scalar_string(example) else {
            continue;
        };
        for assignment in EXAMPLE_COMPATIBLE_ASSIGNMENT.captures_iter(&text) {
            let Some(list) = assignment.get(1) else {
                continue;
            };
            for string in EXAMPLE_QUOTED_STRING.captures_iter(list.as_str()) {
                let Some(candidate) = string.get(1) else {
                    continue;
                };
                let candidate = candidate.as_str();
                if candidate.is_empty() || candidate.starts_with('#') {
                    continue;
                }
                *out.entry(candidate.to_string()).or_default() += 1;
            }
        }
    }
}

/// Is `needle` a subsequence of `haystack` (order-preserving, gaps
/// allowed)? The loose tier of [`fuzzy_compatibles`].
fn is_subsequence(needle: &str, haystack: &str) -> bool {
    let mut needle = needle.chars();
    let Some(mut expected) = needle.next() else {
        return true;
    };
    for ch in haystack.chars() {
        if ch == expected {
            match needle.next() {
                Some(next) => expected = next,
                None => return true,
            }
        }
    }
    false
}

/// Fuzzy search over the WHOLE documented compatible pool — the second
/// stage of the common-compatibles completion, serving any prefix once at
/// least two characters are typed. Deterministic ranking so batteries
/// pin it: contiguous-substring matches outrank loose subsequence
/// matches, and within a rank (length ascending, lexical) decides; the
/// result is capped AFTER ranking. Case-sensitive: compatible strings
/// are lowercase by convention, no silent case-folding surprises.
pub fn fuzzy_compatibles(bindings: &Bindings, prefix: &str, cap: usize) -> Vec<String> {
    let mut substring: Vec<String> = Vec::new();
    let mut subsequence: Vec<String> = Vec::new();
    for candidate in &bindings.documented {
        if candidate.contains(prefix) {
            substring.push(candidate.clone());
        } else if is_subsequence(prefix, candidate) {
            subsequence.push(candidate.clone());
        }
    }
    let rank = |rank: &mut Vec<String>| rank.sort_by(|a, b| a.len().cmp(&b.len()).then_with(|| a.cmp(b)));
    rank(&mut substring);
    rank(&mut subsequence);
    substring.extend(subsequence);
    substring.truncate(cap);
    substring
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
        // `name: true` = allowed-unconstrained in dtschema, NOT boolean —
        // real booleans are spelled `{type: boolean}`
        assert!(!wakeup.boolean);
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

    const PSCI_BINDING: &str = r#"
properties:
  compatible:
    oneOf:
      - const: vendor,psci
      - minItems: 1
        items:
          - const: vendor,psci-1.0
          - const: vendor,psci-0.2
          - const: vendor,psci
required:
  - compatible
"#;

    #[test]
    pub fn min_items_tuple_accepts_positional_prefixes() {
        // the psci.yaml form: a pure-const `items:` tuple with a
        // `minItems:` floor takes dt-schema TUPLE semantics — every
        // prefix of the tuple with a length of at least minItems
        // matches, POSITIONALLY. The bare `[vendor,psci-1.0]`
        // cites the real mainline psci idiom; tail consts alone or
        // re-ordered lists never do (pooling would be the round-1
        // imposter flood)
        let binding: Value = serde_yaml::from_str(PSCI_BINDING).unwrap();
        let mut bindings = Bindings::default();
        bindings.add_schema(PathBuf::from("arm/psci.yaml"), &binding);
        // the bare prefix SHOULD govern: the view exists and binds the
        // psci.yaml file stem (attribution for its requirement lists)
        let views = bindings.normative_views(&["vendor,psci-1.0".to_string()]);
        assert_eq!(views.len(), 1);
        assert_eq!(views[0].0, "psci");
        // positional prefixes of length 2 and the full tuple alike
        assert!(!bindings
            .normative_views(&["vendor,psci-1.0".to_string(), "vendor,psci-0.2".to_string()])
            .is_empty());
        assert!(!bindings.normative_views(&[
            "vendor,psci-1.0".to_string(),
            "vendor,psci-0.2".to_string(),
            "vendor,psci".to_string(),
        ])
        .is_empty());
        // tail consts alone never anchor the tuple branch
        assert!(bindings
            .normative_views(&["vendor,psci-0.2".to_string()])
            .is_empty());
        // re-ordering is positional nonsense: rejected
        assert!(bindings
            .normative_views(&[
                "vendor,psci-0.2".to_string(),
                "vendor,psci-1.0".to_string(),
            ])
            .is_empty());
    }

    const MON_BINDING: &str = r#"
properties:
  compatible:
    items:
      - const: vendor,sec-mon
      - const: syscon
  snvs-rtc-lp:
    type: object
    properties:
      compatible:
        const: vendor,sec-mon-rtc-lp
      interrupts:
        maxItems: 2
"#;

    #[test]
    pub fn child_positions_document_without_becoming_views() {
        // the fsl,sec-v4.0-mon shape: the file's child schema carries
        // its own compatible const. That string marks corpus-known —
        // the bondless HINT stays silent for it — but it NEVER anchors
        // a normative view: acceptance still runs the TOP-LEVEL shape
        // (the sec-mon tuple), so the imposter-flood protection holds
        let binding: Value = serde_yaml::from_str(MON_BINDING).unwrap();
        let mut bindings = Bindings::default();
        bindings.add_schema(PathBuf::from("crypto/vendor,sec-mon.yaml"), &binding);
        assert!(bindings.documents("vendor,sec-mon-rtc-lp"));
        assert!(!bindings.documents("vendor,unknown-thing"));
        assert!(bindings
            .normative_views(&["vendor,sec-mon-rtc-lp".to_string()])
            .is_empty());
    }

    const IDLE_STATES_BINDING: &str = r#"
properties:
  $nodename:
    const: idle-states
patternProperties:
  "^(cpu|cluster)-":
    type: object
    properties:
      compatible:
        enum:
          - vendor,idle-state
"#;

    #[test]
    pub fn name_governed_bindings_document_child_compatible_strings() {
        // the idle-states.yaml shape: NO top-level compatible (a
        // name-governed binding — common-part path), yet its
        // patternProperties children document their compatible
        // strings for the bondless oracle (`arm,idle-state` on real
        // stock trees)
        let binding: Value = serde_yaml::from_str(IDLE_STATES_BINDING).unwrap();
        let mut bindings = Bindings::default();
        bindings.add_schema(PathBuf::from("cpu/idle-states.yaml"), &binding);
        assert!(bindings.documents("vendor,idle-state"));
        assert!(!bindings.documents("vendor,cluster-never"));
    }

    const DEFAULTS_BINDING: &str = r#"
properties:
  compatible:
    const: vendor,dev
  clock-names:
    items:
      - const: enabled
      - const: disabled
    default: [ enabled, disabled ]
  mode:
    enum:
      - host
    default: host
  spare:
    description: canonical spelling only
    default: lone-spelling
"#;

    #[test]
    pub fn default_values_join_the_candidate_pool() {
        // a `default:` names the binding's canonical spelling — it
        // joins the string candidates (deduplicated against the
        // enum/const pool) without ever widening acceptance; a
        // default-only property finally has a candidate at all
        let binding: Value = serde_yaml::from_str(DEFAULTS_BINDING).unwrap();
        let mut bindings = Bindings::default();
        bindings.add_schema(PathBuf::from("vendor/defaults.yaml"), &binding);
        let properties = bindings.properties_for(&["vendor,dev".to_string()]);
        let clock_names = &properties["clock-names"];
        assert_eq!(
            clock_names.values,
            vec!["enabled".to_string(), "disabled".to_string()]
        );
        let mode = &properties["mode"];
        assert_eq!(mode.values, vec!["host".to_string()]);
        let spare = &properties["spare"];
        assert_eq!(spare.values, vec!["lone-spelling".to_string()]);
    }

    #[test]
    pub fn pattern_samples_expand_only_typeable_heads() {
        // the real patternProperties key classes, one behavior each:
        // literal heads emit; alternations fork; optional groups freeze
        // the head; required meta runs drop (too generic) — except the
        // unit-address `[class]` keeping the `@0` convention
        assert_eq!(pattern_samples("grp$"), vec!["grp".to_string()]);
        assert_eq!(pattern_samples("(^led-[0-9a-f]$|led)"), vec!["led".to_string()]);
        assert_eq!(pattern_samples("^led(-.*)?$"), vec!["led".to_string()]);
        assert_eq!(pattern_samples("^port(@.+)?$"), vec!["port".to_string()]);
        assert_eq!(
            pattern_samples("^(cpu|cluster)-"),
            vec!["cluster-".to_string(), "cpu-".to_string()]
        );
        assert_eq!(
            pattern_samples("^sdhci@[0-9a-f]+$"),
            vec!["sdhci@0".to_string()]
        );
        assert!(pattern_samples("^.*@[0-9a-f]+$").is_empty());
        assert!(pattern_samples(".*-supply$").is_empty());
        assert!(pattern_samples("^.*@[0-9a-f]{1,2}$").is_empty());
    }

    const PATTERN_CHILD_BINDING: &str = r#"
properties:
  compatible:
    const: vendor,checker
patternProperties:
  "^led(-.*)?$":
    type: object
    properties:
      gpios:
        description: the led gpio
"#;

    #[test]
    pub fn child_node_samples_from_pattern_children() {
        // patternProperties keys surface as typeable node-creation
        // samples through the cited compatible; purely-regex keys
        // (dropped by the sampler) contribute nothing
        let binding: Value = serde_yaml::from_str(PATTERN_CHILD_BINDING).unwrap();
        let mut bindings = Bindings::default();
        bindings.add_schema(PathBuf::from("vendor/checker.yaml"), &binding);
        let samples = bindings.child_node_samples(&["vendor,checker".to_string()]);
        assert_eq!(samples, BTreeSet::from(["led".to_string()]));
        // unknown compatible: no samples
        assert!(bindings
            .child_node_samples(&["vendor,unknown".to_string()])
            .is_empty());
    }

    const CHILD_NODENAME_BINDING: &str = r#"
properties:
  compatible:
    const: vendor,governor
patternProperties:
  "^mdio$":
    type: object
    properties:
      $nodename:
        pattern: "^mdio(-(bus|external))?(@.+|-([0-9]+))?$"
      compatible:
        type: object
  "^channel@[0-9a-f]+$":
    type: object
    properties:
      $nodename:
        pattern: "^channel(@[0-9a-f]+)?$"
      reg:
        description: channel register
"#;

    #[test]
    pub fn child_nodename_samples_join_through_the_lenient_key_law() {
        // the child's own $nodename (normally living on its $ref'd
        // binding) joins the sample set ONLY when the child's KEY
        // patterns leniently accept it — the landed over-match law is
        // the arbiter, so suggestions and the matcher that lights up
        // the typed node can never disagree
        let binding: Value = serde_yaml::from_str(CHILD_NODENAME_BINDING).unwrap();
        let mut bindings = Bindings::default();
        bindings.add_schema(PathBuf::from("vendor/governor.yaml"), &binding);
        let samples = bindings.child_node_samples(&["vendor,governor".to_string()]);
        // the mdio child: its nodename's optional groups freeze to the
        // head (`mdio-bus` never samples) and the fixed-name key keeps
        // it exactly `mdio` anyway
        assert!(samples.contains(&"mdio".to_string()));
        // the channel child: the KEY mandates the unit address (its
        // lenient form requires the `channel@` head), so the nodename's
        // bare `channel` — a name the key matcher itself rejects —
        // must NOT join; `channel@0` remains the sample
        assert!(samples.contains(&"channel@0".to_string()));
        assert!(!samples.contains(&"channel".to_string()));
        assert!(!samples.contains(&"mdio-bus".to_string()));
    }

    #[test]
    pub fn bare_type_fills_the_type_hint_gap_behind_the_ref_tail() {
        // types.yaml $ref tails remain the corpus source; a bare
        // `type: string`/`string-array` only FILLS the gap (the round's
        // corpus audit found zero bare-string schemas — this hardens
        // for out-of-tree bindings); every other bare type keeps its
        // raw home and boolean rides its own flag
        let parsed: Value = serde_yaml::from_str(
            r#"
properties:
  plain:
    type: string
  several:
    type: string-array
  ref-beats-bare:
    type: uint32
    $ref: /schemas/types.yaml#/definitions/string-array
  cells:
    type: uint32
  flag:
    type: boolean
"#,
        )
        .unwrap();
        let props = parsed.get("properties").unwrap();
        let sub = |name: &str| props.get(name).unwrap();
        assert_eq!(property_hint(sub("plain"), false).type_hint.as_deref(), Some("string"));
        assert_eq!(
            property_hint(sub("several"), false).type_hint.as_deref(),
            Some("string-array")
        );
        assert_eq!(
            property_hint(sub("ref-beats-bare"), false).type_hint.as_deref(),
            Some("string-array")
        );
        assert_eq!(property_hint(sub("cells"), false).type_hint, None);
        let flag = property_hint(sub("flag"), false);
        assert!(flag.boolean);
        assert_eq!(flag.type_hint, None);
    }

    const STUB_BINDING: &str = r#"
properties:
  compatible:
    const: vendor,stub-charger
  gpios:
    description: presence gpio
  charge-current-limit-gpios:
    description: current limit gpio
required:
  - compatible
dependencies:
  charge-current-limit-gpios:
    - charge-current-limit-mapping
oneOf:
  - required:
      - gpios
  - required:
      - charge-current-limit-gpios
"#;

    #[test]
    pub fn requirement_stub_phrases_carry_the_whole_contract() {
        // the completion twin of either/or + dependentRequired: every
        // disjunction member carries the FULL branch list; dependent
        // names carry their trigger
        let binding: Value = serde_yaml::from_str(STUB_BINDING).unwrap();
        let mut bindings = Bindings::default();
        bindings.add_schema(PathBuf::from("gpio-charger.yaml"), &binding);
        let (alternations, dependencies) =
            bindings.requirement_stub_phrases(&["vendor,stub-charger".to_string()]);
        let phrase = "at least one of: `gpios`, `charge-current-limit-gpios`";
        assert_eq!(
            alternations.get("gpios").map(String::as_str),
            Some(phrase)
        );
        assert_eq!(
            alternations
                .get("charge-current-limit-gpios")
                .map(String::as_str),
            Some(phrase)
        );
        assert_eq!(
            dependencies
                .get("charge-current-limit-mapping")
                .map(String::as_str),
            Some("required when `charge-current-limit-gpios` is present")
        );
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
        // panel-dpi's `panel-timing: true` = allowed-unconstrained (the
        // common part carries the real constraint) — not a boolean
        assert!(!top["panel-timing"].boolean);

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

    #[test]
    pub fn example_census_counts_complete_assignments_only() {
        let binding: Value = serde_yaml::from_str(
            r##"
examples:
  - |
    /dts-v1/;
    / {
        demo@d000 {
            compatible = "vendor,demo";
            friend = "vendor,friends";
        };
        pairing@1 {
            compatible = "vendor,pair-first", "vendor,pair-second";
        };
        incomplete@d000 {
            compatible = "vendor,dropped"
        };
        guarded@d000 {
            compatible = "#include-dt-bindings";
        };
        notcompatible@1 {
            other = "vendor,other";
        };
    };
"##,
        )
        .unwrap();
        let mut counts = HashMap::new();
        count_example_compatibles(&binding, &mut counts);
        assert_eq!(counts.get("vendor,demo"), Some(&1));
        assert_eq!(counts.get("vendor,pair-first"), Some(&1));
        assert_eq!(counts.get("vendor,pair-second"), Some(&1));
        // no trailing `;` — not a complete assignment
        assert!(counts.get("vendor,dropped").is_none());
        // dt-include defines are not compatible strings
        assert!(counts.get("#include-dt-bindings").is_none());
        // only `compatible` assignments count
        assert!(counts.get("vendor,other").is_none());
        assert_eq!(counts.len(), 3);
    }

    #[test]
    pub fn common_compatible_tier_is_thresholded_documented_gated_and_ordered() {
        // syscon: documented + used >= COMMON_COMPATIBLE_MIN_USES times -> in
        let mut syscon_yaml =
            String::from("properties:\n  compatible:\n    const: syscon\nexamples:\n  - |\n");
        for _ in 0..COMMON_COMPATIBLE_MIN_USES {
            syscon_yaml.push_str("    d { compatible = \"syscon\"; };\n");
        }
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("syscon.yaml"), syscon_yaml).unwrap();
        // zarro: documented + counted but BELOW the threshold -> out
        fs::write(
            dir.path().join("zarro.yaml"),
            r#"
properties:
  compatible:
    const: zarro
examples:
  - |
    z1 { compatible = "zarro"; };
    z2 { compatible = "zarro"; };
"#,
        )
        .unwrap();
        // example-popular but UNDOCUMENTED anywhere -> out (documented gate)
        fs::write(
            dir.path().join("noprops.yaml"),
            r#"
description: common-part schema, documents nothing
examples:
  - |
    n1 { compatible = "never-documented"; };
    n2 { compatible = "never-documented"; };
    n3 { compatible = "never-documented"; };
    n4 { compatible = "never-documented"; };
    n5 { compatible = "never-documented"; };
    n6 { compatible = "never-documented"; };
    n7 { compatible = "never-documented"; };
    n8 { compatible = "never-documented"; };
    n9 { compatible = "never-documented"; };
"#,
        )
        .unwrap();
        // a documented TIE at the threshold: same use count as syscon,
        // participates with count-desc order breaking by lexical order
        let mut simple_bus_yaml = String::from(
            "properties:\n  compatible:\n    const: simple-bus\nexamples:\n  - |\n",
        );
        for _ in 0..COMMON_COMPATIBLE_MIN_USES {
            simple_bus_yaml.push_str("    d { compatible = \"simple-bus\"; };\n");
        }
        fs::write(dir.path().join("simple-bus.yaml"), simple_bus_yaml).unwrap();

        // equal counts -> lexical tiebreak decides; everything else is out
        let bindings = Bindings::from_paths(&[dir.path().to_path_buf()]);
        let tier: Vec<&str> = bindings
            .common_compatibles()
            .iter()
            .map(String::as_str)
            .collect();
        // equal counts -> lexical tiebreak decides; everything else is out
        assert_eq!(tier, vec!["simple-bus", "syscon"]);
    }

    #[test]
    pub fn fuzzy_compatibles_rank_substrings_first_then_subsequences() {
        let mut bindings = Bindings::default();
        bindings.documented = [
            "fsl,imx8mm",
            "imx",
            "im",
            "arm,primecell",
            "silent",
        ]
        .into_iter()
        .map(str::to_string)
        .collect();

        // substring hits win, ordered (length asc, lexical); subsequence
        // hits follow in their own (length asc, lexical) order; "silent"
        // matches neither ("im": no m after its i)
        let ranked = fuzzy_compatibles(&bindings, "im", 50);
        assert_eq!(
            ranked,
            vec![
                "im".to_string(),
                "imx".to_string(),
                "fsl,imx8mm".to_string(),
                "arm,primecell".to_string(),
            ]
        );

        // the cap trims AFTER ranking — the substring tier survives it
        let capped = fuzzy_compatibles(&bindings, "im", 2);
        assert_eq!(capped, vec!["im".to_string(), "imx".to_string()]);

        // no match at all -> empty (the pool must never serve noise)
        assert!(fuzzy_compatibles(&bindings, "zz", 50).is_empty());

        // subsequence order-preservation: 'q' only AFTER an 'i' matches
        assert!(is_subsequence("iq", "quiet-iq"));
        assert!(!is_subsequence("iq", "quieter-arm"));
        // empty prefix is a subsequence of everything (server gates this)
        assert!(is_subsequence("", "anything"));
    }
}
