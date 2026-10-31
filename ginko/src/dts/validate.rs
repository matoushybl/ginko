use crate::dts::ast::PropertyValue;
use crate::dts::bindings::{node_compatibles, Bindings};
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
///   pool — dtschema's dt-core) matches none — Warning, anchored at the
///   value.
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
    walk(tree.root(), file, bindings, &mut out);
    out
}

fn walk(node: &ResolvedNode, file: &Path, bindings: &Bindings, out: &mut Vec<Diagnostic>) {
    check_node(node, file, bindings, out);
    for child in node.active_children() {
        walk(child, file, bindings, out);
    }
}

fn check_node(node: &ResolvedNode, file: &Path, bindings: &Bindings, out: &mut Vec<Diagnostic>) {
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
    for (binding, properties) in &views {
        for (name, hint) in properties.iter() {
            // required entries are satisfied by a property OR a child node
            // bearing the name — the devicetree property/node duality
            if hint.required
                && node.get_property(name).is_none()
                && !node.child_exists(name)
            {
                out.push(Diagnostic::new(
                    identity.name.span(),
                    identity.end.source(),
                    ErrorCode::CustomWarning,
                    format!("binding `{binding}` requires property `{name}`"),
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
        // the pure-enum branch (no const anywhere) never anchors: a
        // bare ["st,stm32-exti"] node stays suggestion-only
        let enum_only = vec!["st,stm32-exti".to_string()];
        assert!(bindings.normative_views(&enum_only).is_empty());
    }
}
