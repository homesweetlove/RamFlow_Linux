#!/usr/bin/env bash
# RamFlow installer.  Usage: sudo ./install.sh [--enable]
#   Builds the release binary if needed, installs it, a default (dry-run) config and the
#   systemd unit. The service is only enabled when you pass --enable.
# Overridable for packaging/tests: PREFIX, SYSCONFDIR, UNITDIR, NO_SYSTEMCTL=1
set -euo pipefail

PREFIX="${PREFIX:-/usr/local}"
SYSCONFDIR="${SYSCONFDIR:-/etc}"
UNITDIR="${UNITDIR:-/etc/systemd/system}"
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ENABLE=0
[[ "${1:-}" == "--enable" ]] && ENABLE=1

if [[ "$(id -u)" -ne 0 && -z "${PREFIX_OVERRIDE_OK:-}" && "${PREFIX}" == "/usr/local" ]]; then
  echo "root 권한이 필요합니다: sudo ./install.sh" >&2
  exit 1
fi

BIN="$HERE/target/release/ramflow"
if [[ ! -x "$BIN" ]]; then
  command -v cargo >/dev/null 2>&1 || { echo "cargo(러스트)가 필요합니다: https://rustup.rs" >&2; exit 1; }
  echo "빌드 중..."
  (cd "$HERE" && cargo build --release --locked 2>/dev/null || cargo build --release)
fi

install -Dm755 "$BIN" "$PREFIX/bin/ramflow"
install -d "$SYSCONFDIR/ramflow"
if [[ -e "$SYSCONFDIR/ramflow/ramflow.conf" ]]; then
  echo "기존 설정을 그대로 둡니다: $SYSCONFDIR/ramflow/ramflow.conf"
  install -Dm644 "$HERE/packaging/ramflow.conf.example" "$SYSCONFDIR/ramflow/ramflow.conf.example"
else
  install -Dm644 "$HERE/packaging/ramflow.conf.example" "$SYSCONFDIR/ramflow/ramflow.conf"
fi
# Keep the unit's ExecStart in step with PREFIX/SYSCONFDIR.
install -d "$UNITDIR"
sed -e "s#/usr/local/bin/ramflow#$PREFIX/bin/ramflow#" \
    -e "s#/etc/ramflow/ramflow.conf#$SYSCONFDIR/ramflow/ramflow.conf#" \
    "$HERE/packaging/ramflow.service" > "$UNITDIR/ramflow.service"
chmod 644 "$UNITDIR/ramflow.service"

if [[ -z "${NO_SYSTEMCTL:-}" ]] && command -v systemctl >/dev/null 2>&1; then
  systemctl daemon-reload
  if [[ "$ENABLE" -eq 1 ]]; then
    systemctl enable --now ramflow.service
    echo "서비스를 켰습니다 (예행연습 모드). 로그: journalctl -u ramflow -f"
  else
    echo "서비스를 켜려면: sudo systemctl enable --now ramflow"
  fi
fi

echo "설치 완료. 먼저 점검해 보세요: ramflow doctor"
echo "설정 파일: $SYSCONFDIR/ramflow/ramflow.conf  (기본은 예행연습이라 아무것도 바꾸지 않습니다)"
