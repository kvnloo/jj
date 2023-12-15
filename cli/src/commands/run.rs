// Copyright 2024 The Jujutsu Authors
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

//! This file contains the internal implementation of `run`.

use std::collections::HashMap;
use std::collections::HashSet;
use std::fs;
use std::io;
use std::io::Write as _;
use std::path::Path;
use std::path::PathBuf;
use std::process::ExitStatus;
use std::process::Stdio;
use std::sync::Arc;

use futures::TryStreamExt as _;
use itertools::Itertools as _;
use jj_lib::backend::BackendError;
use jj_lib::backend::CommitId;
use jj_lib::commit::Commit;
use jj_lib::commit::CommitIteratorExt as _;
use jj_lib::conflicts::ConflictMarkerStyle;
use jj_lib::fsmonitor::FsmonitorSettings;
use jj_lib::gitignore::GitIgnoreFile;
use jj_lib::local_working_copy::EolConversionMode;
use jj_lib::local_working_copy::ExecChangeSetting;
use jj_lib::local_working_copy::TreeState;
use jj_lib::local_working_copy::TreeStateError;
use jj_lib::local_working_copy::TreeStateSettings;
use jj_lib::matchers::EverythingMatcher;
use jj_lib::matchers::NothingMatcher;
use jj_lib::merged_tree::MergedTree;
use jj_lib::object_id::ObjectId as _;
use jj_lib::repo::Repo as _;
use jj_lib::repo_path::RepoPathBuf;
use jj_lib::tree::Tree;
use jj_lib::working_copy::SnapshotOptions;
use tokio::runtime::Builder;
use tokio::sync::mpsc;
use tokio::sync::mpsc::Sender;
use tokio::task::JoinError;
use tokio::task::JoinSet;

use crate::cli_util::CommandHelper;
use crate::cli_util::RevisionArg;
use crate::cli_util::WorkspaceCommandTransaction;
use crate::command_error::CommandError;
use crate::command_error::CommandErrorKind;
use crate::ui::Ui;

#[derive(Debug, thiserror::Error)]
enum RunError {
    #[error("failed to checkout the commit {}", .0)]
    FailedCheckout(CommitId),
    #[error("the command '{}' failed with {} for commit {}", .0,.1, .2)]
    CommandFailure(String, ExitStatus, CommitId),
    #[error(transparent)]
    IoError(#[from] io::Error),
    #[error("failed to create path {} with {}", .0.to_string_lossy(), .1)]
    PathCreationFailure(PathBuf, io::Error),
    #[error("failed to load a commits tree")]
    TreeState(#[from] TreeStateError),
    #[error(transparent)]
    Backend(#[from] BackendError),
    #[error(transparent)]
    JobFailure(#[from] JoinError),
}

impl From<RunError> for CommandError {
    fn from(value: RunError) -> Self {
        Self::new(CommandErrorKind::Cli, Box::new(value))
    }
}

/// Creates the required directories for a StoredWorkingCopy.
/// Returns a tuple of (`working_copy` and `state`).
async fn create_working_copy_paths(path: &Path) -> Result<(PathBuf, PathBuf), RunError> {
    tracing::debug!(?path, "creating working copy paths for path");
    let working_copy = path.join("working_copy");
    let state = path.join("state");
    tracing::debug!(?working_copy, ?state, "creating paths for a commit");

    fs::create_dir(&working_copy)
        .map_err(|e| RunError::PathCreationFailure(working_copy.clone(), e))?;
    fs::create_dir(&state).map_err(|e| RunError::PathCreationFailure(state.clone(), e))?;

    Ok((working_copy, state))
}

fn get_runtime(jobs: usize) -> tokio::runtime::Runtime {
    let mut builder = Builder::new_multi_thread();
    builder.max_blocking_threads(jobs);
    builder.enable_io();
    builder.build().unwrap()
}

/// Provision an isolated per-commit working copy under `base_path` and check
/// out `commit`'s tree into it. Returns the working-copy directory and its
/// initialized `TreeState`, ready for the caller to spawn a command in and
/// later snapshot.
async fn create_working_copy(
    base_path: &Path,
    commit: &Commit,
) -> Result<(PathBuf, TreeState), RunError> {
    // Per-commit working-copy directory, keyed by commit id so concurrent jobs
    // don't collide and so the working copy is reproducible across runs.
    let commit_path = base_path.join(commit.id().hex());
    // A previous `jj run` that failed before cleanup may have left this
    // directory behind. Clear it so the working_copy/state subdirs below
    // start from a clean slate.
    if commit_path.exists() {
        tracing::debug!(
            dir = ?commit_path,
            commit = commit.id().hex(),
            "removing leftover directory from a previous run"
        );
        fs::remove_dir_all(&commit_path)
            .map_err(|e| RunError::PathCreationFailure(commit_path.clone(), e))?;
    }
    tracing::debug!(
        dir = ?commit_path,
        commit = commit.id().hex(),
        "creating directory for commit"
    );
    fs::create_dir(&commit_path)
        .map_err(|e| RunError::PathCreationFailure(commit_path.clone(), e))?;

    let (working_copy_dir, state_dir) = create_working_copy_paths(&commit_path).await?;
    let tree_state_settings = TreeStateSettings {
        conflict_marker_style: ConflictMarkerStyle::Snapshot,
        eol_conversion_mode: EolConversionMode::None,
        exec_change_setting: ExecChangeSetting::Auto,
        fsmonitor_settings: FsmonitorSettings::None,
    };
    let mut tree_state = TreeState::init(
        commit.store().clone(),
        working_copy_dir.clone(),
        state_dir,
        &tree_state_settings,
    )?;
    tree_state
        .check_out(&commit.tree())
        .map_err(|_| RunError::FailedCheckout(commit.id().clone()))?;

    Ok((working_copy_dir, tree_state))
}

/// Compute and ensure the base directory under which each per-commit working
/// copy lives. Per-commit setup happens inside `rewrite_commit` so that work
/// is parallelized across the runtime instead of serialized up front.
fn ensure_base_path(repo_path: &Path) -> Result<PathBuf, RunError> {
    // TODO: should be stored in a backend and not hardcoded.
    // The parent() call is needed to not write under `.jj/repo/`.
    let base_path = repo_path.parent().unwrap().join("run").join("default");
    if !base_path.exists() {
        tracing::debug!(?base_path, "does not exist, so creating it");
        fs::create_dir_all(&base_path)?;
    }
    Ok(base_path)
}

/// Get the shell to execute in and its first argument.
// TODO: use something like `[run].shell` (making it configurable).
fn get_shell_executable_with_first_arg() -> (&'static str, &'static str) {
    if cfg!(target_os = "windows") {
        ("cmd", "/c")
    } else {
        ("/bin/sh", "-c")
    }
}

/// The result of a single command invocation.
struct RunJob {
    /// The old `CommitId` of the commit.
    old_id: CommitId,
    /// The new tree generated from the commit. `None` when the command wasn't
    /// run (i.e. the commit was skipped).
    new_tree: Option<Tree>,
    /// Was the tree even modified.
    dirty: bool,
    /// Bytes the subprocess wrote to its stdout, captured in full.
    stdout: Vec<u8>,
    /// Bytes the subprocess wrote to its stderr, captured in full.
    stderr: Vec<u8>,
    /// True if the command wasn't run because the per-commit working directory
    /// (the subdirectory `jj run` was invoked from) didn't exist in this
    /// commit's tree.
    skipped: bool,
}

// TODO: make this more revset/commit stream friendly.
async fn run_inner(
    tx: &WorkspaceCommandTransaction<'_>,
    sender: Sender<RunJob>,
    handle: &tokio::runtime::Handle,
    shell_command: Arc<String>,
    subdir: Arc<PathBuf>,
    base_path: Arc<PathBuf>,
    commits: Arc<Vec<Commit>>,
) -> Result<(), RunError> {
    let base_ignores = tx.base_workspace_helper().base_ignores().unwrap().clone();
    let mut command_futures = JoinSet::new();
    for commit in commits.iter() {
        command_futures.spawn_on(
            rewrite_commit(
                // TODO: handle/propagate error here
                base_ignores.clone(),
                base_path.clone(),
                commit.clone(),
                shell_command.clone(),
                subdir.clone(),
            ),
            handle,
        );
    }

    while let Some(res) = command_futures.join_next().await {
        let done = match res {
            Ok(rj) => rj?,
            Err(err) => return Err(RunError::JobFailure(err)),
        };
        let should_quit = sender.send(done).await.is_err();
        if should_quit {
            tracing::debug!(
                ?should_quit,
                "receiver is no longer available, exiting loop"
            );
            break;
        }
    }
    Ok(())
}

/// Run `shell_command` against `commit`. The caller is responsible for
/// committing any returned new tree to the repo.
///
/// Each invocation provisions its own per-commit working copy under
/// `base_path` so multiple `rewrite_commit` futures can do their work in
/// parallel without contending on shared state.
async fn rewrite_commit(
    base_ignores: Arc<GitIgnoreFile>,
    base_path: Arc<PathBuf>,
    commit: Commit,
    shell_command: Arc<String>,
    subdir: Arc<PathBuf>,
) -> Result<RunJob, RunError> {
    let old_id = commit.id().clone();

    let (working_copy_dir, mut tree_state) = create_working_copy(&base_path, &commit).await?;

    // Resolve where the command should run. If the subdir doesn't exist in this
    // commit's checked-out tree, skip the commit entirely.
    let exec_dir = working_copy_dir.join(subdir.as_path());
    if !exec_dir.is_dir() {
        tracing::debug!(
            ?exec_dir,
            commit = old_id.hex(),
            "subdirectory does not exist in commit; skipping"
        );
        return Ok(RunJob {
            old_id,
            new_tree: None,
            dirty: false,
            stdout: Vec::new(),
            stderr: Vec::new(),
            skipped: true,
        });
    }

    // TODO: Later this should take some trait which allows `run` to integrate with
    // something like Bazels RE protocol.
    // e.g
    // ```
    // let mut executor /* Arc<dyn CommandExecutor> */ = store.get_executor();
    // let command = executor.spawn(...)?; // RE or separate processes depending on impl.
    // ...
    // ```
    tracing::debug!(
        "trying to run command '{command}' on commit {}",
        commit.id(),
        command = shell_command.as_str(),
    );
    let (prog, first_arg) = get_shell_executable_with_first_arg();
    // Pipe and buffer the subprocess's stdout/stderr so we can emit them
    // atomically to the parent's stdout/stderr after the process exits. Writing
    // concurrently from multiple jobs would interleave output.
    let command = tokio::process::Command::new(prog)
        .arg(first_arg)
        .arg(shell_command.as_str())
        // set cwd to the subdirectory inside the working copy.
        .current_dir(&exec_dir)
        // .arg()
        // TODO: relativize
        // .env("JJ_PATH", working_copy_dir)
        .env("JJ_CHANGE_ID", commit.change_id().reverse_hex())
        .env("JJ_COMMIT_ID", commit.id().hex())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true) // No zombies allowed.
        .spawn()?;

    let output = command.wait_with_output().await?;

    // TODO: Handle error here
    if !output.status.success() {
        return Err(RunError::CommandFailure(
            shell_command.to_string(),
            output.status,
            old_id.clone(),
        ));
    }

    let options = SnapshotOptions {
        base_ignores,
        // TODO: read from current wc/settings
        start_tracking_matcher: &EverythingMatcher,
        progress: None,
        // TODO: read from current wc/settings
        max_new_file_size: 64_000_u64, // 64 MB for now,
        force_tracking_matcher: &NothingMatcher,
    };
    tracing::debug!("trying to snapshot the new tree");
    let (dirty, _) = tree_state.snapshot(&options).await.unwrap();
    if !dirty {
        tracing::debug!(
            "commit {} was not modified as the passed command did not modify any tracked files",
            commit.id()
        );
    }

    let rewritten_id = tree_state.current_tree().tree_ids();
    let new_id = rewritten_id.as_resolved().unwrap();

    let new_tree = commit.store().get_tree(RepoPathBuf::root(), new_id).await?;

    // TODO: Serialize the new tree into /output/{id-tree} for a cache lookup
    // TODO: supersede with a custom workspace implementation

    Ok(RunJob {
        old_id,
        new_tree: Some(new_tree),
        dirty,
        stdout: output.stdout,
        stderr: output.stderr,
        skipped: false,
    })
}

/// Run a command across a set of revisions.
///
/// Checks out each revision in an isolated working copy, runs the command, then
/// amends the revision with the resulting working copy. Descendants are rebased
/// on top of the amended revisions.
///
/// The command is executed with the following environment variables set:
///
/// - JJ_CHANGE_ID
/// - JJ_COMMIT_ID
///
/// # Example
///
/// ```shell
/// # Run pre-commit on your local work
/// $ jj run 'pre-commit run .github/pre-commit.yaml' -j 4
/// ```
// TODO: align interface with `jj bisect run --`
#[derive(clap::Args, Clone, Debug)]
#[command(verbatim_doc_comment)]
pub struct RunArgs {
    /// The command to run across all selected revisions.
    // TODO: align with bisect run --
    shell_command: String,

    /// The revisions to change.
    #[arg(
        long = "revision",
        short,
        default_value = "reachable(@, mutable())",
        value_name = "REVSETS",
        alias = "revisions"
    )]
    revisions: Vec<RevisionArg>,

    /// A no-op option to match the interface of `git rebase -x`.
    #[arg(short = 'x', hide = true)]
    exec: bool,

    /// How many processes should run in parallel, uses by default all cores.
    #[arg(long, short)]
    jobs: Option<usize>,

    /// Run the command from the working-copy root in each commit instead of
    /// from the subdirectory `jj run` was invoked from.
    #[arg(long)]
    root: bool,
}

pub async fn cmd_run(
    ui: &mut Ui,
    command: &CommandHelper,
    args: &RunArgs,
) -> Result<(), CommandError> {
    let mut workspace_command = command.workspace_helper(ui).await?;
    // The commits are already returned in reverse topological order.
    let resolved_commits: Vec<_> = workspace_command
        .parse_union_revsets(ui, &args.revisions)?
        .evaluate_to_commits()?
        .try_collect()
        .await?;

    // Running on the root commit (or any immutable commit) is nonsensical;
    // silently drop those from the input and bail if nothing remains.
    let root_id = workspace_command.repo().store().root_commit_id().clone();
    let resolved_commits: Vec<_> = resolved_commits
        .into_iter()
        .filter(|c| c.id() != &root_id)
        .collect();
    if resolved_commits.is_empty() {
        return Ok(());
    }

    workspace_command
        .check_rewritable(resolved_commits.iter().ids())
        .await?;
    // Jobs are resolved in this order:
    // 1. Commandline argument iff > 0.
    // 2. the amount of cores available.
    // 3. a single job, if all of the above fails.
    let jobs = match args.jobs {
        Some(0) | None => std::thread::available_parallelism().map(|t| t.into()).ok(),
        Some(jobs) => Some(jobs),
    }
    // Fallback to a single user-visible job.
    .unwrap_or(1usize);
    tracing::debug!(?jobs, "starting with `jj run` with available threads");

    let rt = get_runtime(jobs);
    // TODO: Add a extension point for custom output/status aggregation.
    let mut done_commits = HashSet::new();
    let (sender_tx, mut receiver) = mpsc::channel(resolved_commits.len());

    // Run each command from the subdirectory the user invoked `jj run` from,
    // unless `--root` overrides that. The subdir is relative to the workspace
    // root, which is canonical (per `CommandHelper::cwd` docs).
    let subdir = if args.root {
        PathBuf::new()
    } else {
        command
            .cwd()
            .strip_prefix(workspace_command.workspace_root())
            .map(Path::to_path_buf)
            .unwrap_or_default()
    };

    let store = workspace_command.repo().store().clone();
    let mut tx = workspace_command.start_transaction();
    let repo_path = tx.base_workspace_helper().repo_path();

    // Per-commit working copies are now created on demand inside
    // `rewrite_commit`; we just need the parent directory to exist.
    let base_path = Arc::new(ensure_base_path(repo_path)?);
    let stored_len = resolved_commits.len();

    let shell_command = args.shell_command.clone();
    // Start all the jobs.
    run_inner(
        &tx,
        sender_tx,
        rt.handle(),
        Arc::new(shell_command.clone()),
        Arc::new(subdir.clone()),
        base_path,
        Arc::new(resolved_commits.clone()),
    )
    .await?;

    let mut rewritten_commits = HashMap::new();
    let mut visited = 0;
    loop {
        if let Some(res) = receiver.recv().await {
            if res.skipped {
                writeln!(
                    ui.stderr(),
                    "Skipped commit {}: directory does not exist: {}",
                    res.old_id.hex(),
                    subdir.display(),
                )?;
                visited += 1;
                if visited == stored_len {
                    break;
                }
                continue;
            }
            // Emit the subprocess's captured streams. Acquiring `ui.stdout()` /
            // `ui.stderr()` for the duration of the write keeps each commit's
            // output from interleaving with another's.
            if !res.stdout.is_empty() {
                let mut out = ui.stdout();
                out.write_all(&res.stdout)?;
            }
            if !res.stderr.is_empty() {
                let mut err = ui.stderr();
                err.write_all(&res.stderr)?;
            }
            if res.dirty
                && let Some(new_tree) = res.new_tree
            {
                done_commits.insert(res.old_id.clone());
                rewritten_commits.insert(res.old_id.clone(), new_tree);
            }
            visited += 1;
        }
        if visited == stored_len {
            break;
        }
    }
    drop(receiver);

    let run_path = repo_path.parent().unwrap().join("run").join("default");
    // The operation was a no-op, bail.
    if rewritten_commits.is_empty() {
        // Yeet everything, caching is better implemented in a follow-up.
        fs::remove_dir_all(&run_path)?;

        writeln!(
            ui.stderr(),
            "No commits were rewritten as the command did not modify any tracked files"
        )?;
        tx.finish(
            ui,
            format!("run: No-op on {visited} commits with {shell_command}"),
        )
        .await?;
        return Ok(());
    }

    // The command did something, so rewrite the commits.
    let mut count: u32 = 0;
    // TODO: handle the `--reparent` case here.
    tx.repo_mut()
        .transform_descendants(
            resolved_commits.iter().ids().cloned().collect_vec(),
            async |rewriter| {
                let old_id = rewriter.old_commit().id().clone();
                let builder = rewriter.rebase().await?;
                // Only rewrite the tree if the command changed it. Descendants
                // that weren't part of the input set still need to be rebased
                // but keep their original tree.
                if let Some(new_tree) = rewritten_commits.get(&old_id) {
                    let new_tree_id = new_tree.id().clone();
                    count += 1;
                    builder
                        .set_tree(MergedTree::resolved(store.clone(), new_tree_id))
                        .write()
                        .await?;
                } else {
                    builder.write().await?;
                }
                Ok(())
            },
        )
        .await?;
    writeln!(ui.stderr(), "Rewrote {count} commits with {shell_command}")?;

    // Yeet everything, caching is implemented in a follow-up.
    fs::remove_dir_all(&run_path)?;

    tx.finish(
        ui,
        format!("run: rewrite {count} commits with {shell_command}"),
    )
    .await?;

    Ok(())
}
