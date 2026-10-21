#!/usr/bin/env bash
# Builds Groovy as an AppImage: dist/Groovy-<version>-<arch>.AppImage.
#
# Run from anywhere in the repository. It builds the release binary, lays out an AppDir with the
# desktop entry, metadata and icons, and packs it with appimagetool. The tools are downloaded
# once into target/tools, pinned to a version and checked against a known hash.
#
# The binary links to the system's libraries (glibc, Vulkan or OpenGL, Wayland, X11, fontconfig,
# D-Bus), so build on the oldest distribution the AppImage should run on. CI builds on Ubuntu
# 22.04.

set -euo pipefail

cd "$(dirname "$0")/.."
ROOT=$(pwd)

APP_ID=io.github.groovyapp.Groovy
ARCH=$(uname -m)
if [ "$ARCH" != "x86_64" ]; then
    echo "only x86_64 is set up here; found $ARCH" >&2
    exit 1
fi

APPIMAGETOOL_VERSION=1.9.1
APPIMAGETOOL_SHA256=ed4ce84f0d9caff66f50bcca6ff6f35aae54ce8135408b3fa33abfc3cb384eb0
RUNTIME_VERSION=20251108
RUNTIME_SHA256=2fca8b443c92510f1483a883f60061ad09b46b978b2631c807cd873a47ec260d

TOOLS="$ROOT/target/tools"
APPIMAGETOOL="$TOOLS/appimagetool-$APPIMAGETOOL_VERSION-$ARCH.AppImage"
RUNTIME="$TOOLS/runtime-$RUNTIME_VERSION-$ARCH"

# Downloads $2 to $1 unless it is there, then checks it against the hash $3.
fetch() {
    local path=$1 url=$2 sha=$3
    if [ ! -f "$path" ]; then
        mkdir -p "$(dirname "$path")"
        curl -fsSL -o "$path.part" "$url"
        mv "$path.part" "$path"
    fi
    echo "$sha  $path" | sha256sum --check --quiet
}

fetch "$APPIMAGETOOL" \
    "https://github.com/AppImage/appimagetool/releases/download/$APPIMAGETOOL_VERSION/appimagetool-$ARCH.AppImage" \
    "$APPIMAGETOOL_SHA256"
fetch "$RUNTIME" \
    "https://github.com/AppImage/type2-runtime/releases/download/$RUNTIME_VERSION/runtime-$ARCH" \
    "$RUNTIME_SHA256"
chmod +x "$APPIMAGETOOL"

VERSION=$(cargo metadata --no-deps --format-version 1 \
    | python3 -c 'import json, sys; print(next(p["version"] for p in json.load(sys.stdin)["packages"] if p["name"] == "groovy"))')

cargo build --release --locked -p groovy

APPDIR="$ROOT/target/appimage/AppDir"
rm -rf "$APPDIR"
mkdir -p "$APPDIR/usr/bin" "$APPDIR/usr/share/applications" "$APPDIR/usr/share/metainfo"

install -m755 target/release/groovy "$APPDIR/usr/bin/groovy"
install -m644 "assets/$APP_ID.desktop" "$APPDIR/usr/share/applications/$APP_ID.desktop"
install -m644 "packaging/linux/$APP_ID.metainfo.xml" "$APPDIR/usr/share/metainfo/$APP_ID.appdata.xml"
for size in 16 24 32 48 64 128 256 512; do
    install -Dm644 "assets/icons/app-icon-$size.png" \
        "$APPDIR/usr/share/icons/hicolor/${size}x${size}/apps/$APP_ID.png"
done
install -Dm644 assets/icons/app-icon.svg "$APPDIR/usr/share/icons/hicolor/scalable/apps/$APP_ID.svg"

# appimagetool reads the desktop entry and icon from the top of the AppDir.
cp "assets/$APP_ID.desktop" "$APPDIR/$APP_ID.desktop"
cp assets/icons/app-icon-256.png "$APPDIR/$APP_ID.png"
cp assets/icons/app-icon-256.png "$APPDIR/.DirIcon"

cat > "$APPDIR/AppRun" <<'APPRUN'
#!/bin/sh
# Starts Groovy from inside the AppImage. $APPIMAGE (set by the runtime) is the AppImage's own
# path, which Groovy uses for its login entry and link handler.
HERE=$(dirname "$(readlink -f "$0")")
exec "$HERE/usr/bin/groovy" "$@"
APPRUN
chmod +x "$APPDIR/AppRun"

mkdir -p dist
OUTPUT="$ROOT/dist/Groovy-$VERSION-$ARCH.AppImage"
rm -f "$OUTPUT"
# Extracting rather than mounting lets this run where FUSE is not available, as in CI.
ARCH=$ARCH APPIMAGE_EXTRACT_AND_RUN=1 "$APPIMAGETOOL" --no-appstream --runtime-file "$RUNTIME" \
    "$APPDIR" "$OUTPUT"
echo "built $OUTPUT"
