set -e
path=$1
if [[ $3 == cygwin ]]; then
    value=$(cygpath -au -- "$path" && printf '.') || exit
    path=${value%.}
    path=${path%$'\n'}
fi
parent=${path%/*}
[[ $parent != "$path" ]] || parent=.
mkdir -p -- "$parent"
umask 077
temporary=$(mktemp "$parent/.solaris-write.XXXXXXXXXX")
trap 'rm -f -- "$temporary"' EXIT
# Preserve destination mode using portable cp -p, not GNU chmod --reference.
if [[ -f $path ]]; then cp -p -- "$path" "$temporary"; chmod u+w "$temporary"; fi
cat > "$temporary"
if [[ -n $2 ]]; then chmod "$2" "$temporary"; fi
# GNU sync supports per-file fsync (including directories). BSD/Cygwin use
# the portable whole-system barrier instead of assuming GNU-only options.
barrier() {
    case $OSTYPE in
        linux*) sync -- "$1" ;;
        *) sync ;;
    esac
}
barrier "$temporary"
mv -f -- "$temporary" "$path"
barrier "$parent"
trap - EXIT
