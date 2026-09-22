# Maintainer: WMDE <https://wmde.fun>
# Contributor: System76 <info@system76.com> (original cosmic-applets)
#
# Builds our fork Lin-WMDE/wmde-applets (branch wmde). Standalone WMDE component:
# installs the wmde-applets multiplexer binary plus wmde-applet-* / wmde-app-list /
# wmde-panel-button symlinks, and all resources under fun.wmde.* / wmde-* names, so it
# co-installs cleanly alongside cosmic-applets. NO conflicts/replaces cosmic-applets,
# NO compat symlinks into cosmic-* binary names.
pkgname=wmde-applets
pkgver=1.0.15
pkgrel=5
pkgdesc="WMDE panel applets (fork of cosmic-applets) - fun.wmde.Applet* config namespace"
arch=('x86_64')
url="https://wmde.fun"
license=('GPL-3.0-only')
# Runtime: wayland client + system libs. The applets shell out to wmde-settings,
# wmde-osd and the wmde-panel-button multiplexer at runtime (soft deps, not hard).
# noto-fonts: ships Noto Sans / Noto Sans Mono, the families the applets render their
# labels in. The theme is what NAMES them (fun.wmde.Tk moved to wmde-themes, which
# declares the same dependency); this one keeps the applets standing on their own.
depends=('glibc' 'gcc-libs' 'wayland' 'libxkbcommon' 'libpulse' 'pipewire' 'noto-fonts')
# Not a hard dependency: without it a click on the system monitor does nothing,
# which is the same as turning that setting off.
optdepends=('wmde-taskmgr: a click on the system monitor applet opens it')
makedepends=('rust' 'cargo' 'just' 'git' 'wayland' 'clang' 'lld' 'pkgconf')
# !lto: the weather applet links rustls/ring, whose C core is compiled by gcc with
# makepkg's CFLAGS. With the default lto option gcc emits LTO bytecode objects that
# rustc's ld.lld cannot read - every ring_core_* symbol comes out undefined.
options=('!lto')
source=("$pkgname::git+https://github.com/Lin-WMDE/wmde-applets.git#branch=wmde")
sha256sums=('SKIP')

pkgver() {
  cd "$srcdir/$pkgname"
  # WMDE version: <COSMIC major.minor>.<WMDE release>.<commits since the nearest tag>.g<short>.
  local desc
  desc=$(git describe --long --tags --abbrev=7 2>/dev/null || true)
  if [ -n "$desc" ]; then
    printf '1.8.1.%s.g%s' "$(printf '%s' "$desc" | sed -E 's/.*-([0-9]+)-g[0-9a-f]+$/\1/')" "$(git rev-parse --short=7 HEAD)"
  else
    printf '1.8.1.%s.g%s' "$(git rev-list --count HEAD)" "$(git rev-parse --short=7 HEAD)"
  fi
}

build() {
  cd "$srcdir/$pkgname"
  # x86-64-v3 (AVX2/BMI2) baseline for the WMDE repo; runs on Haswell+ (and the VM).
  export RUSTFLAGS="${RUSTFLAGS:+$RUSTFLAGS }-C target-cpu=x86-64-v3"
  just build-release
}

package() {
  cd "$srcdir/$pkgname"
  # Installs /usr/bin/wmde-applets + wmde-applet-* symlinks, fun.wmde.*.desktop,
  # the fun.wmde.Applets metainfo, the fun.wmde.StatusNotifierWatcher D-Bus/systemd
  # units, the fun.wmde.Settings.Applet.NetworkManager.SecretAgent policy and per-applet
  # icons (wmde-applet-* / fun.wmde.*). The fun.wmde.Tk and fun.wmde.AppList defaults are
  # NOT installed from here - they are part of the theme and ship in wmde-themes.
  just rootdir="$pkgdir" prefix=/usr install
  install -Dm644 LICENSE "$pkgdir/usr/share/licenses/$pkgname/LICENSE"
}
