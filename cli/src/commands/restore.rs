// Copyright 2020 The Jujutsu Authors
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// https://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::io::Write;

use clap_complete::ArgValueCandidates;
use clap_complete::ArgValueCompleter;
use jj_lib::object_id::ObjectId;
use jj_lib::rewrite::restore_tree;
use tracing::instrument;

use crate::cli_util::CommandHelper;
use crate::cli_util::RevisionArg;
use crate::command_error::user_error;
use crate::command_error::CommandError;
use crate::complete;
use crate::ui::Ui;

/// Restore paths from another revision
///
/// That means that the paths get the same content in the destination (`--to`)
/// as they had in the source (`--from`). This is typically used for undoing
/// changes to some paths in the working copy (`jj restore <paths>`).
///
/// If only one of `--from` or `--to` is specified, the other one defaults to
/// the working copy.
///
/// When neither `--from` nor `--to` is specified, the command restores into the
/// working copy from its parent(s). `jj restore` without arguments is similar
/// to `jj abandon`, except that it leaves an empty revision with its
/// description and other metadata preserved.
///
/// See `jj diffedit` if you'd like to restore portions of files rather than
/// entire files.
#[derive(clap::Args, Clone, Debug)]
pub(crate) struct RestoreArgs {
    /// Restore only these paths (instead of all paths)
    #[arg(
        value_hint = clap::ValueHint::AnyPath,
        add = ArgValueCompleter::new(complete::modified_range_files),
    )]
    paths: Vec<String>,
    /// Revision to restore from (source)
    #[arg(long, short, add = ArgValueCandidates::new(complete::all_revisions))]
    from: Option<RevisionArg>,
    /// Revision to restore into (destination)
    #[arg(long, short, add = ArgValueCandidates::new(complete::mutable_revisions))]
    to: Option<RevisionArg>,
    /// Undo the changes in a revision as compared to the merge of its parents.
    ///
    /// This undoes the changes that can be seen with `jj diff -r REVISION`. If
    /// `REVISION` only has a single parent, this option is equivalent to `jj
    ///  restore --to REVISION --from REVISION-`.
    ///
    /// The default behavior of `jj restore` is equivalent to `jj restore
    /// --changes-in @`.
    #[arg(
        long, short,
        value_name = "REVISION",
        conflicts_with_all = ["to", "from"],
        add = ArgValueCandidates::new(complete::all_revisions),
    )]
    changes_in: Option<RevisionArg>,
    /// Prints an error. DO NOT USE.
    ///
    /// If we followed the pattern of `jj diff` and `jj diffedit`, we would use
    /// `--revision` instead of `--changes-in` However, that would make it
    /// likely that someone unfamiliar with this pattern would use `-r` when
    /// they wanted `--from`. This would make a different revision empty, and
    /// the user might not even realize something went wrong.
    #[arg(long, short, hide = true)]
    revision: Option<RevisionArg>,
    /// Preserve the content (not the diff) when rebasing descendants
    #[arg(long)]
    restore_descendants: bool,
}

#[instrument(skip_all)]
pub(crate) fn cmd_restore(
    ui: &mut Ui,
    command: &CommandHelper,
    args: &RestoreArgs,
) -> Result<(), CommandError> {
    let mut workspace_command = command.workspace_helper(ui)?;
    let (from_tree, to_commit);
    if args.revision.is_some() {
        return Err(user_error(
            "`jj restore` does not have a `--revision`/`-r` option. If you'd like to modify\nthe \
             *current* revision, use `--from`. If you'd like to modify a *different* \
             revision,\nuse `--to` or `--changes-in`.",
        ));
    }
    if args.from.is_some() || args.to.is_some() {
        to_commit = workspace_command
            .resolve_single_rev(ui, args.to.as_ref().unwrap_or(&RevisionArg::AT))?;
        from_tree = workspace_command
            .resolve_single_rev(ui, args.from.as_ref().unwrap_or(&RevisionArg::AT))?
            .tree()?;
    } else {
        to_commit = workspace_command
            .resolve_single_rev(ui, args.changes_in.as_ref().unwrap_or(&RevisionArg::AT))?;
        from_tree = to_commit.parent_tree(workspace_command.repo().as_ref())?;
    }
    workspace_command.check_rewritable([to_commit.id()])?;

    let matcher = workspace_command
        .parse_file_patterns(ui, &args.paths)?
        .to_matcher();
    let to_tree = to_commit.tree()?;
    let new_tree_id = restore_tree(&from_tree, &to_tree, matcher.as_ref())?;
    if &new_tree_id == to_commit.tree_id() {
        writeln!(ui.status(), "Nothing changed.")?;
    } else {
        let mut tx = workspace_command.start_transaction();
        let new_commit = tx
            .repo_mut()
            .rewrite_commit(command.settings(), &to_commit)
            .set_tree_id(new_tree_id)
            .write()?;
        // rebase_descendants early; otherwise `new_commit` would always have
        // a conflicted change id at this point.
        let (num_rebased, extra_msg) = if args.restore_descendants {
            (
                tx.repo_mut().reparent_descendants(command.settings())?,
                " (while preserving their content)",
            )
        } else {
            (tx.repo_mut().rebase_descendants(command.settings())?, "")
        };
        if let Some(mut formatter) = ui.status_formatter() {
            write!(formatter, "Created ")?;
            tx.write_commit_summary(formatter.as_mut(), &new_commit)?;
            writeln!(formatter)?;
            if num_rebased > 0 {
                writeln!(
                    formatter,
                    "Rebased {num_rebased} descendant commits{extra_msg}"
                )?;
            }
        }
        tx.finish(ui, format!("restore into commit {}", to_commit.id().hex()))?;
    }
    Ok(())
}
