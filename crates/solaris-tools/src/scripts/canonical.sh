set -e
# Appending a sentinel prevents command substitution from deleting newlines
# that belong to a path. Strip exactly the command's final newline afterward.
get() {
    value=$("$@" && printf '.') || return
    value=${value%.}
    value=${value%$'\n'}
}
p=$1
if [[ $2 == cygwin ]]; then get cygpath -au -- "$p"; p=$value; fi
[[ $p = /* ]] || p=$PWD/$p
# Collapsing a missing "component/.." can expose an existing symlink.
# The first pass removes those lexical suffixes; the second resolves the
# newly exposed existing prefix before permissions or mutations use it.
for normalization_pass in 1 2; do
suffix=()
while [[ ! -e $p && ! -L $p ]]; do
    suffix=("${p##*/}" "${suffix[@]}")
    p=${p%/*}
    [[ -n $p ]] || p=/
done
physical_dir() { cd -P -- "$1" && pwd -P; }
if [[ -d $p ]]; then
    get physical_dir "$p"; p=$value
else
    name=${p##*/}; parent=${p%/*}; [[ -n $parent ]] || parent=/
    get physical_dir "$parent"; p=${value%/}/$name
    count=0
    while [[ -L $p ]]; do
        ((count+=1)); ((count<=40)) || { printf 'too many symbolic links' >&2; exit 1; }
        # GNU and BSD readlink differ in their default final-newline behavior.
        # -n requests raw target bytes; the sentinel preserves target newlines.
        value=$(readlink -n "$p" && printf '.') || exit
        target=${value%.}
        [[ $target = /* ]] && p=$target || p=${p%/*}/$target
        parent=${p%/*}; name=${p##*/}
        get physical_dir "$parent"; p=${value%/}/$name
    done
fi
for part in "${suffix[@]}"; do
    case $part in
        ''|.) ;;
        ..) p=${p%/*}; [[ -n $p ]] || p=/ ;;
        *) p=${p%/}/$part ;;
    esac
done
done
printf '%s\n' "$p"
