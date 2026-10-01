# Appliance security

The default build remains suitable for development and existing deployments.
It does not require a device license or control credentials. Do not distribute
that build as a protected appliance.

## Offline device licenses

The `appliance` feature requires an Ed25519 public key and an absolute hardware
identity path at build time. The verifier and both file paths are embedded in
the executable. Runtime configuration cannot disable the license check, change
the trusted public key, or substitute another identity path.

The firmware integrator must select a stable, device-specific identity provided
by the trusted OS/hardware. A copied `/etc/machine-id`, MAC address, environment
variable, or editable serial-number file is not a secure binding. The core does
not select a board or require a particular hardware vendor.

Generate the signing key on an offline administration/build machine:

```sh
cargo run --locked --example license_issuer -- generate-key /secure/proaudio.pk8 /secure/proaudio-public.hex
```

Keep the private key outside repositories, device images, CI artifacts, and
customer machines. The issuer is a Cargo example, not part of the installed
player executable. It refuses to overwrite existing key/license files.

Build with the public key and the integrator's hardware identity path:

```sh
export PROAUDIO_LICENSE_PUBLIC_KEY_HEX="$(cat /secure/proaudio-public.hex)"
export PROAUDIO_DEVICE_ID_PATH=/sys/path/to/trusted-hardware-identity
cargo build --release --locked --features appliance
```

The path above is a placeholder: use an actual identity source for the chosen
hardware. Missing or malformed build inputs fail the build. The default license
location is `/etc/proaudio-player-alert/device-license.json`; change it at build
time with `PROAUDIO_DEVICE_LICENSE_PATH` if necessary.

On the provisioned device, export the request without launching audio services:

```sh
proaudio-player-native license-request > device-request.json
```

On the offline issuer machine:

```sh
cargo run --locked --example license_issuer -- issue /secure/proaudio.pk8 device-request.json device-license.json DEVICE-LICENSE-ID
```

Install the resulting signed license at the compiled location. The license
contains the product, format version, device fingerprint and license ID. It
contains no signing secret. Tampered licenses and licenses for a different
device, product, or format are rejected before the control plane starts.

Licenses are perpetual and verified locally at startup. Network availability,
wall-clock changes, or expiration cannot interrupt a running air-raid alert.
Provision valid licenses before putting appliances into service.

## Control-plane credentials

Set `api.auth_token_file` to a private file containing a random token of at least
32 ASCII letters/digits, with optional `-` or `_`. For example, provision a
64-character hexadecimal token with `openssl rand -hex 32`. Give its file mode
0600 or 0400 and make it readable by the player service user. The player loads
the token at startup and keeps only its SHA-256 digest. Restart the service to
rotate credentials.

An `appliance` build refuses to start an enabled API without this token. Disabling
the API is allowed for an appliance that needs no management endpoint.

```yaml
api:
  enabled: true
  host: "127.0.0.1"
  port: 8080
  auth_token_file: "/var/lib/proaudio-player-alert/control-token"
  allow_unauthenticated_upnp: false
```

Serve remote management through HTTPS or a trusted VPN. HTTP Basic and Bearer
authentication do not encrypt traffic. TLS termination and certificate/device
provisioning belong to the firmware/runtime integration.

Native clients send `Authorization: Bearer <token>`. Browsers use their standard
HTTP authentication dialog, with username `proaudio` and the token as password;
the same-origin WebUI and EventSource streams inherit these credentials. Tokens
in query parameters or cookies are not accepted. Client applications must support
credentials before enabling this mode on their target device.

Both `/api` and `/api/v1`, status/events/meters, library/settings, diagnostics,
network PCM ingestion, `/` and `/system-info.json` are protected. Health and
capabilities remain public for discovery. Capabilities advertise whether
credentials are required. Requests from another browser origin are rejected,
including when the browser would automatically send Basic credentials.

Public UPnP routing and SSDP discovery are disabled when API authentication is
enabled. `allow_unauthenticated_upnp: true` explicitly restores their unauthenticated
transport/volume controls for a trusted network. This does not authenticate DLNA.
Other receiver services, including external DLNA renderers, Spotify Connect,
AirPlay and MPD, need separate network/service policy in the deployment.

## Firmware and distribution boundary

Signature verification deters copying an unchanged executable and license to a
device with a different trusted identity. It cannot defeat a privileged operator
who can replace the executable, patch the verifier, spoof the identity source,
read decrypted memory, or boot a modified kernel. A software-only check does not
make a root-controlled host confidential.

For a closed appliance, distribute stripped binaries and required runtime
assets only. Keep all first-party source repositories and CI artifacts private.
Previously published source copies cannot be recalled. Browser-delivered code
and observable protocol behavior remain inspectable.

The firmware should provide verified boot, a verified read-only system image,
signed updates with rollback protection, a protected recovery path, and no
customer shell/root/debug access. Keep writable settings and media separate from
the verified system. Use hardware-backed keys when the target platform supports
them. The signing private key must remain outside the appliance.

Verified boot establishes code integrity; encryption protects storage at rest.
Neither, by itself, prevents a root user in a running trusted OS from inspecting
application memory. Hardware-specific provisioning, irreversible boot-policy
changes and final-image validation are separate integration work.

The service unit limits core dumps, uses private file creation permissions, and
protects system/kernel configuration while retaining the audio/runtime access
required by the existing player. Do not give customer administrators the player
service account.
