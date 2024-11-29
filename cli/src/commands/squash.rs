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

use clap_complete::ArgValueCandidates;
use clap_complete::ArgValueCompleter;
use itertools::Itertools as _;
use jj_lib::commit::Commit;
use jj_lib::commit::CommitIteratorExt;
use jj_lib::matchers::Matcher;
use jj_lib::object_id::ObjectId;
use jj_lib::repo::Repo;
use jj_lib::rewrite;
use jj_lib::settings::UserSettings;
use tracing::instrument;

use crate::cli_util::CommandHelper;
use crate::cli_util::DiffSelector;
use crate::cli_util::RevisionArg;
use crate::cli_util::WorkspaceCommandTransaction;
use crate::command_error::user_error;
use crate::command_error::user_error_with_hint;
use crate::command_error::CommandError;
use crate::complete;
use crate::description_util::combine_messages;
use crate::description_util::join_message_paragraphs;
use crate::ui::Ui;

/// Move changes from a revision into another revision
///
/// With the `-r` option, moves the changes from the specified revision to the
/// parent revision. Fails if there are several parent revisions (i.e., the
/// given revision is a merge).
///
/// With the `--from` and/or `--into` options, moves changes from/to the given
/// revisions. If either is left out, it defaults to the working-copy commit.
/// For example, `jj squash --into @--` moves changes from the working-copy
/// commit to the grandparent.
///
/// If, after moving changes out, the source revision is empty compared to its
/// parent(s), and `--keep-emptied` is not set, it will be abandoned. Without
/// `--interactive` or paths, the source revision will always be empty.
///
/// If the source was abandoned and both the source and destination had a
/// non-empty description, you will be asked for the combined description. If
/// either was empty, then the other one will be used.
///
/// If a working-copy commit gets abandoned, it will be given a new, empty
/// commit. This is true in general; it is not specific to this command.
#[derive(clap::Args, Clone, Debug)]
pub(crate) struct SquashArgs {
    /// Revision to squash into its parent (default: @)
    #[arg(long, short, add = ArgValueCandidates::new(complete::mutable_revisions))]
    revision: Option<RevisionArg>,
    /// Revision(s) to squash from (default: @)
    #[arg(
        long, short,
        conflicts_with = "revision",
        add = ArgValueCandidates::new(complete::mutable_revisions),
    )]
    from: Vec<RevisionArg>,
    /// Revision to squash into (default: @)
    #[arg(
        long, short = 't',
        conflicts_with = "revision",
        visible_alias = "to",
        add = ArgValueCandidates::new(complete::mutable_revisions),
    )]
    into: Option<RevisionArg>,
    /// The description to use for squashed revision (don't open editor)
    #[arg(long = "message", short, value_name = "MESSAGE")]
    message_paragraphs: Vec<String>,
    /// Use the description of the destination revision and discard the
    /// description(s) of the source revision(s)
    #[arg(long, short, conflicts_with = "message_paragraphs")]
    use_destination_message: bool,
    /// Interactively choose which parts to squash
    #[arg(long, short)]
    interactive: bool,
    /// Specify diff editor to be used (implies --interactive)
    #[arg(long, value_name = "NAME")]
    tool: Option<String>,
    /// Move only changes to these paths (instead of all paths)
    #[arg(
        conflicts_with_all = ["interactive", "tool"],
        value_hint = clap::ValueHint::AnyPath,
        add = ArgValueCompleter::new(complete::squash_revision_files),
    )]
    paths: Vec<String>,
    /// The source revision will not be abandoned
    #[arg(long, short)]
    keep_emptied: bool,
}

#[instrument(skip_all)]
pub(crate) fn cmd_squash(
    ui: &mut Ui,
    command: &CommandHelper,
    args: &SquashArgs,
) -> Result<(), CommandError> {
    let mut workspace_command = command.workspace_helper(ui)?;

    let mut sources: Vec<Commit>;
    let destination;
    if !args.from.is_empty() || args.into.is_some() {
        sources = if args.from.is_empty() {
            workspace_command.parse_revset(ui, &RevisionArg::AT)?
        } else {
            workspace_command.parse_union_revsets(ui, &args.from)?
        }
        .evaluate_to_commits()?
        .try_collect()?;
        destination = workspace_command
            .resolve_single_rev(ui, args.into.as_ref().unwrap_or(&RevisionArg::AT))?;
        if sources.iter().any(|source| source.id() == destination.id()) {
            return Err(user_error("Source and destination cannot be the same"));
        }
        // Reverse the set so we apply the oldest commits first. It shouldn't affect the
        // result, but it avoids creating transient conflicts and is therefore probably
        // a little faster.
        sources.reverse();
    } else {
        let source = workspace_command
            .resolve_single_rev(ui, args.revision.as_ref().unwrap_or(&RevisionArg::AT))?;
        let mut parents: Vec<_> = source.parents().try_collect()?;
        if parents.len() != 1 {
            return Err(user_error_with_hint(
                "Cannot squash merge commits without a specified destination",
                "Use `--into` to specify which parent to squash into",
            ));
        }
        sources = vec![source];
        destination = parents.pop().unwrap();
    }

    let matcher = workspace_command
        .parse_file_patterns(ui, &args.paths)?
        .to_matcher();
    let diff_selector =
        workspace_command.diff_selector(ui, args.tool.as_deref(), args.interactive)?;
    let mut tx = workspace_command.start_transaction();
    let tx_description = format!("squash commits into {}", destination.id().hex());
    move_diff(
        ui,
        &mut tx,
        command.settings(),
        &sources,
        &destination,
        matcher.as_ref(),
        &diff_selector,
        SquashedDescription::from_args(args),
        args.revision.is_none() && args.from.is_empty() && args.into.is_none(),
        &args.paths,
        args.keep_emptied,
    )?;
    tx.finish(ui, tx_description)?;
    Ok(())
}

enum SquashedDescription {
    // Use this exact description.
    Exact(String),
    // Use the destination's description and discard the descriptions of the
    // source revisions.
    UseDestination,
    // Combine the descriptions of the source and destination revisions.
    Combine,
}

impl SquashedDescription {
    fn from_args(args: &SquashArgs) -> Self {
        // These options are incompatible and Clap is configured to prevent this.
        assert!(args.message_paragraphs.is_empty() || !args.use_destination_message);

        if !args.message_paragraphs.is_empty() {
            let desc = join_message_paragraphs(&args.message_paragraphs);
            SquashedDescription::Exact(desc)
        } else if args.use_destination_message {
            SquashedDescription::UseDestination
        } else {
            SquashedDescription::Combine
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn move_diff(
    ui: &mut Ui,
    tx: &mut WorkspaceCommandTransaction,
    settings: &UserSettings,
    sources: &[Commit],
    destination: &Commit,
    matcher: &dyn Matcher,
    diff_selector: &DiffSelector,
    description: SquashedDescription,
    no_rev_arg: bool,
    path_arg: &[String],
    keep_emptied: bool,
) -> Result<(), CommandError> {
    tx.base_workspace_helper()
        .check_rewritable(sources.iter().chain(std::iter::once(destination)).ids())?;

    let mut source_commits = vec![];
    for source in sources {
        let parent_tree = source.parent_tree(tx.repo())?;
        let source_tree = source.tree()?;
        let format_instructions = || {
            format!(
                "\
You are moving changes from: {}
into commit: {}

The left side of the diff shows the contents of the parent commit. The
right side initially shows the contents of the commit you're moving
changes from.

Adjust the right side until the diff shows the changes you want to move
to the destination. If you don't make any changes, then all the changes
from the source will be moved into the destination.
",
                tx.format_commit_summary(source),
                tx.format_commit_summary(destination)
            )
        };
        let selected_tree_id =
            diff_selector.select(&parent_tree, &source_tree, matcher, format_instructions)?;
        let selected_tree = tx.repo().store().get_root_tree(&selected_tree_id)?;

        source_commits.push(rewrite::CommitToSquash {
            commit: source.clone(),
            selected_tree,
            parent_tree,
        });
    }

    let repo_path = tx.base_workspace_helper().repo_path().to_owned();
    match rewrite::squash_commits(
        settings,
        tx.repo_mut(),
        &source_commits,
        destination,
        keep_emptied,
        |abandoned_commits| match description {
            SquashedDescription::Exact(description) => Ok(description),
            SquashedDescription::UseDestination => Ok(destination.description().to_owned()),
            SquashedDescription::Combine => {
                let abandoned_commits = abandoned_commits.iter().map(|c| &c.commit).collect_vec();
                combine_messages(&repo_path, &abandoned_commits, destination, settings)
            }
        },
    )? {
        rewrite::SquashResult::NoChanges => {
            if diff_selector.is_interactive() {
                return Err(user_error("No changes selected"));
            }

            if let [only_path] = path_arg {
                if no_rev_arg
                    && tx
                        .base_workspace_helper()
                        .parse_revset(ui, &RevisionArg::from(only_path.to_owned()))
                        .is_ok()
                {
                    writeln!(
                        ui.warning_default(),
                        "The argument {only_path:?} is being interpreted as a path. To specify a \
                         revset, pass -r {only_path:?} instead."
                    )?;
                }
            }

            Ok(())
        }
        rewrite::SquashResult::NewCommit(_) => Ok(()),
    }
}
