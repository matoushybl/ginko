use crate::dts::analysis::{Analysis, AnalysisContext, PendingReferenceList};
use crate::dts::ast::{AbsolutePath, DtsFile, Include, Path as DtsPath, Reference, ABSOLUTE_ROOT};
use crate::dts::data::HasSource;
use crate::dts::error_codes::{ErrorCode, SeverityMap};
use crate::dts::loader::IncludeLoaderGuard;
use crate::dts::reader::ByteReader;
use crate::dts::resolved::{CompletionContext, ResolvedContext, ResolvedNode};
use crate::dts::tokens::Lexer;
use crate::dts::visitor::{ItemAtCursor, ReferenceContext};
use crate::dts::{Diagnostic, FileType, HasSpan, Parser, ParserConfig, Position, Severity, Span};
use std::collections::{BTreeSet, HashMap};
use std::iter::empty;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::{fs, io};

enum AnalysisStatus {
    NotAnalyzed,
    Root(Box<AnalysisContext>),
    PendingReferences(PendingReferenceList),
    ChildAnalysisDone,
}

pub struct ProjectFile {
    pub(crate) parent: Option<PathBuf>,
    pub(crate) includes: Vec<PathBuf>,
    pub(crate) parser_diagnostics: Vec<Diagnostic>,
    pub(crate) analysis_diagnostics: Vec<Diagnostic>,
    pub(crate) file: Option<DtsFile>,
    pub(crate) file_type: FileType,
    pub(crate) source: String,
    analysis_status: AnalysisStatus,
}

impl ProjectFile {
    pub fn parsed(
        parent: Option<PathBuf>,
        includes: Vec<PathBuf>,
        diagnostics: Vec<Diagnostic>,
        file_type: FileType,
        file: DtsFile,
        source: String,
    ) -> ProjectFile {
        ProjectFile {
            parent,
            includes,
            parser_diagnostics: diagnostics,
            file: Some(file),
            file_type,
            source,
            analysis_diagnostics: vec![],
            analysis_status: AnalysisStatus::NotAnalyzed,
        }
    }

    pub fn sentinel() -> ProjectFile {
        ProjectFile {
            parent: None,
            includes: vec![],
            parser_diagnostics: vec![],
            analysis_diagnostics: vec![],
            file: None,
            source: "".to_string(),
            file_type: FileType::Unknown,
            analysis_status: AnalysisStatus::NotAnalyzed,
        }
    }

    pub fn unrecoverable(err: Diagnostic, source: String, file_type: FileType) -> ProjectFile {
        ProjectFile {
            parent: None,
            includes: vec![],
            parser_diagnostics: vec![err],
            analysis_diagnostics: vec![],
            file: None,
            source,
            file_type,
            analysis_status: AnalysisStatus::NotAnalyzed,
        }
    }

    pub fn diagnostics(&self) -> impl Iterator<Item = &Diagnostic> {
        self.parser_diagnostics
            .iter()
            .chain(&self.analysis_diagnostics)
    }

    pub fn clear_diagnostics(&mut self) {
        self.parser_diagnostics.clear();
        self.analysis_diagnostics.clear();
    }

    pub fn has_errors(&self, severity_map: &SeverityMap) -> bool {
        self.diagnostics()
            .any(|diagnostic| diagnostic.severity(severity_map) == Severity::Error)
    }

    pub fn source(&self) -> &String {
        &self.source
    }

    pub fn reference_at(&self, position: &Position) -> Option<Reference> {
        let file = self.file.as_ref()?;
        for element in &file.elements {
            match element {
                super::Primary::Root(node) if node.span.contains(position) => {
                    return node.reference_at(&DtsPath::Absolute(ABSOLUTE_ROOT.clone()), position);
                }
                super::Primary::ReferencedNode(node) if node.span.contains(position) => {
                    return node.reference_at(position)
                }
                _ => {}
            }
        }
        None
    }
}

pub struct ExternalDiagnostic {
    pub path: String,
    pub message: String,
}

pub struct ExternalAnalysisFileReader<'a> {
    project: &'a Project,
}

impl ExternalAnalysisFileReader<'_> {
    pub fn read(&self, path: &Path) -> Option<(&Path, &str)> {
        let project_file = self.project.files.get(path)?;
        Some((&project_file.file.as_ref()?.source, &project_file.source))
    }
}

pub struct ExternalAnalysisContext<'a> {
    pub files: ExternalAnalysisFileReader<'a>,
    pub root_file_path: &'a Path,
    pub path: &'a Path,
    pub text: &'a str,
}

pub struct ExternalAnalysis {
    pub errors: Vec<ExternalDiagnostic>,
    pub warnings: Vec<ExternalDiagnostic>,
}

#[derive(Default)]
pub struct Project {
    files: HashMap<PathBuf, ProjectFile>,
    root_file: Option<PathBuf>,
    pub severities: SeverityMap,
    config: ParserConfig,
    canonicalizer: Option<fn(&Path) -> io::Result<PathBuf>>,
}

impl Project {
    pub fn set_parser_config(&mut self, config: ParserConfig) {
        self.config = config;
    }

    pub fn set_canonicalizer(&mut self, f: fn(&Path) -> io::Result<PathBuf>) {
        self.canonicalizer = Some(f);
    }

    fn canonicalize<P: AsRef<Path>>(&self, path: P) -> io::Result<PathBuf> {
        if let Some(f) = self.canonicalizer {
            f(path.as_ref())
        } else {
            // A file not (yet) on disk — e.g. a fresh unsaved editor buffer —
            // cannot be canonicalized, but its spelled path is a stable
            // identity: fall back to it so the whole add/lookup funnel keeps
            // working for new files instead of erroring or panicking.
            self::fs::canonicalize(path.as_ref())
                .or_else(|_| Ok(path.as_ref().to_path_buf()))
        }
    }

    pub fn set_root_file_and_reset(
        &mut self,
        file_name: String,
        loader: &mut IncludeLoaderGuard,
    ) -> Result<(), io::Error> {
        let file_name = self.canonicalize(file_name)?;
        self.root_file = Some(file_name);
        self.reset(loader)
    }

    pub fn reset(&mut self, loader: &mut IncludeLoaderGuard) -> Result<(), io::Error> {
        let Some(root_file) = self.root_file.clone() else {
            return Ok(());
        };
        self.remove_tree(root_file.as_path());
        self.add_path_buf(root_file.clone(), loader)?;
        Ok(())
    }

    fn remove_tree(&mut self, root_file: &Path) {
        let mut files_to_remove = BTreeSet::<_>::new();

        // Tree traversal stack
        let mut stack = vec![root_file];

        while let Some(file) = stack.pop() {
            files_to_remove.insert(file.to_path_buf());
            if let Some(project_file) = self.files.get(file) {
                for include in &project_file.includes {
                    stack.push(include);
                }
            }
        }

        for file in files_to_remove {
            self.files.remove(&file);
        }
    }

    pub fn add_file(
        &mut self,
        file_name: String,
        loader: &mut IncludeLoaderGuard,
    ) -> Result<(), io::Error> {
        let file_name = self.canonicalize(file_name)?;
        self.add_path_buf(file_name, loader)
    }

    fn add_path_buf(
        &mut self,
        file_name: PathBuf,
        loader: &mut IncludeLoaderGuard,
    ) -> Result<(), io::Error> {
        let content = fs::read_to_string(&file_name)?;
        let file_ending = FileType::from(file_name.as_path());
        self.add_file_with_text(file_name, content, file_ending, loader);
        Ok(())
    }

    /// Adds a file to the project with already given text.
    /// Re-evaluates the file, if the file is already present.
    /// Does not re-evaluate dependencies, if they are cached.
    ///
    /// # Parameters
    /// * file_name: The name of the file.
    /// * text: The contents of the file.
    /// * file_type: Defines how the file should be analyzed.
    /// * loader: The loader engine for resolving `/include/` paths.
    ///
    /// # Panics
    /// If `file_name` does not point to a valid file.
    pub fn add_file_with_text(
        &mut self,
        file_name: PathBuf,
        text: String,
        file_type: FileType,
        loader: &mut IncludeLoaderGuard,
    ) {
        let file_name = self.canonicalize(file_name).expect("File must be present");

        // First step: Parse file and all dependencies.
        // Dependencies are cached.
        //
        // Here, `parent` is set to None. That means that, for the root file,
        // the resulting ProjectFile no `parent`; for any "child" files, they will
        // reattach to their previous position in the tree, or, in the case of
        // a new full scan from the root, their will be assigned their correct
        // `parent` values.
        self.parse_file(&file_name, &text, file_type, None, loader);
        self.analyze_tree_for(&file_name, loader);
        self.do_external_analysis(&file_name, &text, loader);
    }

    fn get_external_analysis_span(&self, file: &Path, path: &str) -> Option<(Span, Arc<Path>)> {
        self.get_node_position(
            file,
            &Reference::PropertyPath(path.into()),
            &ReferenceContext::Root,
        )
        .or_else(|| {
            self.get_node_position(file, &Reference::Path(path.into()), &ReferenceContext::Root)
        })
    }

    fn do_external_analysis(
        &mut self,
        path: &Path,
        text: &str,
        loader: &IncludeLoaderGuard,
    ) -> Option<()> {
        let (root_file_path, _) = self.analysis_root_for(path)?;
        let eactx = ExternalAnalysisContext {
            files: ExternalAnalysisFileReader { project: self },
            root_file_path: &root_file_path,
            path,
            text,
        };

        let external_analysis = loader.external_analysis(&eactx)?;

        let mut diagnostics = vec![];

        for (list, kind) in [
            (external_analysis.errors, ErrorCode::CustomError),
            (external_analysis.warnings, ErrorCode::CustomWarning),
        ] {
            for e in list {
                if let Some((span, source)) = self.get_external_analysis_span(path, &e.path) {
                    diagnostics.push(Diagnostic::new(span, source, kind, e.message));
                }
            }
        }

        let proj_file = self.files.get_mut(path)?;
        proj_file.analysis_diagnostics.append(&mut diagnostics);

        Some(())
    }

    fn recursive_analysis(
        &mut self,
        analysis: &mut Analysis,
        file: &Path,
        parent: Option<&Path>,
        seen: &mut BTreeSet<PathBuf>,
    ) -> Option<()> {
        if seen.contains(file) {
            return None;
        }
        seen.insert(file.to_path_buf());

        for include in self.files.get(file)?.includes.clone() {
            self.recursive_analysis(analysis, &include, Some(file), seen);
        }

        let proj_file = self.files.get(file)?;
        if let Some(dts_file) = &proj_file.file {
            let result = analysis.analyze_file(dts_file, proj_file.file_type, self);
            let proj_file = self.files.get_mut(file)?;
            proj_file.analysis_status = if let Some(pending_references) = result.pending_references
            {
                AnalysisStatus::PendingReferences(pending_references)
            } else {
                AnalysisStatus::ChildAnalysisDone
            };
            proj_file.parent = parent.map(Path::to_path_buf);
            proj_file.analysis_diagnostics = result.diagnostics;
        }
        Some(())
    }

    /// Find the analysis root for a given file. It will either return the file itself,
    /// or the root of the include-tree it is a part of.
    fn analysis_root_for(&self, file: &Path) -> Option<(PathBuf, &AnalysisStatus)> {
        let canonical = self.canonicalize(file).ok()?;
        Some({
            let mut current = &canonical;
            loop {
                let proj_file = self.files.get(current)?;
                if let Some(parent) = &proj_file.parent {
                    current = parent;
                } else {
                    break (current.to_path_buf(), &proj_file.analysis_status);
                }
            }
        })
    }

    pub fn analyze_tree_for(&mut self, element: &Path, loader: &mut IncludeLoaderGuard) {
        let (root, include_cache) = {
            let Some((root, root_status)) = self.analysis_root_for(element) else {
                return;
            };
            let include_cache = match root_status {
                AnalysisStatus::Root(context) => context.get_includes().clone(),
                _ => Default::default(),
            };
            (root, include_cache)
        };

        let mut analysis = Analysis::new(Some(include_cache), loader);
        let mut seen = BTreeSet::new();

        self.recursive_analysis(&mut analysis, &root, None, &mut seen);

        for proj_file in self.files.values_mut() {
            if let AnalysisStatus::PendingReferences(pending_references) =
                &proj_file.analysis_status
            {
                let unresolved = analysis.resolve_references(pending_references);
                proj_file
                    .analysis_diagnostics
                    .extend_from_slice(&unresolved);
                proj_file.analysis_status = AnalysisStatus::ChildAnalysisDone;
            }
        }

        let Some(proj_file) = self.files.get_mut(&root) else {
            return;
        };
        proj_file.analysis_status = AnalysisStatus::Root(Box::new(analysis.into_context()))
    }

    pub fn get_diagnostics(&self, path: &Path) -> Box<dyn Iterator<Item = &Diagnostic> + '_> {
        let Some(file) = self.get_file(path) else {
            return Box::new(empty());
        };
        Box::new(file.diagnostics())
    }

    pub fn all_diagnostics(&self) -> impl Iterator<Item = &Diagnostic> {
        self.files.values().flat_map(|file| file.diagnostics())
    }

    pub fn files(&self) -> impl Iterator<Item = &Path> {
        self.files.keys().map(|key| key.as_path())
    }

    pub fn project_files(&self) -> impl Iterator<Item = &ProjectFile> {
        self.files.values()
    }

    pub fn get_analysis(&self, path: &Path) -> Option<&AnalysisContext> {
        let (_, analysis_status) = self.analysis_root_for(path)?;
        match analysis_status {
            AnalysisStatus::Root(context) => Some(context),
            _ => None,
        }
    }

    #[cfg(test)]
    pub fn assert_no_diagnostics(&self) {
        use itertools::Itertools;
        let diagnostics = self
            .files
            .values()
            .flat_map(|file| file.diagnostics())
            .collect_vec();
        if diagnostics.is_empty() {
            return;
        }
        for diag in diagnostics {
            println!("{diag:?}");
        }
        panic!("Found diagnostics")
    }

    pub fn find_at_pos<'a>(&'a self, path: &Path, position: &Position) -> Option<ItemAtCursor<'a>> {
        let file = self.get_file(path).and_then(|file| file.file.as_ref())?;
        file.item_at_cursor(position)
    }

    fn node_at_absolute_path<'a>(
        &'a self,
        ctx: &'a AnalysisContext,
        path: &AbsolutePath,
    ) -> Option<&'a ResolvedNode> {
        let resolved = ctx.get_resolved_tree();
        resolved.node_at(path)
    }

    fn node_at<'a>(
        &'a self,
        ctx: &'a AnalysisContext,
        dts_path: &DtsPath,
    ) -> Option<&'a ResolvedNode> {
        match dts_path {
            DtsPath::Absolute(abs_path) => self.node_at_absolute_path(ctx, abs_path),
            DtsPath::LabelRelative(lr_path) => {
                let abs_base = ctx.get_absolute_path_by_label(&lr_path.label)?;
                let abs_path = abs_base.with_children(lr_path.elements());
                self.node_at_absolute_path(ctx, &abs_path)
            }
            DtsPath::DotRelative(_dr_path) => {
                // TODO
                None
            }
        }
    }

    pub fn context_at<'a>(
        &'a self,
        path: &Path,
        position: &Position,
    ) -> Option<ResolvedContext<'a>> {
        let project_file = self.get_file(path)?;
        let reference = project_file.reference_at(position)?;

        let ctx = self.get_analysis(path)?;

        match reference {
            // FIXME what if a label resolves to a property
            Reference::Label(label) => self
                .node_at_absolute_path(ctx, &ctx.get_absolute_path_by_label(&label)?)
                .map(ResolvedContext::Node),
            Reference::Path(dts_path) => self.node_at(ctx, &dts_path).map(ResolvedContext::Node),
            Reference::PropertyPath(ppath) => {
                self.node_at(ctx, ppath.node_path()).and_then(|rnode| {
                    rnode
                        .get_property(ppath.property_name())
                        .map(ResolvedContext::Property)
                })
            }
        }
    }

    /// What a completion at `position` is inside: the enclosing merged node or
    /// the property the position lies in. Unlike `context_at`, this does not
    /// need a `&ref` under the cursor — it resolves the enclosing node via the
    /// span geometry of the merged tree, so completion works while typing
    /// inside node bodies. `None` when the file is not known to the project;
    /// `TopLevel` when the position is outside any node.
    pub fn completion_context_at(
        &self,
        path: &Path,
        position: &Position,
    ) -> Option<CompletionContext<'_>> {
        let file = self.canonicalize(path).ok()?;
        let analysis = self.get_analysis(path)?;
        let tree = analysis.get_resolved_tree();
        match tree.node_at_position(&file, position) {
            Some(node) => match node.property_at_position(&file, position) {
                Some(property) => Some(CompletionContext::InProperty(node, property)),
                None => Some(CompletionContext::InNode(node)),
            },
            None => Some(CompletionContext::TopLevel),
        }
    }

    pub fn token_at(&self, path: &Path, position: &Position) -> Option<String> {
        let file = self.get_file(path)?;
        let line = file.source.lines().nth(position.line() as usize)?;
        let mut token = String::new();

        // Nothing typed before the cursor on this line: the token is empty.
        // Also guards the `character - 1` below, which would underflow at 0.
        if position.character() == 0 {
            return Some(token);
        }

        for (i, c) in line.chars().enumerate() {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '@' || c == '?' || c == '&'
            {
                token.push(c);
            } else {
                token.clear();
            }
            if i == position.character() as usize - 1 {
                return Some(token);
            }
        }
        None
    }

    pub fn document_reference(
        &self,
        path: &Path,
        reference: &Reference,
        ctx: &ReferenceContext<'_>,
    ) -> Option<String> {
        let name = self.get_analysis(path)?.get_referred(reference, ctx)?;
        Some(name)
    }

    pub fn get_node_position(
        &self,
        path: &Path,
        reference: &Reference,
        ctx: &ReferenceContext<'_>,
    ) -> Option<(Span, Arc<Path>)> {
        self.get_analysis(path)?.get_position(reference, ctx)
    }

    pub fn get_root(&self, path: &Path) -> Option<&DtsFile> {
        match self.get_file(path) {
            Some(ProjectFile {
                file: Some(file), ..
            }) => Some(file),
            _ => None,
        }
    }

    fn parse_file(
        &mut self,
        file_name: &Path,
        text: &str,
        file_type: FileType,
        parent: Option<PathBuf>,
        loader: &mut IncludeLoaderGuard,
    ) {
        let reader = ByteReader::from_string(text.to_string());
        let lexer = Lexer::new(reader, file_name.into());

        let parent = if parent.is_some() {
            parent
        } else if let Some(old) = self.files.get(file_name) {
            old.parent.clone()
        } else {
            None
        };

        let mut parser = Parser::new(lexer, self.config.clone());

        match parser.file() {
            Ok(file) => {
                // insert sentinel so that no cyclic dependency can occur.
                self.files
                    .insert(file_name.to_path_buf(), ProjectFile::sentinel());

                // Recurse into includes.
                let includes: Vec<PathBuf> = file
                    .elements
                    .iter()
                    .filter_map(|primary| primary.as_include())
                    .filter_map(|include| {
                        self.parse_included_file(
                            &mut parser.diagnostics,
                            &file.source,
                            include,
                            loader,
                        )
                    })
                    .collect();

                // Insert the file proper, with the `parent` and `includes` forming a tree.
                self.files.insert(
                    file_name.to_path_buf(),
                    ProjectFile::parsed(
                        parent,
                        includes,
                        parser.diagnostics,
                        file_type,
                        file,
                        text.to_string(),
                    ),
                );
            }
            Err(err) => {
                self.files.insert(
                    file_name.to_path_buf(),
                    ProjectFile::unrecoverable(err, text.to_string(), file_type),
                );
            }
        };
    }

    fn parse_included_file(
        &mut self,
        diagnostics: &mut Vec<Diagnostic>,
        parent: &Path,
        include: &Include,
        loader: &mut IncludeLoaderGuard,
    ) -> Option<PathBuf> {
        let canonicalized_path = self
            .canonicalize(
                loader
                    .load(parent, &include.file_name(), include.kind())
                    .ok()?,
            )
            .ok()?;

        // parse only dts-like files
        // C headers parsed by follow_header
        if !include.follow_as_dts(&canonicalized_path) {
            return None;
        }

        // Avoids duplicate insertion and cyclic dependencies
        if !self.files.contains_key(&canonicalized_path) {
            match fs::read_to_string(&canonicalized_path) {
                Ok(text) => {
                    let typ = FileType::from(canonicalized_path.as_path());
                    self.parse_file(
                        &canonicalized_path,
                        &text,
                        typ,
                        Some(parent.to_path_buf()),
                        loader,
                    );
                }
                Err(err) => {
                    diagnostics.push(Diagnostic::io_error(include.span(), include.source(), err))
                }
            }
        }
        Some(canonicalized_path)
    }

    pub fn get_file(&self, path: &Path) -> Option<&ProjectFile> {
        match self.canonicalize(path) {
            Ok(path) => self.files.get(&path),
            Err(_) => None,
        }
    }

    /// The current text of a project file — the last didOpen/didChange
    /// content as ingested, not re-read from disk.
    pub fn file_source(&self, path: &Path) -> Option<&str> {
        self.get_file(path).map(|file| file.source.as_str())
    }

    /// The project-internal identity of `path`: canonicalized, or the
    /// spelled path when the file does not exist on disk (new buffers).
    pub fn canonical_path(&self, path: &Path) -> PathBuf {
        self.canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
    }
}

#[cfg(test)]
// For some reason, this fails under windows with error "The system cannot find the file specified. (os error 2)"
#[cfg(not(windows))]
mod tests {
    use crate::dts::ast::PropertyPath;
    use crate::dts::error_codes::ErrorCode;
    use crate::dts::loader::IncludeLoaderGuard;
    use crate::dts::test::Code;
    use crate::dts::tokens::TokenKind;
    use crate::dts::visitor::ReferenceContext;
    use crate::dts::{ast, Diagnostic, HasSpan, ItemAtCursor, Project};
    use assert_matches::assert_matches;
    use itertools::Itertools;
    use std::fs;
    use std::fs::File;
    use std::io::Write;
    use std::path::{Path, PathBuf};
    use tempfile::tempdir;

    struct TempDir {
        pub inner: tempfile::TempDir,
    }

    impl TempDir {
        pub fn new() -> TempDir {
            let dir = tempdir().expect("Cannot create temporary directory");
            TempDir { inner: dir }
        }

        pub fn add_file(
            &self,
            name: impl AsRef<Path>,
            content: impl AsRef<str>,
        ) -> (Code, PathBuf) {
            let code = Code::new(content.as_ref());
            let file_path = self.inner.path().join(name);

            fs::write(&file_path, code.code()).expect("Cannot write to file");
            (code, file_path)
        }

        pub fn new_file(&self, name: impl AsRef<Path>) -> (File, PathBuf) {
            let file_path = self.inner.path().join(name);
            (
                File::create(&file_path).expect("Cannot create temporary file"),
                file_path,
            )
        }
    }

    #[test]
    pub fn file_with_includes() {
        let mut project = Project::default();
        let mut loader = IncludeLoaderGuard::default();
        let temp_dir = TempDir::new();
        let (_, path1) = temp_dir.add_file("tests-include.dtsi", "");
        project
            .add_file(
                path1.clone().into_os_string().into_string().unwrap(),
                &mut loader,
            )
            .expect("Cannot add file");

        let (_, file2) = temp_dir.add_file(
            "tests-file.dts",
            format!(
                r#"
/dts-v1/;

/include/ "{}"
"#,
                path1.display()
            ),
        );
        project
            .add_file(
                file2.clone().into_os_string().into_string().unwrap(),
                &mut loader,
            )
            .expect("Cannot add file");
        assert!(project.get_file(&path1).is_some());
        assert!(project.get_file(&file2).is_some());
        assert_eq!(project.get_diagnostics(&path1).next(), None);
        assert_eq!(project.get_diagnostics(&file2).next(), None);
        assert!(project.get_root(&path1).is_some());
        assert!(project.get_analysis(&path1).is_some());
        assert!(project.get_analysis(&file2).is_some());
    }

    #[test]
    pub fn cross_file_references() {
        let mut project = Project::default();
        let mut loader = IncludeLoaderGuard::default();
        let temp_dir = TempDir::new();
        let (code1, file1) = temp_dir.add_file(
            "tests-include.dtsi",
            r#"
/ {
    some_node: node_a {
        // ...
    };
};
"#,
        );
        let (code2, file2) = temp_dir.add_file(
            "tests-file.dts",
            format!(
                r#"
/dts-v1/;

/include/ "{}"

&some_node {{
}};

&{{/node_a}} {{
}};
"#,
                file1.display()
            ),
        );

        project
            .add_file(
                file2.clone().into_os_string().into_string().unwrap(),
                &mut loader,
            )
            .expect("Unexpected IO error");

        project.assert_no_diagnostics();

        let substr = code2.s1("&some_node");
        let item = project
            .find_at_pos(&file2, &substr.span().start())
            .expect("Found no item");
        let ItemAtCursor::Reference(reference, _) = item else {
            panic!("Found non-node at cursor")
        };
        assert_eq!(reference, &ast::Reference::Label("some_node".to_owned()));
        match project.get_node_position(&file2, reference, &ReferenceContext::Root) {
            Some((span, path)) => {
                assert_eq!(span, code1.s1("node_a").span());
                assert_eq!(
                    path.to_path_buf(),
                    project.canonicalize(&file1).expect("File does not exist")
                );
            }
            None => panic!("References does not reference nodes"),
        }

        let substr = code2.s1("&{/node_a}");
        let item = project
            .find_at_pos(&file2, &substr.span().start())
            .expect("Found no item");
        let ItemAtCursor::Reference(reference, _) = item else {
            panic!("Found non-node at cursor")
        };
        assert_eq!(reference, &ast::Reference::Path("/node_a".into()));
        match project.get_node_position(&file2, reference, &ReferenceContext::Root) {
            Some((span, path)) => {
                assert_eq!(span, code1.s1("node_a").span());
                assert_eq!(
                    path.to_path_buf(),
                    project.canonicalize(&file1).expect("File does not exist")
                );
            }
            None => panic!("References does not reference nodes"),
        }
    }

    #[test]
    pub fn cyclic_import_error() {
        let temp_dir = TempDir::new();
        let (mut file1, path1) = temp_dir.new_file("test.dts");
        let (mut file2, path2) = temp_dir.new_file("tests-include.dtsi");

        write!(file1, r#"/dts-v1/; /include/ "{}""#, path2.display())
            .expect("Cannot write to file 2");
        write!(file2, r#"/include/ "{}""#, path1.display()).expect("Cannot write to file1");

        let mut project = Project::default();
        let mut loader = IncludeLoaderGuard::default();
        project
            .add_file(path1.into_os_string().into_string().unwrap(), &mut loader)
            .expect("Cannot add file to project");

        let diag = project.all_diagnostics().cloned().collect_vec();
        // This is explicitly vague. There should be some error somewhere,
        // but the exact location is not perfect at the moment.
        // This is because the error only occurs in one file while it should occur in all
        // files affected by the cyclic include.
        assert_matches!(
            &diag[..],
            &[Diagnostic {
                kind: ErrorCode::CyclicDependencyError,
                ..
            }]
        );
    }

    #[test]
    pub fn file_with_multiple_includes() {
        let mut project = Project::default();
        let mut loader = IncludeLoaderGuard::default();

        let temp_dir = TempDir::new();
        let (_, file1) = temp_dir.add_file("test1.dtsi", "");
        project
            .add_file(
                file1.clone().into_os_string().into_string().unwrap(),
                &mut loader,
            )
            .expect("Cannot add file");
        let (_, file2) = temp_dir.add_file("test2.dtsi", "");
        project
            .add_file(
                file2.clone().into_os_string().into_string().unwrap(),
                &mut loader,
            )
            .expect("Cannot add file");
        let (_, file3) = temp_dir.add_file(
            "test.dts",
            format!(
                r#"
/dts-v1/;

/include/ "{}"
/include/ "{}"
"#,
                file1.display(),
                file2.display()
            ),
        );

        project
            .add_file(file3.into_os_string().into_string().unwrap(), &mut loader)
            .expect("Unexpected IO error");
        project.assert_no_diagnostics();
    }

    #[test]
    pub fn file_with_nested_includes() {
        let mut project = Project::default();
        let mut loader = IncludeLoaderGuard::default();

        let temp_dir = TempDir::new();
        let (_, file1) = temp_dir.add_file("tests-include1.dtsi", "");
        let (_, file2) = temp_dir.add_file(
            "tests-include2.dtsi",
            format!(r#"/include/ "{}""#, file1.display()).as_str(),
        );
        let (_, file3) = temp_dir.add_file(
            "test.dts",
            format!(
                r#"
/dts-v1/;

/include/ "{}"
"#,
                file2.display()
            ),
        );

        project
            .add_file(
                file3.clone().into_os_string().into_string().unwrap(),
                &mut loader,
            )
            .expect("Unexpected IO error");

        project.assert_no_diagnostics();

        assert!(project.get_file(&file1).is_some());
        assert!(project.get_file(&file2).is_some());
        assert!(project.get_file(&file3).is_some());
    }

    // TODO removal by trees is yet to be implemented
    /*
    #[test]
    pub fn remove_file_uses_refcount() {
        let mut project = Project::default();
        let mut loader = IncludeLoaderGuard::default();

        let temp_dir = TempDir::new();
        let (_, file1) = temp_dir.add_file("tests-include1.dtsi", "");
        let (_, file2) = temp_dir.add_file(
            "tests-include2.dtsi",
            format!(r#"/include/ "{}""#, file1.display()).as_str(),
        );
        let (_, file3) = temp_dir.add_file(
            "test3.dts",
            format!(
                r#"
    /dts-v1/;

    /include/ "{}"
    "#,
                file2.display()
            ),
        );
        let (_, file4) = temp_dir.add_file(
            "test4.dts",
            format!(
                r#"
    /dts-v1/;

    /include/ "{}"
    "#,
                file2.display()
            ),
        );

        project
            .add_file(
                file3.clone().into_os_string().into_string().unwrap(),
                &mut loader,
            )
            .expect("Unexpected IO error");

        project
            .add_file(
                file4.clone().into_os_string().into_string().unwrap(),
                &mut loader,
            )
            .expect("Unexpected IO error");

        project.assert_no_diagnostics();

        assert!(project.get_file(&file1).is_some());
        assert!(project.get_file(&file2).is_some());
        assert!(project.get_file(&file3).is_some());
        assert!(project.get_file(&file4).is_some());

        // Equivalent to opening the included files by themselves in an editor:
        // this increases their reference count.
        project
            .add_file(
                file1.clone().into_os_string().into_string().unwrap(),
                &mut loader,
            )
            .expect("Cannot add file");
        project
            .add_file(
                file2.clone().into_os_string().into_string().unwrap(),
                &mut loader,
            )
            .expect("Cannot add file");

        assert!(project.get_file(&file1).is_some());
        assert!(project.get_file(&file2).is_some());
        assert!(project.get_file(&file3).is_some());
        assert!(project.get_file(&file4).is_some());
    }
    */

    #[test]
    pub fn bad_path_in_include_and_incbin() {
        let mut project = Project::default();
        let mut loader = IncludeLoaderGuard::default();

        let temp_dir = TempDir::new();
        let (code, file) = temp_dir.add_file(
            "test.dts",
            r#"
/dts-v1/;

/include/ "/missing_file_1"

/ {
    prop = /incbin/( "/missing_file_2" );
};
"#,
        );
        project
            .add_file(
                file.clone().into_os_string().into_string().unwrap(),
                &mut loader,
            )
            .expect("Cannot add file to project");

        assert!(project.get_file(&file).is_some());
        let diagnostics = project.get_diagnostics(&file).cloned().collect_vec();
        assert_eq!(diagnostics.len(), 2);
        assert_eq!(*diagnostics[0].kind(), ErrorCode::IOError);
        assert_eq!(
            diagnostics[0].span(),
            code.s1("/include/ \"/missing_file_1\"").span()
        );
        assert_eq!(*diagnostics[1].kind(), ErrorCode::IOError);
        assert_eq!(
            diagnostics[1].span(),
            code.s1("/incbin/( \"/missing_file_2\" )").span()
        );
    }

    #[test]
    pub fn error_in_included_file_add_include_before_dts() {
        let mut project = Project::default();
        let mut loader = IncludeLoaderGuard::default();

        let temp_dir = TempDir::new();
        let (code1, file1) = temp_dir.add_file("error.dtsi", "/ {}"); // missing semicolon
        let (code2, file2) = temp_dir.add_file(
            "test.dts",
            format!(
                r#"
/dts-v1/;

/include/ "{}"
"#,
                file1.display()
            ),
        );
        project
            .add_file(
                file2.clone().into_os_string().into_string().unwrap(),
                &mut loader,
            )
            .expect("Cannot add file to project");

        assert!(project.get_file(&file1).is_some());
        assert!(project.get_file(&file2).is_some());
        assert_eq!(
            project.get_diagnostics(&file1).cloned().collect_vec(),
            vec![Diagnostic::expected(
                code1.s1("}").end().as_span(),
                project
                    .canonicalize(&file1)
                    .expect("Cannot canonicalize")
                    .into(),
                &[TokenKind::Semicolon]
            )]
        );
        assert_eq!(
            project.get_diagnostics(&file2).cloned().collect_vec(),
            vec![Diagnostic::new(
                code2
                    .s1(format!(r#"/include/ "{}""#, file1.display()).as_str())
                    .span(),
                project
                    .canonicalize(file2)
                    .expect("Cannot canonicalize")
                    .into(),
                ErrorCode::ErrorsInInclude,
                "Included file contains errors"
            )]
        );
    }

    #[test]
    pub fn file_with_include_paths_includes() {
        let includes_dir = TempDir::new();
        let (_, file1) = includes_dir.add_file("tests-include1.dtsi", "");
        let another_includes_dir = TempDir::new();
        let (_, file2) = another_includes_dir.add_file("tests-include2.dtsi", "");

        let temp_dir = TempDir::new();
        let (_, file3) = temp_dir.add_file(
            "test.dts",
            r#"
/dts-v1/;

/include/ "tests-include1.dtsi"
/include/ "tests-include2.dtsi"
"#,
        );

        let mut project = Project::default();
        let mut loader = IncludeLoaderGuard::default();

        let include_paths = vec![
            includes_dir.inner.path().display().to_string(),
            another_includes_dir.inner.path().display().to_string(),
        ];

        loader.set_include_paths(include_paths);

        project
            .add_file(
                file3.clone().into_os_string().into_string().unwrap(),
                &mut loader,
            )
            .expect("Unexpected IO error");

        project.assert_no_diagnostics();

        assert!(project.get_file(&file1).is_some());
        assert!(project.get_file(&file2).is_some());
        assert!(project.get_file(&file3).is_some());
    }

    #[test]
    pub fn cpp_include_gate() {
        let includes_dir = TempDir::new();
        let (_, angled) = includes_dir.add_file("angled.h", "#define B_LOCAL 2");
        let temp_dir = TempDir::new();
        let (_, main) = temp_dir.add_file(
            "test.dts",
            r#"
/dts-v1/;
#include "frag.dtsi"
#include "head.h"
#include <angled.h>
/ { prop = <A_LOCAL B_LOCAL C_LOCAL>; };
"#,
        );
        let (_, frag) =
            temp_dir.add_file("frag.dtsi", "#define C_LOCAL 3\n/ { frag_node: frag{ };};");
        let (_, head) = temp_dir.add_file("head.h", "#define A_LOCAL 1");

        let mut project = Project::default();
        let mut loader = IncludeLoaderGuard::default();
        loader.set_include_paths(vec![angled.parent().unwrap().display().to_string()]);

        project
            .add_file(
                main.clone().into_os_string().into_string().unwrap(),
                &mut loader,
            )
            .expect("Cannot add file");

        // quoted .dtsi fragment is followed (and its own #define is collected)
        assert!(project.get_file(&frag).is_some());
        // headers never enter the DTS parse
        assert!(project.get_file(&head).is_none());
        assert!(project.get_file(&angled).is_none());
        assert_eq!(project.files().count(), 2); // main + frag only
        project.assert_no_diagnostics();

        let analysis = project.get_analysis(&main).unwrap();
        assert!(project
            .get_analysis(&main)
            .and_then(|a| a.get_node_by_label("frag_node"))
            .is_some());
        assert!(analysis.get_define("A_LOCAL").is_some()); // quoted .h
        assert!(analysis.get_define("B_LOCAL").is_some()); // angled via include path
        assert!(analysis.get_define("C_LOCAL").is_some()); // defined in the fragment itself
    }

    #[test]
    pub fn completion_context_resolves_enclosing_node_mid_body() {
        use crate::dts::Position;
        use crate::dts::CompletionContext;
        let temp_dir = TempDir::new();
        let (_, main) = temp_dir.add_file(
            "test.dts",
            r#"/dts-v1/;
/ {
	outer {
		inner {
			prop = <1>;
		};
	};
};"#,
        );

        let mut project = Project::default();
        let mut loader = IncludeLoaderGuard::default();
        project
            .add_file(main.clone().into_os_string().into_string().unwrap(), &mut loader)
            .expect("Cannot add file");

        // top level, e.g. on the /dts-v1/ line
        let Some(CompletionContext::TopLevel) =
            project.completion_context_at(&main, &Position::new(0, 4))
        else {
            panic!("expected TopLevel on the /dts-v1/ line")
        };

        // on the `outer` node name — resolves to the outer node
        let Some(CompletionContext::InNode(rnode)) =
            project.completion_context_at(&main, &Position::new(2, 2))
        else {
            panic!("expected InNode on the outer node name")
        };
        assert!(rnode.active_child_nodes().contains(&"inner".to_string()));

        // on the property name — resolves to the property and its node
        let Some(CompletionContext::InProperty(rnode, property)) =
            project.completion_context_at(&main, &Position::new(4, 4))
        else {
            panic!("expected InProperty on the property name")
        };
        assert_eq!(property.property().name.item(), "prop");
        assert!(rnode.get_property("prop").is_some());

        // whitespace inside the inner node body, outside every property span —
        // where typing a new property actually happens
        assert!(matches!(
            project.completion_context_at(&main, &Position::new(4, 2)),
            Some(CompletionContext::InNode(_))
        ));
    }

    #[test]
    pub fn completion_context_merges_overlays_across_files() {
        use crate::dts::Position;
        use crate::dts::CompletionContext;
        let temp_dir = TempDir::new();
        let (_, base) = temp_dir.add_file(
            "base.dts",
            r#"/dts-v1/;
/ {
	base: node {
		compatible = "vendor,gpio";
	};
};"#,
        );
        let _ = base;
        let (_, overlay) = temp_dir.add_file(
            "overlay.dts",
            r#"/dts-v1/;
#include "base.dts"
&base {
	extra = <1>;
};"#,
        );

        let mut project = Project::default();
        let mut loader = IncludeLoaderGuard::default();
        project
            .add_file(overlay.clone().into_os_string().into_string().unwrap(), &mut loader)
            .expect("Cannot add file");

        // typing inside the `&base { }` body of the overlay resolves to the
        // MERGED node: the base's compatible (which powers binding lookup)
        // is visible alongside the overlay's own property
        let Some(CompletionContext::InNode(rnode)) =
            project.completion_context_at(&overlay, &Position::new(3, 0))
        else {
            panic!("expected InNode inside the overlay body")
        };
        assert!(rnode.get_property("compatible").is_some());
        assert!(rnode.get_property("extra").is_some());
    }

    #[test]
    pub fn file_with_dot_relative_references() {
        let temp_dir = TempDir::new();
        let (code, file) = temp_dir.add_file(
            "test.dts",
            r#"
/dts-v1/;

/ {
    some_label: node {
        relative = ${./another/nested/prop};
        second_label: another {
            nested {
                prop = <0>;
            };
        };
    };
};
"#,
        );

        let mut project = Project::default();
        let mut loader = IncludeLoaderGuard::default();

        project
            .add_file(
                file.clone().into_os_string().into_string().unwrap(),
                &mut loader,
            )
            .expect("Unexpected IO error");

        project.assert_no_diagnostics();

        assert!(project.get_file(&file).is_some());

        let item = project
            .find_at_pos(&file, &code.s1("nested").position())
            .unwrap();

        let ItemAtCursor::Reference(reference, ref_ctx) = item else {
            panic!("Found non-node at cursor")
        };

        let ReferenceContext::Node(_) = &ref_ctx else {
            panic!("Reference has no context")
        };

        assert_eq!(
            reference,
            &ast::Reference::PropertyPath(PropertyPath::new(
                ast::Path::new_dot_relative(vec!["another".into(), "nested".into()]),
                "prop".into()
            ))
        );
        match project.get_node_position(&file, reference, &ref_ctx) {
            Some((span, path)) => {
                assert_eq!(span, code.s1("prop = <0>;").span());
                assert_eq!(
                    path.to_path_buf(),
                    project.canonicalize(&file).expect("File does not exist")
                );
            }
            None => panic!("References does not reference nodes"),
        }
    }

    #[test]
    #[cfg(unix)]
    fn accept_non_canonical_paths() {
        use std::os::unix::fs::symlink;

        let temp_dir = TempDir::new();
        let (_, main) = temp_dir.add_file("test.dts", "/dts-v1/;\n/ { prop = <1>; };");
        let link = main.parent().unwrap().join("link.dts");
        symlink(&main, &link).expect("cannot create symlink");

        let mut project = Project::default();
        let mut loader = IncludeLoaderGuard::default();
        project
            .add_file(link.display().to_string(), &mut loader)
            .expect("Cannot add file");

        assert!(project.get_analysis(&link).is_some());
    }

    #[test]
    pub fn token_at_handles_column_zero_and_boundaries() {
        let mut project = Project::default();
        let mut loader = IncludeLoaderGuard::default();
        let text = "/dts-v1/;\n/ {\n\treg@1000 {\n\t\tcompatible = \"vendor,device\";\n\t};\n};\n";
        let temp_dir = TempDir::new();
        let (_, path) = temp_dir.add_file("test.dts", text);
        project
            .add_file(path.display().to_string(), &mut loader)
            .expect("Cannot add file");

        let compatible_line = 3usize; // `\t\tcompatible = "vendor,device";`
        let line2 = text.lines().nth(compatible_line).unwrap();
        let after_vendor = line2.find("vendor").unwrap() + "vendor".len();

        // column 0: nothing typed before the cursor —
        // used to underflow `character - 1`
        assert_eq!(
            project.token_at(&path, &crate::dts::Position::new(compatible_line as u32, 0)),
            Some(String::new())
        );
        // mid-line behavior is unchanged
        assert_eq!(
            project.token_at(
                &path,
                &crate::dts::Position::new(compatible_line as u32, after_vendor as u32)
            ),
            Some("vendor".to_string())
        );
        // cursor past the end of the line: no token, as before
        assert_eq!(
            project.token_at(
                &path,
                &crate::dts::Position::new(compatible_line as u32, 200)
            ),
            None
        );
    }

    #[test]
    pub fn blank_lines_inside_nodes_resolve_to_enclosing_context() {
        let mut project = Project::default();
        let mut loader = IncludeLoaderGuard::default();
        let text = "/dts-v1/;\n\n/ {\n\tblank_ctx {\n\t\tcompatible = \"vendor,device\";\n\n\t};\n};\n";
        let temp_dir = TempDir::new();
        let (_, path) = temp_dir.add_file("test.dts", text);
        project
            .add_file(path.display().to_string(), &mut loader)
            .expect("Cannot add file");

        // line 5 is the blank line inside the `blank_ctx` node
        let pos = crate::dts::Position::new(5, 0);
        let ctx = project
            .completion_context_at(&path, &pos)
            .expect("blank interior line must resolve to an enclosing context");
        assert!(matches!(ctx, crate::dts::CompletionContext::InNode(_)));

        let ref_ctx = project
            .context_at(&path, &pos)
            .expect("reference visitor must find the enclosing node for blank positions");
        assert!(matches!(ref_ctx, crate::dts::ResolvedContext::Node(_)));
    }

    #[test]
    pub fn overlay_redefined_property_keeps_base_span() {
        let mut project = Project::default();
        let mut loader = IncludeLoaderGuard::default();
        let temp_dir = TempDir::new();
        let base_text =
            "/dts-v1/;\n\n/ {\n\tnode_l: node {\n\t\tcompatible = \"vendor,x\";\n\t\tvalue = <1>;\n\t};\n};\n";
        let (_, base_path) = temp_dir.add_file("base.dtsi", base_text);
        let overlay_text = "/dts-v1/;\n\n&node_l {\n\tvalue = <2>;\n};\n";
        let (_, overlay_path) = temp_dir.add_file("overlay.dts", overlay_text);
        project
            .add_file(base_path.display().to_string(), &mut loader)
            .expect("Cannot add base");
        project
            .add_file(overlay_path.display().to_string(), &mut loader)
            .expect("Cannot add overlay");

        let base_lines = base_text.lines().collect_vec();
        let base_value_line = base_lines
            .iter()
            .position(|l| l.contains("value = <1>;"))
            .unwrap() as u32;
        let base_value_col = (base_lines[base_value_line as usize].find("<1").unwrap() + 1) as u32;

        // the property is redefined by the overlay, but its BASE side span
        // must survive the merge for span-based lookups
        let ctx = project
            .completion_context_at(
                &base_path,
                &crate::dts::Position::new(base_value_line, base_value_col),
            )
            .expect("base-side property span must resolve");
        assert!(matches!(
            ctx,
            crate::dts::CompletionContext::InProperty(..)
        ));

        // and the overlay side still resolves, of course
        let overlay_lines = overlay_text.lines().collect_vec();
        let overlay_value_line = overlay_lines
            .iter()
            .position(|l| l.contains("value = <2>;"))
            .unwrap() as u32;
        let ctx = project
            .completion_context_at(
                &overlay_path,
                &crate::dts::Position::new(overlay_value_line, 5),
            )
            .expect("overlay-side property span must resolve");
        assert!(matches!(
            ctx,
            crate::dts::CompletionContext::InProperty(..)
        ));
    }

    #[test]
    pub fn node_trail_and_parent_lookup_for_children() {
        let mut project = Project::default();
        let mut loader = IncludeLoaderGuard::default();
        let text = "/dts-v1/;\n/ {\n\tbase: ctrl@1000 {\n\t\tcompatible = \"vendor,ctl\";\n\t\tchild_by_name {\n\t\t\tvalue = <2>;\n\t\t};\n\t};\n};\n";
        let temp_dir = TempDir::new();
        let (_, path) = temp_dir.add_file("test.dts", text);
        project
            .add_file(path.display().to_string(), &mut loader)
            .expect("Cannot add file");

        let analysis = project.get_analysis(&path).expect("analysis");
        let tree = analysis.get_resolved_tree();
        let (node, trail) = tree
            .node_and_path_at_position(&path, &crate::dts::Position::new(5, 3))
            .expect("node at child position");
        // full names incl. unit addresses, own node name last
        assert_eq!(trail, vec!["ctrl@1000".to_string(), "child_by_name".to_string()]);
        assert!(node.get_property("value").is_some());

        let parent = tree
            .node_at_path(&trail[..trail.len() - 1])
            .expect("parent node");
        assert_eq!(
            crate::dts::node_compatibles(parent),
            vec!["vendor,ctl".to_string()]
        );
    }

    #[test]
    pub fn unsaved_new_buffer_is_added_without_panicking() {
        let mut project = Project::default();
        let mut loader = IncludeLoaderGuard::default();
        let missing = std::env::temp_dir().join(format!("ginko-never-{}.dts", std::process::id()));
        assert!(!missing.exists());

        project.add_file_with_text(
            missing.clone(),
            "/dts-v1/;\n\n/ {\n\tled_status: node3722 {\n\t\tcompatible = \"unipi,zulu\";\n\t};\n};\n"
                .to_string(),
            crate::dts::data::FileType::from(missing.as_path()),
            &mut loader,
        );

        // the spelled path is the file's identity; adding does not panic and
        // the usual lookup funnel answers for it like for any disk file
        assert!(project.get_file(&missing).is_some());
        assert!(project.get_analysis(&missing).is_some());
        let Some(crate::dts::CompletionContext::InProperty(..)) =
            project.completion_context_at(&missing, &crate::dts::Position::new(4, 8))
        else {
            panic!("expected InProperty at the compatible property");
        };
    }
}
