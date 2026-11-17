#!/usr/bin/env bash
# Build the Android app (arm64), signed with the local debug key for
# sideloading: target/vlichen-<version>-arm64.apk. Needs JDK 17, the Android
# SDK (platform 35+, build-tools 35, an NDK) and Rust's
# aarch64-linux-android target; the defaults are where Homebrew and
# sdkmanager put them on a Mac. Install on a phone with USB debugging:
#   adb install -r target/vlichen-<version>-arm64.apk
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd)
export JAVA_HOME=${JAVA_HOME:-/opt/homebrew/opt/openjdk@17/libexec/openjdk.jdk/Contents/Home}
export ANDROID_HOME=${ANDROID_HOME:-$HOME/Library/Android/sdk}
export NDK_HOME=${NDK_HOME:-$(ls -d "$ANDROID_HOME"/ndk/* | sort -V | tail -1)}
bt=$(ls -d "$ANDROID_HOME"/build-tools/* | sort -V | tail -1)
ks=$HOME/.android/debug.keystore
[ -f "$ks" ] || "$JAVA_HOME/bin/keytool" -genkeypair -keystore "$ks" -storepass android -keypass android \
    -alias androiddebugkey -dname "CN=Android Debug,O=Android,C=US" -keyalg RSA -keysize 2048 -validity 10000
cd "$here"
cargo tauri android build --apk --target aarch64 --ci
version=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
unsigned=gen/android/app/build/outputs/apk/universal/release/app-universal-release-unsigned.apk
out=$here/../target/vlichen-$version-arm64.apk
tmp=$(mktemp -d)
"$bt/zipalign" -f -p 4 "$unsigned" "$tmp/aligned.apk"
"$bt/apksigner" sign --ks "$ks" --ks-pass pass:android --out "$out" "$tmp/aligned.apk"
rm -r "$tmp"
echo "$out"
