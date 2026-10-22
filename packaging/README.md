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

`packaging/flatpak/io.github.groovyapp.Groovy.yml` builds the app against the Freedesktop runtime 25.08 and its Rust extension. It builds offline, from the sources listed in `packaging/flatpak/cargo-sources.json`, which must be regenerated whenever `Cargo.lock` changes:

```bash
scripts/generate-flatpak-sources.sh
flatpak install --user flathub org.freedesktop.Platform//25.08 org.freedesktop.Sdk//25.08 org.freedesktop.Sdk.Extension.rust-stable//25.08
flatpak-builder --user --force-clean --disable-rofiles-fuse target/flatpak-build packaging/flatpak/io.github.groovyapp.Groovy.yml
```

The generator script needs Python with `aiohttp`, `PyYAML` and `tomlkit`. It pins Flathub's `flatpak-cargo-generator.py` by hash, and applies two fixes to it: Zed's lint test fixture has a crate named `gpui` that would replace the real one, and cargo's config is named `config.toml`.

The build was tested on this machine, with flatpak-builder 1.4 and the 25.08 runtime. The app starts in the sandbox, selects the GPU and uses X11. Under a Wayland session it hasn't been run yet.

The build runs `cargo build` with `--offline` but not `--locked`. The generated vendor manifests don't match `Cargo.lock` exactly, so cargo may adjust the lock file inside the build. The sources are still exactly those in `Cargo.lock`.

To install a built copy locally, export it to a repository and install from there:

```bash
flatpak build-export target/flatpak-repo target/flatpak-build
flatpak remote-add --user --no-gpg-verify groovy-local target/flatpak-repo
flatpak install --user groovy-local io.github.groovyapp.Groovy
```

Inside a Flatpak, links are handled by the package's own desktop entry, and Groovy does not write
a login entry: starting with the session will need the background portal.
