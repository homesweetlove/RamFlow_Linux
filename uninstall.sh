#!/usr/bin/env bash
# RamFlow uninstaller. Stops the service first, which restores every priority it changed.
# Usage: sudo ./uninstall.sh [--purge]    (--purge also removes /etc/ramflow and /var/lib/ramflow)
set -euo pipefail

PREFIX="${PREFIX:-/usr/local}"
SYSCONFDIR="${SYSCONFDIR:-/etc}"
UNITDIR="${UNITDIR:-/etc/systemd/system}"
STATEDIR="${STATEDIR:-/var/lib/ramflow}"

if [[ -z "${NO_SYSTEMCTL:-}" ]] && command -v systemctl >/dev/null 2>&1; then
  systemctl disable --now ramflow.service 2>/dev/null || true
fi
# Belt and braces: undo anything a crashed run may have left behind.
if [[ -x "$PREFIX/bin/ramflow" ]]; then
  "$PREFIX/bin/ramflow" restore --state-dir "$STATEDIR" || true
fi
rm -f "$UNITDIR/ramflow.service" "$PREFIX/bin/ramflow"
if [[ -z "${NO_SYSTEMCTL:-}" ]] && command -v systemctl >/dev/null 2>&1; then
  systemctl daemon-reload
fi
if [[ "${1:-}" == "--purge" ]]; then
  rm -rf "$SYSCONFDIR/ramflow" "$STATEDIR"
  echo "설정과 저널까지 지웠습니다."
else
  echo "설정은 남겨두었습니다: $SYSCONFDIR/ramflow  (모두 지우려면 --purge)"
fi
echo "제거 완료."
