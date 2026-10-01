#!/usr/bin/env bash
set -euo pipefail

cd "$(dirname "$0")/.."
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

cargo build --locked --example license_issuer
issuer="${CARGO_TARGET_DIR:-target}/debug/examples/license_issuer"
player="${CARGO_TARGET_DIR:-target}/debug/proaudio-player-native"

if env -u PROAUDIO_LICENSE_PUBLIC_KEY_HEX -u PROAUDIO_DEVICE_ID_PATH cargo build --locked --features appliance >"$work/missing-key.log" 2>&1; then
    echo 'appliance build unexpectedly accepted missing trust inputs' >&2
    exit 1
fi
grep -Fq 'appliance builds require PROAUDIO_LICENSE_PUBLIC_KEY_HEX' "$work/missing-key.log"

"$issuer" generate-key "$work/issuer.pk8" "$work/issuer.hex"
printf '%s\n' 'device-provisioning-test-12345678' >"$work/identity"
export PROAUDIO_LICENSE_PUBLIC_KEY_HEX="$(cat "$work/issuer.hex")"
export PROAUDIO_DEVICE_ID_PATH="$work/identity"
export PROAUDIO_DEVICE_LICENSE_PATH="$work/device-license.json"
cargo build --locked --features appliance
"$player" license-request >"$work/request.json"
printf '%s\n' 'api:' '  enabled: false' >"$work/config.yaml"

reject_status() {
    if "$player" --config "$work/config.yaml" status >"$work/status.out" 2>"$work/status.err"; then
        echo "$1" >&2
        exit 1
    fi
}

reject_status 'appliance started without a license'
"$issuer" issue "$work/issuer.pk8" "$work/request.json" "$work/device-license.json" provisioning-test
cp "$work/device-license.json" "$work/valid-license.json"
"$player" --config "$work/config.yaml" status >"$work/status.out"

PROAUDIO_DEVICE_ID_PATH=/nonexistent PROAUDIO_LICENSE_PUBLIC_KEY_HEX=invalid "$player" --config "$work/config.yaml" status >"$work/status.out"

printf '%s\n' 'another-device-87654321' >"$work/identity"
reject_status 'appliance accepted a copied license on another identity'
printf '%s\n' 'device-provisioning-test-12345678' >"$work/identity"
sed 's/provisioning-test/tampered-license/g' "$work/valid-license.json" >"$work/device-license.json"
reject_status 'appliance accepted a tampered signed payload'

"$issuer" generate-key "$work/other.pk8" "$work/other.hex"
"$issuer" issue "$work/other.pk8" "$work/request.json" "$work/other-license.json" other-issuer
cp "$work/other-license.json" "$work/device-license.json"
reject_status 'appliance accepted a replacement signing authority'
cp "$work/valid-license.json" "$work/device-license.json"
"$player" --config "$work/config.yaml" status >"$work/status.out"

cargo test --locked --features appliance
cargo clippy --locked --features appliance --all-targets -- -D warnings
echo 'Appliance license, device binding and credential tests passed'
