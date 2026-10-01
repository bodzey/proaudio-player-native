use std::fs::File;
use std::io::Read;
use std::path::Path;

use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub const PRODUCT: &str = "proaudio-player-native";
pub const VERSION: u32 = 1;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LicenseRequest {
    pub version: u32,
    pub product: String,
    pub device_id_sha256: String,
}

impl LicenseRequest {
    pub fn validate(&self) -> Result<()> {
        validate_binding(self.version, &self.product, &self.device_id_sha256)
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LicenseClaims {
    pub version: u32,
    pub product: String,
    pub device_id_sha256: String,
    pub license_id: String,
}

impl LicenseClaims {
    pub fn validate(&self) -> Result<()> {
        validate_binding(self.version, &self.product, &self.device_id_sha256)?;
        if self.license_id.is_empty()
            || self.license_id.len() > 128
            || self.license_id.chars().any(char::is_control)
        {
            bail!("некоректний ідентифікатор дозволу на запуск");
        }
        Ok(())
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SignedLicense {
    pub payload: String,
    pub signature: String,
}

fn validate_binding(version: u32, product: &str, fingerprint: &str) -> Result<()> {
    if version != VERSION || product != PRODUCT {
        bail!("непідтримуваний продукт або версія дозволу на запуск");
    }
    if fingerprint.len() != 64
        || !fingerprint
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        bail!("некоректний відбиток пристрою");
    }
    Ok(())
}

pub fn device_fingerprint(identity: &[u8]) -> Result<String> {
    let start = identity
        .iter()
        .position(|byte| !byte.is_ascii_whitespace())
        .unwrap_or(identity.len());
    let end = identity
        .iter()
        .rposition(|byte| !byte.is_ascii_whitespace())
        .map_or(start, |index| index + 1);
    let identity = &identity[start..end];
    if identity.len() < 8 || identity.iter().all(|byte| matches!(byte, 0 | b'0' | b'-')) {
        bail!("апаратний ідентифікатор пристрою відсутній або некоректний");
    }
    let mut digest = Sha256::new();
    digest.update(b"proaudio-device-identity-v1\0");
    digest.update(identity);
    Ok(hex::encode(digest.finalize()))
}

pub fn read_bounded(path: impl AsRef<Path>, limit: usize) -> Result<Vec<u8>> {
    let file = File::open(path)?;
    if !file.metadata()?.is_file() {
        bail!("очікується звичайний файл");
    }
    let mut bytes = Vec::new();
    file.take(limit as u64 + 1).read_to_end(&mut bytes)?;
    if bytes.len() > limit {
        bail!("файл перевищує допустимий розмір {limit} байтів");
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hardware_identity_is_required_and_stable_across_line_endings() {
        assert!(device_fingerprint(b"\n\t").is_err());
        assert!(device_fingerprint(b"00000000-0000-0000-0000-000000000000").is_err());
        assert_eq!(
            device_fingerprint(b"device-12345678\n").unwrap(),
            device_fingerprint(b"device-12345678\r\n").unwrap()
        );
        assert_ne!(
            device_fingerprint(b"device-12345678").unwrap(),
            device_fingerprint(b"device-87654321").unwrap()
        );
        LicenseRequest {
            version: VERSION,
            product: PRODUCT.into(),
            device_id_sha256: device_fingerprint(b"device-12345678").unwrap(),
        }
        .validate()
        .unwrap();
    }

    #[test]
    fn rejects_oversized_files_before_parsing() {
        let path =
            std::env::temp_dir().join(format!("proaudio-license-limit-{}", std::process::id()));
        std::fs::write(&path, b"123456789").unwrap();
        assert!(read_bounded(&path, 8).is_err());
        assert_eq!(read_bounded(&path, 9).unwrap(), b"123456789");
        std::fs::remove_file(path).unwrap();
    }
}
