#!/bin/bash
# 앱 정식(release) 서명용 전용 키를 만든다. 한 번만 실행.
#
# - 키 파일: $NOTIFIER_KEYSTORE (기본 ~/.config/ai-task-notifier/release.keystore, 600, PKCS12, RSA 4096)
# - 비밀번호: 무작위 32자를 만들어 맥 키체인("ai-task-notifier release keystore")에만 둔다.
#   화면·파일에 쓰지 않는다. android/publish.sh가 서명할 때 키체인에서 읽는다.
# - 백업: android/backup-signing-key.sh가 키와 비밀번호를 함께 암호화 이미지에 넣는다.
set -euo pipefail

KEY="${NOTIFIER_KEYSTORE:-$HOME/.config/ai-task-notifier/release.keystore}"
SERVICE="ai-task-notifier release keystore"

if [ -e "$KEY" ]; then
    echo "이미 있습니다: $KEY (새로 만들면 폰 앱을 다시 설치해야 하므로 덮어쓰지 않습니다)" >&2
    exit 1
fi
if security find-generic-password -s "$SERVICE" >/dev/null 2>&1; then
    echo "키체인에 같은 이름의 항목이 이미 있습니다: $SERVICE" >&2
    exit 1
fi

mkdir -p "$(dirname "$KEY")"
chmod 700 "$(dirname "$KEY")"
umask 077
PW=$(openssl rand -base64 48 | tr -d '/+=\n' | cut -c1-32)
export PW
keytool -genkeypair -keystore "$KEY" -storetype PKCS12 -alias notifier \
    -keyalg RSA -keysize 4096 -validity 10000 -dname "CN=ai-task-notifier" \
    -storepass:env PW -keypass:env PW >/dev/null 2>&1
chmod 600 "$KEY"
security add-generic-password -a "$USER" -s "$SERVICE" -w "$PW"
unset PW
FP=$(keytool -list -keystore "$KEY" -storetype PKCS12 \
    -storepass "$(security find-generic-password -s "$SERVICE" -w)" 2>/dev/null \
    | grep -E "SHA-?256" | sed -E 's/^.*SHA-?256\)?: *//' | head -1)
echo "만듦: $KEY"
echo "인증서 SHA-256: $FP"
echo "비밀번호는 키체인 '$SERVICE'에 있습니다. 이제 android/backup-signing-key.sh로 백업하세요."
