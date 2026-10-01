use std::fs::OpenOptions;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Result};
use clap::{Parser, Subcommand};
use ring::rand::SystemRandom;
use ring::signature::{Ed25519KeyPair, KeyPair};

#[path = "../src/entitlement/format.rs"]
#[allow(dead_code)]
mod format;

#[derive(Parser)]
#[command(about = "Offline device license issuer; never install on a player")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    GenerateKey {
        private_key: PathBuf,
        public_key: PathBuf,
    },
    Issue {
        private_key: PathBuf,
        request: PathBuf,
        output: PathBuf,
        license_id: String,
    },
}

fn write_new(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(())
}

fn main() -> Result<()> {
    match Cli::parse().command {
        Command::GenerateKey {
            private_key,
            public_key,
        } => {
            let pkcs8 = Ed25519KeyPair::generate_pkcs8(&SystemRandom::new())
                .map_err(|_| anyhow!("не вдалося згенерувати ключ підписування"))?;
            let key = Ed25519KeyPair::from_pkcs8(pkcs8.as_ref())
                .map_err(|_| anyhow!("некоректний ключ підписування"))?;
            write_new(&private_key, pkcs8.as_ref())?;
            write_new(
                &public_key,
                format!("{}\n", hex::encode(key.public_key().as_ref())).as_bytes(),
            )?;
        }
        Command::Issue {
            private_key,
            request,
            output,
            license_id,
        } => {
            let pkcs8 = format::read_bounded(private_key, 4_096)?;
            let key = Ed25519KeyPair::from_pkcs8(&pkcs8)
                .map_err(|_| anyhow!("некоректний ключ підписування"))?;
            let request: format::LicenseRequest =
                serde_json::from_slice(&format::read_bounded(request, 4_096)?)?;
            request.validate()?;
            let claims = format::LicenseClaims {
                version: request.version,
                product: request.product,
                device_id_sha256: request.device_id_sha256,
                license_id,
            };
            claims.validate()?;
            let payload = serde_json::to_string(&claims)?;
            let envelope = format::SignedLicense {
                signature: hex::encode(key.sign(payload.as_bytes()).as_ref()),
                payload,
            };
            write_new(&output, &serde_json::to_vec_pretty(&envelope)?)?;
        }
    }
    Ok(())
}
