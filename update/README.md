# Update trust manifest

`UPDATE-TRUST.json` is the root-signed trust manifest for the in-app updater (see
DEVELOPMENT.md §13.3). The app fetches it, verifies it against the **root public key embedded
at build time**, and reads out the currently valid signing public key and a monotonic `seq`.

That is the whole manifest: `schema`, `seq`, `signing_key`, `issued_at`, `signature`. There is
deliberately **no revoked-key list and no minimum-version field** — a monotonic `seq` already
makes rotation retire the old key, and a version watermark buys little while adding one more
persisted value whose failure mode is "legitimate updates are refused" (see §13.3).

**The file is not in the repository yet.** It must be generated offline with the **root private
key**, which deliberately never touches CI or git — so it cannot be produced by a build.

## Generating it

Run on an offline/trusted machine, with the root private key at hand. **No Rust toolchain
needed** — use whichever of these is available (all three produce the same bytes):

```bash
# Node (>=12; no npm install, uses the built-in OpenSSL Ed25519)
node tools/updatesign/updatesign.mjs issue-trust \
  -r <root-priv-hex> -k <signing-pub-hex> -s 1 -o update/UPDATE-TRUST.json
node tools/updatesign/updatesign.mjs verify-trust \
  -i update/UPDATE-TRUST.json -p <root-pub-hex>

# Rust equivalent, if cargo happens to be available
cargo run --manifest-path tools/updatesign/Cargo.toml -- issue-trust \
  -r <root-priv-hex> -k <signing-pub-hex> -s 1 -o update/UPDATE-TRUST.json
cargo run --manifest-path tools/updatesign/Cargo.toml -- verify-trust \
  -i update/UPDATE-TRUST.json -p <root-pub-hex>
```

The two implementations are independent but share one contract, and CI verifies the committed
manifest with **both** — so a field-order or escaping drift on either side is caught at publish
time rather than after release, when every user's in-app update would silently fail to verify.

Then commit the generated `UPDATE-TRUST.json` (and only that file — never any `priv`; `*.pem` /
`*.key` / `*priv*.hex` are gitignored as a last-resort guard).

The release pipeline (`checksums` job) re-verifies this file against the `NETSENSE_ROOT_PUBKEY`
repo variable and **fails the release** if it is missing, malformed, or signed by a different
root key. That is intentional: a Release published without a valid manifest looks fine on the
download page but every user's in-app update would refuse to install.

## Key generation without Cargo

`node tools/updatesign/updatesign.mjs gen` prints a fresh `priv=…` / `pub=…` pair (run it twice:
once for root, once for signing). Plain OpenSSL works too:

```bash
openssl genpkey -algorithm ed25519 -out key.pem
openssl pkey -in key.pem -outform DER 2>/dev/null | tail -c 32 | od -An -tx1 | tr -d ' \n'   # priv hex
openssl pkey -in key.pem -pubout -outform DER 2>/dev/null | tail -c 32 | od -An -tx1 | tr -d ' \n'  # pub hex
```

Both give a 32-byte seed and a 32-byte compressed point in lowercase hex — exactly what the
secret, the repo variable and the manifest expect. Keep the private key **out of the repository**
and **out of this directory**.

## Why it is committed rather than uploaded as a Release asset

Signing it requires the root private key, and by design CI never has it. So the manifest is a
repository file that changes only when keys are rotated — not on every release. Bump `seq` only
for those events; reusing or lowering a `seq` is rejected by clients (the monotonic
anti-rollback check), which would leave users unable to update.

⚠️ Rotation only works if `seq` goes **up**. If you re-issue the manifest with the same `seq`
pointing at a new signing key, clients that already accepted that `seq` will reject it.
