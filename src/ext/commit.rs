// SPDX-License-Identifier: GPL-2.0-only

use anyhow::{anyhow, Result};
use bstr::{BString, ByteSlice};

use crate::wrap::Message;

/// Extension trait for [`git_repository::Commit`].
pub(crate) trait CommitExtended<'a> {
    /// Get author signature, strictly.
    ///
    /// The author signature, in an arbitrary git commit object, may (should? must?) be
    /// encoded with the commit encoding. However, gitoxide does not perform any
    /// decoding when it parses commit objects, thus a
    /// `git_repository::actor::Signature` from a `git_repository::Commit` is not
    /// decoded in the general case, and thus `git_repository::actor::Signature.name`
    /// and `email` may only be decoded UTF-8 strs iff the commit happens to be using
    /// the UTF-8 encoding.
    ///
    /// This method takes into account the commit's encoding and attempts to decode the
    /// author name and email into UTF-8. The signature returned by this method is
    /// guaranteed to have valid UTF-8 name and email strs.
    fn author_strict(&self) -> Result<git_repository::actor::Signature>;

    /// Get committer signature, strictly.
    ///
    /// See [`CommitExtended::author_strict()`].
    fn committer_strict(&self) -> Result<git_repository::actor::Signature>;

    /// Get commit message with extended capabilities.
    fn message_ex(&self) -> Message;

    /// Determine whether the commit has the same tree as its parent.
    fn is_no_change(&self) -> Result<bool>;

    fn get_parent_commit(&self) -> Result<git_repository::Commit<'a>>;
}

impl<'a> CommitExtended<'a> for git_repository::Commit<'a> {
    fn author_strict(&self) -> Result<git_repository::actor::Signature> {
        let commit_ref = self.decode()?;
        let sig = commit_ref.author();
        let encoding = if let Some(encoding_name) = commit_ref.encoding {
            encoding_rs::Encoding::for_label(encoding_name).ok_or_else(|| {
                anyhow!(
                    "Unhandled commit encoding `{}` in commit `{}`",
                    encoding_name.to_str_lossy(),
                    self.id,
                )
            })?
        } else {
            encoding_rs::UTF_8
        };

        if let Some(name) = encoding.decode_without_bom_handling_and_without_replacement(sig.name) {
            if let Some(email) =
                encoding.decode_without_bom_handling_and_without_replacement(sig.email)
            {
                Ok(git_repository::actor::Signature {
                    name: BString::from(name.as_ref()),
                    email: BString::from(email.as_ref()),
                    time: sig.time,
                })
            } else {
                Err(anyhow!(
                    "Could not decode author email as `{}` for commit `{}`",
                    encoding.name(),
                    self.id,
                ))
            }
        } else {
            Err(anyhow!(
                "Could not decode author name as `{}` for commit `{}`",
                encoding.name(),
                self.id,
            ))
        }
    }

    fn committer_strict(&self) -> Result<git_repository::actor::Signature> {
        let commit_ref = self.decode()?;
        let sig = commit_ref.committer();
        let encoding = if let Some(encoding_name) = commit_ref.encoding {
            encoding_rs::Encoding::for_label(encoding_name).ok_or_else(|| {
                anyhow!(
                    "Unhandled commit encoding `{}` in commit `{}`",
                    encoding_name.to_str_lossy(),
                    self.id,
                )
            })?
        } else {
            encoding_rs::UTF_8
        };

        if let Some(name) = encoding.decode_without_bom_handling_and_without_replacement(sig.name) {
            if let Some(email) =
                encoding.decode_without_bom_handling_and_without_replacement(sig.email)
            {
                Ok(git_repository::actor::Signature {
                    name: BString::from(name.as_ref()),
                    email: BString::from(email.as_ref()),
                    time: sig.time,
                })
            } else {
                Err(anyhow!(
                    "Could not decode committer email as `{}` for commit `{}`",
                    encoding.name(),
                    self.id,
                ))
            }
        } else {
            Err(anyhow!(
                "Could not decode committer name as `{}` for commit `{}`",
                encoding.name(),
                self.id,
            ))
        }
    }

    fn message_ex(&self) -> Message {
        let commit_ref = self.decode().expect("commit can be decoded");
        if let Ok(message) = commit_ref.message.to_str() {
            Message::Str(message)
        } else {
            Message::Raw {
                bytes: commit_ref.message,
                encoding: commit_ref
                    .encoding
                    .and_then(|encoding| encoding.to_str().ok()),
            }
        }
    }

    fn is_no_change(&self) -> Result<bool> {
        let mut parent_ids = self.parent_ids();
        if let Some(parent_id) = parent_ids.next() {
            if parent_ids.next().is_none() {
                let parent_tree_id = parent_id.object()?.try_into_commit()?.tree_id()?;
                let tree_id = self.tree_id()?;
                Ok(parent_tree_id == tree_id)
            } else {
                Ok(false)
            }
        } else {
            Ok(false)
        }
    }

    fn get_parent_commit(&self) -> Result<git_repository::Commit<'a>> {
        Ok(self
            .parent_ids()
            .next()
            .ok_or_else(|| anyhow!("commit `{}` does not have a parent", self.id))?
            .object()?
            .try_into_commit()?)
    }
}
