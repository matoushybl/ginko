use crate::dts::ast::{
    AnyDirective, Cell, DtsFile, Include, Node, NodePayload, Primary, Property, PropertyValue,
    Reference, ReferencedNode, WithToken,
};
use crate::dts::{HasSpan, NodeItem, Position, Span};

#[derive(Debug, Clone)]
pub enum ReferenceContext<'a> {
    Root,
    Node(&'a Node),
}

#[derive(Debug)]
#[allow(unused)]
pub enum ItemAtCursor<'a> {
    Property(&'a Property),
    Node(&'a Node),
    ReferencedNode(&'a ReferencedNode),
    Reference(&'a Reference, ReferenceContext<'a>),
    Label(&'a WithToken<String>),
    Include(&'a Include),
    Macro(&'a WithToken<String>),
}

/// Every reference of a file with its enclosing node context — the
/// reverse-lookup companion of `item_at_cursor`: reference listing
/// (`textDocument/references`) and document highlight walk the whole
/// file, resolving every `&label` / `&{/path}` / overlay head /
/// delete-directive target on demand. The WITH-TOKEN form keeps the
/// source span for location output.
#[derive(Debug, Clone)]
pub struct CollectedReference<'a> {
    pub token: &'a WithToken<Reference>,
    pub context: ReferenceContext<'a>,
}

impl DtsFile {
    pub fn item_at_cursor(&self, cursor: &Position) -> Option<ItemAtCursor<'_>> {
        for element in &self.elements {
            if let Some(item) = element.item_at_cursor(cursor) {
                return Some(item);
            }
        }
        None
    }

    pub fn collect_references<'a>(&'a self, out: &mut Vec<CollectedReference<'a>>) {
        for element in &self.elements {
            element.collect_references(out);
        }
    }
}

impl Primary {
    pub fn item_at_cursor(&self, cursor: &Position) -> Option<ItemAtCursor<'_>> {
        match self {
            Primary::Directive(directive) => directive.item_at_cursor(cursor),
            Primary::Root(root) => root.item_at_cursor(cursor),
            Primary::ReferencedNode(node) => node.item_at_cursor(cursor),
        }
    }
}

impl AnyDirective {
    pub fn item_at_cursor(&self, cursor: &Position) -> Option<ItemAtCursor<'_>> {
        match self {
            AnyDirective::Include(include) => {
                if include.span().contains(cursor) {
                    Some(ItemAtCursor::Include(include))
                } else {
                    None
                }
            }
            AnyDirective::DeletedNode(_, node_ref) => Some(ItemAtCursor::Reference(
                node_ref.item(),
                ReferenceContext::Root,
            )),
            AnyDirective::OmitIfNoRef(_, node_ref) => Some(ItemAtCursor::Reference(
                node_ref.item(),
                ReferenceContext::Root,
            )),
            _ => None,
        }
    }
}

impl ReferencedNode {
    pub fn item_at_cursor(&self, cursor: &Position) -> Option<ItemAtCursor<'_>> {
        if self.reference.span().contains(cursor) {
            return Some(ItemAtCursor::Reference(
                self.reference.item(),
                ReferenceContext::Root,
            ));
        }
        self.payload
            .item_at_cursor(cursor, &ReferenceContext::Root)
            .or(if self.span().contains(cursor) {
                Some(ItemAtCursor::ReferencedNode(self))
            } else {
                None
            })
    }
}

impl Node {
    pub fn item_at_cursor(&self, cursor: &Position) -> Option<ItemAtCursor<'_>> {
        if let Some(label) = &self.label {
            if label.span().contains(cursor) {
                return Some(ItemAtCursor::Label(label));
            }
        }
        self.payload
            .item_at_cursor(cursor, &ReferenceContext::Node(self))
            .or(if self.span().contains(cursor) {
                Some(ItemAtCursor::Node(self))
            } else {
                None
            })
    }
}

impl NodePayload {
    pub fn item_at_cursor<'a>(
        &'a self,
        cursor: &Position,
        ctx: &ReferenceContext<'a>,
    ) -> Option<ItemAtCursor<'a>> {
        for node in &self.items {
            if let Some(item) = node.item_at_cursor(cursor, ctx) {
                return Some(item);
            }
        }
        None
    }
}

impl NodeItem {
    pub fn item_at_cursor<'a>(
        &'a self,
        cursor: &Position,
        ctx: &ReferenceContext<'a>,
    ) -> Option<ItemAtCursor<'a>> {
        match self {
            NodeItem::Property(property) => property.item_at_cursor(cursor, ctx),
            NodeItem::Node(node) => node.item_at_cursor(cursor),
            NodeItem::DeletedNode(..) => None,
            NodeItem::DeletedProperty(..) => None,
        }
    }
}

impl Property {
    pub fn item_at_cursor<'a>(
        &'a self,
        cursor: &Position,
        ctx: &ReferenceContext<'a>,
    ) -> Option<ItemAtCursor<'a>> {
        if let Some(label) = &self.label {
            if label.span().contains(cursor) {
                return Some(ItemAtCursor::Label(label));
            }
        }
        for value in &self.values {
            if let Some(item) = value.item_at_cursor(cursor, ctx) {
                return Some(item);
            }
        }
        None
    }
}

impl PropertyValue {
    pub fn item_at_cursor<'a>(
        &'a self,
        cursor: &Position,
        ctx: &ReferenceContext<'a>,
    ) -> Option<ItemAtCursor<'a>> {
        match self {
            PropertyValue::String(_) => None,
            PropertyValue::Cells(_, cells, _) => {
                for cell in cells {
                    match cell {
                        Cell::Number(_, _) | Cell::Expression(_) => {}
                        Cell::Reference(reference) => {
                            if reference.span().contains(cursor) {
                                return Some(ItemAtCursor::Reference(
                                    reference.item(),
                                    ctx.clone(),
                                ));
                            }
                        }
                        Cell::Macro(name) => {
                            if name.span().contains(cursor) {
                                return Some(ItemAtCursor::Macro(name));
                            }
                        }
                    }
                }
                None
            }
            PropertyValue::Reference(reference) => {
                if reference.span().contains(cursor) {
                    Some(ItemAtCursor::Reference(reference.item(), ctx.clone()))
                } else {
                    None
                }
            }
            PropertyValue::ByteStrings(..) => None,
            PropertyValue::Incbin(_, include, _) => {
                if include.span().contains(cursor) {
                    Some(ItemAtCursor::Include(include))
                } else {
                    None
                }
            }
        }
    }

    pub fn collect_references<'a>(&'a self, refs: &mut Vec<CollectedReference<'a>>, ctx: &ReferenceContext<'a>) {
        match self {
            PropertyValue::String(_) | PropertyValue::ByteStrings(..) | PropertyValue::Incbin(..) => {}
            PropertyValue::Cells(_, cells, _) => {
                for cell in cells {
                    if let Cell::Reference(reference) = cell {
                        refs.push(CollectedReference {
                            token: reference,
                            context: ctx.clone(),
                        });
                    }
                }
            }
            PropertyValue::Reference(reference) => refs.push(CollectedReference {
                token: reference,
                context: ctx.clone(),
            }),
        }
    }
}

impl Primary {
    pub fn collect_references<'a>(&'a self, refs: &mut Vec<CollectedReference<'a>>) {
        match self {
            Primary::Directive(directive) => directive.collect_references(refs),
            Primary::Root(root) => root.collect_references(refs),
            Primary::ReferencedNode(node) => node.collect_references(refs),
        }
    }
}

impl AnyDirective {
    pub fn collect_references<'a>(&'a self, refs: &mut Vec<CollectedReference<'a>>) {
        match self {
            // delete/omit directives carry a reference with Root context
            AnyDirective::DeletedNode(_, node_ref) | AnyDirective::OmitIfNoRef(_, node_ref) => {
                refs.push(CollectedReference {
                    token: node_ref,
                    context: ReferenceContext::Root,
                });
            }
            _ => {}
        }
    }
}

impl ReferencedNode {
    pub fn collect_references<'a>(&'a self, refs: &mut Vec<CollectedReference<'a>>) {
        // the overlay head itself is a reference at Root level...
        refs.push(CollectedReference {
            token: &self.reference,
            context: ReferenceContext::Root,
        });
        // ...and the payload's property values resolve relative to the
        // overlay position — the plain walk answers for the inlined payload
        let ctx = ReferenceContext::Root;
        self.payload.collect_references(refs, &ctx);
    }
}

impl Node {
    pub fn collect_references<'a>(&'a self, refs: &mut Vec<CollectedReference<'a>>) {
        let ctx = ReferenceContext::Node(self);
        self.payload.collect_references(refs, &ctx);
    }
}

impl NodePayload {
    pub fn collect_references<'a>(
        &'a self,
        refs: &mut Vec<CollectedReference<'a>>,
        ctx: &ReferenceContext<'a>,
    ) {
        for node in &self.items {
            node.collect_references(refs, ctx);
        }
    }
}

impl NodeItem {
    pub fn collect_references<'a>(
        &'a self,
        refs: &mut Vec<CollectedReference<'a>>,
        ctx: &ReferenceContext<'a>,
    ) {
        match self {
            NodeItem::Property(property) => property.collect_references(refs, ctx),
            NodeItem::Node(node) => node.collect_references(refs),
            NodeItem::DeletedNode(..) | NodeItem::DeletedProperty(..) => {}
        }
    }
}

impl Property {
    pub fn collect_references<'a>(
        &'a self,
        refs: &mut Vec<CollectedReference<'a>>,
        ctx: &ReferenceContext<'a>,
    ) {
        for value in &self.values {
            value.collect_references(refs, ctx);
        }
    }
}

// The compatible-string harvest: every `compatible = "..."` string
// assignment of a file in first-seen source order, keeping each
// string's span so the caller can exclude the property currently being
// typed — the per-file project tier of compatible VALUE completion.
// The corpus tiers answer from bindings; this pool answers from what
// the project itself declares (board dtsi extensions, carrier strings
// the corpus never documents). Only literal string values count:
// macros, phandles and byte runs are skipped honestly, and so are
// EMPTY strings — `compatible = ""` is cursor noise, not a
// declaration.
impl DtsFile {
    pub fn compatible_strings(&self, out: &mut Vec<(String, Span)>) {
        for element in &self.elements {
            element.compatible_strings(out);
        }
    }
}

impl Primary {
    pub fn compatible_strings(&self, out: &mut Vec<(String, Span)>) {
        match self {
            Primary::Directive(_) => {}
            Primary::Root(root) => root.compatible_strings(out),
            Primary::ReferencedNode(node) => node.payload.compatible_strings(out),
        }
    }
}

impl Node {
    pub fn compatible_strings(&self, out: &mut Vec<(String, Span)>) {
        self.payload.compatible_strings(out);
    }
}

impl NodePayload {
    pub fn compatible_strings(&self, out: &mut Vec<(String, Span)>) {
        for node in &self.items {
            node.compatible_strings(out);
        }
    }
}

impl NodeItem {
    pub fn compatible_strings(&self, out: &mut Vec<(String, Span)>) {
        match self {
            NodeItem::Property(property) => property.compatible_strings(out),
            NodeItem::Node(node) => node.compatible_strings(out),
            NodeItem::DeletedNode(..) | NodeItem::DeletedProperty(..) => {}
        }
    }
}

impl Property {
    pub fn compatible_strings(&self, out: &mut Vec<(String, Span)>) {
        if self.name.item() != "compatible" {
            return;
        }
        for value in &self.values {
            if let PropertyValue::String(string) = value {
                if !string.item().is_empty() {
                    out.push((string.item().clone(), string.span()));
                }
            }
        }
    }
}
