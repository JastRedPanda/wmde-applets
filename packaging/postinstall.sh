#!/bin/sh
# Обновляет кэши иконок и .desktop после установки. Ошибки не роняют установку.
set -e
command -v gtk-update-icon-cache >/dev/null 2>&1 && gtk-update-icon-cache -f -t /usr/share/icons/hicolor || true
command -v update-desktop-database >/dev/null 2>&1 && update-desktop-database /usr/share/applications || true
command -v systemctl >/dev/null 2>&1 && systemctl --user daemon-reload || true
