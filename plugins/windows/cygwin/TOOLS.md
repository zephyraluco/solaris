# Bash-backed execution environment

Solaris starts this runtime's `bin/bash.exe --noprofile --norc` and explicitly loads `/etc/profile` for shell commands and all
structured file/search tools. The login profile restricts command lookup to
`/usr/local/bin:/usr/bin:/bin`. System files are addressed through `/cygdrive`;
HOME and TMPDIR refer to user-writable locations outside the installation.

The runtime must include Bash, cygpath, cat, head, cp, ls, chmod, mkdir, mktemp,
mv, rm, readlink, rg and fd, plus their DLL dependencies. Startup checks their
presence. Rust sends scripts and positional arguments as NUL-delimited stdin frames, followed
by raw file bytes. Only a fixed dispatcher crosses the native Windows argv boundary;
Cygwin startup globbing cannot expand regexes or strip argument backslashes.

`bin/readlink.exe` was extracted from the official Cygwin coreutils 9.0-1 package:

- Package: https://mirrors.kernel.org/sourceware/cygwin/x86_64/release/coreutils/coreutils-9.0-1.tar.xz
- SHA-512: `530f47640d878f5953580f3556c492a93786191abe573322026b70206a46c7f17d05927784db3e7f2c044d1555f35941880316f7288b887eaec770a9eda4c687`
- Corresponding source: https://mirrors.kernel.org/sourceware/cygwin/x86_64/release/coreutils/coreutils-9.0-1-src.tar.xz

readlink follows file symlinks when normalizing approval and mutation paths;
Bash `cd -P` resolves directory symlinks. No fallback to a Windows host tool is
performed.
