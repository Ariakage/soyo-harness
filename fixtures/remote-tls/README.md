# Remote transport test certificate

Test-only TLS material for the `F-13` D-113 path (b) HTTP(S) transport
regression tests. Never use these keys outside the test suite.

- `ca.pem`: self-signed test CA (`CN=soyo-remote-test-ca`, `CA:TRUE`).
- `server-cert.pem`: leaf certificate signed by `ca.pem`, `CN=localhost`,
  SAN `DNS:localhost, IP:127.0.0.1, IP:::1`, 100-year validity.
- `server-key.pem`: matching RSA-2048 leaf key.
- `chain.pem`: `server-cert.pem` followed by `ca.pem` — what the helper serves.

The client side trusts `ca.pem` through an explicit root (`SOYO_REMOTE_CA` or
`[remote.client] ca_path`), which is also how self-signed production
deployments are expected to work.
