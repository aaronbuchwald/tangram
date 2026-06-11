# ADR-0004: Secret resolution interface (`scheme://` refs + a resolver seam)

**Status:** accepted (2026-06-11)
**Deciders:** Aaron (owner), with analysis by Claude
**Related:** [RUNTIME_PLAN.md](../RUNTIME_PLAN.md) Phase 10; ADR-0005 (egress
credential injection — the second axis); ADR-0001 addendum (Keyhive as the
long-term per-device-key story)

## Context

Apps need secrets (e.g. nutrition's `CALORIENINJAS_API_KEY`). Today a spec
carries `env = { CALORIENINJAS_API_KEY = "${CALORIENINJAS_API_KEY}" }` and the
host expands `${VAR}` from its own process environment at converge time,
injecting the **value** into the component. That is already a secret resolver
— with exactly one hardcoded strategy (process env) and no value-type
hygiene. Federation (Phase 9) made the limitation visible: every device must
independently provision every secret, because only the `${VAR}` *reference*
replicates, never the value.

We want a seam we can establish now — barely more than today's behavior — and
iterate on later for provenance (where the value comes from), without ever
changing app specs or app code. We explicitly do **not** want to invent a
secret-reference format or hand-roll cryptography.

This ADR covers ONE axis only: **provenance** — how a reference resolves to a
value, host-side. The orthogonal axis — whether the component ever sees the
plaintext at all — is ADR-0005.

## Decision

Adopt two off-the-shelf conventions and a thin trait:

1. **`scheme://locator` secret references** — the de-facto cross-tool format
   (1Password `op://…`, SOPS, Vault, Doppler all emit "reference, not value").
   The **scheme selects the resolver**:
   - `env://NAME` — host process env (today's behavior; `${VAR}` becomes sugar
     that rewrites to `env://VAR` for back-compat)
   - `op://vault/item/field` — 1Password (copy-paste compatible with `op`)
   - `sops://file#key`, `age://name` — file / synced-blob resolvers (later)
2. **The [`secrecy`](https://docs.rs/secrecy) crate** for the value type
   (`SecretString`: zeroize-on-drop, redacted `Debug`) — prevents accidental
   logging and lingering plaintext.
3. **A `SecretResolver` trait + scheme registry**, resolved host-side at
   converge just before component instantiation:
   ```rust
   pub struct SecretRef(String);                 // "scheme://locator"
   #[async_trait] pub trait SecretResolver {
       fn scheme(&self) -> &'static str;
       async fn resolve(&self, r: &SecretRef) -> anyhow::Result<SecretString>;
   }
   ```

**Phase 10a ships exactly one resolver — `env://` — and nothing else.**
Behavior is byte-identical to today; the only change is that resolution flows
through the trait and values are `SecretString`. `op://`, `sops://`, `age://`
(the E2EE-synced-blob feature) are later, each a new resolver behind the
unchanged trait.

## Consequences

- App specs reference `scheme://…`; apps never learn how it resolves — every
  future provenance option is additive and spec/code-invisible.
- The `age://` resolver is where "secrets sync E2E-encrypted across replicas"
  lands: decrypt a blob from a synced secrets document with the host's device
  key. Symmetric AEAD via a vetted library (libsodium/age), key established by
  secure pairing on first device link; documented limitation = no per-device
  revocation / no forward secrecy (lose a device ⇒ rotate + re-encrypt), with
  migration to per-device keys (Keyhive) when multi-user matters.
- This seam future-proofs **provenance only**. "The component never sees
  plaintext" is a different contract (ADR-0005) and is NOT delivered here —
  `env://` still injects the value into the component, as today.
