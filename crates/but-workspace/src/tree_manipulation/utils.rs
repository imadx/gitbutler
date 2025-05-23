//! Utility types related to discarding changes in the worktree.

use anyhow::Context;
use bstr::{BString, ByteSlice as _, ByteVec};
use but_core::{ChangeState, TreeStatus};
use but_rebase::{RebaseOutput, RebaseStep};
use but_status::create_wd_tree;
use std::{
    collections::{HashMap, HashSet},
    path::Path,
};

use crate::{DiffSpec, HunkHeader, commit_engine::apply_hunks, relapath::RelaPath as _};

use super::hunk::{HunkSubstraction, subtract_hunks};

pub(crate) fn checkout_repo_worktree(
    parent_worktree_dir: &Path,
    mut repo: gix::Repository,
) -> anyhow::Result<()> {
    // No need to cache anything, it's just single-use for the most part.
    repo.object_cache_size(0);
    let mut index = repo.index_from_tree(&repo.head_tree_id_or_empty()?)?;
    if index.entries().is_empty() {
        // The worktree directory is created later, so we don't have to deal with it here.
        return Ok(());
    }
    for entry in index.entries_mut().iter_mut().filter(|e| {
        e.mode
            .contains(gix::index::entry::Mode::DIR | gix::index::entry::Mode::COMMIT)
    }) {
        entry.flags.insert(gix::index::entry::Flags::SKIP_WORKTREE);
    }

    let mut opts =
        repo.checkout_options(gix::worktree::stack::state::attributes::Source::IdMapping)?;
    opts.destination_is_initially_empty = true;
    opts.keep_going = true;

    let checkout_destination = repo.workdir().context("non-bare repository")?.to_owned();
    if !checkout_destination.exists() {
        std::fs::create_dir(&checkout_destination)?;
    }
    let sm_repo_dir = gix::path::relativize_with_prefix(
        repo.path().strip_prefix(parent_worktree_dir)?,
        checkout_destination.strip_prefix(parent_worktree_dir)?,
    )
    .into_owned();
    let out = gix::worktree::state::checkout(
        &mut index,
        checkout_destination.clone(),
        repo,
        &gix::progress::Discard,
        &gix::progress::Discard,
        &gix::interrupt::IS_INTERRUPTED,
        opts,
    )?;

    let mut buf = BString::from("gitdir: ");
    buf.extend_from_slice(&gix::path::os_string_into_bstring(sm_repo_dir.into())?);
    buf.push_byte(b'\n');
    std::fs::write(checkout_destination.join(".git"), &buf)?;

    tracing::debug!(directory = ?checkout_destination, outcome = ?out, "submodule checkout result");
    Ok(())
}

/// Takes a rebase output and returns the commit mapping with any extra
/// mapping overrides provided.
///
/// This will only include commits that have actually changed. If a commit was
/// mapped to itself it will not be included in the resulting HashMap.
///
/// Overrides are used to handle the case where the caller of the rebase engine
/// has manually replaced a particular commit with a rewritten one. This is
/// needed because a manually re-written commit that ends up matching the
/// base when the rebase occurs will end up showing up as a no-op in the
/// resulting commit_mapping.
///
/// Overrides should be provided as a vector that contains tuples of object
/// ids, where the first item is the before object_id, and the second item is
/// the after object_id.
pub(crate) fn rebase_mapping_with_overrides(
    rebase_output: &RebaseOutput,
    overrides: impl IntoIterator<Item = (gix::ObjectId, gix::ObjectId)>,
) -> HashMap<gix::ObjectId, gix::ObjectId> {
    let mut mapping = rebase_output
        .commit_mapping
        .iter()
        .filter(|(_, old, new)| old != new)
        .map(|(_, old, new)| (*old, *new))
        .collect::<HashMap<_, _>>();

    for (old, new) in overrides {
        if old != new {
            mapping.insert(old, new);
        }
    }

    mapping
}

pub(crate) fn index_entries_to_update(
    status_changes: Vec<gix::status::Item>,
) -> anyhow::Result<HashSet<BString>> {
    let mut head_to_index = vec![];
    let mut index_to_worktree = HashSet::new();

    for change in status_changes {
        match change {
            gix::status::Item::IndexWorktree(change) => {
                index_to_worktree.insert(change.rela_path().to_owned());
            }
            gix::status::Item::TreeIndex(change) => {
                head_to_index.push(change.rela_path().to_owned());
            }
        }
    }

    let mut paths_to_update = HashSet::new();

    for path in head_to_index {
        if !index_to_worktree.contains(&path) {
            paths_to_update.insert(path);
        }
    }

    Ok(paths_to_update)
}

pub(crate) fn update_wd_to_tree(
    repository: &gix::Repository,
    source_tree: gix::ObjectId,
) -> anyhow::Result<()> {
    let source_tree = repository.find_tree(source_tree)?;
    let wd_tree = create_wd_tree(repository, 0)?;
    let wt_changes = but_core::diff::tree_changes(repository, Some(wd_tree), source_tree.id)?;

    let mut path_check = gix::status::plumbing::SymlinkCheck::new(
        repository.workdir().context("non-bare repository")?.into(),
    );

    for change in wt_changes.0 {
        match &change.status {
            TreeStatus::Deletion { .. } => {
                // Work tree has the file but the source tree doesn't.
                std::fs::remove_file(path_check.verified_path(&change.path)?)?;
            }
            TreeStatus::Addition { .. } => {
                let entry = source_tree
                    .lookup_entry(change.path.clone().split_str("/"))?
                    .context("path must exist")?;
                // Work tree doesn't have the file but the source tree does.
                write_entry(
                    change.path.as_bstr(),
                    &entry,
                    &mut path_check,
                    WriteKind::Addition,
                )?;
            }
            TreeStatus::Modification { .. } => {
                let entry = source_tree
                    .lookup_entry(change.path.clone().split_str("/"))?
                    .context("path must exist")?;
                // Work tree doesn't have the file but the source tree does.
                write_entry(
                    change.path.as_bstr(),
                    &entry,
                    &mut path_check,
                    WriteKind::Modification,
                )?;
            }
            TreeStatus::Rename { previous_path, .. } => {
                let entry = source_tree
                    .lookup_entry(change.path.clone().split_str("/"))?
                    .context("path must exist")?;
                // Work tree has the file under `previous_path`, but the source tree wants it under `path`.
                let previous_path = path_check.verified_path(previous_path)?;
                if std::path::Path::new(&previous_path).is_dir() {
                    // We don't want to remove the directory as it might
                    // contain other files.
                } else {
                    std::fs::remove_file(previous_path)?;
                }
                write_entry(
                    change.path.as_bstr(),
                    &entry,
                    &mut path_check,
                    WriteKind::Addition,
                )?;
            }
        }
    }

    Ok(())
}

#[derive(Debug, Clone, Copy)]
enum WriteKind {
    Addition,
    Modification,
}

fn write_entry(
    relative_path: &bstr::BStr,
    entry: &gix::object::tree::Entry<'_>,
    path_check: &mut gix::status::plumbing::SymlinkCheck,
    write_kind: WriteKind,
) -> anyhow::Result<()> {
    match entry.mode().kind() {
        gix::objs::tree::EntryKind::Tree => {
            unreachable!(
                "The tree changes produced from the diff will always be a file-like entry"
            );
        }
        gix::objs::tree::EntryKind::Blob | gix::objs::tree::EntryKind::BlobExecutable => {
            let mut blob = entry.object()?.into_blob();
            let path = path_check.verified_path_allow_nonexisting(relative_path)?;
            prepare_path(&path)?;
            std::fs::write(&path, blob.take_data())?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt as _;
                if entry.mode().kind() == gix::objs::tree::EntryKind::BlobExecutable {
                    let mut permissions = std::fs::metadata(&path)?.permissions();
                    // Set the executable bit
                    permissions.set_mode(permissions.mode() | 0o111);
                    std::fs::set_permissions(&path, permissions)?;
                } else {
                    let mut permissions = std::fs::metadata(&path)?.permissions();
                    // Unset the executable bit
                    permissions.set_mode(permissions.mode() & !0o111);
                    std::fs::set_permissions(&path, permissions)?;
                }
            }
        }
        gix::objs::tree::EntryKind::Link => {
            let blob = entry.object()?.into_blob();
            let link_target = gix::path::from_bstr(blob.data.as_bstr());
            let path = path_check.verified_path_allow_nonexisting(relative_path)?;
            prepare_path(&path)?;
            gix::fs::symlink::create(&link_target, &path)?;
        }
        gix::objs::tree::EntryKind::Commit => match write_kind {
            WriteKind::Modification => {
                let path = path_check.verified_path_allow_nonexisting(relative_path)?;
                let out = std::process::Command::from(
                    gix::command::prepare(format!(
                        "git reset --hard {id} && git clean -fxd",
                        id = entry.id()
                    ))
                    .with_shell(),
                )
                .current_dir(&path)
                .output()?;
                if !out.status.success() {
                    anyhow::bail!(
                        "Could not reset submodule at '{sm_dir}' to commit {id}: {err}",
                        sm_dir = path.display(),
                        id = entry.id(),
                        err = out.stderr.as_bstr()
                    );
                }
            }
            WriteKind::Addition => {
                let sm_repo = entry
                    .repo
                    .submodules()?
                    .into_iter()
                    .flatten()
                    .find_map(|sm| {
                        let is_active = sm.is_active().ok()?;
                        is_active.then(|| -> anyhow::Result<_> {
                            Ok(
                                if sm
                                    .path()
                                    .ok()
                                    .is_some_and(|sm_path| sm_path == relative_path)
                                {
                                    sm.open()?
                                } else {
                                    None
                                },
                            )
                        })
                    })
                    .transpose()?
                    .flatten();
                match sm_repo {
                    None => {
                        // A directory is what git creates with `git restore` even if the thing to restore is a submodule.
                        // We are trying to be better than that if we find a submodule, hoping that this is what users expect.
                        // We do that as baseline as there is no need to fail here.
                    }
                    Some(repo) => {
                        // We will only restore the submodule if there is a local clone already available, to avoid any network
                        // activity that would likely happen during an actual clone.
                        // Thus, all we have to do is to check out the submodule.
                        // TODO(gix): find a way to deal with nested submodules - they should also be checked out which
                        //            isn't done by `gitoxide`, but probably should be an option there.

                        let wt_root = path_check.inner.root().to_owned();
                        checkout_repo_worktree(&wt_root, repo)?;
                    }
                }
                let path = path_check.verified_path_allow_nonexisting(relative_path)?;
                std::fs::create_dir(path).or_else(|err| {
                    if err.kind() == std::io::ErrorKind::AlreadyExists {
                        Ok(())
                    } else {
                        Err(err)
                    }
                })?;
            }
        },
    };

    Ok(())
}

fn prepare_path(path: &std::path::Path) -> anyhow::Result<()> {
    let parent = path.parent().context("paths will always have a parent")?;
    if std::fs::exists(parent)? {
        if !std::path::Path::new(&parent).is_dir() {
            std::fs::remove_file(parent)?;
            std::fs::create_dir_all(parent)?;
        }
    } else {
        std::fs::create_dir_all(parent)?;
    }
    if std::fs::exists(path)? {
        if std::path::Path::new(&path).is_dir() {
            std::fs::remove_dir_all(path)?;
        } else {
            std::fs::remove_file(path)?;
        }
    }
    Ok(())
}

pub enum ChangesSource {
    Worktree,
    #[allow(dead_code)]
    Commit {
        id: gix::ObjectId,
    },
    #[allow(dead_code)]
    Tree {
        after_id: gix::ObjectId,
        before_id: gix::ObjectId,
    },
}

impl ChangesSource {
    fn before<'a>(&self, repository: &'a gix::Repository) -> anyhow::Result<gix::Tree<'a>> {
        match self {
            ChangesSource::Worktree => {
                Ok(repository.find_tree(repository.head_tree_id_or_empty()?)?)
            }
            ChangesSource::Commit { id } => {
                let commit = repository.find_commit(*id)?;
                let parent_id = commit.parent_ids().next().context("no parent")?;
                let parent = repository.find_commit(parent_id)?;
                Ok(parent.tree()?)
            }
            ChangesSource::Tree { before_id, .. } => Ok(repository.find_tree(*before_id)?),
        }
    }

    fn after<'a>(&self, repository: &'a gix::Repository) -> anyhow::Result<gix::Tree<'a>> {
        match self {
            ChangesSource::Worktree => {
                let wd_tree = create_wd_tree(repository, 0)?;
                Ok(repository.find_tree(wd_tree)?)
            }
            ChangesSource::Commit { id } => Ok(repository.find_commit(*id)?.tree()?),
            ChangesSource::Tree { after_id, .. } => Ok(repository.find_tree(*after_id)?),
        }
    }
}

/// Discard the given `changes` in either the work tree or an arbitrary commit or tree. If a change could not be matched with an
/// actual worktree change, for instance due to a race, that's not an error, instead it will be returned in the result Vec, along
/// with all hunks that couldn't be matched.
///
/// The returned Vec is typically empty, meaning that all `changes` could be discarded.
///
/// `context_lines` is the amount of context lines we should assume when obtaining hunks of worktree changes to match against
/// the ones we have specified in the hunks contained within `changes`.
///
/// Discarding a change is really more of an 'undo' of a change as it will restore the previous state to the desired extent - Git
/// doesn't have a notion of this on a whole-file basis.
///
/// Each of the `changes` will be matched against actual worktree changes to make this operation as safe as possible, after all, it
/// discards changes without recovery.
///
/// In practice, this is like a selective 'inverse-checkout', as such it must have a lot of the capabilities of checkout, but focussed
/// on just a couple of paths, and with special handling for renamed files, something that `checkout` can't naturally handle
/// as it's only dealing with single file-paths.
///
/// ### Hunk-based discarding
///
/// When an instance in `changes` contains hunks, these are the hunks to be discarded. If they match a whole hunk in the worktree changes,
/// it will be discarded entirely, simply by not applying it.
///
/// ### Sub-Hunk discarding
///
/// It's possible to specify ranges of hunks to discard. To do that, they need an *anchor*. The *anchor* is the pair of
/// `(line_number, line_count)` that should not be changed, paired with the *other* pair with the new `(line_number, line_count)`
/// to discard.
///
/// For instance, when there is a single patch `-1,10 +1,10` and we want to bring back the removed 5th line *and* the added 5th line,
/// we'd specify *just* two selections, one in the old via `-5,1 +1,10` and one in the new via `-1,10 +5,1`.
/// This works because internally, it will always match the hunks (and sub-hunks) with their respective pairs obtained through a
/// worktree status.
pub fn create_tree_without_diff(
    repository: &gix::Repository,
    changes_source: ChangesSource,
    changes_to_discard: impl IntoIterator<Item = DiffSpec>,
    context_lines: u32,
) -> anyhow::Result<(gix::ObjectId, Vec<DiffSpec>)> {
    let mut dropped = Vec::new();

    let before = changes_source.before(repository)?;
    let after = changes_source.after(repository)?;

    let mut builder = repository.edit_tree(after.id())?;

    for change in changes_to_discard {
        let before_path = change
            .previous_path_bytes
            .clone()
            .unwrap_or_else(|| change.path_bytes.clone());
        let before_entry = before.lookup_entry(before_path.clone().split_str("/"))?;

        let Some(after_entry) = after.lookup_entry(change.path_bytes.clone().split_str("/"))?
        else {
            let Some(before_entry) = before_entry else {
                // If there is no before entry and no after entry, then
                // something has gone wrong.
                dropped.push(change);
                continue;
            };

            if change.hunk_headers.is_empty() {
                // If there is no after_change, then it must have been deleted.
                // Therefore, we can just add it again.
                builder.upsert(
                    change.path_bytes.as_bstr(),
                    before_entry.mode().kind(),
                    before_entry.object_id(),
                )?;
                continue;
            } else {
                anyhow::bail!(
                    "Deletions or additions aren't well-defined for hunk-based operations - use the whole-file mode instead"
                );
            }
        };

        match after_entry.mode().kind() {
            gix::objs::tree::EntryKind::Blob | gix::objs::tree::EntryKind::BlobExecutable => {
                let after_blob = after_entry.object()?.into_blob();
                if change.hunk_headers.is_empty() {
                    revert_file_to_before_state(&before_entry, &mut builder, &change)?;
                } else {
                    let Some(before_entry) = before_entry else {
                        anyhow::bail!(
                            "Deletions or additions aren't well-defined for hunk-based operations - use the whole-file mode instead"
                        );
                    };

                    let diff = but_core::UnifiedDiff::compute(
                        repository,
                        change.path_bytes.as_bstr(),
                        Some(before_path.as_bstr()),
                        ChangeState {
                            id: after_entry.id().detach(),
                            kind: after_entry.mode().kind(),
                        },
                        ChangeState {
                            id: before_entry.id().detach(),
                            kind: before_entry.mode().kind(),
                        },
                        context_lines,
                    )?;

                    let but_core::UnifiedDiff::Patch {
                        hunks: diff_hunks, ..
                    } = diff
                    else {
                        anyhow::bail!("expected a patch");
                    };

                    let mut good_hunk_headers = vec![];
                    let mut bad_hunk_headers = vec![];

                    for hunk in &change.hunk_headers {
                        if diff_hunks
                            .iter()
                            .any(|diff_hunk| HunkHeader::from(diff_hunk.clone()).contains(*hunk))
                        {
                            good_hunk_headers.push(*hunk);
                        } else {
                            bad_hunk_headers.push(*hunk);
                        }
                    }

                    if !bad_hunk_headers.is_empty() {
                        dropped.push(DiffSpec {
                            previous_path_bytes: change.previous_path_bytes.clone(),
                            path_bytes: change.path_bytes.clone(),
                            hunk_headers: bad_hunk_headers,
                        });
                    }

                    // TODO: Validate that the hunks coorespond with actual changes?
                    let before_blob = before_entry.object()?.into_blob();

                    let new_hunks = new_hunks_after_removals(
                        diff_hunks.into_iter().map(Into::into).collect(),
                        good_hunk_headers,
                    )?;
                    let new_after_contents = apply_hunks(
                        before_blob.data.as_bstr(),
                        after_blob.data.as_bstr(),
                        &new_hunks,
                    )?;
                    let mode = if new_after_contents == before_blob.data {
                        before_entry.mode().kind()
                    } else {
                        after_entry.mode().kind()
                    };
                    let new_after_contents = repository.write_blob(&new_after_contents)?;

                    // Keep the mode of the after state. We _should_ at some
                    // point introduce the mode specifically as part of the
                    // DiscardSpec, but for now, we can just use the after state.
                    builder.upsert(change.path_bytes.as_bstr(), mode, new_after_contents)?;
                }
            }
            _ => {
                revert_file_to_before_state(&before_entry, &mut builder, &change)?;
            }
        }
    }

    let final_tree = builder.write()?;
    Ok((final_tree.detach(), dropped))
}

fn new_hunks_after_removals(
    change_hunks: Vec<HunkHeader>,
    mut removal_hunks: Vec<HunkHeader>,
) -> anyhow::Result<Vec<HunkHeader>> {
    // If a removal hunk matches completly then we can drop it entirely.
    let hunks_to_keep: Vec<HunkHeader> = change_hunks
        .into_iter()
        .filter(|hunk| {
            match removal_hunks
                .iter()
                .enumerate()
                .find_map(|(idx, hunk_to_discard)| (hunk_to_discard == hunk).then_some(idx))
            {
                None => true,
                Some(idx_to_remove) => {
                    removal_hunks.remove(idx_to_remove);
                    false
                }
            }
        })
        .collect();

    // TODO(perf): instead of brute-force searching, assure hunks_to_discard are sorted and speed up the search that way.
    let mut hunks_to_keep_with_splits = Vec::new();
    for hunk_to_split in hunks_to_keep {
        let mut subtractions = Vec::new();
        removal_hunks.retain(|sub_hunk_to_discard| {
            if sub_hunk_to_discard.old_range() == hunk_to_split.old_range() {
                subtractions.push(HunkSubstraction::New(sub_hunk_to_discard.new_range()));
                false
            } else if sub_hunk_to_discard.new_range() == hunk_to_split.new_range() {
                subtractions.push(HunkSubstraction::Old(sub_hunk_to_discard.old_range()));
                false
            } else {
                true
            }
        });
        if subtractions.is_empty() {
            hunks_to_keep_with_splits.push(hunk_to_split);
        } else {
            let hunk_with_subtractions = subtract_hunks(hunk_to_split, subtractions)?;
            hunks_to_keep_with_splits.extend(hunk_with_subtractions);
        }
    }
    Ok(hunks_to_keep_with_splits)
}

fn revert_file_to_before_state(
    before_entry: &Option<gix::object::tree::Entry<'_>>,
    builder: &mut gix::object::tree::Editor<'_>,
    change: &DiffSpec,
) -> Result<(), anyhow::Error> {
    // If there are no hunk headers, then we want to revert the
    // whole file to the state it was in before tree.
    if let Some(before_entry) = before_entry {
        builder.remove(change.path_bytes.as_bstr())?;
        builder.upsert(
            change
                .previous_path_bytes
                .clone()
                .unwrap_or(change.path_bytes.clone())
                .as_bstr(),
            before_entry.mode().kind(),
            before_entry.object_id(),
        )?;
    } else {
        builder.remove(change.path_bytes.as_bstr())?;
    }
    Ok(())
}

pub fn replace_pick_with_commit(
    steps: &mut Vec<RebaseStep>,
    target_commit_id: gix::ObjectId,
    replacement_commit_id: gix::ObjectId,
) -> anyhow::Result<()> {
    let mut found = false;
    for step in steps {
        if step.commit_id() != Some(&target_commit_id) {
            continue;
        }
        let RebaseStep::Pick { commit_id, .. } = step else {
            continue;
        };
        found = true;
        *commit_id = replacement_commit_id;
    }

    if found {
        Ok(())
    } else {
        Err(anyhow::anyhow!(
            "Failed to replace pick step {} with {}",
            target_commit_id,
            replacement_commit_id
        ))
    }
}
