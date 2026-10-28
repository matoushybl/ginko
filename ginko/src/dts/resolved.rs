use crate::dts::ast::{AbsolutePath, NodeName, Property};
use crate::dts::{HasSpan, Node, Position, Span};
use std::collections::btree_map::Entry::{Occupied, Vacant};
use std::collections::BTreeMap;
use std::fmt::Display;
use std::path::Path;
use std::sync::Arc;

pub struct Location {
    deleted: bool,
    file: Arc<Path>,
    span: Span,
}

impl Location {
    fn new(source: Arc<Path>, span: Span) -> Self {
        Location {
            deleted: false,
            file: source.clone(),
            span,
        }
    }

    pub fn file(&self) -> &Path {
        &self.file
    }

    pub fn span(&self) -> Span {
        self.span
    }
}

pub enum ResolvedContext<'a> {
    Node(&'a ResolvedNode),
    Property(&'a ResolvedProperty),
}

/// What a completion at a position is inside. Unlike [`ResolvedContext`],
/// the enclosing node is found via span geometry rather than a `&ref` under
/// the cursor, so completion works while typing inside node bodies.
pub enum CompletionContext<'a> {
    InNode(&'a ResolvedNode),
    /// Cursor inside an existing property definition — property names make
    /// no sense there; value completion is a later phase.
    InProperty(&'a ResolvedNode, &'a ResolvedProperty),
    /// Cursor outside any node — e.g. between top-level elements.
    TopLevel,
}

pub struct ResolvedProperty {
    deleted: bool,
    inner: Arc<Property>,
    sources: Vec<Location>,
}

#[derive(Default)]
pub struct ResolvedNode {
    deleted: bool,
    properties: BTreeMap<String, ResolvedProperty>,
    children: BTreeMap<NodeName, ResolvedNode>,
    sources: Vec<Location>,
}

impl Display for ResolvedNode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(f, "{{")?;
        for rprop in self.properties.values() {
            if !rprop.deleted {
                writeln!(f, "    {}", rprop.inner)?;
            }
        }
        for (name, child_rnode) in &self.children {
            if !child_rnode.deleted {
                writeln!(f, "    {name} = {{ ... }};")?;
            }
        }
        writeln!(f, "}}")
    }
}

impl ResolvedNode {
    pub fn active_child_nodes<'a>(&'a self) -> impl Iterator<Item = String> + use<'a> {
        self.children.iter().filter_map(|(name, child)| {
            if child.deleted {
                None
            } else {
                Some(name.to_string())
            }
        })
    }

    pub fn get_property(&self, name: &str) -> Option<&ResolvedProperty> {
        self.properties.get(name)
    }

    pub fn active_child_properties<'a>(
        &'a self,
    ) -> impl Iterator<Item = (String, Arc<Property>)> + use<'a> {
        self.properties.iter().filter_map(|(name, child)| {
            if child.deleted {
                None
            } else {
                Some((name.to_string(), child.inner.clone()))
            }
        })
    }

    pub fn sources<'a>(&'a self) -> impl Iterator<Item = &'a Location> + use<'a> {
        self.sources.iter().filter(|location| !location.deleted)
    }

    /// The property of this node whose definition in `file` spans `position`.
    pub fn property_at_position(
        &self,
        file: &Path,
        position: &Position,
    ) -> Option<&ResolvedProperty> {
        self.properties
            .values()
            .filter(|property| !property.deleted)
            .find(|property| {
                property.sources.iter().any(|location| {
                    !location.deleted
                        && location.file() == file
                        && location.span().contains(position)
                })
            })
    }
}

impl ResolvedProperty {
    /// The merged property definition this resolution is based on.
    pub fn property(&self) -> &Property {
        &self.inner
    }
}

#[derive(Default)]
pub struct ResolvedTree {
    root: ResolvedNode,
}

impl ResolvedTree {
    /// The deepest node whose definition in `file` spans `position` — the node
    /// a cursor at `position` is being written into. Node sources carry one
    /// span per defining file (base and overlay definitions), so this works
    /// for both. Children are tried first: a parent's span contains its
    /// children's.
    pub fn node_at_position(
        &self,
        file: &Path,
        position: &Position,
    ) -> Option<&ResolvedNode> {
        fn walk<'a>(
            node: &'a ResolvedNode,
            file: &Path,
            position: &Position,
        ) -> Option<&'a ResolvedNode> {
            if node.deleted {
                return None;
            }
            for child in node.children.values() {
                if let Some(found) = walk(child, file, position) {
                    return Some(found);
                }
            }
            node.sources.iter().any(|location| {
                !location.deleted
                    && location.file() == file
                    && location.span().contains(position)
            })
            .then_some(node)
        }
        walk(&self.root, file, position)
    }

    pub(crate) fn add_node(&mut self, path: &AbsolutePath, node: Arc<Node>, source: Arc<Path>) {
        let mut rnode = &mut self.root;
        for element in path.iter() {
            rnode = match rnode.children.entry(element.clone()) {
                Occupied(entry) => entry.into_mut(),
                Vacant(entry) => {
                    let child = ResolvedNode::default();
                    entry.insert(child)
                }
            }
        }
        rnode.sources.push(Location::new(source, node.span));
        rnode.deleted = false;
    }

    /// Register a node definition's source span without (re-)adding node
    /// content — used for `&ref { … }` overlay elements, whose payload
    /// merges into the referenced node: without this, the overlay file stays
    /// out of the node's sources and span-based lookups inside the overlay
    /// body find no enclosing node.
    pub(crate) fn add_node_span(
        &mut self,
        path: &AbsolutePath,
        span: Span,
        source: Arc<Path>,
    ) {
        let mut rnode = &mut self.root;
        for element in path.iter() {
            rnode = match rnode.children.entry(element.clone()) {
                Occupied(entry) => entry.into_mut(),
                Vacant(entry) => entry.insert(ResolvedNode::default()),
            };
        }
        rnode.sources.push(Location::new(source, span));
        rnode.deleted = false;
    }

    pub(crate) fn delete_node(&mut self, parent_path: &AbsolutePath, node_name: &NodeName) {
        let mut rparent = &mut self.root;
        for element in parent_path.iter() {
            rparent = match rparent.children.entry(element.clone()) {
                Occupied(entry) => entry.into_mut(),
                Vacant(entry) => {
                    let child = ResolvedNode::default();
                    entry.insert(child)
                }
            }
        }
        if let Some(rnode) = rparent.children.get_mut(node_name) {
            for source in rnode.sources.iter_mut() {
                source.deleted = true;
            }
            rnode.deleted = true;
        }
    }

    pub(crate) fn add_property(
        &mut self,
        path: &AbsolutePath,
        key: &str,
        value: Arc<Property>,
        source: Arc<Path>,
    ) {
        let mut rnode = &mut self.root;
        for element in path.iter() {
            rnode = rnode.children.get_mut(element).expect("proper tree");
        }
        let span = value.span();
        let rprop = ResolvedProperty {
            deleted: false,
            inner: value,
            sources: vec![Location::new(source, span)],
        };
        rnode.properties.insert(key.to_string(), rprop);
    }

    pub(crate) fn delete_property(&mut self, parent_path: &AbsolutePath, name: &str) {
        let mut rparent = &mut self.root;
        for element in parent_path.iter() {
            rparent = match rparent.children.entry(element.clone()) {
                Occupied(entry) => entry.into_mut(),
                Vacant(entry) => {
                    let child = ResolvedNode::default();
                    entry.insert(child)
                }
            }
        }
        if let Some(rprop) = rparent.properties.get_mut(name) {
            for source in rprop.sources.iter_mut() {
                source.deleted = true;
            }
            rprop.deleted = true;
        }
    }

    pub(crate) fn node_at<'a>(&'a self, path: &AbsolutePath) -> Option<&'a ResolvedNode> {
        let mut rnode = &self.root;
        for element in path.iter() {
            rnode = match rnode.children.get(element) {
                Some(child) => child,
                None => {
                    return None;
                }
            }
        }
        Some(rnode)
    }
}
