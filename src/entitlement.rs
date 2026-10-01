use anyhow::{bail, Context, Result};
use ring::signature::{UnparsedPublicKey, ED25519};

pub mod format;

#[cfg(feature = "appliance")]
pub fn authorize() -> Result<()> {
    let public_key = hex::decode(env!("PROAUDIO_LICENSE_PUBLIC_KEY_HEX"))?;
    let identity = format::read_bounded(env!("PROAUDIO_DEVICE_ID_PATH"), 4_096)
        .context("не вдалося прочитати апаратний ідентифікатор пристрою")?;
    let fingerprint = format::device_fingerprint(&identity)?;
    let license = format::read_bounded(env!("PROAUDIO_DEVICE_LICENSE_PATH"), 16_384)
        .context("не вдалося прочитати дозвіл на запуск пристрою")?;
    verify(&license, &public_key, &fingerprint)?;
    Ok(())
}

#[cfg(feature = "appliance")]
pub fn request() -> Result<format::LicenseRequest> {
    let identity = format::read_bounded(env!("PROAUDIO_DEVICE_ID_PATH"), 4_096)?;
    let request = format::LicenseRequest {
        version: format::VERSION,
        product: format::PRODUCT.into(),
        device_id_sha256: format::device_fingerprint(&identity)?,
    };
    request.validate()?;
    Ok(request)
}

fn verify(
    bytes: &[u8],
    public_key: &[u8],
    device_fingerprint: &str,
) -> Result<format::LicenseClaims> {
    let envelope: format::SignedLicense =
        serde_json::from_slice(bytes).context("некоректний формат дозволу на запуск")?;
    let signature = hex::decode(&envelope.signature).context("некоректний підпис дозволу")?;
    UnparsedPublicKey::new(&ED25519, public_key)
        .verify(envelope.payload.as_bytes(), &signature)
        .map_err(|_| anyhow::anyhow!("підпис дозволу на запуск не підтверджено"))?;
    let claims: format::LicenseClaims = serde_json::from_str(&envelope.payload)?;
    claims.validate()?;
    if claims.device_id_sha256 != device_fingerprint {
        bail!("дозвіл на запуск видано для іншого пристрою");
    }
    Ok(claims)
}

#[cfg(test)]
mod tests {
    use ring::rand::SystemRandom;
    use ring::signature::{Ed25519KeyPair, KeyPair};

    use super::*;

    fn signed_license() -> (Ed25519KeyPair, format::SignedLicense, String) {
        let pkcs8 = Ed25519KeyPair::generate_pkcs8(&SystemRandom::new()).unwrap();
        let key = Ed25519KeyPair::from_pkcs8(pkcs8.as_ref()).unwrap();
        let fingerprint = format::device_fingerprint(b"device-12345678").unwrap();
        let payload = serde_json::to_string(&format::LicenseClaims {
            version: format::VERSION,
            product: format::PRODUCT.into(),
            device_id_sha256: fingerprint.clone(),
            license_id: "test-license".into(),
        })
        .unwrap();
        let envelope = format::SignedLicense {
            signature: hex::encode(key.sign(payload.as_bytes()).as_ref()),
            payload,
        };
        (key, envelope, fingerprint)
    }

    #[test]
    fn accepts_only_licenses_signed_for_the_matching_device() {
        let (key, envelope, fingerprint) = signed_license();
        let bytes = serde_json::to_vec(&envelope).unwrap();
        assert!(verify(&bytes, key.public_key().as_ref(), &fingerprint).is_ok());
        let other = format::device_fingerprint(b"device-87654321").unwrap();
        assert!(verify(&bytes, key.public_key().as_ref(), &other).is_err());
    }

    #[test]
    fn rejects_tampered_payload_and_replacement_signing_keys() {
        let (key, mut envelope, fingerprint) = signed_license();
        let (replacement_key, _, _) = signed_license();
        assert!(verify(
            &serde_json::to_vec(&envelope).unwrap(),
            replacement_key.public_key().as_ref(),
            &fingerprint
        )
        .is_err());
        envelope.payload = envelope.payload.replace("test-license", "changed-license");
        assert!(verify(
            &serde_json::to_vec(&envelope).unwrap(),
            key.public_key().as_ref(),
            &fingerprint
        )
        .is_err());
    }

    #[test]
    fn rejects_even_signed_licenses_for_another_product_or_version() {
        let (key, envelope, fingerprint) = signed_license();
        for payload in [
            envelope.payload.replace(format::PRODUCT, "other-product"),
            envelope.payload.replace("\"version\":1", "\"version\":2"),
        ] {
            let altered = format::SignedLicense {
                signature: hex::encode(key.sign(payload.as_bytes()).as_ref()),
                payload,
            };
            assert!(verify(
                &serde_json::to_vec(&altered).unwrap(),
                key.public_key().as_ref(),
                &fingerprint
            )
            .is_err());
        }
    }
}
