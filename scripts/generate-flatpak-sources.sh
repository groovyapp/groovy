#!/usr/bin/env bash
# Writes packaging/flatpak/cargo-sources.json from Cargo.lock, for the Flatpak build.
#
# The generator is Flathub's flatpak-cargo-generator, downloaded at a pinned hash. It needs
# Python with aiohttp, PyYAML and tomlkit. Two fixes are applied to the generator before it runs:
#
# - Zed's lint test fixture has a crate named "gpui" that would replace the real one, so
#   directories named test_fixture are skipped.
# - The cargo config is written as config.toml, because cargo warns about the old name.
#
# Run from anywhere in the repository.

set -euo pipefail

cd "$(dirname "$0")/.."

GENERATOR_URL="https://raw.githubusercontent.com/flatpak/flatpak-builder-tools/master/cargo/flatpak-cargo-generator.py"
GENERATOR_SHA256=0a2db6be87d75910facef28ab46d4d6460802e8419ab850d0caa6a364d26b380
PYTHON="${PYTHON:-python3}"

WORK=$(mktemp -d "${TMPDIR:-/tmp}/groovy-flatpak.XXXXXX")
trap 'rm -rf "$WORK"' EXIT

# The downloaded script is run from its own directory and with python -I, so nothing next to it
# can be imported in its place.
curl -fsSL -o "$WORK/flatpak-cargo-generator.py" "$GENERATOR_URL"
echo "$GENERATOR_SHA256  $WORK/flatpak-cargo-generator.py" | sha256sum --check --quiet

python3 - "$WORK/flatpak-cargo-generator.py" <<'PY'
import sys
path = sys.argv[1]
s = open(path).read()
old = 'for child in os.scandir(root_dir):\n            if child.is_dir():'
new = ('for child in os.scandir(root_dir):\n'
       '            # Test fixtures can reuse the name of a real crate; leave them out.\n'
       '            if child.is_dir() and child.name != "test_fixture":')
assert s.count(old) == 1, "the generator changed; update this script"
open(path, "w").write(s.replace(old, new))
PY

"$PYTHON" -I "$WORK/flatpak-cargo-generator.py" Cargo.lock -o "$WORK/cargo-sources.json"

"$PYTHON" - "$WORK/cargo-sources.json" packaging/flatpak/cargo-sources.json <<'PY'
import json, sys
sources = json.load(open(sys.argv[1]))
for source in sources:
    if source.get("dest") == "cargo" and source.get("dest-filename") == "config":
        source["dest-filename"] = "config.toml"
with open(sys.argv[2], "w") as out:
    json.dump(sources, out, indent=4)
    out.write("\n")
print(f"wrote {sys.argv[2]} with {len(sources)} sources")
PY
