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
use jj_lib::repo::Repo;
use tracing::instrument;

use crate::cli_util::CommandHelper;
use crate::cli_util::RevisionArg;
use crate::command_error::user_error_with_hint;
use crate::command_error::CommandError;
use crate::complete;
use crate::description_util::description_template;
use crate::description_util::edit_description;
use crate::ui::Ui;

/// Split a revision in two
///
/// Starts a [diff editor] on the changes in the revision. Edit the right side
/// of the diff until it has the content you want in the first revision. Once
/// you close the editor, your edited content will replace the previous
/// revision. The remaining changes will be put in a new revision on top.
///
/// [diff editor]:
///     https://martinvonz.github.io/jj/latest/config/#editing-diffs
///
/// If the change you split had a description, you will be asked to enter a
/// change description for each commit. If the change did not have a
/// description, the second part will not get a description, and you will be
/// asked for a description only for the first part.
///
/// Splitting an empty commit is not supported because the same effect can be
/// achieved with `jj new`.
#[derive(clap::Args, Clone, Debug)]
pub(crate) struct SplitArgs {
    /// Interactively choose which parts to split. This is the default if no
    /// paths are provided.
    #[arg(long, short)]
    interactive: bool,
    /// Specify diff editor to be used (implies --interactive)
    #[arg(long, value_name = "NAME")]
    tool: Option<String>,
    /// The revision to split
    #[arg(
        long, short,
        default_value = "@",
        add = ArgValueCandidates::new(complete::mutable_revisions)
    )]
    revision: RevisionArg,
    /// Split the revision into two parallel revisions instead of a parent and
    /// child.
    // TODO: Delete `--siblings` alias in jj 0.25+
    #[arg(long, short, alias = "siblings")]
    parallel: bool,
    /// Put these paths in the first commit
    #[arg(
        value_hint = clap::ValueHint::AnyPath,
        add = ArgValueCompleter::new(complete::modified_revision_files),
    )]
    paths: Vec<String>,
}

#[instrument(skip_all)]
pub(crate) fn cmd_split(
    ui: &mut Ui,
    command: &CommandHelper,
    args: &SplitArgs,
) -> Result<(), CommandError> {
    let mut workspace_command = command.workspace_helper(ui)?;
    let commit = workspace_command.resolve_single_rev(ui, &args.revision)?;
    if commit.is_empty(workspace_command.repo().as_ref())? {
        return Err(user_error_with_hint(
            format!("Refusing to split empty commit {}.", commit.id().hex()),
            "Use `jj new` if you want to create another empty commit.",
        ));
    }

    workspace_command.check_rewritable([commit.id()])?;
    let matcher = workspace_command
        .parse_file_patterns(ui, &args.paths)?
        .to_matcher();
    let diff_selector = workspace_command.diff_selector(
        ui,
        args.tool.as_deref(),
        args.interactive || args.paths.is_empty(),
    )?;
    let mut tx = workspace_command.start_transaction();
    let end_tree = commit.tree()?;
    let base_tree = commit.parent_tree(tx.repo())?;
    let format_instructions = || {
        format!(
            "\
You are splitting a commit into two: {}

The diff initially shows the changes in the commit you're splitting.

Adjust the right side until it shows the contents you want for the first commit.
The remainder will be in the second commit.
",
            tx.format_commit_summary(&commit)
        )
    };

    // Prompt the user to select the changes they want for the first commit.
    let selected_tree_id =
        diff_selector.select(&base_tree, &end_tree, matcher.as_ref(), format_instructions)?;
    if &selected_tree_id == commit.tree_id() {
        // The user selected everything from the original commit.
        writeln!(
            ui.warning_default(),
            "All changes have been selected, so the second commit will be empty"
        )?;
    } else if selected_tree_id == base_tree.id() {
        // The user selected nothing, so the first commit will be empty.
        writeln!(
            ui.warning_default(),
            "No changes have been selected, so the first commit will be empty"
        )?;
    }

    // Create the first commit, which includes the changes selected by the user.
    let selected_tree = tx.repo().store().get_root_tree(&selected_tree_id)?;
    let first_commit = {
        let mut commit_builder = tx
            .repo_mut()
            .rewrite_commit(command.settings(), &commit)
            .detach();
        commit_builder.set_tree_id(selected_tree_id);
        if commit_builder.description().is_empty() {
            commit_builder.set_description(command.settings().default_description());
        }
        let temp_commit = commit_builder.write_hidden()?;
        let template = description_template(
            ui,
            &tx,
            "Enter a description for the first commit.",
            &temp_commit,
        )?;
        let description = edit_description(
            tx.base_workspace_helper().repo_path(),
            &template,
            command.settings(),
        )?;
        commit_builder.set_description(description);
        commit_builder.write(tx.repo_mut())?
    };

    // Create the second commit, which includes everything the user didn't
    // select.
    let second_commit = {
        let new_tree = if args.parallel {
            // Merge the original commit tree with its parent using the tree
            // containing the user selected changes as the base for the merge.
            // This results in a tree with the changes the user didn't select.
            end_tree.merge(&selected_tree, &base_tree)?
        } else {
            end_tree
        };
        let parents = if args.parallel {
            commit.parent_ids().to_vec()
        } else {
            vec![first_commit.id().clone()]
        };
        let mut commit_builder = tx
            .repo_mut()
            .rewrite_commit(command.settings(), &commit)
            .detach();
        commit_builder
            .set_parents(parents)
            .set_tree_id(new_tree.id())
            // Generate a new change id so that the commit being split doesn't
            // become divergent.
            .generate_new_change_id();
        let description = if commit.description().is_empty() {
            // If there was no description before, don't ask for one for the
            // second commit.
            "".to_string()
        } else {
            let temp_commit = commit_builder.write_hidden()?;
            let template = description_template(
                ui,
                &tx,
                "Enter a description for the second commit.",
                &temp_commit,
            )?;
            edit_description(
                tx.base_workspace_helper().repo_path(),
                &template,
                command.settings(),
            )?
        };
        commit_builder.set_description(description);
        commit_builder.write(tx.repo_mut())?
    };

    // Mark the commit being split as rewritten to the second commit. As a
    // result, if @ points to the commit being split, it will point to the
    // second commit after the command finishes. This also means that any
    // bookmarks pointing to the commit being split are moved to the second
    // commit.
    tx.repo_mut()
        .set_rewritten_commit(commit.id().clone(), second_commit.id().clone());
    let mut num_rebased = 0;
    tx.repo_mut().transform_descendants(
        command.settings(),
        vec![commit.id().clone()],
        |mut rewriter| {
            num_rebased += 1;
            if args.parallel {
                rewriter
                    .replace_parent(second_commit.id(), [first_commit.id(), second_commit.id()]);
            }
            // We don't need to do anything special for the non-parallel case
            // since we already marked the original commit as rewritten.
            rewriter.rebase(command.settings())?.write()?;
            Ok(())
        },
    )?;

    if let Some(mut formatter) = ui.status_formatter() {
        if num_rebased > 0 {
            writeln!(formatter, "Rebased {num_rebased} descendant commits")?;
        }
        write!(formatter, "First part: ")?;
        tx.write_commit_summary(formatter.as_mut(), &first_commit)?;
        write!(formatter, "\nSecond part: ")?;
        tx.write_commit_summary(formatter.as_mut(), &second_commit)?;
        writeln!(formatter)?;
    }
    tx.finish(ui, format!("split commit {}", commit.id().hex()))?;
    Ok(())
}
