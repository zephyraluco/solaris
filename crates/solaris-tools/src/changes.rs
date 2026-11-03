//! Atomic file changes with private before-images and conflict-checked undo.

use std::io::{self, Write};
use std::path::{Path, PathBuf};

use serde_json::json;
use sha2::{Digest, Sha256};

use crate::{ToolContext, ToolMetadata};

const MAX_FILE_BYTES: usize = 16 * 1024 * 1024;

fn hash(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

/// A recorded change. Backups contain user data and must remain private.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct FileChange {
    pub id: String,
    pub path: PathBuf,
    pub before_hash: Option<String>,
    pub after_hash: String,
    pub patch: String,
    pub original_mode: Option<u32>,
}

pub(crate) fn read_optional(path: &Path) -> io::Result<Option<Vec<u8>>> {
    match std::fs::metadata(path) {
        Ok(metadata) if metadata.len() > MAX_FILE_BYTES as u64 => {
            return Err(io::Error::other("file exceeds the 16 MiB mutation limit"));
        }
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    }
    // take prevents a growing file from bypassing the metadata size check.
    use std::io::Read;
    let mut bytes = Vec::new();
    std::fs::File::open(path)?
        .take((MAX_FILE_BYTES + 1) as u64)
        .read_to_end(&mut bytes)?;
    if bytes.len() > MAX_FILE_BYTES {
        return Err(io::Error::other("file grew beyond the mutation limit"));
    }
    Ok(Some(bytes))
}

fn atomic_replace(
    path: &Path,
    bytes: &[u8],
    permissions: Option<std::fs::Permissions>,
) -> io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::other("file has no parent"))?;
    let mut file = tempfile::NamedTempFile::new_in(parent)?;
    if let Some(permissions) = permissions {
        file.as_file().set_permissions(permissions)?;
    }
    file.write_all(bytes)?;
    file.as_file().sync_all()?;
    file.persist(path).map_err(|error| error.error)?;
    #[cfg(unix)]
    std::fs::File::open(parent)?.sync_all()?;
    Ok(())
}

fn private_directory(path: &Path) -> io::Result<()> {
    std::fs::create_dir_all(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

/// Commit with an optimistic check against the bytes the edit was prepared from.
/// Caller must hold its mutation queue lock. User edits are never silently restored over.
pub fn commit(
    path: &Path,
    before: Option<&[u8]>,
    after: &[u8],
    ctx: &ToolContext,
) -> io::Result<ToolMetadata> {
    if after.len() > MAX_FILE_BYTES {
        return Err(io::Error::other(
            "content exceeds the 16 MiB mutation limit",
        ));
    }
    if ctx.is_cancelled() {
        return Err(io::Error::other("file change cancelled"));
    }
    let target = if path.exists() {
        std::fs::canonicalize(path)?
    } else {
        let parent = path
            .parent()
            .ok_or_else(|| io::Error::other("file has no parent"))?;
        std::fs::create_dir_all(parent)?;
        std::fs::canonicalize(parent)?.join(
            path.file_name()
                .ok_or_else(|| io::Error::other("missing filename"))?,
        )
    };
    if read_optional(&target)?.as_deref() != before {
        return Err(io::Error::other(
            "file changed while preparing the edit; read it again",
        ));
    }
    let directory = ctx.temp_dir.join("changes");
    private_directory(&directory)?;
    let entry = tempfile::Builder::new()
        .prefix("change-")
        .tempdir_in(&directory)?;
    let id = entry
        .path()
        .file_name()
        .ok_or_else(|| io::Error::other("missing change ID"))?
        .to_string_lossy()
        .into_owned();
    let old = String::from_utf8_lossy(before.unwrap_or_default());
    let new = String::from_utf8_lossy(after);
    let mut patch = similar::TextDiff::configure()
        .timeout(std::time::Duration::from_millis(100))
        .diff_lines(&old, &new)
        .unified_diff()
        .context_radius(3)
        .header("before", "after")
        .to_string();
    if patch.len() > 64 * 1024 {
        let mut end = 64 * 1024;
        while !patch.is_char_boundary(end) {
            end -= 1;
        }
        patch.truncate(end);
        patch.push_str("\n[patch truncated; inspect the file for the full change]\n");
    }
    #[cfg(unix)]
    let original_mode = {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(&target)
            .ok()
            .map(|m| m.permissions().mode())
    };
    #[cfg(not(unix))]
    let original_mode = None;
    let change = FileChange {
        id,
        path: target.clone(),
        before_hash: before.map(hash),
        after_hash: hash(after),
        patch,
        original_mode,
    };
    if let Some(before) = before {
        atomic_replace(&entry.path().join("before"), before, None)?;
    }
    atomic_replace(
        &entry.path().join("change.json"),
        &serde_json::to_vec(&change)?,
        None,
    )?;
    // Preserve the intent and before-image even if applying the replacement fails.
    let kept = entry.keep();
    if ctx.is_cancelled() {
        return Err(io::Error::other("file change cancelled"));
    }
    if read_optional(&target)?.as_deref() != before {
        return Err(io::Error::other(
            "file changed before replacement; read it again",
        ));
    }
    let permissions = std::fs::metadata(&target).ok().map(|m| m.permissions());
    atomic_replace(&target, after, permissions)?;
    Ok(ToolMetadata {
        details: json!({ "change": change, "undo_record": kept.join("change.json") }),
        ..Default::default()
    })
}

/// Load an explicit change ID; IDs cannot select arbitrary files or directories.
pub fn load(directory: &Path, id: &str) -> io::Result<FileChange> {
    if !id.starts_with("change-")
        || id.len() > 80
        || !id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
    {
        return Err(io::Error::other("invalid change ID"));
    }
    Ok(serde_json::from_slice(&std::fs::read(
        directory.join("changes").join(id).join("change.json"),
    )?)?)
}

/// List recorded mutations for review, without changing the workspace.
pub fn list(directory: &Path) -> io::Result<Vec<FileChange>> {
    let entries = match std::fs::read_dir(directory.join("changes")) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };
    let mut changes = Vec::new();
    for entry in entries {
        let entry = entry?;
        if entry.file_type()?.is_dir()
            && let Some(id) = entry.file_name().to_str()
        {
            changes.push(load(directory, id)?);
        }
    }
    changes.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(changes)
}

/// Restore only when the current file still matches the recorded after-image.
/// This does not undo arbitrary effects from shell commands.
pub fn undo(directory: &Path, id: &str) -> io::Result<FileChange> {
    let change = load(directory, id)?;
    let current = read_optional(&change.path)?
        .ok_or_else(|| io::Error::other("file no longer exists; cannot undo"))?;
    if hash(&current) != change.after_hash {
        return Err(io::Error::other(
            "undo conflict: file changed after this operation",
        ));
    }
    if let Some(expected) = &change.before_hash {
        let before = std::fs::read(directory.join("changes").join(id).join("before"))?;
        if hash(&before) != *expected {
            return Err(io::Error::other("undo backup checksum mismatch"));
        }
        let permissions = std::fs::metadata(&change.path)?.permissions();
        #[cfg(unix)]
        let mut permissions = permissions;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if let Some(mode) = change.original_mode {
                permissions.set_mode(mode);
            }
        }
        atomic_replace(&change.path, &before, Some(permissions))?;
    } else {
        std::fs::remove_file(&change.path)?;
    }
    Ok(change)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn undo_restores_original_bytes_but_refuses_later_user_edits() {
        let directory = tempfile::tempdir().expect("directory");
        let file = directory.path().join("file");
        let ctx = ToolContext::new(directory.path(), directory.path().join("artifacts"));
        std::fs::write(&file, "original\r\n中文").expect("original");
        let metadata =
            commit(&file, Some("original\r\n中文".as_bytes()), b"updated", &ctx).expect("commit");
        let change: FileChange =
            serde_json::from_value(metadata.details["change"].clone()).expect("change");
        std::fs::write(&file, "user edit").expect("user edit");
        assert!(
            undo(&ctx.temp_dir, &change.id)
                .expect_err("conflict")
                .to_string()
                .contains("conflict")
        );
        assert_eq!(
            std::fs::read_to_string(&file).expect("preserved"),
            "user edit"
        );
        std::fs::write(&file, "updated").expect("restore after-image");
        undo(&ctx.temp_dir, &change.id).expect("undo");
        assert_eq!(
            std::fs::read_to_string(file).expect("restored"),
            "original\r\n中文"
        );
    }

    #[test]
    fn undo_creation_removes_only_the_matching_created_file() {
        let directory = tempfile::tempdir().expect("directory");
        let file = directory.path().join("new");
        let ctx = ToolContext::new(directory.path(), directory.path().join("artifacts"));
        let metadata = commit(&file, None, b"new", &ctx).expect("create");
        let id = metadata.details["change"]["id"].as_str().expect("id");
        undo(&ctx.temp_dir, id).expect("undo creation");
        assert!(!file.exists());
        assert!(load(&ctx.temp_dir, "../../bad").is_err());
    }
}
