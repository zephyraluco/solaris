//! Native user-file operations with bounded reads and atomic replacement.
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use crate::Cancel;
use crate::environment::ExecutionEnvironment;

#[cfg(test)]
pub(crate) fn read(path: &Path) -> io::Result<Vec<u8>> {
    read_limited(path, 32 * 1024 * 1024)
}

pub(crate) fn read_limited(path: &Path, limit: usize) -> io::Result<Vec<u8>> {
    read_with_cancel(path, limit, None)
}

pub(crate) fn read_with_cancel(
    path: &Path,
    limit: usize,
    cancel: Option<&Cancel>,
) -> io::Result<Vec<u8>> {
    if cancel.is_some_and(Cancel::is_cancelled) {
        return Err(io::Error::other("tool cancelled"));
    }
    let metadata = std::fs::metadata(path)?;
    if !metadata.is_file() {
        return Err(io::Error::other("not a regular file"));
    }
    if metadata.len() > limit as u64 {
        return Err(io::Error::other(format!(
            "file exceeds the {limit} byte input limit"
        )));
    }
    let mut bytes = Vec::new();
    std::fs::File::open(path)?
        .take((limit + 1) as u64)
        .read_to_end(&mut bytes)?;
    if cancel.is_some_and(Cancel::is_cancelled) {
        return Err(io::Error::other("tool cancelled"));
    }
    if bytes.len() > limit {
        return Err(io::Error::other(format!(
            "file exceeds the {limit} byte input limit"
        )));
    }
    Ok(bytes)
}

pub(crate) fn create_dir_all(path: &Path) -> io::Result<()> {
    std::fs::create_dir_all(path)
}

pub(crate) fn remove_file(path: &Path) -> io::Result<()> {
    std::fs::remove_file(path)
}

pub(crate) fn canonicalize(path: &Path) -> io::Result<PathBuf> {
    ExecutionEnvironment.canonical_path(path)
}

/// Directory entries as `(name, is_real_directory)`.
pub(crate) fn entries(path: &Path) -> io::Result<Vec<(String, bool)>> {
    let mut entries = Vec::new();
    for entry in std::fs::read_dir(path)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        let name = entry.file_name().to_string_lossy().into_owned();
        entries.push((name, file_type.is_dir()));
    }
    Ok(entries)
}

pub(crate) fn mode(path: &Path) -> io::Result<u32> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        Ok(std::fs::metadata(path)?.permissions().mode() & 0o7777)
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "POSIX file modes are unavailable on this platform",
        ))
    }
}

/// Same-directory atomic replacement with durability barriers.
pub(crate) fn atomic_replace(
    path: &Path,
    bytes: &[u8],
    mode: Option<u32>,
    cancel: Option<&Cancel>,
) -> io::Result<()> {
    if cancel.is_some_and(Cancel::is_cancelled) {
        return Err(io::Error::other("file change cancelled"));
    }
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::other("file has no parent"))?;
    std::fs::create_dir_all(parent)?;

    #[cfg(unix)]
    let resolved_mode = mode.or_else(|| self::mode(path).ok());
    #[cfg(not(unix))]
    let _ = mode;

    let mut file = tempfile::NamedTempFile::new_in(parent)?;
    file.write_all(bytes)?;
    file.as_file().sync_all()?;

    #[cfg(unix)]
    if let Some(bits) = resolved_mode {
        use std::os::unix::fs::PermissionsExt;
        file.as_file()
            .set_permissions(std::fs::Permissions::from_mode(bits & 0o7777))?;
    }

    if cancel.is_some_and(Cancel::is_cancelled) {
        return Err(io::Error::other("file change cancelled"));
    }
    file.persist(path).map_err(|error| error.error)?;
    #[cfg(unix)]
    std::fs::File::open(parent)?.sync_all()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_fs_round_trips_binary_and_unicode_filenames() {
        let directory = tempfile::tempdir().unwrap();
        let filename = if cfg!(windows) {
            "space 中文 ' ; file"
        } else {
            "space 中文 ' ; $(echo BAD)\nfile"
        };
        let file = directory.path().join(filename);
        let bytes = b"\0\xff\r\noriginal\n";
        atomic_replace(&file, bytes, None, None).unwrap();
        assert_eq!(read(&file).unwrap(), bytes);
        let entries = entries(directory.path()).unwrap();
        assert_eq!(
            entries,
            vec![(
                file.file_name().unwrap().to_string_lossy().into_owned(),
                false
            )]
        );
        assert_eq!(read(&canonicalize(&file).unwrap()).unwrap(), bytes);
        remove_file(&file).unwrap();
        assert!(!file.exists());
    }

    #[cfg(unix)]
    #[test]
    fn collapsing_missing_parent_segments_resolves_newly_exposed_symlinks() {
        use std::os::unix::fs::symlink;
        let directory = tempfile::tempdir().unwrap();
        let workspace = directory.path().join("workspace");
        let protected = directory.path().join("protected");
        std::fs::create_dir(&workspace).unwrap();
        std::fs::create_dir(&protected).unwrap();
        symlink(&protected, workspace.join("link")).unwrap();
        let input = workspace.join("missing/../link/new.txt");
        let target = canonicalize(&input).unwrap();
        assert_eq!(target, protected.canonicalize().unwrap().join("new.txt"));
        atomic_replace(&target, b"actual target", None, None).unwrap();
        assert_eq!(read(&protected.join("new.txt")).unwrap(), b"actual target");
    }

    #[cfg(unix)]
    #[test]
    fn canonicalization_follows_file_and_directory_symlinks_and_new_suffixes() {
        use std::os::unix::fs::symlink;
        let directory = tempfile::tempdir().unwrap();
        let real = directory.path().join("real\n");
        std::fs::create_dir(&real).unwrap();
        let file = real.join("file\n");
        std::fs::write(&file, "data").unwrap();
        symlink(&real, directory.path().join("dir-link")).unwrap();
        symlink(&file, directory.path().join("file-link")).unwrap();
        assert_eq!(
            canonicalize(&directory.path().join("file-link")).unwrap(),
            file.canonicalize().unwrap()
        );
        assert_eq!(
            canonicalize(&directory.path().join("dir-link/new/../file\n")).unwrap(),
            file.canonicalize().unwrap()
        );
        assert_eq!(
            canonicalize(&directory.path().join("dir-link/new/file")).unwrap(),
            real.canonicalize().unwrap().join("new/file")
        );
    }
}

#[cfg(all(test, unix))]
mod permission_tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn atomic_replacement_preserves_read_only_and_executable_modes() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("file");
        for mode in [0o444, 0o755] {
            std::fs::write(&path, "before").unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
            atomic_replace(&path, b"after", None, None).unwrap();
            assert_eq!(read(&path).unwrap(), b"after");
            assert_eq!(super::mode(&path).unwrap() & 0o777, mode);
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
    }
}
