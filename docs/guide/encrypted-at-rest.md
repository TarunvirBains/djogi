# Encrypted at Rest

> [Back to index](./index.md) · [Back to README](../../ReadMe.MD)

Djogi ships a built-in **encrypted-at-rest field codec** — `aes256_gcm_v1` —
that transparently encrypts a model's `String` field on write and decrypts it on
read. The ciphertext is stored in a Postgres `BYTEA` column; your application
code sees and sets the plaintext `String` as if the column were ordinary text.

The codec lives behind the `aes-codec` Cargo feature (off by default):

```toml
[dependencies]
djogi = { version = "...", features = ["aes-codec"] }
```

What it provides:

- **AEAD encryption** — AES-256-GCM gives confidentiality *and* integrity
  (tamper detection) in a single pass. Each value is bound to its model and
  field via AES-GCM additional-authenticated-data (AAD), so ciphertext moved to
  a different column or row fails authentication rather than decrypting with the
  wrong context.
- **A 32-entry key ring** read from `DJOGI_FIELD_CODEC_KEY_0` …
  `DJOGI_FIELD_CODEC_KEY_31`, with in-band key rotation.
- **Per-(model, field) subkeys** derived with HKDF-SHA256, so compromise of one
  field's ciphertext does not expose other fields under the same ring entry.
- **Startup validation** — if a model in your binary uses the codec but no valid
  key is configured, `DjogiPool` construction fails with a clear error naming the
  missing variable, before any query runs.

Explicitly **not** in scope (see [Non-goals](#non-goals)): searchable /
deterministic encryption, `Vec<u8>` → `Vec<u8>` encryption, and KMS integration.

## Declaring an encrypted field

Annotate any `String` (or `Option<String>`) field with
`#[field(protected(codec = "aes256_gcm_v1"))]`:

```rust
use djogi::prelude::*;

#[model(table = "accounts")]
#[derive(Debug, Clone)]
pub struct Account {
    pub email: String,

    #[field(protected(
        sensitivity = "secret",
        rationale = "API token — encrypted at rest",
        codec = "aes256_gcm_v1"
    ))]
    pub api_token: String,

    #[field(protected(
        sensitivity = "secret",
        rationale = "recovery code — nullable, encrypted at rest",
        codec = "aes256_gcm_v1"
    ))]
    pub recovery_code: Option<String>,
}
```

The generated migration creates `api_token` and `recovery_code` as **`BYTEA`**
columns (not `VARCHAR`/`TEXT`) — the codec's stored shape is `Vec<u8>`, so the
column type override is automatic. CRUD then round-trips transparently:

```rust
let saved = Account::create(&mut ctx, Account {
    email: "user@example.test".into(),
    api_token: "tok_live_abc123".into(),
    recovery_code: Some("R-7788".into()),
}).await?;

// Read back — the plaintext is decrypted for you.
let loaded = Account::get(&mut ctx, saved.id).await?;
assert_eq!(loaded.api_token, "tok_live_abc123");
```

A `None` value on a nullable encrypted field stores SQL `NULL` (encryption is
skipped) and decodes back to `None`.

## Key management

### Environment variables (the key ring)

Keys are supplied as environment variables, never in `Djogi.toml`:

```
DJOGI_FIELD_CODEC_KEY_0  = "<64 lowercase hex characters>"   # required (base key)
DJOGI_FIELD_CODEC_KEY_1  = "<64 lowercase hex characters>"   # optional (index 1)
...
DJOGI_FIELD_CODEC_KEY_31 = "<64 lowercase hex characters>"   # optional (max ring size)
```

Ring rules:

- **`DJOGI_FIELD_CODEC_KEY_0` is always required.**
- **No gaps.** If the highest index present is `N`, every index `0..=N` must be
  set. This guarantees every ciphertext in the database can be decrypted — no
  blob can reference a ring slot that is missing.
- **The active index is the highest index present.** New encryptions use the
  active key; decryption uses whatever index each ciphertext recorded in its
  `key_index` byte.

Each entry is exactly **64 lowercase hexadecimal characters** (32 bytes /
256 bits) — the same format as `DJOGI_PRESENTATION_HMAC_KEY`. Uppercase `A`–`F`
is rejected at startup, so the accepted format is unambiguous. The
`DJOGI_FIELD_CODEC_KEY_*` family is independent of `DJOGI_PRESENTATION_HMAC_KEY`
(different keys, different purposes — field encryption vs. presentation-codec
signing).

### Generating a key

```bash
openssl rand -hex 32
```

`openssl rand -hex` emits lowercase, so this is transparent to the lowercase-only
validation.

### Startup validation

When `aes-codec` is enabled and any model in the binary references the codec,
`DjogiPool` construction validates the ring before opening connections. On
failure it returns `DjogiError::FieldCodecStartup`, aggregating every codec key
problem so you can fix them all at once. Each error names the exact variable —
for example a gap at index 1 names `DJOGI_FIELD_CODEC_KEY_1`, and a malformed
entry names the specific index. A binary with no encrypted fields starts without
any key, because validation is driven by which models are actually linked in.

After a successful startup the validated ring is cached for the process lifetime
and is immutable — later changes to the environment variables are ignored. This
is deliberate: it closes a side channel (a caller cannot tell "key absent" from
"decryption failed" at query time) and avoids races during a rolling deploy.

## Key rotation

Rotation is **in-band, append-only**:

1. Add a new key at the next free index (`DJOGI_FIELD_CODEC_KEY_{N+1}`). It
   becomes the active key.
2. New writes encrypt under index `N+1`. Existing rows remain decryptable under
   whichever index their `key_index` byte recorded.
3. Optionally re-encrypt old rows forward to the active index.

**Superseded entries must stay in place** until every row that referenced them
has been re-encrypted — the no-gap rule means you cannot simply delete the low
indices and keep a high one. Shrinking the ring is a manual, carefully-sequenced
renumbering step; the framework does not require it (a held-but-unused key costs
nothing).

### Rolling-deploy ordering

The active index is computed per process from the ring present at startup, and
that snapshot is immutable for the process. So during a fleet rollout:

- **Deploy the new key to every process and restart all of them *before* any
  new-key write is committed.** An instance still running the shorter ring
  rejects a blob whose `key_index` is the newer active index with
  `UnknownKeyIndex` — it physically cannot decrypt rows written under a key it
  has never seen.
- Treat the window between "first instance restarted with the new key" and "last
  instance restarted" as unsafe for any new-key write. Old rows stay readable
  throughout under their recorded index; re-encrypting them forward can proceed
  after the whole fleet is uniform.

### Migrating an existing plaintext column to encrypted

Adding a codec to a column that already holds plaintext is an **offline**
operation. The differ classifies a plaintext→encrypted transition as
`OfflineOnly` and refuses to auto-generate an online migration — a naive
in-place `::BYTEA` cast would write the raw UTF-8 bytes into the column
unencrypted. Re-encode every row out of band (read, encrypt under the active
key, write back) during a maintenance window, or stage the change with a parallel
encrypted column. **Online codec rotation (an automatic in-place re-encode
backfill) is not yet implemented (issue #371).**

## Ciphertext layout

Each stored value is:

```
+---------+-----------+--------+------------------+
| version | key_index | nonce  | ciphertext + tag |
| 1 B     | 1 B       | 12 B   | variable length  |
+---------+-----------+--------+------------------+
```

- **version** (`0x01` for `aes256_gcm_v1`): future layouts increment this byte;
  an unrecognized value is rejected.
- **key_index** (`0`–`31`): the ring slot this value was encrypted under.
- **nonce** (96 bits): drawn fresh from the OS CSPRNG for every encryption,
  stored in-band so decode needs no separate column.
- **ciphertext + tag**: AES-GCM output (the 128-bit authentication tag is
  appended).

**Storage overhead is `plaintext.len() + 30` bytes** (1 + 1 + 12 + 16). The
minimum valid ciphertext is 30 bytes (empty plaintext).

## Nonce security

Each `encode` draws a fresh 12-byte nonce directly from the OS CSPRNG via
`getrandom`. **Nonce reuse under GCM is catastrophic** — it can leak the
authentication key, not merely the plaintext — so the codec generates a new nonce
per encryption and does **not** offer a deterministic mode. If you need
deterministic encryption for indexed equality lookups, that is a different codec
design with its own trade-offs and is tracked separately, not part of this codec.

### Write-volume rotation bound

Per NIST SP 800-38D §8.3, the number of random-nonce encryptions under a single
key should stay below roughly 2³² (~4.3 billion) to keep the probability of a
nonce collision negligible. The count grows with **writes**, not rows — every
`save()` re-encrypts with a fresh nonce. For high-write workloads, rotate the key
(append a new ring entry) before approaching that bound; a conservative policy
rotates at ~10% of it. The codec does not enforce a hard counter — treat this as
an operational constraint to monitor.

## At-rest threat model

The codec provides confidentiality at rest: ciphertext in `BYTEA` columns cannot
be read without the key ring. Two properties define the boundary:

1. **AAD binding.** Each value's AAD is `model\x00field`, binding ciphertext to
   its column and model. Ciphertext relocated to a different field or model fails
   authentication rather than silently decrypting with the wrong context — this
   defeats row/column-swapping attacks within the same database.
2. **HKDF-derived field keys.** Every `(model, field)` pair gets a distinct
   subkey derived from the active ring entry, so compromise of one field's
   derived key does not expose other fields. Compromise of a *ring entry* exposes
   all subkeys derived from it; rotate by appending a new ring entry. Per-tenant
   key scoping (a separate ring entry per tenant) is out of scope.

Error messages from the codec carry only structural information (lengths, ring
indices, the codec ID) — never plaintext, key material, nonce, or ciphertext
bytes.

## Errors

Codec failures surface through `DjogiError`:

- `FieldCodecStartup` — startup validation failed (missing / malformed key).
  Terminal; the operator must fix the environment variable.
- `FieldCodecEncode` — encryption failed on a write. Terminal; the transaction
  rolls back.
- `FieldCodecDecode` — decryption failed on a read (wrong key, tampered data,
  schema drift). Terminal.

All three are terminal (not retryable) — none resolves on a retry. The
underlying `CodecError` (carried as a rendered string) distinguishes the specific
cause: `RingEmpty`, `MissingKey` (gap / malformed), `UnknownVersion`,
`UnknownKeyIndex`, `CiphertextTooShort`, an AEAD authentication failure, an RNG
failure, or a UTF-8 decode failure.

## Non-goals

- **Searchable / deterministic encryption** — nonce-based GCM is non-deterministic
  by design; a blind-index / deterministic variant is a separate codec.
- **`Vec<u8>` → `Vec<u8>` encryption** — v1 targets `String` fields (PII,
  credentials, tokens). A byte-to-byte variant can be added later if requested.
- **KMS integration, passphrase derivation, hardware security modules** — keys
  are raw material the operator supplies; key-governance integrations are an
  application-layer concern.
