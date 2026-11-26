#!/bin/bash
# 앱을 빌드해 맥 서버의 앱 업데이트 폴더에 올린다. 폰 앱의 '업데이트 설치'가 여기서 받아 간다.
#
# 사용: android/publish.sh
#   NOTIFIER_APP_DIR  올릴 폴더(기본 ~/.local/share/ai-task-notifier/app, 서버 기본값과 같음)
#   JAVA_HOME         JDK 17(없으면 /usr/libexec/java_home -v 17)
#
# 폰에 설치된 앱과 같은 키로 서명돼야 기존 앱 위에 설치된다(지금은 이 맥의 디버그 키).
set -euo pipefail
cd "$(dirname "$0")"

DEST="${NOTIFIER_APP_DIR:-$HOME/.local/share/ai-task-notifier/app}"
export JAVA_HOME="${JAVA_HOME:-$(/usr/libexec/java_home -v 17)}"

./gradlew -q assembleDebug
OUT=app/build/outputs/apk/debug

mkdir -p "$DEST"
chmod 700 "$DEST"
# 새 파일을 임시 이름으로 다 만든 뒤 APK → 버전 파일 순으로 바꾼다.
# 버전 파일에 APK의 크기·SHA-256을 함께 적어, 서버는 둘이 어긋나면(바꾸는 도중) 내려주지 않는다.
cp "$OUT/app-debug.apk" "$DEST/latest.apk.tmp"
python3 -I - "$OUT/output-metadata.json" "$DEST/latest.apk.tmp" > "$DEST/latest.json.tmp" <<'PY'
import hashlib, json, os, sys
e = json.load(open(sys.argv[1]))["elements"][0]
apk = sys.argv[2]
sha = hashlib.sha256(open(apk, "rb").read()).hexdigest()
print(json.dumps({"version_code": e["versionCode"], "version_name": e["versionName"],
                  "size": os.path.getsize(apk), "sha256": sha}))
PY
mv -f "$DEST/latest.apk.tmp" "$DEST/latest.apk"
mv -f "$DEST/latest.json.tmp" "$DEST/latest.json"
chmod 600 "$DEST/latest.apk" "$DEST/latest.json"
echo "올림: $(cat "$DEST/latest.json") → $DEST"
