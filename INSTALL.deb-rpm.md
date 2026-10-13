# Установка на deb/rpm-дистрибутивах без сборки из сырцов вручную

Шаблон: см. `wmde-start-menu` и далее. Автор собирает пакеты у себя локально, как сейчас `makepkg -si`.

Опорные платформы — Debian stable и Fedora: `.deb` собираем на Debian (встаёт и на Ubuntu/Mint),
`.rpm` — на Fedora (встаёт и на RHEL/openSUSE). Наоборот не гарантируется.

## Исходники рядом

Обычная сборка требует соседние чек-ауты (path-зависимости Cargo):

```
work/
  wmde-applets/                <- этот репозиторий (ветка wmde)
  libcosmic/                   <- git clone -b wmde https://github.com/Lin-WMDE/libcosmic.git
  wmde-comp/                   <- git clone -b wmde https://github.com/Lin-WMDE/wmde-comp.git
  wmde-settings-daemon/        <- git clone -b wmde https://github.com/Lin-WMDE/wmde-settings-daemon.git
  wmde-settings/               <- git clone -b wmde https://github.com/Lin-WMDE/wmde-settings.git
  wmde-notifications/          <- git clone -b wmde https://github.com/Lin-WMDE/wmde-notifications.git
  wmde-dbus-settings-bindings/ <- git clone -b wmde https://github.com/Lin-WMDE/wmde-dbus-settings-bindings.git
  wmde-panel/                  <- git clone -b wmde https://github.com/Lin-WMDE/wmde-panel.git
```

Внутри `libcosmic` лежат git-сабмодули (`iced` ветки `wmde`, `cosmic-icons`).
Официальный URL сабмодуля `iced` — SSH (`git@github.com:`), без ключей не клонируется.
Обход без ключей:

```sh
git config --global url."https://github.com/".insteadOf "git@github.com:"
git -C libcosmic submodule update --init
```

## Собрать

```sh
# зависимости сборки: rustc>=1.93 cargo just clang lld pkgconf wayland pulse pipewire + nfpm
cargo install --locked nfpm  # или бинарник с github.com/goreleaser/nfpm/releases

just deb   # -> target/wmde-applets_*.deb
just rpm   # -> target/wmde-applets-*.rpm
```

Версия — `1.10.0.<commits>.g<sha>`, как `pkgver()` в `PKGBUILD`.
По умолчанию сборка под чистый `x86-64`. Опционально под `x86-64-v3` (как в Arch-репозитории):

```sh
WMDE_CPU=x86-64-v3 just deb
```

## Офлайн-сборка (без сети к git-зависимостям)

`vendor.tar` в git не лежит (см. `.gitignore`), генерим на машине с сетью один раз
(соседние чек-ауты для резолва нужны):

```sh
just vendor          # -> vendor.tar (реестр + git-крейты)
just deb-vendored    # сборка --frozen --offline -> target/*.deb
just rpm-vendored    # аналогично -> target/*.rpm
```

На машину без сети привозим `vendor.tar` файлом. Соседние чек-ауты тоже нужны:
path-зависимости вендором не покрываются, Cargo читает их с диска.

## Поставить

```sh
# Debian/Ubuntu/Mint
sudo apt install ./target/wmde-applets_*.deb

# Fedora/RHEL/openSUSE
sudo dnf install ./target/wmde-applets-*.rpm
```

Рантайм-зависимости подтянутся сами (см. `packaging/nfpm.yaml`), включая шрифты Noto
для подписей (в rpm — через виртуальные `font()`-provides, общие для Fedora и openSUSE).
Клик по системному монитору опционально тянет `wmde-taskmgr` (suggests).
Десктоп-записи и иконки подхватываются через postinstall-хуки.
