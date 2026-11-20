#!/usr/bin/env bash
# Regenerates po/phodder.pot from the files in po/POTFILES.in, then merges
# it into each po/<lang>.po listed in po/LINGUAS.
#
# Rust strings are marked with gettext, ngettext, gettext_f, ngettext_f and
# N_ (see crates/phodder/src/i18n.rs). xgettext reads Rust from 0.24; older
# versions read it as C, which handles the string literals used here.

set -euo pipefail

cd "$(dirname "$0")/.."

POT=po/phodder.pot
mapfile -t files < <(grep -v '^\s*#' po/POTFILES.in | grep -v '^\s*$')
rust_files=()
data_files=()
for file in "${files[@]}"; do
    case $file in
        *.rs) rust_files+=("$file") ;;
        *) data_files+=("$file") ;;
    esac
done

if xgettext --help | grep -q '\bRust\b'; then
    rust_language=Rust
else
    rust_language=C
fi

common=(--from-code=UTF-8 --package-name=phodder --msgid-bugs-address=https://github.com/
    --add-comments=Translators: --sort-by-file)

xgettext "${common[@]}" --output="$POT" "${data_files[@]}"
xgettext "${common[@]}" --join-existing --output="$POT" --language="$rust_language" \
    --keyword=gettext --keyword=ngettext:1,2 --keyword=gettext_f --keyword=ngettext_f:1,2 --keyword=N_ \
    "${rust_files[@]}"

echo "Wrote $POT ($(grep -c '^msgid ' "$POT") strings)"

while read -r lang; do
    [[ -z $lang || $lang == \#* ]] && continue
    msgmerge --update --backup=none "po/$lang.po" "$POT"
    echo "Updated po/$lang.po"
done <po/LINGUAS
