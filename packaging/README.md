# Packaging

## Linux

The app ID is `io.github.groovyapp.Groovy`. The desktop entry is
`assets/io.github.groovyapp.Groovy.desktop`, and the AppStream metadata is
`packaging/linux/io.github.groovyapp.Groovy.metainfo.xml`. Check them with:

```bash
desktop-file-validate assets/io.github.groovyapp.Groovy.desktop
appstreamcli validate --no-net packaging/linux/io.github.groovyapp.Groovy.metainfo.xml
```

### AppImage

```bash
scripts/package-appimage.sh                                   # builds dist/Groovy-<version>-x86_64.AppImage
scripts/smoke-test-linux.sh dist/Groovy-0.0.1-x86_64.AppImage # starts it on Wayland and on X11
```

The script downloads appimagetool and the AppImage runtime once into `target/tools`, at pinned
versions checked against known hashes. The AppImage carries the Groovy binary, its desktop entry,
metadata and icons; it uses the system's glibc and graphics libraries, so build it on the oldest
distribution it should run on. The release workflow builds on Ubuntu 22.04.

When an AppImage runs, Groovy adds itself to the app launcher: a desktop entry that runs the
AppImage file, with its icon, under `~/.local/share`. The entry also handles `groovy://` links,
unless links are turned off in Settings. If the AppImage file is moved, running it again updates
the entry.

A tag like `v0.1.0` runs `.github/workflows/release.yml`, which builds the AppImage and attaches
it to the GitHub release.

### Flatpak

`packaging/flatpak/io.github.groovyapp.Groovy.yml` is a manifest for a Flatpak build. **It has
not been built yet**: flatpak-builder was not available where it was written. Before building,
generate the offline sources for the Rust dependencies:

```bash
python3 flatpak-cargo-generator.py Cargo.lock -o packaging/flatpak/cargo-sources.json
flatpak-builder --user --install --force-clean target/flatpak packaging/flatpak/io.github.groovyapp.Groovy.yml
```

`flatpak-cargo-generator.py` is in
[flatpak-builder-tools](https://github.com/flatpak/flatpak-builder-tools/tree/master/cargo).

Inside a Flatpak, links are handled by the package's own desktop entry, and Groovy does not write
a login entry: starting with the session will need the background portal.
