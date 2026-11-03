/// Module for analyzing Device-Tree Source files
mod analysis;
mod ast;
mod bindings;
mod data;
mod diagnostics;
mod error_codes;
mod import_guard;
mod loader;
mod parser;
mod project;
mod reader;
mod resolved;
#[cfg(test)]
mod test;
mod tokens;
mod validate;
mod visitor;

pub use ast::{AnyDirective, Node, NodeItem, NodePayload, Primary, PropertyValue};
pub use bindings::{
    Bindings, CompletionSchema, PropertyHint, fuzzy_compatibles, merge_hint, node_compatibles,
};
pub use validate::{
    binding_diagnostics, missing_requirements, MissingRequirement, RequirementKind,
};
pub use data::{FileType, HasSource, HasSpan, Position, Span};
pub use diagnostics::{Diagnostic, DiagnosticPrinter, Severity};
pub use error_codes::{ErrorCode, SeverityMap};
pub use loader::{
    DefaultIncludeLoader, IncludeLoader, IncludeLoaderGuard, IncludeLoaderNotifyAction,
};
pub use parser::Parser;
pub use parser::ParserConfig;
pub use project::{ExternalAnalysis, ExternalAnalysisContext, ExternalDiagnostic, Project};
pub use reader::{ByteReader, Reader};
pub use resolved::{CompletionContext, ResolvedContext, ResolvedNode, ResolvedProperty};
pub use visitor::ItemAtCursor;
