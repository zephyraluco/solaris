//! User-file operations implemented by fixed Bash scripts and system utilities.
use std::io;
use std::path::{Path, PathBuf};

use crate::Cancel;
use crate::environment::ExecutionEnvironment;

fn call(script: &str, args: &[&Path], input: &[u8]) -> io::Result<Vec<u8>> {
    ExecutionEnvironment.checked(script, args, input, None)
}

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
    let size = (limit + 1).to_string();
    let bytes = ExecutionEnvironment.checked(
        "[[ -e $1 ]] || { printf 'file not found' >&2; exit 44; }; [[ -f $1 ]] || { printf 'not a regular file' >&2; exit 1; }; head -c \"$2\" -- \"$1\"",
        &[path, Path::new(&size)],
        &[], cancel,
    )?;
    if bytes.len() > limit {
        return Err(io::Error::other(format!(
            "file exceeds the {limit} byte input limit"
        )));
    }
    Ok(bytes)
}

pub(crate) fn exists(path: &Path) -> bool {
    call("[[ -e $1 ]]", &[path], &[]).is_ok()
}
pub(crate) fn is_dir(path: &Path) -> bool {
    call("[[ -d $1 ]]", &[path], &[]).is_ok()
}
pub(crate) fn create_dir_all(path: &Path) -> io::Result<()> {
    call("mkdir -p -- \"$1\"", &[path], &[]).map(|_| ())
}
pub(crate) fn remove_file(path: &Path) -> io::Result<()> {
    call("rm -- \"$1\"", &[path], &[]).map(|_| ())
}
pub(crate) fn canonicalize(path: &Path) -> io::Result<PathBuf> {
    ExecutionEnvironment.canonical_path(path)
}

/// NUL-delimited names and type tags preserve even embedded newlines.
pub(crate) fn entries(path: &Path) -> io::Result<Vec<(String, bool)>> {
    let output = call(
        "[[ -d $1 ]] || { printf 'not a directory or file not found' >&2; exit 44; }; [[ -r $1 && -x $1 ]] || { printf 'permission denied' >&2; exit 43; }; shopt -s dotglob nullglob; base=${1%/}; for file in \"$base\"/*; do kind=f; [[ -d $file && ! -L $file ]] && kind=d; printf '%s\\0%s\\0' \"${file##*/}\" \"$kind\"; done",
        &[path],
        &[],
    )?;
    let mut fields = output.split(|b| *b == 0);
    let mut entries = Vec::new();
    while let Some(name) = fields.next().filter(|name| !name.is_empty()) {
        let directory = fields
            .next()
            .ok_or_else(|| io::Error::other("invalid directory response"))?
            == b"d";
        entries.push((String::from_utf8_lossy(name).into_owned(), directory));
    }
    Ok(entries)
}

pub(crate) fn mode(path: &Path) -> io::Result<u32> {
    let output = call("LC_ALL=C ls -ld -- \"$1\"", &[path], &[])?;
    let flags = output
        .get(1..10)
        .ok_or_else(|| io::Error::other("invalid permission response"))?;
    let mut mode = 0;
    for (i, flag) in flags.iter().enumerate() {
        let present = match i % 3 {
            0 => *flag == b'r',
            1 => *flag == b'w',
            _ => matches!(flag, b'x' | b's' | b't'),
        };
        if present {
            mode |= 1 << (8 - i);
        }
    }
    if matches!(flags[2], b's' | b'S') {
        mode |= 0o4000;
    }
    if matches!(flags[5], b's' | b'S') {
        mode |= 0o2000;
    }
    if matches!(flags[8], b't' | b'T') {
        mode |= 0o1000;
    }
    Ok(mode)
}

/// Same-directory replacement. Data is binary stdin, never a shell argument.
pub(crate) fn atomic_replace(
    path: &Path,
    bytes: &[u8],
    mode: Option<u32>,
    cancel: Option<&Cancel>,
) -> io::Result<()> {
    let mode = mode.or_else(|| self::mode(path).ok());
    let mode = mode
        .map(|mode| format!("{:o}", mode & 0o7777))
        .unwrap_or_default();
    ExecutionEnvironment
        .checked(
            include_str!("scripts/replace.sh"),
            &[path, Path::new(&mode)],
            bytes,
            cancel,
        )
        .map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn system_tools_round_trip_binary_and_hostile_filenames() {
        let directory = tempfile::tempdir().unwrap();
        let file = directory.path().join("space 中文 ' ; $(echo BAD)\nfile");
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
        assert!(!exists(&file));
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
            // Reset mode for the next iteration without relying on root privileges.
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
    }
}
