// SPDX-License-Identifier: GPL-2.0-only

//! Modify the StGit stack state atomically.
//!
//! Modifying the StGit stack typically involves performing a sequence of fallible
//! operations, where each operation depends on the previous. The stack transaction
//! mechanism found in this module allows these operations to be performed in an
//! all-or-nothing fashion such that the stack, working tree, and index will either
//! successfully transition to their new state or fallback to their starting state.
//!
//! The entry point to stack transactions is via the `Stack::setup_transaction()`
//! method. The transaction operations are defined in a closure passed to the (required)
//! `transact()` method. And the transaction is finalized via the `execute()` method.
//!
//! # Example
//!
//! ```no_run
//! let new_stack = stack
//!     .setup_transaction()
//!     .with_output_stream(...)
//!     ...  // Transaction option method calls
//!     .transact(|trans| {
//!         // Call StackTransaction methods
//!         ...
//!     })
//!     .execute("<reflog message>")?;
//! ```

mod builder;
mod options;
mod ui;

use std::collections::BTreeMap;

use anyhow::{anyhow, Result};
use indexmap::IndexSet;

pub(crate) use self::builder::TransactionBuilder;
use self::options::{ConflictMode, TransactionOptions};
use self::ui::TransactionUserInterface;

use crate::{
    commit::{CommitExtended, RepositoryCommitExtended},
    patchname::PatchName,
    signature::SignatureExtended,
    stack::{PatchState, Stack, StackStateAccess},
    stupid::{Stupid, StupidContext},
};

use super::{error::Error, state::StackState};

/// Stack transaction state.
pub(crate) struct StackTransaction<'repo> {
    stack: Stack<'repo>,
    ui: TransactionUserInterface,
    options: TransactionOptions,

    applied: Vec<PatchName>,
    unapplied: Vec<PatchName>,
    hidden: Vec<PatchName>,
    updated_patches: BTreeMap<PatchName, Option<PatchState<'repo>>>,
    updated_head: Option<git2::Commit<'repo>>,
    updated_base: Option<git2::Commit<'repo>>,

    current_tree_id: git2::Oid,
    error: Option<anyhow::Error>,
}

/// Status of a pushed patch.
///
/// Pushing a patch successfully may result in one of several states. This status is
/// used for control flow in the event of conflicts as well as to fine-tune the
/// user-facing output of push operations.
#[derive(Debug, PartialEq, Clone, Copy)]
enum PushStatus {
    /// The pushed patch is newly added to the stack.
    New,

    /// The pushed patch's changes have been determined to have already been merged into
    /// the stack's base tree.
    AlreadyMerged,

    /// The push resulted in merge conflicts.
    Conflict,

    /// The push resulted in the patch's diff becoming empty.
    Empty,

    /// The push resulted in the patch's diff being modified.
    Modified,

    /// The push resulted in the patch's diff remaining the same.
    Unmodified,
}

/// Context for executing a [`StackTransaction`].
///
/// Wraps [`StackTransaction`] to ensure [`ExecuteContext::execute()`] is called after
/// [`TransactionBuilder::transact()`].
pub(crate) struct ExecuteContext<'repo>(StackTransaction<'repo>);

impl<'repo> ExecuteContext<'repo> {
    /// Execute the transaction.
    ///
    /// If any of the transaction operations (i.e. from `transact()`) fail, the
    /// stack, index, and worktree state will be rolled back.
    ///
    /// A new `Stack` instance is returned.
    pub(crate) fn execute(self, reflog_msg: &str) -> Result<Stack<'repo>> {
        let transaction = self.0;

        // Check consistency
        for (patchname, oid) in transaction.updated_patches.iter() {
            if oid.is_none() {
                assert!(transaction.stack.has_patch(patchname));
            } else {
                assert!(transaction.all_patches().any(|pn| pn == patchname));
            }
        }

        let trans_head = transaction.head().clone();

        let StackTransaction {
            stack,
            mut ui,
            mut options,
            applied,
            unapplied,
            hidden,
            updated_patches,
            current_tree_id,
            error,
            ..
        } = transaction;

        // Only proceed for halt errors
        let has_conflicts = if let Some(err) = &error {
            match err.downcast_ref::<Error>() {
                Some(Error::TransactionHalt { conflicts, .. }) => *conflicts,
                _ => return Err(error.unwrap()),
            }
        } else {
            false
        };

        // Log external modifications
        let mut stack = if stack.is_head_top() {
            stack
        } else {
            // TODO: why update the stack state ref unconditional of transaction.error?
            stack.log_external_mods()?
        };

        if options.set_head {
            if options.use_index_and_worktree {
                let stack_head = stack.branch_head.clone();
                checkout(
                    &stack,
                    &options,
                    current_tree_id,
                    applied.last(),
                    &trans_head,
                )
                .or_else(|err| {
                    options.allow_bad_head = true;
                    checkout(
                        &stack,
                        &options,
                        current_tree_id,
                        applied.last(),
                        &stack_head,
                    )?;
                    Err(anyhow!(
                        "{err}\n\
                         Command aborted (all changes rolled back)"
                    ))
                })?;
            }

            let updated_ref = stack
                .branch
                .get_mut()
                .set_target(trans_head.id(), reflog_msg)?;
            stack.update_head(git2::Branch::wrap(updated_ref), trans_head.clone());
        }

        let conflict_msg;
        let reflog_msg = if has_conflicts {
            conflict_msg = format!("{reflog_msg} (CONFLICT)");
            &conflict_msg
        } else {
            reflog_msg
        };

        // Update patch refs and stack state refs
        let repo = stack.repo;
        let mut git_trans = repo.transaction()?;
        let reflog_signature = None; // Use default signature

        git_trans.lock_ref(&stack.refname)?;

        for (patchname, maybe_patch) in &updated_patches {
            let patch_refname = stack.patch_refname(patchname);
            let state = stack.state_mut();
            git_trans.lock_ref(&patch_refname)?;

            if let Some(patch) = maybe_patch {
                git_trans.set_target(
                    &patch_refname,
                    patch.commit.id(),
                    reflog_signature,
                    reflog_msg,
                )?;
                state.patches.insert(patchname.clone(), patch.clone());
            } else {
                git_trans.remove(&patch_refname)?;
                state.patches.remove(patchname);
            }
        }

        if !ui.printed_top() {
            let new_top_patchname = applied.last().cloned();
            if let Some(top_patchname) = new_top_patchname.as_ref() {
                ui.print_pushed(top_patchname, PushStatus::Unmodified, true)?;
            }
        }

        let stack_ref = repo.find_reference(&stack.refname)?;
        let prev_state_commit = stack_ref.peel_to_commit()?;

        let state = stack.state_mut();
        state.prev = Some(prev_state_commit);
        state.head = trans_head;
        state.applied = applied;
        state.unapplied = unapplied;
        state.hidden = hidden;

        let state_commit_id = state.commit(repo, None, reflog_msg)?;
        git_trans.set_target(
            &stack.refname,
            state_commit_id,
            reflog_signature,
            reflog_msg,
        )?;

        git_trans.commit()?;

        if let Some(err) = error {
            Err(err)
        } else {
            Ok(stack)
        }
    }
}

fn checkout(
    stack: &Stack,
    options: &TransactionOptions,
    current_tree_id: git2::Oid,
    trans_top: Option<&PatchName>,
    commit: &git2::Commit<'_>,
) -> Result<()> {
    if !options.allow_bad_head {
        stack.check_head_top_mismatch()?;
    }

    let stupid = stack.repo.stupid();

    if current_tree_id == commit.tree_id() && !options.discard_changes {
        match options.conflict_mode {
            ConflictMode::Allow => {}
            ConflictMode::AllowIfSameTop => {
                if trans_top.is_none() || trans_top != stack.applied().last() {
                    stupid.statuses(None)?.check_conflicts()?;
                }
            }
            ConflictMode::Disallow => stupid.statuses(None)?.check_conflicts()?,
        };
    } else if options.discard_changes {
        stupid.read_tree_checkout_hard(commit.tree_id())?;
    } else {
        stupid.update_index_refresh()?;
        stupid
            .read_tree_checkout(current_tree_id, commit.tree_id())
            .map_err(|e| Error::CheckoutConflicts(format!("{e:#}")))?;
    }

    Ok(())
}

impl<'repo> StackTransaction<'repo> {
    /// Get an immutable reference to the original stack.
    pub(crate) fn stack(&self) -> &Stack<'repo> {
        &self.stack
    }

    /// Get a reference to the repo.
    pub(crate) fn repo(&self) -> &'repo git2::Repository {
        self.stack.repo
    }

    /// Reset stack to a previous stack state.
    pub(crate) fn reset_to_state(&mut self, state: StackState<'repo>) -> Result<()> {
        for pn in self.all_patches().cloned().collect::<Vec<_>>() {
            self.updated_patches.insert(pn, None);
        }
        let StackState {
            prev: _prev,
            head,
            applied,
            unapplied,
            hidden,
            patches,
        } = state;
        self.updated_base = Some(if let Some(pn) = applied.first() {
            patches[pn].commit.parent(0)?
        } else {
            head.clone()
        });
        self.updated_head = Some(head);
        for (pn, patch_state) in patches {
            self.updated_patches.insert(pn, Some(patch_state));
        }
        self.applied = applied;
        self.unapplied = unapplied;
        self.hidden = hidden;
        Ok(())
    }

    /// Reset stack to previous stack state, but only for the specified patch names.
    pub(crate) fn reset_to_state_partially<P>(
        &mut self,
        state: StackState<'repo>,
        patchnames: &[P],
    ) -> Result<()>
    where
        P: AsRef<PatchName>,
    {
        let only_patches: IndexSet<_> = patchnames.iter().map(|pn| pn.as_ref()).collect();
        let state_patches: IndexSet<_> = state.all_patches().collect();
        let to_reset_patches: IndexSet<_> =
            state_patches.intersection(&only_patches).copied().collect();
        let existing_patches: IndexSet<_> = self.all_patches().cloned().collect();
        let existing_patches: IndexSet<_> = existing_patches.iter().collect();
        let original_applied_order = self.applied.clone();
        let to_delete_patches: IndexSet<_> = existing_patches
            .difference(&to_reset_patches)
            .copied()
            .collect::<IndexSet<_>>()
            .intersection(&only_patches)
            .copied()
            .collect();

        let matching_patches: IndexSet<_> = state
            .patches
            .iter()
            .filter_map(|(pn, patch_state)| {
                if self.has_patch(pn) && self.get_patch_commit(pn).id() == patch_state.commit.id() {
                    Some(pn)
                } else {
                    None
                }
            })
            .collect();

        self.pop_patches(|pn| {
            if !only_patches.contains(pn) {
                false
            } else if !to_delete_patches.contains(pn) {
                true
            } else {
                !matching_patches.contains(pn)
            }
        })?;

        self.delete_patches(|pn| to_delete_patches.contains(pn))?;

        for pn in to_reset_patches {
            if existing_patches.contains(pn) {
                if matching_patches.contains(pn) {
                    continue;
                }
            } else if state.hidden.contains(pn) {
                self.hidden.push(pn.clone());
            } else {
                self.unapplied.push(pn.clone());
            }
            self.updated_patches
                .insert(pn.clone(), Some(state.patches[pn].clone()));
            self.ui.print_updated(pn, self.applied())?;
        }

        let to_push_patches: Vec<_> = original_applied_order
            .iter()
            .filter(|pn| self.unapplied.contains(pn) || self.hidden.contains(pn))
            .collect();

        self.push_patches(&to_push_patches, false)?;

        Ok(())
    }

    /// Update a patch with a different commit object.
    ///
    /// Any notes associated with the patch's previous commit are copied to the new
    /// commit.
    pub(crate) fn update_patch(
        &mut self,
        patchname: &PatchName,
        commit_id: git2::Oid,
    ) -> Result<()> {
        let commit = self.stack.repo.find_commit(commit_id)?;
        let old_commit = self.get_patch_commit(patchname);
        // Failure to copy is okay. The old commit may not have a note to copy.
        self.stack
            .repo
            .stupid()
            .notes_copy(old_commit.id(), commit_id)
            .ok();
        self.updated_patches
            .insert(patchname.clone(), Some(PatchState { commit }));
        self.ui.print_updated(patchname, self.applied())?;
        Ok(())
    }

    /// Add new patch to the top of the stack.
    ///
    /// The commit for the new patch must be parented by the former top commit of the
    /// stack.
    pub(crate) fn new_applied(&mut self, patchname: &PatchName, oid: git2::Oid) -> Result<()> {
        let commit = self.stack.repo.find_commit(oid)?;
        assert_eq!(commit.parent_id(0).unwrap(), self.top().id());
        self.applied.push(patchname.clone());
        self.updated_patches
            .insert(patchname.clone(), Some(PatchState { commit }));
        self.ui.print_pushed(patchname, PushStatus::New, true)?;
        Ok(())
    }

    /// Add new unapplied patch to the stack.
    ///
    /// The new patch may be pushed to any position in the unapplied list.
    pub(crate) fn new_unapplied(
        &mut self,
        patchname: &PatchName,
        commit_id: git2::Oid,
        insert_pos: usize,
    ) -> Result<()> {
        let commit = self.stack.repo.find_commit(commit_id)?;
        self.unapplied.insert(insert_pos, patchname.clone());
        self.updated_patches
            .insert(patchname.clone(), Some(PatchState { commit }));
        self.ui.print_popped(&[patchname.clone()])?;
        Ok(())
    }

    /// Push patches, but keep their existing trees.
    pub(crate) fn push_tree_patches<P>(&mut self, patchnames: &[P]) -> Result<()>
    where
        P: AsRef<PatchName>,
    {
        for (i, patchname) in patchnames.iter().enumerate() {
            let is_last = i + 1 == patchnames.len();
            self.push_tree(patchname.as_ref(), is_last)?;
        }
        Ok(())
    }

    /// Push patch keeping its existing tree.
    ///
    /// For a normal patch push, the patch's diff is applied to the topmost patch's tree
    /// which typically results in a new tree being associated with the pushed patch's
    /// commit. For this operation, instead of applying the pushed patch's diff to the
    /// topmost patch's tree, the pushed patch's tree is preserved as-is.
    pub(crate) fn push_tree(&mut self, patchname: &PatchName, is_last: bool) -> Result<()> {
        let patch_commit = self.get_patch_commit(patchname);
        let repo = self.stack.repo;
        let config = repo.config()?;
        let parent = patch_commit.parent(0)?;
        let is_empty = parent.tree_id() == patch_commit.tree_id();

        let push_status = if patch_commit.parent_id(0)? != self.top().id() {
            let default_committer = git2::Signature::default_committer(Some(&config))?;
            let message = patch_commit.message_ex();
            let parent_ids = [self.top().id()];
            let new_commit_id = repo.commit_ex(
                &patch_commit.author_strict()?,
                &default_committer,
                &message,
                patch_commit.tree_id(),
                parent_ids,
            )?;

            let commit = repo.find_commit(new_commit_id)?;
            repo.stupid()
                .notes_copy(patch_commit.id(), new_commit_id)
                .ok();
            self.updated_patches
                .insert(patchname.clone(), Some(PatchState { commit }));

            PushStatus::Modified
        } else {
            PushStatus::Unmodified
        };

        let push_status = if is_empty {
            PushStatus::Empty
        } else {
            push_status
        };

        if let Some(pos) = self.unapplied.iter().position(|pn| pn == patchname) {
            self.unapplied.remove(pos);
        } else if let Some(pos) = self.hidden.iter().position(|pn| pn == patchname) {
            self.hidden.remove(pos);
        } else {
            panic!("push_tree `{patchname}` was not in unapplied or hidden");
        }

        self.applied.push(patchname.clone());

        self.ui.print_pushed(patchname, push_status, is_last)
    }

    /// Update patches' applied, unapplied, and hidden dispositions.
    ///
    /// This is used by `stg repair` to account for changes to the repository made by
    /// StGit-unaware git tooling. All existing patchnames must be present in the
    /// updated lists and no new patchnames may be introduced to the updated lists. I.e.
    /// this is strictly a rearrangement of existing patches.
    pub(crate) fn repair_appliedness(
        &mut self,
        applied: Vec<PatchName>,
        unapplied: Vec<PatchName>,
        hidden: Vec<PatchName>,
    ) -> Result<()> {
        let mut old: IndexSet<PatchName> = IndexSet::from_iter(
            self.applied
                .drain(..)
                .chain(self.unapplied.drain(..).chain(self.hidden.drain(..))),
        );
        self.applied = applied;
        self.unapplied = unapplied;
        self.hidden = hidden;

        for pn in self.all_patches() {
            old.take(pn)
                .expect("new patchname must be an old patchname");
        }
        assert!(
            old.is_empty(),
            "all old patchnames must be in the new applied/unapplied/hidden: {old:?}"
        );
        Ok(())
    }

    /// Perform push and pop operations to achieve a new stack ordering.
    ///
    /// The current ordering is maintained for any patch list that is not provided.
    pub(crate) fn reorder_patches(
        &mut self,
        applied: Option<&[PatchName]>,
        unapplied: Option<&[PatchName]>,
        hidden: Option<&[PatchName]>,
    ) -> Result<()> {
        if let Some(applied) = applied {
            let num_common = self
                .applied
                .iter()
                .zip(applied)
                .take_while(|(old, new)| old == new)
                .count();

            let to_pop: IndexSet<PatchName> = self.applied[num_common..].iter().cloned().collect();
            self.pop_patches(|pn| to_pop.contains(pn))?;

            let to_push = &applied[num_common..];
            self.push_patches(to_push, false)?;

            assert_eq!(self.applied, applied);

            if to_push.is_empty() {
                if let Some(last) = applied.last() {
                    self.ui.print_pushed(last, PushStatus::Unmodified, true)?;
                }
            }
        }

        if let Some(unapplied) = unapplied {
            self.unapplied = unapplied.to_vec();
        }

        if let Some(hidden) = hidden {
            self.hidden = hidden.to_vec();
        }

        Ok(())
    }

    // Finalize patches to be regular Git commits.
    //
    // Committed patches are no longer managed by StGit, but their commit objects remain
    // part of the regular git commit history. Committed patches are/become the base for
    // the remaining StGit stack.
    //
    // If the chosen `to_commit` patches are not currently the bottommost patches in the
    // stack, pops and pushes will be performed to move them to the bottom of the stack.
    // This may result in merge conflicts.
    pub(crate) fn commit_patches(&mut self, to_commit: &[PatchName]) -> Result<()> {
        let num_common = self
            .applied()
            .iter()
            .zip(to_commit.iter())
            .take_while(|(pn0, pn1)| pn0 == pn1)
            .count();

        let to_push: Vec<PatchName> = if num_common < to_commit.len() {
            let to_push: Vec<PatchName> = self.applied()[num_common..]
                .iter()
                .filter(|pn| !to_commit.contains(*pn))
                .cloned()
                .collect();

            self.pop_patches(|pn| to_push.contains(pn))?;
            self.push_patches(&to_commit[num_common..], false)?;
            to_push
        } else {
            vec![]
        };

        self.ui.print_committed(to_commit)?;

        self.updated_base = Some(self.get_patch_commit(to_commit.last().unwrap()).clone());
        for patchname in to_commit {
            self.updated_patches.insert(patchname.clone(), None);
        }
        self.applied = self.applied.split_off(to_commit.len());
        self.push_patches(&to_push, false)
    }

    /// Transform regular git commits from the base of the stack into StGit patches.
    ///
    /// The (patchname, commit_id) pairs must be in application order. I.e. the furthest
    /// ancestor of the current base first and the current base last.
    pub(crate) fn uncommit_patches<'a>(
        &mut self,
        patches: impl IntoIterator<Item = (&'a PatchName, git2::Oid)>,
    ) -> Result<()> {
        let mut new_applied: Vec<_> = Vec::with_capacity(self.applied.len());
        for (patchname, commit_id) in patches {
            let commit = self.stack.repo.find_commit(commit_id)?;
            self.updated_patches
                .insert(patchname.clone(), Some(PatchState { commit }));
            new_applied.push(patchname.clone());
        }
        new_applied.append(&mut self.applied);
        self.applied = new_applied;
        Ok(())
    }

    /// Hide patches that are currently applied or unapplied.
    ///
    /// Hidden patches are not shown by default by `stg series` and are excluded from
    /// several other operations.
    pub(crate) fn hide_patches(&mut self, to_hide: &[PatchName]) -> Result<()> {
        let applied: Vec<PatchName> = self
            .applied
            .iter()
            .filter(|pn| !to_hide.contains(pn))
            .cloned()
            .collect();

        let unapplied: Vec<PatchName> = self
            .unapplied
            .iter()
            .filter(|pn| !to_hide.contains(pn))
            .cloned()
            .collect();

        let hidden: Vec<PatchName> = to_hide.iter().chain(self.hidden.iter()).cloned().collect();

        self.reorder_patches(Some(&applied), Some(&unapplied), Some(&hidden))?;

        self.ui.print_hidden(to_hide)
    }

    /// Move hidden patches to the unapplied list.
    pub(crate) fn unhide_patches(&mut self, to_unhide: &[PatchName]) -> Result<()> {
        let unapplied: Vec<PatchName> = self
            .unapplied
            .iter()
            .chain(to_unhide.iter())
            .cloned()
            .collect();

        let hidden: Vec<PatchName> = self
            .hidden
            .iter()
            .filter(|pn| !to_unhide.contains(pn))
            .cloned()
            .collect();

        self.reorder_patches(None, Some(&unapplied), Some(&hidden))?;

        self.ui.print_unhidden(to_unhide)
    }

    /// Rename a patch.
    ///
    /// An error will be returned if either the old patchname does not exist or if the
    /// new patchname conflicts with an existing patch.
    pub(crate) fn rename_patch(
        &mut self,
        old_patchname: &PatchName,
        new_patchname: &PatchName,
    ) -> Result<()> {
        if new_patchname == old_patchname {
            return Ok(());
        } else if let Some(colliding_patchname) = self.stack.collides(new_patchname) {
            if self
                .updated_patches
                .get(colliding_patchname)
                .map_or(true, |maybe_patch| maybe_patch.is_some())
            {
                return Err(anyhow!("Patch `{colliding_patchname}` already exists"));
            }
        } else if !self.stack.has_patch(old_patchname) {
            return Err(anyhow!("Patch `{old_patchname}` does not exist"));
        }

        if let Some(pos) = self.applied.iter().position(|pn| pn == old_patchname) {
            self.applied[pos] = new_patchname.clone();
        } else if let Some(pos) = self.unapplied.iter().position(|pn| pn == old_patchname) {
            self.unapplied[pos] = new_patchname.clone();
        } else if let Some(pos) = self.hidden.iter().position(|pn| pn == old_patchname) {
            self.hidden[pos] = new_patchname.clone();
        } else {
            panic!("old `{old_patchname}` not found in applied, unapplied, or hidden");
        }

        let patch = self.stack.get_patch(old_patchname).clone();
        self.updated_patches.insert(old_patchname.clone(), None);
        self.updated_patches
            .insert(new_patchname.clone(), Some(patch));

        self.ui.print_rename(old_patchname, new_patchname)
    }

    /// Delete one or more patches from the stack.
    ///
    /// Deleted patches' commits become disconnected from the regular git history and
    /// are thus subject to eventual garbage collection.
    pub(crate) fn delete_patches<F>(&mut self, should_delete: F) -> Result<Vec<PatchName>>
    where
        F: Fn(&PatchName) -> bool,
    {
        let all_popped = if let Some(first_pop_pos) = self.applied.iter().position(&should_delete) {
            self.applied.split_off(first_pop_pos)
        } else {
            vec![]
        };

        let incidental: Vec<PatchName> = all_popped
            .iter()
            .filter(|pn| !should_delete(pn))
            .cloned()
            .collect();

        let unapplied_size = incidental.len() + self.unapplied.len();
        let unapplied = std::mem::replace(&mut self.unapplied, Vec::with_capacity(unapplied_size));
        self.unapplied.append(&mut incidental.clone());

        self.ui.print_popped(&all_popped)?;

        // Gather contiguous groups of deleted patchnames for printing.
        let mut deleted_group: Vec<PatchName> = Vec::with_capacity(all_popped.len());

        for patchname in all_popped {
            if should_delete(&patchname) {
                deleted_group.push(patchname.clone());
                self.updated_patches.insert(patchname, None);
            } else if !deleted_group.is_empty() {
                self.ui.print_deleted(&deleted_group)?;
                deleted_group.clear();
            }
        }

        for patchname in unapplied {
            if should_delete(&patchname) {
                deleted_group.push(patchname.clone());
                self.updated_patches.insert(patchname, None);
            } else {
                self.ui.print_deleted(&deleted_group)?;
                deleted_group.clear();
                self.unapplied.push(patchname);
            }
        }

        let mut i = 0;
        while i < self.hidden.len() {
            if should_delete(&self.hidden[i]) {
                let patchname = self.hidden.remove(i);
                deleted_group.push(patchname.clone());
                self.updated_patches.insert(patchname, None);
            } else {
                i += 1;
                self.ui.print_deleted(&deleted_group)?;
                deleted_group.clear();
            }
        }

        if !deleted_group.is_empty() {
            self.ui.print_deleted(&deleted_group)?;
        }

        Ok(incidental)
    }

    /// Pop applied patches, making them unapplied.
    ///
    /// The `should_pop` closure should return true for each patch name to be popped and
    /// false for patches that are to remain applied.
    pub(crate) fn pop_patches<F>(&mut self, should_pop: F) -> Result<Vec<PatchName>>
    where
        F: Fn(&PatchName) -> bool,
    {
        let all_popped = if let Some(first_pop_pos) = self.applied.iter().position(&should_pop) {
            self.applied.split_off(first_pop_pos)
        } else {
            vec![]
        };

        let incidental: Vec<PatchName> = all_popped
            .iter()
            .filter(|pn| !should_pop(pn))
            .cloned()
            .collect();

        let mut requested: Vec<PatchName> = all_popped
            .iter()
            .filter(|pn| should_pop(pn))
            .cloned()
            .collect();

        let unapplied_size = incidental.len() + requested.len() + self.unapplied.len();
        let mut unapplied =
            std::mem::replace(&mut self.unapplied, Vec::with_capacity(unapplied_size));

        self.unapplied.append(&mut incidental.clone());
        self.unapplied.append(&mut requested);
        self.unapplied.append(&mut unapplied);

        self.ui.print_popped(&all_popped)?;

        Ok(incidental)
    }

    /// Push unapplied patches to become applied.
    ///
    /// Pushing a patch may result in a merge conflict. When this occurs, a
    /// `Error::TransactionHalt` will be returned which will cause the current
    /// transaction to halt. This condition is not an error, per-se, so the stack state
    /// is *not* rolled back. Instead, the conflicts will be left in the working tree
    /// and index for the user to resolve.
    ///
    /// The `check_merged` option, when true, performs an extra check to determine
    /// whether the patches' changes have already been merged into the stack's base
    /// tree. Patches that are determined to have already been merged will still be
    /// pushed successfully, but their diff will be empty.
    pub(crate) fn push_patches<P>(&mut self, patchnames: &[P], check_merged: bool) -> Result<()>
    where
        P: AsRef<PatchName>,
    {
        let stupid = self.stack.repo.stupid();
        stupid.with_temp_index(|stupid_temp| {
            let mut temp_index_tree_id: Option<git2::Oid> = None;

            let merged = if check_merged {
                Some(self.check_merged(patchnames, stupid_temp, &mut temp_index_tree_id)?)
            } else {
                None
            };

            for (i, patchname) in patchnames.iter().enumerate() {
                let patchname = patchname.as_ref();
                let is_last = i + 1 == patchnames.len();
                let already_merged = merged
                    .as_ref()
                    .map(|merged| merged.contains(&patchname))
                    .unwrap_or(false);
                self.push_patch(
                    patchname,
                    already_merged,
                    is_last,
                    stupid_temp,
                    &mut temp_index_tree_id,
                )?;
            }

            Ok(())
        })
    }

    fn push_patch(
        &mut self,
        patchname: &PatchName,
        already_merged: bool,
        is_last: bool,
        stupid_temp: &StupidContext,
        temp_index_tree_id: &mut Option<git2::Oid>,
    ) -> Result<()> {
        let repo = self.stack.repo;
        let config = repo.config()?;
        let stupid = repo.stupid();
        let default_committer = git2::Signature::default_committer(Some(&config))?;
        let patch_commit = self.get_patch_commit(patchname).clone();
        let old_parent = patch_commit.parent(0)?;
        let new_parent = self.top().clone();

        let mut push_status = PushStatus::Unmodified;

        let new_tree_id = if already_merged {
            push_status = PushStatus::AlreadyMerged;
            new_parent.tree_id()
        } else if old_parent.tree_id() == new_parent.tree_id() {
            patch_commit.tree_id()
        } else if old_parent.tree_id() == patch_commit.tree_id() {
            new_parent.tree_id()
        } else if new_parent.tree_id() == patch_commit.tree_id() {
            patch_commit.tree_id()
        } else {
            let (ours, theirs) = if temp_index_tree_id == &Some(patch_commit.tree_id()) {
                (patch_commit.tree_id(), new_parent.tree_id())
            } else {
                (new_parent.tree_id(), patch_commit.tree_id())
            };
            let base = old_parent.tree_id();

            if temp_index_tree_id != &Some(ours) {
                stupid_temp.read_tree(ours)?;
                *temp_index_tree_id = Some(ours);
            }

            let maybe_tree_id = if stupid_temp.apply_treediff_to_index(base, theirs)? {
                stupid_temp.write_tree().ok()
            } else {
                None
            };

            if let Some(tree_id) = maybe_tree_id {
                tree_id
            } else if !self.options.use_index_and_worktree {
                return Err(Error::TransactionHalt {
                    msg: format!("{patchname} does not apply cleanly"),
                    conflicts: false,
                }
                .into());
            } else {
                if stupid
                    .read_tree_checkout(self.current_tree_id, ours)
                    .is_err()
                {
                    return Err(Error::TransactionHalt {
                        msg: "Index/worktree dirty".to_string(),
                        conflicts: false,
                    }
                    .into());
                }
                self.current_tree_id = ours;

                let use_mergetool = config.get_bool("stgit.autoimerge").unwrap_or(false);
                match stupid.merge_recursive_or_mergetool(base, ours, theirs, use_mergetool) {
                    Ok(true) => {
                        // Success, no conflicts
                        let tree_id = stupid.write_tree().map_err(|_| Error::TransactionHalt {
                            msg: "Conflicting merge".to_string(),
                            conflicts: false,
                        })?;
                        self.current_tree_id = tree_id;
                        push_status = PushStatus::Modified;
                        tree_id
                    }
                    Ok(false) => {
                        push_status = PushStatus::Conflict;
                        ours
                    }
                    Err(e) => {
                        return Err(Error::TransactionHalt {
                            msg: format!("{e:#}"),
                            conflicts: false,
                        }
                        .into())
                    }
                }
            }
        };

        if new_tree_id != patch_commit.tree_id() || new_parent.id() != old_parent.id() {
            let commit_id = repo.commit_ex(
                &patch_commit.author_strict()?,
                &default_committer,
                &patch_commit.message_ex(),
                new_tree_id,
                [new_parent.id()],
            )?;
            let commit = repo.find_commit(commit_id)?;
            stupid.notes_copy(patch_commit.id(), commit_id).ok();
            if push_status == PushStatus::Conflict {
                // In the case of a conflict, update() will be called after the
                // execute() performs the checkout. Setting the transaction head
                // here ensures that the real stack top will be checked-out.
                self.updated_head = Some(commit.clone());
            } else if push_status != PushStatus::AlreadyMerged
                && new_tree_id == new_parent.tree_id()
            {
                push_status = PushStatus::Empty;
            }

            self.updated_patches
                .insert(patchname.clone(), Some(PatchState { commit }));
        }

        if push_status == PushStatus::Conflict {
            // The final checkout at execute-time must allow these push conflicts.
            self.options.conflict_mode = ConflictMode::Allow;
        }

        if let Some(pos) = self.unapplied.iter().position(|pn| pn == patchname) {
            self.unapplied.remove(pos);
        } else if let Some(pos) = self.hidden.iter().position(|pn| pn == patchname) {
            self.hidden.remove(pos);
        }
        self.applied.push(patchname.clone());

        self.ui.print_pushed(patchname, push_status, is_last)?;

        if push_status == PushStatus::Conflict {
            Err(Error::TransactionHalt {
                msg: "Merge conflicts".to_string(),
                conflicts: true,
            }
            .into())
        } else {
            Ok(())
        }
    }

    /// Find patches that have already been merged into the stack base's tree.
    ///
    /// The diffs for each provided patchname are applied to the stack's base tree (in
    /// the context of the provided temp index) to determine whether the patches'
    /// changes are already manifest in the base tree.
    fn check_merged<'a, P>(
        &self,
        patchnames: &'a [P],
        stupid_temp: &StupidContext,
        temp_index_tree_id: &mut Option<git2::Oid>,
    ) -> Result<Vec<&'a PatchName>>
    where
        P: AsRef<PatchName>,
    {
        let head_tree_id = self.stack.branch_head.tree_id();
        let mut merged: Vec<&PatchName> = vec![];

        if temp_index_tree_id != &Some(head_tree_id) {
            stupid_temp.read_tree(head_tree_id)?;
            *temp_index_tree_id = Some(head_tree_id);
        }

        for patchname in patchnames.iter().rev() {
            let patchname = patchname.as_ref();
            let patch_commit = self.get_patch_commit(patchname);

            if patch_commit.is_no_change()? {
                continue; // No change
            }

            let parent_commit = patch_commit.parent(0)?;

            if stupid_temp
                .apply_treediff_to_index(patch_commit.tree_id(), parent_commit.tree_id())?
            {
                merged.push(patchname);
                *temp_index_tree_id = None;
            }
        }

        self.ui.print_merged(&merged)?;

        Ok(merged)
    }
}

impl<'repo> StackStateAccess<'repo> for StackTransaction<'repo> {
    fn applied(&self) -> &[PatchName] {
        &self.applied
    }

    fn unapplied(&self) -> &[PatchName] {
        &self.unapplied
    }

    fn hidden(&self) -> &[PatchName] {
        &self.hidden
    }

    fn get_patch(&self, patchname: &PatchName) -> &PatchState<'repo> {
        if let Some(maybe_patch) = self.updated_patches.get(patchname) {
            maybe_patch
                .as_ref()
                .expect("should not attempt to access deleted patch")
        } else {
            self.stack.get_patch(patchname)
        }
    }

    fn has_patch(&self, patchname: &PatchName) -> bool {
        if let Some(maybe_patch) = self.updated_patches.get(patchname) {
            maybe_patch.is_some()
        } else {
            self.stack.has_patch(patchname)
        }
    }

    fn top(&self) -> &git2::Commit<'repo> {
        if let Some(patchname) = self.applied.last() {
            self.get_patch_commit(patchname)
        } else {
            self.base()
        }
    }

    fn head(&self) -> &git2::Commit<'repo> {
        if let Some(commit) = self.updated_head.as_ref() {
            commit
        } else {
            self.top()
        }
    }

    fn base(&self) -> &git2::Commit<'repo> {
        if let Some(commit) = self.updated_base.as_ref() {
            commit
        } else {
            self.stack.base()
        }
    }
}