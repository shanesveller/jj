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

use std::slice;

use clap::ArgGroup;
use clap_complete::ArgValueCandidates;
use clap_complete::ArgValueCompleter;
use tracing::instrument;

use crate::cli_util::CommandHelper;
use crate::cli_util::RevisionArg;
use crate::command_error::CommandError;
use crate::complete;
use crate::diff_util::DiffFormatArgs;
use crate::ui::Ui;

/// Compare the changes of two commits
///
/// This excludes changes from other commits by temporarily rebasing `--from`
/// onto `--to`'s parents. If you wish to compare the same change across
/// versions, consider `jj evolog -p` instead.
#[derive(clap::Args, Clone, Debug)]
#[command(group(ArgGroup::new("to_diff").args(&["from", "to"]).multiple(true).required(true)))]
#[command(mut_arg("ignore_all_space", |a| a.short('w')))]
#[command(mut_arg("ignore_space_change", |a| a.short('b')))]
pub(crate) struct InterdiffArgs {
    /// Show changes from this revision
    #[arg(long, short, add = ArgValueCandidates::new(complete::all_revisions))]
    from: Option<RevisionArg>,
    /// Show changes to this revision
    #[arg(long, short, add = ArgValueCandidates::new(complete::all_revisions))]
    to: Option<RevisionArg>,
    /// Restrict the diff to these paths
    #[arg(
        value_hint = clap::ValueHint::AnyPath,
        add = ArgValueCompleter::new(complete::interdiff_files),
    )]
    paths: Vec<String>,
    #[command(flatten)]
    format: DiffFormatArgs,
}

#[instrument(skip_all)]
pub(crate) fn cmd_interdiff(
    ui: &mut Ui,
    command: &CommandHelper,
    args: &InterdiffArgs,
) -> Result<(), CommandError> {
    let workspace_command = command.workspace_helper(ui)?;
    let from =
        workspace_command.resolve_single_rev(ui, args.from.as_ref().unwrap_or(&RevisionArg::AT))?;
    let to =
        workspace_command.resolve_single_rev(ui, args.to.as_ref().unwrap_or(&RevisionArg::AT))?;
    let matcher = workspace_command
        .parse_file_patterns(ui, &args.paths)?
        .to_matcher();
    let diff_renderer = workspace_command.diff_renderer_for(&args.format)?;
    ui.request_pager();
    diff_renderer.show_inter_diff(
        ui,
        ui.stdout_formatter().as_mut(),
        slice::from_ref(&from),
        &to,
        matcher.as_ref(),
        ui.term_width(),
    )?;
    Ok(())
}
