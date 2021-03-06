//! This module defines `AssistCtx` -- the API surface that is exposed to assists.
use either::Either;
use hir::{db::HirDatabase, InFile, SourceAnalyzer, SourceBinder};
use ra_db::FileRange;
use ra_fmt::{leading_indent, reindent};
use ra_syntax::{
    algo::{self, find_covering_element, find_node_at_offset},
    AstNode, SourceFile, SyntaxElement, SyntaxKind, SyntaxNode, SyntaxToken, TextRange, TextUnit,
    TokenAtOffset,
};
use ra_text_edit::TextEditBuilder;

use crate::{AssistAction, AssistId, AssistLabel, ResolvedAssist};

#[derive(Clone, Debug)]
pub(crate) enum Assist {
    Unresolved { label: AssistLabel },
    Resolved { assist: ResolvedAssist },
}

/// `AssistCtx` allows to apply an assist or check if it could be applied.
///
/// Assists use a somewhat over-engineered approach, given the current needs. The
/// assists workflow consists of two phases. In the first phase, a user asks for
/// the list of available assists. In the second phase, the user picks a
/// particular assist and it gets applied.
///
/// There are two peculiarities here:
///
/// * first, we ideally avoid computing more things then necessary to answer
///   "is assist applicable" in the first phase.
/// * second, when we are applying assist, we don't have a guarantee that there
///   weren't any changes between the point when user asked for assists and when
///   they applied a particular assist. So, when applying assist, we need to do
///   all the checks from scratch.
///
/// To avoid repeating the same code twice for both "check" and "apply"
/// functions, we use an approach reminiscent of that of Django's function based
/// views dealing with forms. Each assist receives a runtime parameter,
/// `should_compute_edit`. It first check if an edit is applicable (potentially
/// computing info required to compute the actual edit). If it is applicable,
/// and `should_compute_edit` is `true`, it then computes the actual edit.
///
/// So, to implement the original assists workflow, we can first apply each edit
/// with `should_compute_edit = false`, and then applying the selected edit
/// again, with `should_compute_edit = true` this time.
///
/// Note, however, that we don't actually use such two-phase logic at the
/// moment, because the LSP API is pretty awkward in this place, and it's much
/// easier to just compute the edit eagerly :-)
#[derive(Debug)]
pub(crate) struct AssistCtx<'a, DB> {
    pub(crate) db: &'a DB,
    pub(crate) frange: FileRange,
    source_file: SourceFile,
    should_compute_edit: bool,
}

impl<'a, DB> Clone for AssistCtx<'a, DB> {
    fn clone(&self) -> Self {
        AssistCtx {
            db: self.db,
            frange: self.frange,
            source_file: self.source_file.clone(),
            should_compute_edit: self.should_compute_edit,
        }
    }
}

impl<'a, DB: HirDatabase> AssistCtx<'a, DB> {
    pub(crate) fn with_ctx<F, T>(db: &DB, frange: FileRange, should_compute_edit: bool, f: F) -> T
    where
        F: FnOnce(AssistCtx<DB>) -> T,
    {
        let parse = db.parse(frange.file_id);

        let ctx = AssistCtx { db, frange, source_file: parse.tree(), should_compute_edit };
        f(ctx)
    }

    pub(crate) fn add_assist(
        self,
        id: AssistId,
        label: impl Into<String>,
        f: impl FnOnce(&mut ActionBuilder),
    ) -> Option<Assist> {
        let label = AssistLabel { label: label.into(), id };
        assert!(label.label.chars().nth(0).unwrap().is_uppercase());

        let assist = if self.should_compute_edit {
            let action = {
                let mut edit = ActionBuilder::default();
                f(&mut edit);
                edit.build()
            };
            Assist::Resolved { assist: ResolvedAssist { label, action_data: Either::Left(action) } }
        } else {
            Assist::Unresolved { label }
        };

        Some(assist)
    }

    #[allow(dead_code)] // will be used for auto import assist with multiple actions
    pub(crate) fn add_assist_group(
        self,
        id: AssistId,
        label: impl Into<String>,
        f: impl FnOnce() -> Vec<ActionBuilder>,
    ) -> Option<Assist> {
        let label = AssistLabel { label: label.into(), id };
        let assist = if self.should_compute_edit {
            let actions = f();
            assert!(!actions.is_empty(), "Assist cannot have no");

            Assist::Resolved {
                assist: ResolvedAssist {
                    label,
                    action_data: Either::Right(
                        actions.into_iter().map(ActionBuilder::build).collect(),
                    ),
                },
            }
        } else {
            Assist::Unresolved { label }
        };

        Some(assist)
    }

    pub(crate) fn token_at_offset(&self) -> TokenAtOffset<SyntaxToken> {
        self.source_file.syntax().token_at_offset(self.frange.range.start())
    }

    pub(crate) fn find_token_at_offset(&self, kind: SyntaxKind) -> Option<SyntaxToken> {
        self.token_at_offset().find(|it| it.kind() == kind)
    }

    pub(crate) fn find_node_at_offset<N: AstNode>(&self) -> Option<N> {
        find_node_at_offset(self.source_file.syntax(), self.frange.range.start())
    }
    pub(crate) fn covering_element(&self) -> SyntaxElement {
        find_covering_element(self.source_file.syntax(), self.frange.range)
    }
    pub(crate) fn source_binder(&self) -> SourceBinder<'a, DB> {
        SourceBinder::new(self.db)
    }
    pub(crate) fn source_analyzer(
        &self,
        node: &SyntaxNode,
        offset: Option<TextUnit>,
    ) -> SourceAnalyzer {
        let src = InFile::new(self.frange.file_id.into(), node);
        self.source_binder().analyze(src, offset)
    }

    pub(crate) fn covering_node_for_range(&self, range: TextRange) -> SyntaxElement {
        find_covering_element(self.source_file.syntax(), range)
    }
}

#[derive(Default)]
pub(crate) struct ActionBuilder {
    edit: TextEditBuilder,
    cursor_position: Option<TextUnit>,
    target: Option<TextRange>,
    label: Option<String>,
}

impl ActionBuilder {
    #[allow(dead_code)]
    /// Adds a custom label to the action, if it needs to be different from the assist label
    pub(crate) fn label(&mut self, label: impl Into<String>) {
        self.label = Some(label.into())
    }

    /// Replaces specified `range` of text with a given string.
    pub(crate) fn replace(&mut self, range: TextRange, replace_with: impl Into<String>) {
        self.edit.replace(range, replace_with.into())
    }

    /// Replaces specified `node` of text with a given string, reindenting the
    /// string to maintain `node`'s existing indent.
    // FIXME: remove in favor of ra_syntax::edit::IndentLevel::increase_indent
    pub(crate) fn replace_node_and_indent(
        &mut self,
        node: &SyntaxNode,
        replace_with: impl Into<String>,
    ) {
        let mut replace_with = replace_with.into();
        if let Some(indent) = leading_indent(node) {
            replace_with = reindent(&replace_with, &indent)
        }
        self.replace(node.text_range(), replace_with)
    }

    /// Remove specified `range` of text.
    #[allow(unused)]
    pub(crate) fn delete(&mut self, range: TextRange) {
        self.edit.delete(range)
    }

    /// Append specified `text` at the given `offset`
    pub(crate) fn insert(&mut self, offset: TextUnit, text: impl Into<String>) {
        self.edit.insert(offset, text.into())
    }

    /// Specify desired position of the cursor after the assist is applied.
    pub(crate) fn set_cursor(&mut self, offset: TextUnit) {
        self.cursor_position = Some(offset)
    }

    /// Specify that the assist should be active withing the `target` range.
    ///
    /// Target ranges are used to sort assists: the smaller the target range,
    /// the more specific assist is, and so it should be sorted first.
    pub(crate) fn target(&mut self, target: TextRange) {
        self.target = Some(target)
    }

    /// Get access to the raw `TextEditBuilder`.
    pub(crate) fn text_edit_builder(&mut self) -> &mut TextEditBuilder {
        &mut self.edit
    }

    pub(crate) fn replace_ast<N: AstNode>(&mut self, old: N, new: N) {
        algo::diff(old.syntax(), new.syntax()).into_text_edit(&mut self.edit)
    }

    fn build(self) -> AssistAction {
        AssistAction {
            edit: self.edit.finish(),
            cursor_position: self.cursor_position,
            target: self.target,
            label: self.label,
        }
    }
}
