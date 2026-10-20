//! TypeScript in strip-only mode: the constructs that have runtime semantics
//! of their own are rejected before the transformer erases the types.
//!
//! The sandbox accepts exactly what Node's type stripping accepts (erasable
//! syntax: annotations, interfaces, type aliases, generics, `as`, `satisfies`,
//! `!`, `declare`, type-only imports) and refuses the rest with Node's own
//! messages, so a `.ts` case runs identically under both. Enums, namespaces
//! with values, parameter properties, `import =`, `export =` and the
//! angle-bracket assertion are the refused forms.

use oxc_ast::ast::{
    Declaration, FormalParameter, Program, Statement, TSEnumDeclaration, TSExportAssignment, TSImportEqualsDeclaration,
    TSNamespaceDeclaration, TSNamespaceDeclarationBody, TSNamespaceDeclarationKind, TSNamespaceExportDeclaration,
    TSTypeAssertion,
};
use oxc_ast_visit::{Visit, walk};
use oxc_span::Span;

use crate::error::CompileError;

/// Rejects the first construct in source order that type stripping cannot erase.
pub(crate) fn check_strip_only(program: &Program<'_>) -> Result<(), CompileError> {
    let mut checker = StripOnlyChecker { first: None };
    checker.visit_program(program);
    match checker.first {
        Some((span, message)) => Err(CompileError::syntax(message, (span.start, span.end))),
        None => Ok(()),
    }
}

/// Records the earliest offending node; later ones are ignored, as Node
/// reports only the first.
struct StripOnlyChecker {
    first: Option<(Span, &'static str)>,
}

impl StripOnlyChecker {
    fn reject(&mut self, span: Span, message: &'static str) {
        if self.first.is_none_or(|(first, _)| span.start < first.start) {
            self.first = Some((span, message));
        }
    }
}

impl<'a> Visit<'a> for StripOnlyChecker {
    fn visit_ts_enum_declaration(&mut self, it: &TSEnumDeclaration<'a>) {
        // an ambient (`declare`) enum has no runtime value and is erased
        if !it.declare {
            self.reject(it.span, "TypeScript enum is not supported in strip-only mode");
        }
    }

    fn visit_ts_namespace_declaration(&mut self, it: &TSNamespaceDeclaration<'a>) {
        if it.kind == TSNamespaceDeclarationKind::Module {
            self.reject(it.span, "`module` keyword is not supported. Use `namespace` instead.");
        } else if !it.declare && namespace_has_values(it) {
            self.reject(
                it.span,
                "TypeScript namespace declaration is not supported in strip-only mode",
            );
        } else {
            walk::walk_ts_namespace_declaration(self, it);
        }
    }

    fn visit_ts_import_equals_declaration(&mut self, it: &TSImportEqualsDeclaration<'a>) {
        self.reject(
            it.span,
            "TypeScript import equals declaration is not supported in strip-only mode",
        );
    }

    fn visit_ts_export_assignment(&mut self, it: &TSExportAssignment<'a>) {
        self.reject(
            it.span,
            "TypeScript export assignment is not supported in strip-only mode",
        );
    }

    fn visit_ts_namespace_export_declaration(&mut self, it: &TSNamespaceExportDeclaration<'a>) {
        // Node's stripper leaves `export as namespace X` for V8 to choke on
        self.reject(it.span, "Unexpected token 'export'");
    }

    fn visit_ts_type_assertion(&mut self, it: &TSTypeAssertion<'a>) {
        self.reject(
            it.span,
            "The angle-bracket syntax for type assertions, `<T>expr`, is not supported in type strip mode. Instead, use the 'as' syntax: `expr as T`.",
        );
        walk::walk_ts_type_assertion(self, it);
    }

    fn visit_formal_parameter(&mut self, it: &FormalParameter<'a>) {
        // `constructor(private x: number)` declares and assigns a field
        if it.accessibility.is_some() || it.readonly || it.r#override {
            self.reject(
                it.span,
                "TypeScript parameter property is not supported in strip-only mode",
            );
        }
        walk::walk_formal_parameter(self, it);
    }
}

/// Whether a namespace body declares anything with a runtime value; one that
/// holds only types is erased, as TypeScript itself does.
fn namespace_has_values(namespace: &TSNamespaceDeclaration<'_>) -> bool {
    match &namespace.body {
        TSNamespaceDeclarationBody::TSNamespaceDeclaration(inner) => !inner.declare && namespace_has_values(inner),
        TSNamespaceDeclarationBody::TSModuleBlock(block) => block.body.iter().any(statement_has_value),
    }
}

/// Whether a statement inside a namespace survives type erasure.
fn statement_has_value(statement: &Statement<'_>) -> bool {
    match statement {
        Statement::TSTypeAliasDeclaration(_)
        | Statement::TSInterfaceDeclaration(_)
        | Statement::TSExternalModuleDeclaration(_)
        | Statement::TSGlobalDeclaration(_) => false,
        Statement::TSEnumDeclaration(declaration) => !declaration.declare,
        Statement::TSNamespaceDeclaration(inner) => !inner.declare && namespace_has_values(inner),
        Statement::VariableDeclaration(declaration) => !declaration.declare,
        Statement::FunctionDeclaration(function) => !function.declare && function.body.is_some(),
        Statement::ClassDeclaration(class) => !class.declare,
        Statement::ExportDeclaration(export) => declaration_has_value(&export.declaration),
        // `export { A }` re-exports a name declared elsewhere in the block
        Statement::ExportNamedDeclaration(export) => export.export_kind.is_value() && !export.specifiers.is_empty(),
        _ => true,
    }
}

/// `export <declaration>` inside a namespace: the declaration decides.
fn declaration_has_value(declaration: &Declaration<'_>) -> bool {
    match declaration {
        Declaration::TSTypeAliasDeclaration(_) | Declaration::TSInterfaceDeclaration(_) => false,
        Declaration::TSEnumDeclaration(declaration) => !declaration.declare,
        Declaration::TSNamespaceDeclaration(inner) => !inner.declare && namespace_has_values(inner),
        Declaration::VariableDeclaration(declaration) => !declaration.declare,
        Declaration::FunctionDeclaration(function) => !function.declare && function.body.is_some(),
        Declaration::ClassDeclaration(class) => !class.declare,
        _ => true,
    }
}
