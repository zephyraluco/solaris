set -e
path=$1
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
mv -f -- "$temporary" "$path"
trap - EXIT
