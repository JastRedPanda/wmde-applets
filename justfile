name := 'wmde-applets'
# Чтобы shell() работал и на Windows с Git Bash; на Linux игнорируется
set windows-shell := ["C:\\Program Files\\Git\\bin\\sh.exe", "-cu"]
rootdir := ''
prefix := '/usr'
clean := '0'
debug := '0'
vendor := '0'
target := if debug == '1' { 'debug' } else { 'release' }
vendor_args := if vendor == '1' { '--frozen --offline' } else { '' }
debug_args := if debug == '1' { '' } else { '--release' }
cargo_args := vendor_args + ' ' + debug_args
targetdir := env('CARGO_TARGET_DIR', 'target')
sharedir := rootdir + prefix + '/share'
iconsdir := sharedir + '/icons/hicolor'
prefixdir := prefix + '/bin'
bindir := rootdir + prefixdir
libdir := rootdir + prefix + '/lib'
# The dock's pinned entries and the toolkit defaults are not shipped from here: they are
# part of the theme and live in the wmde-themes package.
cosmic-applets-bin := prefixdir / 'wmde-applets'
# Settings reads these to build a page per applet; the path is part of the contract with
# third-party applets, which install their own schema into the same directory.
applet-settings-dir := sharedir + '/wmde/applet-settings'
metainfo := 'fun.wmde.Applets.metainfo.xml'
metainfo-src := 'data' / metainfo
metainfo-dst := clean(rootdir / prefix) / 'share' / 'metainfo' / metainfo

default: build-release

# Compiles with debug profile
build-debug *args:
    cargo build {{ args }}

# Compiles with release profile
build-release *args: (build-debug '--release' args)

# Compile with a vendored tarball
build-vendored *args: vendor-extract (build-release '--frozen --offline' args)

_link_applet name:
    ln -sf {{ cosmic-applets-bin }} {{ bindir }}/{{ name }}

_install_icons name:
    find {{ name }}/'data'/'icons' -type f -exec echo {} \; | rev | cut -d'/' -f-3 | rev | xargs -d '\n' -I {} install -Dm0644 {{ name }}/'data'/'icons'/{} {{ iconsdir }}/{}

_install_desktop path:
    install -Dm0644 {{ path }} {{ sharedir }}/applications/{{ file_name(path) }}

_install_bin name:
    install -Dm0755 {{ targetdir }}/{{ target }}/{{ name }} {{ bindir }}/{{ name }}

# `dir` is the source crate directory (upstream cosmic-* name, not renamed);
# `link` is the installed symlink/argv0 name (wmde-*). They intentionally diverge:
# only the ROOT binary is renamed to wmde-applets, member crate dirs keep upstream names.
_install_applet id dir link: (_install_icons dir) (_install_desktop 'target/xdgen/' + id + '.desktop') (_link_applet link)

_install_applet_schemas:
    install -Dm0644 -t {{ applet-settings-dir }} data/applet-settings/*.ron

_install_metainfo:
    install -Dm0644 {{ metainfo-src }} {{ metainfo-dst }}

_install_status_notifier_watcher:
    sed "s|@bindir@|{{ prefixdir }}|" cosmic-applet-status-area/data/dbus-1/fun.wmde.StatusNotifierWatcher.service.in > cosmic-applet-status-area/data/dbus-1/fun.wmde.StatusNotifierWatcher.service
    install -Dm0644 cosmic-applet-status-area/data/dbus-1/fun.wmde.StatusNotifierWatcher.service {{ sharedir }}/dbus-1/services/fun.wmde.StatusNotifierWatcher.service
    sed "s|@bindir@|{{ prefixdir }}|" cosmic-applet-status-area/data/fun.wmde.StatusNotifierWatcher.service.in > cosmic-applet-status-area/data/fun.wmde.StatusNotifierWatcher.service
    install -Dm0644 cosmic-applet-status-area/data/fun.wmde.StatusNotifierWatcher.service {{ libdir }}/systemd/user/fun.wmde.StatusNotifierWatcher.service

_install_secret_agent_policy:
    install -Dm0644 cosmic-applet-network/data/dbus-1/system.d/fun.wmde.Settings.Applet.NetworkManager.SecretAgent.conf {{ sharedir }}/dbus-1/system.d/fun.wmde.Settings.Applet.NetworkManager.SecretAgent.conf

# Installs files into the system
install: (_install_bin 'wmde-applets') (_link_applet 'wmde-panel-button') (_install_applet 'fun.wmde.AppList' 'cosmic-app-list' 'wmde-app-list') (_install_applet 'fun.wmde.AppletA11y' 'cosmic-applet-a11y' 'wmde-applet-a11y') (_install_applet 'fun.wmde.AppletAudio' 'cosmic-applet-audio' 'wmde-applet-audio') (_install_applet 'fun.wmde.AppletInputSources' 'cosmic-applet-input-sources' 'wmde-applet-input-sources') (_install_applet 'fun.wmde.AppletBattery' 'cosmic-applet-battery' 'wmde-applet-battery') (_install_applet 'fun.wmde.AppletBluetooth' 'cosmic-applet-bluetooth' 'wmde-applet-bluetooth') (_install_applet 'fun.wmde.AppletMinimize' 'cosmic-applet-minimize' 'wmde-applet-minimize') (_install_applet 'fun.wmde.AppletNetwork' 'cosmic-applet-network' 'wmde-applet-network') (_install_applet 'fun.wmde.AppletNotifications' 'cosmic-applet-notifications' 'wmde-applet-notifications') (_install_applet 'fun.wmde.AppletPower' 'cosmic-applet-power' 'wmde-applet-power') (_install_applet 'fun.wmde.AppletStatusArea' 'cosmic-applet-status-area' 'wmde-applet-status-area') (_install_applet 'fun.wmde.AppletSysmon' 'cosmic-applet-sysmon' 'wmde-applet-sysmon') (_install_applet 'fun.wmde.AppletTiling' 'cosmic-applet-tiling' 'wmde-applet-tiling') (_install_applet 'fun.wmde.AppletTime' 'cosmic-applet-time' 'wmde-applet-time') (_install_applet 'fun.wmde.AppletWeather' 'cosmic-applet-weather' 'wmde-applet-weather') (_link_applet 'wmde-weather-settings') (_install_applet 'fun.wmde.AppletWorkspaces' 'cosmic-applet-workspaces' 'wmde-applet-workspaces') _install_applet_schemas _install_metainfo _install_status_notifier_watcher _install_secret_agent_policy

# Vendor Cargo dependencies locally
vendor:
    mkdir -p .cargo
    cargo vendor --locked | head -n -1 > .cargo/config
    echo 'directory = "vendor"' >> .cargo/config
    tar pcf vendor.tar vendor
    rm -rf vendor

# Extracts vendored dependencies
[private]
vendor-extract:
    rm -rf vendor
    tar pxf vendor.tar

# Версия пакета как pkgver() в PKGBUILD: 1.10.0.<commits>.g<sha>.
# Теги epoch-* дают только счётчик через git describe, fallback — rev-list.
wmde-version := shell('desc=$(git describe --long --tags --abbrev=7 2>/dev/null || true); if [ -n "$desc" ]; then printf "1.10.0.%s.g%s" "$(printf "%s" "$desc" | sed -E "s/.*-([0-9]+)-g[0-9a-f]+$/\\1/")" "$(git rev-parse --short=7 HEAD)"; else printf "1.10.0.%s.g%s" "$(git rev-list --count HEAD)" "$(git rev-parse --short=7 HEAD)"; fi')
# CPU-baseline: пусто = чистый x86-64 (для deb/rpm по умолчанию), для v3: WMDE_CPU=x86-64-v3
wmde-cpu := env('WMDE_CPU', '')

# Раскладывает собранное в target/staging (тот же набор, что PKGBUILD кладёт в $pkgdir)
package-staging staging="target/staging":
    #!/usr/bin/env bash
    set -e
    rm -rf {{staging}}
    if [ -n "{{wmde-cpu}}" ]; then export RUSTFLAGS="${RUSTFLAGS:+$RUSTFLAGS }-C target-cpu={{wmde-cpu}}"; fi
    just build-release
    just rootdir="{{staging}}" prefix=/usr install
    # Categories=WMDE в .desktop от xdgen — незарегистрированная категория (E lintian/rpmlint).
    # Файлы NoDisplay, панель ищет апплеты по X-CosmicApplet, не по категориям — правим
    # только стейджинг в X-WMDE по спеке, сырцы не трогаем.
    sed -i 's/^Categories=WMDE;/Categories=X-WMDE;/' "{{staging}}/usr/share/applications/"*.desktop
    # makepkg на Arch стрипает бинарники сам, в deb/rpm это делаем руками
    # (симлинки не трогаем — strip ходит только по обычным файлам)
    find "{{staging}}/usr/bin" -type f -exec strip --strip-unneeded {} + 2>/dev/null || true
    just package-doc "{{staging}}"

# doc для deb/rpm: копирайт + changelog.yaml (nFPM сам разложит changelog
# в changelog.Debian.gz для deb и в %changelog для rpm)
package-doc staging="target/staging":
    #!/usr/bin/env bash
    set -e
    mkdir -p "{{staging}}/usr/share/doc/{{name}}"
    cp packaging/copyright "{{staging}}/usr/share/doc/{{name}}/copyright"
    chmod 644 "{{staging}}/usr/share/doc/{{name}}/copyright"
    printf -- '- semver: %s\n  date: %s\n  packager: WMDE <wmde@wmde.fun>\n  urgency: low\n  distribution: unstable\n  deb:\n    urgency: low\n    distributions:\n      - unstable\n  changes:\n    - note: Automated build via nFPM (see INSTALL.deb-rpm.md).\n' "{{wmde-version}}-1" "$(date -u +%Y-%m-%dT%H:%M:%SZ)" > target/changelog.yaml

# Стейджинг из вендора: сборка без сети к git-зависимостям.
# vendor.tar генерим один раз при наличии сети (`just vendor`), сам тарболл
# в git не коммитится (см. .gitignore), на офлайн-машину его привозим файлом.
# Соседние чек-ауты нужны и тут: path-зависимости вендором не покрываются.
package-staging-vendored staging="target/staging":
    #!/usr/bin/env bash
    set -e
    rm -rf {{staging}}
    if [ ! -f vendor.tar ]; then just vendor; fi
    if [ -n "{{wmde-cpu}}" ]; then export RUSTFLAGS="${RUSTFLAGS:+$RUSTFLAGS }-C target-cpu={{wmde-cpu}}"; fi
    just build-vendored # сам распакует vendor.tar и соберёт --frozen --offline
    just rootdir="{{staging}}" prefix=/usr install
    sed -i 's/^Categories=WMDE;/Categories=X-WMDE;/' "{{staging}}/usr/share/applications/"*.desktop
    find "{{staging}}/usr/bin" -type f -exec strip --strip-unneeded {} + 2>/dev/null || true
    just package-doc "{{staging}}"
    rm -rf vendor .cargo

# Собирает .deb через nFPM (нужен nfpm в PATH)
deb:
    #!/usr/bin/env bash
    set -e
    just package-staging
    export WMDE_VERSION="{{wmde-version}}"
    nfpm package --config packaging/nfpm.yaml --packager deb --target target/

# Собирает .rpm через nFPM (нужен nfpm в PATH)
rpm:
    #!/usr/bin/env bash
    set -e
    just package-staging
    export WMDE_VERSION="{{wmde-version}}"
    nfpm package --config packaging/nfpm.yaml --packager rpm --target target/

# .deb из вендора (офлайн-сборка)
deb-vendored:
    #!/usr/bin/env bash
    set -e
    just package-staging-vendored
    export WMDE_VERSION="{{wmde-version}}"
    nfpm package --config packaging/nfpm.yaml --packager deb --target target/

# .rpm из вендора (офлайн-сборка)
rpm-vendored:
    #!/usr/bin/env bash
    set -e
    just package-staging-vendored
    export WMDE_VERSION="{{wmde-version}}"
    nfpm package --config packaging/nfpm.yaml --packager rpm --target target/

# Bump cargo version, create git commit, and create tag
tag version:
    find -type f -name Cargo.toml -exec sed -i '0,/^version/s/^version.*/version = "{{ version }}"/' '{}' \; -exec git add '{}' \;
    cargo check
    cargo clean
    dch -D noble -v {{ version }}
    git add Cargo.lock debian/changelog
