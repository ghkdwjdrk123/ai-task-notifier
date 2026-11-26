#!/bin/bash
# 앱 정식 서명 키와 그 비밀번호를 비밀번호를 건 디스크 이미지(AES-256)로 백업한다(기본: iCloud Drive).
#
# 사용: android/backup-signing-key.sh [만들 이미지 경로]
#   키        $NOTIFIER_KEYSTORE (기본 ~/.config/ai-task-notifier/release.keystore)
#   키 비밀번호 맥 키체인 "ai-task-notifier release keystore"에서 읽어 이미지 안에만 넣는다
#   이미지    기본 iCloud Drive/ai-task-notifier-release-key.dmg
#
# 이미지 비밀번호는 실행 중에 직접 두 번 입력한다(비밀번호 관리자에 저장해 두기).
# 만든 뒤 같은 비밀번호로 열어 키·비밀번호가 그대로 들어갔는지 확인한다.
#
# 되살리기: 이미지를 열고 안의 README.txt대로 키 파일 복사와 키체인 등록.
set -euo pipefail

KEY="${NOTIFIER_KEYSTORE:-$HOME/.config/ai-task-notifier/release.keystore}"
SERVICE="ai-task-notifier release keystore"
OUT="${1:-$HOME/Library/Mobile Documents/com~apple~CloudDocs/ai-task-notifier-release-key.dmg}"

[ -f "$KEY" ] || { echo "키 파일이 없습니다: $KEY" >&2; exit 1; }
if [ -e "$OUT" ]; then
    echo "이미 있습니다: $OUT" >&2
    echo "새로 만들려면 먼저 지우거나 다른 경로를 주세요." >&2
    exit 1
fi

umask 077
TMP=$(mktemp -d)
MNT="$TMP/mnt"
cleanup() {
    hdiutil detach -quiet "$MNT" 2>/dev/null || true
    rm -rf "$TMP"
}
trap cleanup EXIT

mkdir -p "$TMP/key" "$MNT"
cp "$KEY" "$TMP/key/release.keystore"
security find-generic-password -s "$SERVICE" -w > "$TMP/key/keystore-password.txt"
FP=$(keytool -list -keystore "$KEY" -storetype PKCS12 -storepass:file "$TMP/key/keystore-password.txt" 2>/dev/null \
    | grep -E "SHA-?256" | sed -E 's/^.*SHA-?256\)?: *//' | head -1)
cat > "$TMP/key/README.txt" <<EOF
ai-task-notifier 폰 앱 정식 서명 키 백업 ($(date '+%Y-%m-%d'))

- release.keystore: 서명 키(PKCS12, 별칭 notifier)
- keystore-password.txt: 키 비밀번호(이 이미지 밖에 두지 말 것)
- 인증서 SHA-256: ${FP:-확인 못 함}

되살리기(새 맥 또는 디스크 교체 뒤):
  mkdir -p ~/.config/ai-task-notifier && chmod 700 ~/.config/ai-task-notifier
  cp release.keystore ~/.config/ai-task-notifier/release.keystore
  chmod 600 ~/.config/ai-task-notifier/release.keystore
  security add-generic-password -a "\$USER" -s "$SERVICE" -w "\$(cat keystore-password.txt)"
  그 뒤 android/publish.sh로 빌드하면 폰에 깔린 앱 위에 업데이트됩니다.
  (이 키가 없으면 앱을 지우고 새로 설치·등록해야 합니다.)
EOF

echo "디스크 이미지 비밀번호를 정해 두 번 입력하세요. 비밀번호 관리자에 꼭 저장해 두세요."
hdiutil create -quiet -encryption AES-256 -srcfolder "$TMP/key" -volname "ai-task-notifier key" -format UDZO "$OUT"
chmod 600 "$OUT"

echo "확인을 위해 같은 비밀번호로 이미지를 한 번 엽니다."
hdiutil attach -quiet -nobrowse -readonly -mountpoint "$MNT" "$OUT"
if cmp -s "$KEY" "$MNT/release.keystore" && cmp -s "$TMP/key/keystore-password.txt" "$MNT/keystore-password.txt"; then
    echo "완료: 키와 비밀번호가 그대로 들어갔습니다 → $OUT"
else
    echo "확인 실패: 이미지 안의 내용이 원본과 다릅니다. 이미지를 지우고 다시 실행하세요." >&2
    exit 1
fi
