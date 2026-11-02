use crate::dts::ast::PropertyValue;
use crate::dts::bindings::{name_matches, node_compatibles, shape_satisfied, Bindings};
use crate::dts::diagnostics::Diagnostic;
use crate::dts::error_codes::ErrorCode;
use crate::dts::resolved::{ResolvedNode, ResolvedTree};
use crate::dts::{HasSource, HasSpan};
use std::path::Path;

/// Binding-driven diagnostics over a resolved tree — the editor-side
/// `dtbs_check` baseline. Deliberately conservative: only compatible-
/// bearing nodes are judged (the index matches compatibles exactly — none
/// of the completion matcher's over-match semantics are involved), and
/// only unconditional `required:` lists spawn required-missing
/// diagnostics (the flattener folds `allOf` members; if-then/oneOf
/// conditionals never contribute). The checks:
/// - required-missing: a binding's unconditional `required` property is
///   absent — Warning, anchored at the node's `compatible` property,
///   ATTRIBUTED to the imposing binding by name (its file stem) via
///   the per-binding `normative_views`.
/// - boolean-with-value: a boolean-flagged property carries a value —
///   Warning, anchored at the property.
/// - string-enum-mismatch: a string value of a property with known
///   candidates (matched binding enum, else the select:true universal
///   pool — dtschema's dt-core) matches none — Warning, anchored at
///   the value.
/// - requirement-alternation: a pure-required `oneOf`/`anyOf` none of
///   whose branches the node satisfies (neither `interrupts` nor
///   `interrupts-extended`) — Warning, anchored at the `compatible`,
///   at-least-one semantics: carrying several branches stays silent.
/// - dependent-required: a present trigger property calls for its
///   dependent names (gpio-charger's gpios family) — Warning, no
///   trigger, no judgement.
/// - deprecated-use: DEFERRED to a policy round — stock in-tree dtsi
///   legitimately carries deprecated idioms.
pub fn binding_diagnostics(
    tree: &ResolvedTree,
    file: &Path,
    bindings: &Bindings,
) -> Vec<Diagnostic> {
    if bindings.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::new();
    walk(tree.root(), "", file, bindings, &mut out);
    out
}

fn walk(
    node: &ResolvedNode,
    name: &str,
    file: &Path,
    bindings: &Bindings,
    out: &mut Vec<Diagnostic>,
) {
    check_node(node, name, file, bindings, out);
    for (child_name, child) in node.active_child_entries() {
        let child_name = child_name.to_string();
        walk(child, &child_name, file, bindings, out);
    }
}

fn check_node(
    node: &ResolvedNode,
    name: &str,
    file: &Path,
    bindings: &Bindings,
    out: &mut Vec<Diagnostic>,
) {
    let compatibles = node_compatibles(node);
    if compatibles.is_empty() {
        return;
    }
    let Some(identity) = node.get_property("compatible") else {
        return;
    };
    let identity = identity.property();
    // the node's binding identity lives where `compatible` was last
    // written; that file owns the node's diagnostics
    if identity.end.source().as_ref() != file {
        return;
    }
    let views = bindings.normative_views(&compatibles);
    if views.is_empty() {
        // no binding matched — unmatched-compatible notices are deferred;
        // bondless nodes must stay silent
        return;
    }
    // required entries are judged PER BINDING so the message names the
    // binding that imposes — the preamble proves which binding's
    // requires list a property lives on
    for (binding, schema) in &views {
        let mut fired: Vec<String> = Vec::new();
        for (name, hint) in schema.properties.iter() {
            // required entries are satisfied by a property OR a child node
            // bearing the name — the devicetree property/node duality
            if hint.required
                && node.get_property(name).is_none()
                && !node.child_exists(name)
            {
                fired.push(name.clone());
                out.push(Diagnostic::new(
                    identity.name.span(),
                    identity.end.source(),
                    ErrorCode::CustomWarning,
                    format!("binding `{binding}` requires property `{name}`"),
                ));
            }
        }
        // compatible-controlled supplements: the `allOf: [if:, then:]`
        // idiom — when the node's compatibles satisfy the condition
        // shape, the then-required names bind too (the fsl,imx8mp-xcvr
        // `resets` idiom, ~240 kernel bindings); the then-VALUES flow
        // through the pooled hint map instead (enum judgement)
        for conditional in &schema.conditionals {
            if !conditional
                .shapes
                .iter()
                .any(|shape| shape_satisfied(shape, &compatibles))
            {
                continue;
            }
            for (name, hint) in &conditional.properties {
                if !hint.required
                    || fired.contains(name)
                    || node.get_property(name).is_some()
                    || node.child_exists(name)
                {
                    continue;
                }
                fired.push(name.clone());
                out.push(Diagnostic::new(
                    identity.name.span(),
                    identity.end.source(),
                    ErrorCode::CustomWarning,
                    format!("binding `{binding}` requires property `{name}`"),
                ));
            }
        }
        // requirement disjunctions: a governed node must satisfy AT
        // LEAST ONE pure-required branch (the `interrupts` vs
        // `interrupts-extended` idiom, `clocks` vs `power-domains`) —
        // carrying several stays silent, the conservative reading
        for alternation in &schema.requirement_alternations {
            let satisfied = alternation.branches.iter().any(|branch| {
                branch.iter().all(|name| {
                    node.get_property(name).is_some() || node.child_exists(name)
                })
            });
            if satisfied {
                continue;
            }
            let expected = alternation
                .branches
                .iter()
                .map(|branch| {
                    branch
                        .iter()
                        .map(|name| format!("`{name}`"))
                        .collect::<Vec<_>>()
                        .join(" + ")
                })
                .collect::<Vec<_>>()
                .join(", ");
            out.push(Diagnostic::new(
                identity.name.span(),
                identity.end.source(),
                ErrorCode::CustomWarning,
                format!("binding `{binding}` requires one of: {expected}"),
            ));
        }
        // $nodename shaping: the binding's `properties.$nodename`
        // constrains the NAME of the nodes it governs — the LENIENT
        // matcher (the same one child-name completion uses) means a
        // rejection is really a mismatch; a miss here can never be a
        // false positive
        if let Some(pattern) = &schema.nodename {
            if !name_matches(pattern, name) {
                out.push(Diagnostic::new(
                    identity.name.span(),
                    identity.end.source(),
                    ErrorCode::CustomWarning,
                    format!(
                        "node name `{name}` does not match `{pattern}` — binding `{binding}`"
                    ),
                ));
            }
        }
        // dependentRequired: PRESENCE of the trigger property calls
        // for each dependent name (gpio-charger's
        // charge-current-limit-gpios ⇒ charge-current-limit-mapping) —
        // no trigger, no judgement — along the property-or-child duality
        for (trigger, needed) in &schema.dependencies {
            if !(node.get_property(trigger).is_some() || node.child_exists(trigger)) {
                continue;
            }
            for name in needed {
                if fired.contains(name)
                    || node.get_property(name).is_some()
                    || node.child_exists(name)
                {
                    continue;
                }
                fired.push(name.clone());
                out.push(Diagnostic::new(
                    identity.name.span(),
                    identity.end.source(),
                    ErrorCode::CustomWarning,
                    format!(
                        "binding `{binding}` requires property `{name}` alongside `{trigger}`"
                    ),
                ));
            }
        }
    }
    let hints = bindings.normative_properties_for(&compatibles);
    for (name, property) in node.active_child_properties() {
        if name == "compatible" {
            continue;
        }
        let source = property.end.source();
        if source.as_ref() != file {
            // defined in the base or an overlay file — that file's pass
            // reports it
            continue;
        }
        let hint = hints.get(&name);
        if let Some(hint) = hint {
            // deprecated-use notices are deferred: in-tree dtsi files
            // legitimately carry deprecated idioms — a policy decision,
            // not this round
            if hint.boolean && !property.values.is_empty() {
                out.push(Diagnostic::new(
                    property.name.span(),
                    source.clone(),
                    ErrorCode::CustomWarning,
                    format!("property `{name}` is boolean — write it bare, without a value"),
                ));
            }
        }
        // string values are judged against the property's AUTHORITATIVE
        // candidates: the matched binding's enum, else the universal
        // select:true map (dtschema's dt-core). The suggestion-only common
        // fragments (rk3399-pcie-common style) must never judge —
        // completion's `common_values`, not validation's pool
        let candidates: &[String] = match hint {
            Some(hint) if !hint.values.is_empty() => &hint.values,
            _ => bindings.universal_values(&name),
        };
        if candidates.is_empty() {
            continue;
        }
        for value in &property.values {
            let PropertyValue::String(string) = value else {
                continue;
            };
            if candidates.contains(string.item()) {
                continue;
            }
            let expected = candidates
                .iter()
                .take(6)
                .cloned()
                .chain((candidates.len() > 6).then(|| "…".to_string()))
                .collect::<Vec<_>>()
                .join(", ");
            out.push(Diagnostic::new(
                string.span(),
                source.clone(),
                ErrorCode::CustomWarning,
                format!("unknown value `{}` for property `{name}` — expected one of: {expected}",
                    string.item()),
            ));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dts::{IncludeLoaderGuard, Project};
    use itertools::Itertools;
    use std::fs;
    use std::path::PathBuf;

    const DEV_BINDING: &str = r#"
properties:
  compatible:
    const: vendor,dev
  req-prop:
    description: always needed
  bool-prop:
    type: boolean
  allow-prop:
    true
  sub:
    type: object
    properties:
      inner: true
  old-prop:
    deprecated: true
  enum-prop:
    enum: [ alpha, beta ]
required:
  - compatible
  - req-prop
  - sub
"#;
    const COMMON_BINDING: &str = r#"
select: true
properties:
  compatible:
    $ref: types.yaml#/definitions/string-array
  status:
    enum: [ okay, disabled ]
"#;
    fn dev_bindings() -> Bindings {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("vendor,dev.yaml"), DEV_BINDING).unwrap();
        fs::write(dir.path().join("dt-core.yaml"), COMMON_BINDING).unwrap();
        Bindings::from_paths(&[dir.path().to_path_buf()])
    }

    fn open_project(files: &[(&str, &str)]) -> (Project, tempfile::TempDir, Vec<PathBuf>) {
        let mut project = Project::default();
        let mut loader = IncludeLoaderGuard::default();
        let dir = tempfile::tempdir().unwrap();
        let mut paths = Vec::new();
        for (name, text) in files {
            let path = dir.path().join(name);
            fs::write(&path, text).unwrap();
            project
                .add_file(path.display().to_string(), &mut loader)
                .expect("Cannot add file");
            paths.push(path);
        }
        (project, dir, paths)
    }

    fn diags_for(project: &Project, path: &Path, bindings: &Bindings) -> Vec<Diagnostic> {
        let analysis = project.get_analysis(path).expect("analysis");
        binding_diagnostics(analysis.get_resolved_tree(), path, bindings)
    }

    #[test]
    pub fn required_boolean_enum_deprecated_checks_fire() {
        let text = concat!(
            "/dts-v1/;\n\n/ {\n",
            "\tgood@1 {\n",
            "\t\tcompatible = \"vendor,dev\";\n",
            "\t\treq-prop;\n",
            "\t\tstatus = \"okay\";\n",
            "\t\tenum-prop = \"alpha\";\n",
            "\t\tbool-prop;\n",
            "\t\tallow-prop = \"unconstrained\";\n",
            "\t\tsub {\n",
            "\t\t\tinner;\n",
            "\t\t};\n",
            "\t};\n",
            "\tbad@2 {\n",
            "\t\tcompatible = \"vendor,dev\";\n",
            "\t\tstatus = \"ok\";\n",
            "\t\tenum-prop = \"gamma\";\n",
            "\t\tbool-prop = <1>;\n",
            "\t\told-prop;\n",
            "\t};\n",
            "\tunmatched@3 {\n",
            "\t\tcompatible = \"vendor,404\";\n",
            "\t\tstatus = \"whatever\";\n",
            "\t};\n",
            "\tplain@4 {\n",
            "\t\treg = <1>;\n",
            "\t};\n",
            "};\n",
        );
        let (project, _temp, paths) = open_project(&[("test.dts", text)]);
        let bindings = dev_bindings();
        let diags = diags_for(&project, &paths[0], &bindings);
        // good@1 clean (req-prop as property, sub as CHILD node,
        // allow-prop spelled `true` = allowed-unconstrained, not boolean);
        // unmatched@3 has no binding (silent); plain@4 compatible-less;
        // old-prop stays silent — deprecated notices deferred to a policy
        // round; bad@2 fires the checks
        assert_eq!(diags.len(), 5, "{:#?}", diags);
        let compat_line = text.lines().position(|l| l.contains("bad@2")).unwrap();
        let by_kind = |code: ErrorCode| {
            diags
                .iter()
                .filter(|d| d.kind == code)
                .collect_vec()
        };
        let required = by_kind(ErrorCode::CustomWarning)
            .into_iter()
            .filter(|d| d.message.contains("requires property"))
            .collect_vec();
        assert_eq!(required.len(), 2);
        assert!(required.iter().all(|d| d.message.contains("vendor,dev")));
        assert!(required
            .iter()
            .any(|d| d.message.contains("`req-prop`")));
        assert!(required.iter().any(|d| d.message.contains("`sub`")));
        assert_eq!(required[0].span.start().line() as usize, compat_line + 1);
        let unknown = by_kind(ErrorCode::CustomWarning)
            .into_iter()
            .filter(|d| d.message.contains("unknown value"))
            .collect_vec();
        assert_eq!(unknown.len(), 2);
        assert!(unknown.iter().any(|d| d.message.contains("`ok`")));
        assert!(unknown.iter().any(|d| d.message.contains("`gamma`")));
        let bare = by_kind(ErrorCode::CustomWarning)
            .into_iter()
            .filter(|d| d.message.contains("boolean"))
            .collect_vec();
        assert_eq!(bare.len(), 1);
        let deprecated = by_kind(ErrorCode::CustomHint);
        // deprecated-use is deferred — no Hint diagnostics at all
        assert!(deprecated.is_empty());
    }

    #[test]
    pub fn string_enum_judgement_uses_only_string_literals() {
        let text = concat!(
            "/dts-v1/;\n\n/ {\n",
            "\tdev@1 {\n",
            "\t\tcompatible = \"vendor,dev\";\n",
            "\t\treq-prop;\n",
            "\t\tsub {};\n",
            "\t\tenum-prop = <MACRO_CELLS>;\n",
            "\t};\n",
            "};\n",
        );
        let (project, _temp, paths) = open_project(&[("test.dts", text)]);
        let bindings = dev_bindings();
        // cells/macro values are never judged against string enums
        assert!(diags_for(&project, &paths[0], &bindings).is_empty());
    }

    #[test]
    pub fn missing_required_anchors_the_identity_file_only() {
        // each file is judged standing alone: its own include closure.
        // the includer never owns the base node's diagnostics — the base
        // file's pass reports them
        let base =
            "/dts-v1/;\n\n/ {\n\tnoded: base@1 {\n\t\tcompatible = \"vendor,dev\";\n\t\tsub {};\n\t};\n};\n";
        let main = "/dts-v1/;\n\n#include \"base.dtsi\"\n\n&noded {\n\tanything;\n};\n";
        let (project, _temp, paths) = open_project(&[("base.dtsi", base), ("main.dts", main)]);
        let bindings = dev_bindings();
        // base.dtsi stands alone: the node lacks req-prop -> one report,
        // anchored at its own `compatible` property
        let base_diags = diags_for(&project, &paths[0], &bindings);
        assert_eq!(base_diags.len(), 1);
        assert!(base_diags[0].message.contains("req-prop"));
        assert!(base_diags[0].source.ends_with("base.dtsi"));
        // main.dts: the node's identity lives in base.dtsi, and its own
        // tree is complete anyway -> silent
        assert!(diags_for(&project, &paths[1], &bindings).is_empty());
    }

    #[test]
    pub fn overlay_supplied_property_satisfies_required() {
        // an in-file &overlay merges into the parser's own tree: a
        // property added by the overlay satisfies required
        let text = concat!(
            "/dts-v1/;\n\n/ {\n",
            "\tnoded: base@1 {\n",
            "\t\tcompatible = \"vendor,dev\";\n",
            "\t\tsub {};\n",
            "\t};\n",
            "};\n",
            "&noded {\n",
            "\treq-prop;\n",
            "};\n",
        );
        let (project, _temp, paths) = open_project(&[("test.dts", text)]);
        let bindings = dev_bindings();
        assert!(diags_for(&project, &paths[0], &bindings).is_empty());
    }

    #[test]
    pub fn empty_bindings_index_stays_silent() {
        let text = "/dts-v1/;\n\n/ {\n\tdev@1 {\n\t\tcompatible = \"vendor,dev\";\n\t};\n};\n";
        let (project, _temp, paths) = open_project(&[("test.dts", text)]);
        let bindings = Bindings::default();
        assert!(diags_for(&project, &paths[0], &bindings).is_empty());
    }

    #[test]
    pub fn syscon_style_sibling_bindings_do_not_impose_requirements() {
        // a `[chip, "syscon"]` node must not inherit the requirements of
        // every sibling binding that merely mentions a shared string:
        // only bindings whose compatible constraint the node ACCEPTS
        // (every const present, an enum hit otherwise) are normative
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("soc.yaml"),
            r#"
properties:
  compatible:
    items:
      - const: fsl,other-soc
      - const: syscon
  sibling-only-prop:
    description: only meant for the other SoC
required:
  - compatible
  - sibling-only-prop
"#,
        )
        .unwrap();
        fs::write(
            dir.path().join("real.yaml"),
            r#"
properties:
  compatible:
    items:
      - const: vendor,dev
      - const: syscon
  req-prop:
    description: always needed
required:
  - compatible
  - req-prop
"#,
        )
        .unwrap();
        fs::write(
            dir.path().join("ixp-style.yaml"),
            r#"
properties:
  compatible:
    items:
      - enum: [ ixp-a, ixp-b ]
      - const: syscon
  ixp-only-prop:
    description: expansion-bus-only
required:
  - compatible
  - ixp-only-prop
"#,
        )
        .unwrap();
        let bindings = Bindings::from_paths(&[dir.path().to_path_buf()]);
        let node = vec![
            "vendor,dev".to_string(),
            "syscon".to_string(),
        ];
        // completion over-collects by any-shared-string (syscon suggest
        // pollution is accepted there)…
        assert!(bindings
            .properties_for(&node)
            .contains_key("sibling-only-prop"));
        assert!(bindings
            .properties_for(&node)
            .contains_key("ixp-only-prop"));
        // …but validation merges only accepting schemas: the sibling's
        // chipB const is absent, and the ixp-style LINKED enum has no hit
        assert!(!bindings
            .normative_properties_for(&node)
            .contains_key("sibling-only-prop"));
        assert!(!bindings
            .normative_properties_for(&node)
            .contains_key("ixp-only-prop"));
        assert!(bindings
            .normative_properties_for(&node)
            .contains_key("req-prop"));
    }

    #[test]
    pub fn one_of_branch_consts_stay_disjunctive() {
        // syscon-impostor regression: a `oneOf` branch pinning `const:
        // syscon` is ONE match path, not a global requirement — the
        // qcom,gcc-apq8064 / st,stm32-exti family once pooled branch
        // consts and judged every syscon node on the tree (the imx8mm
        // silence-gate leak: the impostor required-list union was
        // #clock-cells, #interrupt-cells, #reset-cells,
        // interrupt-controller)
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("qcom,gcc-apq8064.yaml"),
            r#"
properties:
  compatible:
    oneOf:
      - items:
          - enum: [ "qcom,gcc-apq8064", "qcom,gcc-msm8960" ]
          - const: syscon
      - enum:
          - "qcom,gcc-apq8064"
          - "qcom,gcc-msm8960"
        deprecated: true
  qcom-only-prop:
    description: clock-controller requirement
required:
  - compatible
  - qcom-only-prop
"#,
        )
        .unwrap();
        fs::write(
            dir.path().join("st,stm32-exti.yaml"),
            r#"
properties:
  compatible:
    oneOf:
      - items:
          - enum: [ "st,stm32-exti", "st,stm32h7-exti" ]
      - items:
          - enum: [ "st,stm32mp1-exti", "st,stm32mp13-exti" ]
          - const: syscon
  st-only-prop:
    description: interrupt-controller requirement
required:
  - compatible
  - st-only-prop
"#,
        )
        .unwrap();
        let bindings = Bindings::from_paths(&[dir.path().to_path_buf()]);
        let vendor_node = vec!["vendor,dev".to_string(), "syscon".to_string()];
        // an unrelated syscon node is bondless for BOTH impostors —
        // branch consts never leave their disjunctive frame
        let views: Vec<String> = bindings
            .normative_views(&vendor_node)
            .into_iter()
            .map(|(name, _)| name)
            .collect();
        assert!(views.is_empty(), "{views:?}");
        // completion still over-collects the shared syscon suggestion…
        assert!(bindings
            .properties_for(&vendor_node)
            .contains_key("qcom-only-prop"));
        assert!(bindings
            .properties_for(&vendor_node)
            .contains_key("st-only-prop"));
        // …while a node carrying a vendor string AND syscon satisfies
        // the branch alternative — governed, attributed by name
        let qcom_node = vec!["qcom,gcc-apq8064".to_string(), "syscon".to_string()];
        let views: Vec<String> = bindings
            .normative_views(&qcom_node)
            .into_iter()
            .map(|(name, _)| name)
            .collect();
        assert_eq!(views, vec!["qcom,gcc-apq8064".to_string()]);
        let st_node = vec!["st,stm32mp1-exti".to_string(), "syscon".to_string()];
        let views: Vec<String> = bindings
            .normative_views(&st_node)
            .into_iter()
            .map(|(name, _)| name)
            .collect();
        // attribution names the binding FILE (`st,stm32-exti.yaml`), not
        // the matched node string
        assert_eq!(views, vec!["st,stm32-exti".to_string()]);
        // ENUM-ANCHORED since the acceptance round: the pure-enum branch
        // (no const anywhere) governs too — its whole anchoring is the
        // linked values (the deprecated gcc spelling and the bare
        // ['st,stm32-exti'] list alike)
        let enum_only = vec!["st,stm32-exti".to_string()];
        let views: Vec<String> = bindings
            .normative_views(&enum_only)
            .into_iter()
            .map(|(name, _)| name)
            .collect();
        assert_eq!(views, vec!["st,stm32-exti".to_string()]);
        let deprecated_gcc = vec!["qcom,gcc-apq8064".to_string()];
        let views: Vec<String> = bindings
            .normative_views(&deprecated_gcc)
            .into_iter()
            .map(|(name, _)| name)
            .collect();
        assert_eq!(views, vec!["qcom,gcc-apq8064".to_string()]);
    }

    #[test]
    pub fn enum_anchored_bindings_impose_requirements() {
        // the fsl,imx8-pcie-phy shape: `compatible: enum:` with an
        // unconditional required list — enum anchoring was the round-2
        // head precisely because the user's imx8mm pcie-phy node cites
        // ONLY the enum value (no const fallback) and the binding's
        // fsl,refclk-pad-mode went unseen
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("vendor,phy.yaml"),
            r#"
properties:
  compatible:
    enum:
      - "vendor,phy-a"
      - "vendor,phy-b"
  refclk-mode:
    description: pad reference clock mode
required:
  - compatible
  - refclk-mode
"#,
        )
        .unwrap();
        let bindings = Bindings::from_paths(&[dir.path().to_path_buf()]);
        let (project, _temp, paths) = open_project(&[(
            "test.dts",
            "/dts-v1/;\n\n/ {\n\tphy@1 {\n\t\tcompatible = \"vendor,phy-b\";\n\t};\n};\n",
        )]);
        let diags = diags_for(&project, &paths[0], &bindings);
        assert_eq!(diags.len(), 1, "{:#?}", diags);
        assert!(diags[0].message.contains("binding `vendor,phy` requires property `refclk-mode`"));
        let (project, _temp, paths) = open_project(&[(
            "test.dts",
            "/dts-v1/;\n\n/ {\n\tphy@1 {\n\t\tcompatible = \"vendor,phy-a\";\n\t\trefclk-mode = <1>;\n\t};\n};\n",
        )]);
        let diags = diags_for(&project, &paths[0], &bindings);
        assert!(diags.is_empty(), "{diags:#?}");
        let (project, _temp, paths) = open_project(&[(
            "test.dts",
            "/dts-v1/;\n\n/ {\n\tphy@1 {\n\t\tcompatible = \"vendor,unrelated\", \"syscon\";\n\t};\n};\n",
        )]);
        assert!(diags_for(&project, &paths[0], &bindings).is_empty());
    }

    #[test]
    pub fn if_then_conditionals_bind_required_per_compatible() {
        // the `allOf: [if:, then:]` idiom: a node whose compatibles
        // satisfy the if-compatible condition carries the then-required
        // names IN ADDITION to the unconditional list; unconditioned
        // variants stay clean (contains-const AND contains-enum
        // condition spellings both parse)
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("vendor,dev-family.yaml"),
            r#"
properties:
  compatible:
    enum:
      - "vendor,dev"
      - "vendor,dev2"
      - "vendor,dev3"
  plain-prop:
    description: always needed
  turbo-prop:
    description: only the -dev2 variant
  extra-prop:
    description: only the -dev3 variant
required:
  - compatible
  - plain-prop
allOf:
  - if:
      properties:
        compatible:
          contains:
            const: vendor,dev2
    then:
      required:
        - turbo-prop
  - if:
      properties:
        compatible:
          contains:
            enum:
              - "vendor,dev3"
    then:
      required:
        - extra-prop
"#,
        )
        .unwrap();
        let bindings = Bindings::from_paths(&[dir.path().to_path_buf()]);
        let (project, _temp, paths) = open_project(&[(
            "test.dts",
            "/dts-v1/;\n\n/ {\n\tdev1: dev@1 {\n\t\tcompatible = \"vendor,dev\";\n\t\tplain-prop = <1>;\n\t};\n\tdev2: dev@2 {\n\t\tcompatible = \"vendor,dev2\";\n\t\tplain-prop = <2>;\n\t};\n\tdev3: dev@3 {\n\t\tcompatible = \"vendor,dev3\";\n\t\tplain-prop = <3>;\n\t\textra-prop = <3>;\n\t};\n};\n",
        )]);
        let diags = diags_for(&project, &paths[0], &bindings);
        // dev1 clean (no condition matches); dev2 missing only
        // turbo-prop; dev3 complete with extra-prop
        assert_eq!(diags.len(), 1, "{:#?}", diags);
        assert!(diags[0]
            .message
            .contains("binding `vendor,dev-family` requires property `turbo-prop`"));
        // with the conditional property supplied, the whole tree is
        // silent — the unconditional list never fires
        let (project, _temp, paths) = open_project(&[(
            "test.dts",
            "/dts-v1/;\n\n/ {\n\tdev2: dev@2 {\n\t\tcompatible = \"vendor,dev2\";\n\t\tplain-prop = <2>;\n\t\tturbo-prop = <2>;\n\t};\n};\n",
        )]);
        assert!(diags_for(&project, &paths[0], &bindings).is_empty());
    }

    #[test]
    pub fn conditionals_refine_value_pools_per_compatible() {
        // the refinement round: then.properties values overlay a
        // matching node's pools with UNION semantics — conditional-only
        // pools (rpmcc clock-names: base carries no enum) fill for the
        // matching variant and stay empty for every other node, and
        // required-only + properties-value idiom live in ONE overlay.
        // minItems/maxItems-only then-shapes enrich nothing and drop.
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("vendor,dev-family.yaml"),
            r#"
properties:
  compatible:
    enum:
      - "vendor,dev"
      - "vendor,dev2"
  mode-prop:
    description: variant dependent, unconstrained by the base
  bulk-prop:
    description: only ever listwise-shaped
required:
  - compatible
allOf:
  - if:
      properties:
        compatible:
          contains:
            const: vendor,dev2
    then:
      required:
        - turbo-prop
      properties:
        mode-prop:
          enum: [ turbo, hyperspeed ]
  - if:
      properties:
        compatible:
          contains:
            const: vendor,dev2
    then:
      properties:
        power-names:
          items:
            - const: bus
            - const: core
  - if:
      properties:
        compatible:
          contains:
            enum:
              - "vendor,dev"
    then:
      properties:
        bulk-prop:
          minItems: 1
          maxItems: 4
"#,
        )
        .unwrap();
        let bindings = Bindings::from_paths(&[dir.path().to_path_buf()]);
        let dev2 = vec!["vendor,dev2".to_string()];
        let dev1 = vec!["vendor,dev".to_string()];
        // completion narrows per variant: dev2 sees the conditional
        // values, dev1 sees none of them (shape not satisfied)
        let pool2 = bindings.properties_for(&dev2);
        let pool1 = bindings.properties_for(&dev1);
        assert_eq!(
            pool2.get("mode-prop").unwrap().values,
            vec!["turbo".to_string(), "hyperspeed".to_string()]
        );
        assert!(pool2.get("power-names").unwrap().values.contains(&"bus".to_string()));
        assert!(pool2.get("turbo-prop").unwrap().required);
        assert!(!pool1.contains_key("power-names"));
        assert!(pool1.get("mode-prop").unwrap().values.is_empty());
        assert!(!pool1.contains_key("turbo-prop"));
        // minItems-only conditional dropped everywhere
        assert!(pool1.get("bulk-prop").unwrap().values.is_empty());
        assert!(pool2.get("bulk-prop").unwrap().values.is_empty());
        // the normative pool mirrors the overlay: enum-anchored binding
        // accepts, conditional-only candidates reach the judge
        let norm2 = bindings.normative_properties_for(&dev2);
        assert_eq!(
            norm2.get("mode-prop").unwrap().values,
            vec!["turbo".to_string(), "hyperspeed".to_string()]
        );
        // a dev2 node with a wrong mode-prop is judged against the
        // conditional pool; a dev1 node's same string stays silent
        // (no candidates anywhere) — union never produces constraints
        // where the binding lists none
        let (project, _temp, paths) = open_project(&[(
            "test.dts",
            "/dts-v1/;\n\n/ {\n\tdev2: dev@2 {\n\t\tcompatible = \"vendor,dev2\";\n\t\tmode-prop = \"wrong\";\n\t\tturbo-prop = <1>;\n\t};\n\tdev1: dev@1 {\n\t\tcompatible = \"vendor,dev\";\n\t\tmode-prop = \"wrong\";\n\t};\n};\n",
        )]);
        let diags = diags_for(&project, &paths[0], &bindings);
        assert_eq!(diags.len(), 1, "{:#?}", diags);
        assert!(diags[0]
            .message
            .contains("unknown value `wrong` for property `mode-prop` — expected one of: turbo, hyperspeed"));
    }

    #[test]
    pub fn requirement_alternations_enforce_either_or() {
        // the either/or round: pure-required oneOf/anyOf members bind
        // at-least-one semantics (carrying several branches stays
        // silent); branches may carry multiple names (joined with +,
        // satisfied by property OR child node); a member with property
        // constraints opts the WHOLE disjunction out — a bare-required
        // reading would misjudge refinement as requirement
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("vendor,either.yaml"),
            r#"
properties:
  compatible:
    const: vendor,either
  interrupts:
    description: classic interrupt parent link
  interrupts-extended:
    description: extended interrupt parent link
required:
  - compatible
oneOf:
  - required:
      - interrupts
  - required:
      - interrupts-extended
"#,
        )
        .unwrap();
        fs::write(
            dir.path().join("vendor,multi.yaml"),
            r#"
properties:
  compatible:
    const: vendor,multi
  a: true
  b: true
  c: true
required:
  - compatible
oneOf:
  - required:
      - a
      - b
  - required:
      - c
"#,
        )
        .unwrap();
        fs::write(
            dir.path().join("vendor,duality.yaml"),
            r#"
properties:
  compatible:
    const: vendor,duality
  supply:
    description: node property duality counterpart
required:
  - compatible
anyOf:
  - required:
      - regulators
  - required:
      - supply
"#,
        )
        .unwrap();
        fs::write(
            dir.path().join("vendor,trap.yaml"),
            r#"
properties:
  compatible:
    const: vendor,trap
  interrupts:
    description: property-constrained branch member below
  interrupts-extended:
    description: the pure member
required:
  - compatible
oneOf:
  - properties:
      compatible:
        const: vendor,trap
    required:
      - interrupts
  - required:
      - interrupts-extended
"#,
        )
        .unwrap();
        let bindings = Bindings::from_paths(&[dir.path().to_path_buf()]);
        let text = concat!(
            "/dts-v1/;\n\n/ {\n",
            "\tvia-interrupts {\n\t\tcompatible = \"vendor,either\";\n\t\tinterrupts = <1 2>;\n\t};\n",
            "\tvia-extended {\n\t\tcompatible = \"vendor,either\";\n\t\tinterrupts-extended = <&intc 3>;\n\t};\n",
            "\tvia-none {\n\t\tcompatible = \"vendor,either\";\n\t};\n",
            "\tmulti-ok {\n\t\tcompatible = \"vendor,multi\";\n\t\ta; b;\n\t};\n",
            "\tmulti-partial {\n\t\tcompatible = \"vendor,multi\";\n\t\ta;\n\t};\n",
            "\tduality-ok {\n\t\tcompatible = \"vendor,duality\";\n\t\tregulators {\n\t\t};\n\t};\n",
            "\ttrap-silent {\n\t\tcompatible = \"vendor,trap\";\n\t};\n",
            "};\n",
        );
        let (project, _temp, paths) = open_project(&[("test.dts", text)]);
        let diags = diags_for(&project, &paths[0], &bindings);
        // exactly the two violated alternatives fire — satisfied
        // branches (property, child-duality) add NOTHING, and the
        // impure-member oneOf was never collected (no vendor,trap
        // diagnostics at all)
        assert_eq!(diags.len(), 2, "{:#?}", diags);
        assert!(diags
            .iter()
            .any(|d| d.message.contains(
                "binding `vendor,either` requires one of: `interrupts`, `interrupts-extended`",
            )));
        assert!(diags.iter().any(|d| d.message.contains(
            "binding `vendor,multi` requires one of: `a` + `b`, `c`",
        )));
        assert!(diags.iter().all(|d| !d.message.contains("vendor,trap")), "{:#?}", diags);
    }

    #[test]
    pub fn nodename_patterns_judge_governed_nodes() {
        // $nodename shaping: the binding's properties.$nodename pattern
        // constrains the NAME of governed nodes — matched names stay
        // silent (unit addresses tolerated through the lenient matcher),
        // a mismatch fires with the node's own name and the pattern,
        // attributed to the binding; pattern-less bindings and const^$
        // roots stay untouched
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("vendor,dev.yaml"),
            r#"
properties:
  $nodename:
    pattern: "^dev(@[0-9a-f]+)?$"
  compatible:
    const: vendor,dev
required:
  - compatible
"#,
        )
        .unwrap();
        fs::write(
            dir.path().join("vendor,bare.yaml"),
            r#"
properties:
  compatible:
    const: vendor,bare
required:
  - compatible
"#,
        )
        .unwrap();
        let bindings = Bindings::from_paths(&[dir.path().to_path_buf()]);
        let text = concat!(
            "/dts-v1/;\n\n/ {\n",
            "\tdev {\n\t\tcompatible = \"vendor,dev\";\n\t};\n",
            "\tdev@f00d {\n\t\tcompatible = \"vendor,dev\";\n\t};\n",
            "\tdev@badname {\n\t\tcompatible = \"vendor,dev\";\n\t};\n",
            "\tbare-whatever {\n\t\tcompatible = \"vendor,bare\";\n\t};\n",
            "};\n",
        );
        let (project, _temp, paths) = open_project(&[("test.dts", text)]);
        let diags = diags_for(&project, &paths[0], &bindings);
        // EXACT regex semantics: bare `dev` and `dev@f00d` match
        // `^dev(@[0-9a-f]+)?$`; dev@badname REJECTS — its unit segment
        // is not hex (`badname`'s n is outside [0-9a-f]) — the exact
        // match the lenient completion matcher deliberately cannot
        // deliver; vendor,bare never defines a pattern
        assert_eq!(
            diags
                .iter()
                .filter(|d| d.message.contains("does not match"))
                .count(),
            1,
            "{:#?}",
            diags
        );
        assert!(diags[0].message.contains(
            "node name `dev@badname` does not match `^dev(@[0-9a-f]+)?$` — binding `vendor,dev`"
        ));
        // the definite rejection: a name missing the anchored `dev`
        // run entirely — exactly one diagnostic, attributed, anchored
        // at the node's compatible line
        let text_two = concat!(
            "/dts-v1/;\n\n/ {\n",
            "\twrongname@1 {\n\t\tcompatible = \"vendor,dev\";\n\t};\n",
            "};\n",
        );
        let (project, _temp, paths) = open_project(&[("test.dts", text_two)]);
        let diags = diags_for(&project, &paths[0], &bindings);
        assert_eq!(diags.len(), 1, "{:#?}", diags);
        assert!(diags[0].message.contains(
            "node name `wrongname@1` does not match `^dev(@[0-9a-f]+)?$` — binding `vendor,dev`"
        ));
        let expected_line = text_two
            .lines()
            .position(|l| l.contains("vendor,dev"))
            .unwrap();
        assert_eq!(diags[0].span.start().line() as usize, expected_line);
        // the gate-found bug class as a regression: real $nodename
        // patterns carry `|` ALTERNATIVES (simple-bus's
        // `^([a-z][a-z0-9\-]+-bus|bus|soc)(@.+)?$`) — each member must
        // be judged on its own, never the whole pattern as one regex
        fs::write(
            dir.path().join("vendor,buses.yaml"),
            r#"
properties:
  $nodename:
    pattern: '^([a-z][a-z0-9\-]+-bus|bus|soc)(@.+)?$'
  compatible:
    const: vendor,buses
required:
  - compatible
"#,
        )
        .unwrap();
        let bindings = Bindings::from_paths(&[dir.path().to_path_buf()]);
        let text_three = concat!(
            "/dts-v1/;\n\n/ {\n",
            "\tsoc@0 {\n\t\tcompatible = \"vendor,buses\";\n\t};\n",
            "\tspba-bus@30000000 {\n\t\tcompatible = \"vendor,buses\";\n\t};\n",
            "\tbus@30400000 {\n\t\tcompatible = \"vendor,buses\";\n\t};\n",
            "\twrongnode@1 {\n\t\tcompatible = \"vendor,buses\";\n\t};\n",
            "};\n",
        );
        let (project, _temp, paths) = open_project(&[("test.dts", text_three)]);
        let diags = diags_for(&project, &paths[0], &bindings);
        // every alternative member accepts its names leniently
        // (`soc@0`, `bus@…`, `spba-bus@…` all hit a member); only a
        // name outside ALL members fires — the alternatives-split is
        // exactly what the gate found missing (whole-pattern-as-one
        // regex rejected soc@0 and bus@30400000 outright)
        assert_eq!(diags.len(), 1, "{:#?}", diags);
        assert!(diags[0].message.contains(
            "node name `wrongnode@1` does not match `^([a-z][a-z0-9\\-]+-bus|bus|soc)(@.+)?$` — binding `vendor,buses`"
        ));
    }

    #[test]
    pub fn dependent_required_fires_alongside_present_triggers() {
        // `dependencies:` (JSON-schema's dependentRequired): PRESENCE of
        // the trigger property demands each dependent name — no trigger,
        // no judgement — along the property-or-child duality; the
        // gpio-charger gpios family is the real-corpus idiom
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("vendor,dep.yaml"),
            r#"
properties:
  compatible:
    const: vendor,dep
  trigger-prop:
    description: calls for its dependent
  needed-prop:
    description: the dependent itself
  node-trigger:
    description: a trigger satisfied the usual way
  node-need:
    description: a dependent satisfiable as a child node
required:
  - compatible
dependencies:
  trigger-prop:
    - needed-prop
  node-trigger:
    - node-need
"#,
        )
        .unwrap();
        let bindings = Bindings::from_paths(&[dir.path().to_path_buf()]);
        let text = concat!(
            "/dts-v1/;\n\n/ {\n",
            "\tdep-ok {\n\t\tcompatible = \"vendor,dep\";\n\t\ttrigger-prop = <1>;\n\t\tneeded-prop = <1>;\n\t};\n",
            "\tdep-fire {\n\t\tcompatible = \"vendor,dep\";\n\t\ttrigger-prop = <1>;\n\t};\n",
            "\tdep-unttrigger {\n\t\tcompatible = \"vendor,dep\";\n\t\tneeded-prop = <1>;\n\t};\n",
            "\tdep-duality {\n\t\tcompatible = \"vendor,dep\";\n\t\tnode-trigger = <1>;\n\t\tnode-need {\n\t\t};\n\t};\n",
            "\tdep-duality-fire {\n\t\tcompatible = \"vendor,dep\";\n\t\tnode-trigger = <1>;\n\t};\n",
            "};\n",
        );
        let (project, _temp, paths) = open_project(&[("test.dts", text)]);
        let diags = diags_for(&project, &paths[0], &bindings);
        assert_eq!(diags.len(), 2, "{:#?}", diags);
        assert!(diags
            .iter()
            .any(|d| d.message.contains(
                "binding `vendor,dep` requires property `needed-prop` alongside `trigger-prop`"
            )));
        assert!(diags
            .iter()
            .any(|d| d.message.contains(
                "binding `vendor,dep` requires property `node-need` alongside `node-trigger`"
            )));
    }
}
