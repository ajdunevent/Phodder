#!/bin/sh
# Installs the built binary, data files and translations into /app. Runs
# inside the Flatpak SDK, from the project root, for both build-aux/dev.sh
# and the manifest. Usage: install.sh BINARY_DIR

set -eu

bin_dir=$1
app_id=net.dunevent.Phodder

install -Dm755 "$bin_dir/phodder" /app/bin/phodder
install -Dm644 COPYING "/app/share/licenses/$app_id/COPYING"

# msgfmt merges translations from po/ into the desktop entry and metainfo;
# with none listed in po/LINGUAS it copies them unchanged.
install -d /app/share/applications /app/share/metainfo
msgfmt --desktop --template "data/$app_id.desktop" -d po -o "/app/share/applications/$app_id.desktop"
msgfmt --xml --template "data/$app_id.metainfo.xml" -d po -o "/app/share/metainfo/$app_id.metainfo.xml"

# D-Bus activation starts the service from the window, and the window from
# the service and notifications. Inside Flatpak it's the only way: a process
# the window spawned would die with the window's sandbox.
install -d /app/share/dbus-1/services
for service in "$app_id" "$app_id.Service"; do
    sed 's|@bindir@|/app/bin|' "data/dbus-1/$service.service.in" > "/app/share/dbus-1/services/$service.service"
done

install -Dm644 "data/icons/hicolor/scalable/apps/$app_id.svg" "/app/share/icons/hicolor/scalable/apps/$app_id.svg"
install -Dm644 "data/icons/hicolor/symbolic/apps/$app_id-symbolic.svg" "/app/share/icons/hicolor/symbolic/apps/$app_id-symbolic.svg"

for po in po/*.po; do
    [ -e "$po" ] || continue
    lang=$(basename "$po" .po)
    install -d "/app/share/locale/$lang/LC_MESSAGES"
    msgfmt --check -o "/app/share/locale/$lang/LC_MESSAGES/phodder.mo" "$po"
done
