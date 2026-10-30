#!/usr/bin/env bash
# Add the small Java platform bridge to an APK already built by cargo-apk.
# Rust/native libraries are reused; this compiles only Java and manifest data.
set -euo pipefail

if [[ $# -ne 1 ]]; then
  echo "Usage: $0 <cargo-apk release APK>" >&2
  exit 2
fi

task_script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
task_sdk_root="${ANDROID_SDK_ROOT:-${ANDROID_HOME:-}}"
task_build_tools="${task_sdk_root}/build-tools/35.0.0"
task_android_jar="${task_sdk_root}/platforms/android-35/android.jar"
task_apk="$(realpath -- "$1")"
task_manifest="$(dirname -- "$task_apk")/AndroidManifest.xml"

if [[ -z "$task_sdk_root" || ! -f "$task_android_jar" || ! -f "$task_manifest" ]]; then
  echo "Android SDK 35 and cargo-apk's adjacent AndroidManifest.xml are required." >&2
  exit 1
fi
for task_tool in aapt d8 zipalign apksigner; do
  test -x "${task_build_tools}/${task_tool}"
done
test -f "$task_apk"
: "${CARGO_APK_RELEASE_KEYSTORE:?Release keystore path is required}"
: "${CARGO_APK_RELEASE_KEYSTORE_PASSWORD:?Release keystore password is required}"

task_work="$(mktemp -d "${TMPDIR:-/tmp}/friendshub-android-java.XXXXXX")"
trap 'rm -rf -- "$task_work"' EXIT
mkdir -p "$task_work/classes" "$task_work/dex"
mapfile -d '' task_sources < <(find "$task_script_dir/java" -name '*.java' -print0)
if [[ ${#task_sources[@]} -eq 0 ]]; then
  echo "The Android Java launcher sources are missing." >&2
  exit 1
fi
javac -encoding UTF-8 -source 8 -target 8 -bootclasspath "$task_android_jar" \
  -d "$task_work/classes" "${task_sources[@]}"
jar --create --file "$task_work/bridge.jar" -C "$task_work/classes" .
"$task_build_tools/d8" --release --min-api 26 --lib "$task_android_jar" \
  --output "$task_work/dex" "$task_work/bridge.jar"
test -s "$task_work/dex/classes.dex"

# cargo-apk supports a custom Activity but has no Java compilation or provider
# manifest hook. Merge the local additive fragment into its generated manifest.
python3 - "$task_manifest" "$task_script_dir/manifest-fragment.xml" "$task_work/AndroidManifest.xml" <<'PY'
import copy
import pathlib
import sys
import xml.etree.ElementTree as ET

android = "http://schemas.android.com/apk/res/android"
ET.register_namespace("android", android)
source, fragment, output = map(pathlib.Path, sys.argv[1:])
manifest = ET.parse(source)
application = manifest.getroot().find("application")
if application is None:
    raise SystemExit("cargo-apk manifest has no application element")
activity = application.find("activity")
if activity is None or activity.get(f"{{{android}}}name") != "com.somuchsystem.friendshub.FriendsHubActivity":
    raise SystemExit("cargo-apk manifest does not select FriendsHubActivity")
if application.get(f"{{{android}}}hasCode") != "true":
    raise SystemExit("cargo-apk manifest must enable application.has_code")
if fragment.exists():
    additions = ET.parse(fragment).getroot().find("application")
    if additions is None:
        raise SystemExit("Android manifest fragment has no application element")
    for child in additions:
        if child.tag != "provider":
            raise SystemExit("Only provider additions are allowed in the Android manifest fragment")
        name = child.get(f"{{{android}}}name")
        if not name or any(existing.get(f"{{{android}}}name") == name for existing in application):
            raise SystemExit("Android manifest fragment contains a missing or duplicate component name")
        application.append(copy.deepcopy(child))
manifest.write(output, encoding="utf-8", xml_declaration=True)
PY

task_aapt_args=(package -f -F "$task_work/manifest.apk" -M "$task_work/AndroidManifest.xml" -I "$task_android_jar")
if [[ -d "$task_script_dir/res" ]]; then
  task_aapt_args+=(-S "$task_script_dir/res")
fi
"$task_build_tools/aapt" "${task_aapt_args[@]}"

python3 - "$task_apk" "$task_work/manifest.apk" "$task_work/dex/classes.dex" "$task_work/with-java.apk" <<'PY'
import pathlib
import sys
import zipfile

original, manifest, dex, output = map(pathlib.Path, sys.argv[1:])

def signature(name):
    upper = name.upper()
    return upper.startswith("META-INF/") and (
        upper.endswith((".SF", ".RSA", ".DSA", ".EC")) or upper == "META-INF/MANIFEST.MF"
    )

# Rewriting omits the old APK signing block. Keep original native compression
# and permissions, replace compiled manifest/resources, then sign the new APK.
with zipfile.ZipFile(output, "w") as destination:
    included = set()
    with zipfile.ZipFile(manifest) as source:
        for info in source.infolist():
            destination.writestr(info, source.read(info.filename))
            included.add(info.filename)
    with zipfile.ZipFile(original) as source:
        for info in source.infolist():
            if info.filename in included or info.filename == "classes.dex" or signature(info.filename):
                continue
            destination.writestr(info, source.read(info.filename))
    destination.write(dex, "classes.dex", compress_type=zipfile.ZIP_DEFLATED)
PY

"$task_build_tools/zipalign" -f -P 16 4 "$task_work/with-java.apk" "$task_work/aligned.apk"
"$task_build_tools/apksigner" sign \
  --ks "$CARGO_APK_RELEASE_KEYSTORE" \
  --ks-pass env:CARGO_APK_RELEASE_KEYSTORE_PASSWORD \
  --out "$task_work/signed.apk" "$task_work/aligned.apk"
"$task_build_tools/apksigner" verify "$task_work/signed.apk"
"$task_build_tools/aapt" dump badging "$task_work/signed.apk" | \
  grep -F "launchable-activity: name='com.somuchsystem.friendshub.FriendsHubActivity'" >/dev/null
cp -- "$task_work/signed.apk" "$task_apk"
