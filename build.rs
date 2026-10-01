use std::env;
use std::path::Path;

fn main() {
    for name in [
        "PROAUDIO_LICENSE_PUBLIC_KEY_HEX",
        "PROAUDIO_DEVICE_ID_PATH",
        "PROAUDIO_DEVICE_LICENSE_PATH",
    ] {
        println!("cargo:rerun-if-env-changed={name}");
    }
    if env::var_os("CARGO_FEATURE_APPLIANCE").is_none() {
        return;
    }

    let key = env::var("PROAUDIO_LICENSE_PUBLIC_KEY_HEX")
        .expect("appliance builds require PROAUDIO_LICENSE_PUBLIC_KEY_HEX");
    let key = key.trim();
    assert!(
        key.len() == 64
            && key.bytes().all(|byte| byte.is_ascii_hexdigit())
            && key.bytes().any(|byte| byte != b'0'),
        "PROAUDIO_LICENSE_PUBLIC_KEY_HEX must be a nonzero 32-byte Ed25519 public key in hex"
    );
    println!("cargo:rustc-env=PROAUDIO_LICENSE_PUBLIC_KEY_HEX={key}");

    let identity = env::var("PROAUDIO_DEVICE_ID_PATH").expect(
        "appliance builds require a trusted hardware identity path in PROAUDIO_DEVICE_ID_PATH",
    );
    embed_path("PROAUDIO_DEVICE_ID_PATH", &identity);
    let license = env::var("PROAUDIO_DEVICE_LICENSE_PATH")
        .unwrap_or_else(|_| "/etc/proaudio-player-alert/device-license.json".into());
    embed_path("PROAUDIO_DEVICE_LICENSE_PATH", &license);
}

fn embed_path(name: &str, value: &str) {
    assert!(
        Path::new(value).is_absolute()
            && Path::new(value).file_name().is_some()
            && !value.contains(['\n', '\r', '\0']),
        "{name} must be an absolute file path"
    );
    println!("cargo:rustc-env={name}={value}");
}
