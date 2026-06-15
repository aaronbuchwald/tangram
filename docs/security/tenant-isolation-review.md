# Tenant Isolation Security Review — In-Process WASM, Side Channels, and Secret Exposure

**Status:** research / due-diligence (read-only). No production code changed.
**Date:** 2026-06-11
**Scope:** the `tangram-host` embedded-Wasmtime runtime (RUNTIME_PLAN Phases 2,
5, 9; ADR-0001), its multi-tenant mode (Phase 5), and secret handling today
(`env://` injection per ADR-0004) vs. the pending host-egress injection
(ADR-0005). Grounded in this codebase, not generic WASM.

---

## Executive summary (the bottom line)

**Question: can a malicious tenant's WASM blob steal another tenant's API key
via timing, and does gVisor fix it?**

- **Software-fault isolation is solid.** Different tenants' components run as
  separate `wasmtime` instances with separate linear memories, an empty WASI
  context (no preopens, no sockets, no inbound HTTP), and Wasmtime's Spectre
  mitigations on its own bounds checks and indirect branches. A malicious
  component **cannot directly read another component's memory** — there is no
  architectural path, and the obvious speculative paths are mitigated. (See
  `crates/tangram-host/src/runtime.rs:179-198`, `wit/tangram.wit`.)

- **Microarchitectural side channels are NOT covered by running WASM in one
  process, and this is the real exposure.** All tenants share one OS process,
  one set of CPU cores, and therefore the L1/L2/LLC caches, the branch
  predictors, the TLB, and (if SMT is on) execution ports of the sibling
  hyperthread. Co-resident cache attacks across tenants are a demonstrated,
  *practical* class of attack in modern clouds (including a 2024–2025
  demonstration of cross-tenant leakage on Google Cloud Run). Neither Wasmtime
  nor WASM-the-bytecode is a microarchitectural barrier — both vendors say so.

- **But the *specific* secret at risk here is unusually hard to extract by
  timing, today and especially after ADR-0005.** A cache/timing attack
  recovers a key only when the *victim performs secret-dependent
  memory/branch accesses* (table-driven crypto, secret-indexed lookups). In
  Tangram, the victim component merely **stores** its API key and hands it to
  the host's `http-fetch` as a header string. It does essentially no
  secret-dependent computation. There is almost nothing for Prime+Probe /
  Flush+Reload to observe. The classic "extract an AES key from a co-resident
  VM" result does not transfer to "extract a passive bearer token that is
  copied once into an HTTP header."

- **gVisor does NOT fix the microarchitectural channel.** gVisor interposes
  *syscalls* to protect the host kernel from a guest; its own docs state
  plainly it "does not provide protection against hardware side channels."
  Putting each tenant in a gVisor sandbox would harden the host kernel and
  contain memory-disclosure bugs, but two gVisor sandboxes on the same core
  still share the same L1/L2/LLC and SMT siblings. gVisor is a syscall
  barrier, not a cache barrier.

- **The highest-leverage mitigation is ADR-0005, not stronger process/VM
  isolation.** If the plaintext credential never enters the tenant's address
  space — the host injects it at the egress boundary inside `http-fetch` —
  then there is no victim secret co-resident with the attacker to leak in the
  first place. Removing the secret from the tenant's memory shrinks the
  side-channel surface for *that secret* to essentially zero, **independent of
  whether tenants share a process, a VM, or a core.** That is the single most
  important change for the credential-theft threat.

**One-line answer:** In-process WASM gives you memory isolation but not
timing isolation; a malicious blob *can* in principle observe co-resident
microarchitectural state, and gVisor would not change that. However, the only
high-value secret in the current design (a stored API key) is a poor timing
target, and ADR-0005 removes it from the tenant entirely — making the
practical risk low for first-party/semi-trusted tenants and making
"host-injected credentials + no SMT co-scheduling" the right answer before
in-process WASM hosts genuinely untrusted third-party code.

---

## Q1 — Threat model: what in-process untrusted WASM actually isolates

It is essential to separate four distinct isolation layers, because the design
provides some and not others.

**(a) WASM software-fault isolation (memory safety between instances) —
PROVIDED.**
Each app is a distinct `wasmtime` component instance with its own `Store` and
its own linear memory (`runtime.rs:197-198`). WebAssembly cannot name an
address outside its own linear memory; there is no pointer into another
instance. The host links the wasip2 std plumbing with an **empty WASI
context** — `wasi.inherit_stderr()` and env vars only; no preopened dirs, no
`wasi:sockets`, no `wasi:http`, no inbound HTTP (`runtime.rs:179-188`,
`wit/tangram.wit:16-32`). The component's entire view of the outside world is
three host functions: `http-fetch` (allowlist-enforced), `log`, `now-ms`. So a
malicious component cannot open files, open sockets, or address another
tenant's heap. Wasmtime additionally mitigates the speculative escapes of its
own sandbox: `call_indirect` and `br_table` bounds are Spectre-hardened, and
linear-memory accesses are protected by 2 GB guard regions (or explicit
bounds-check mitigations when guard pages are off). This layer is genuinely
strong and is the design's main selling point over a syscall sandbox.

**(b) Microarchitectural side channels (cache, TLB, branch predictor, port
contention, Spectre v1/v2/MDS-class) — NOT PROVIDED.**
SFI is about *architectural* state (what the ISA lets you address).
Side channels are about *microarchitectural* state (what the hardware does
underneath, shared by everything on the core/package). One shared process on
one set of cores means all tenants share:
- L1/L2 (per-core) and the **LLC (shared across the whole package)**;
- the branch predictor and BTB;
- the TLB;
- with SMT enabled, the execution ports and store buffers of the sibling
  hyperthread (the MDS / port-contention surface).

Wasmtime's own security page mitigates Spectre *for the WASM sandbox's
control-flow* but makes **no claim** to stop timing/cache leakage between
co-resident instances, and explicitly calls Spectre mitigation "ongoing." WASM
even *helps* an attacker in one narrow sense: it gives untrusted code a
predictable, JIT-compiled instruction stream and a `now-ms` host call (coarse,
but the guest can also build timing primitives from loop counters). This is
the layer the current design does not address.

**(c) Host-syscall sandboxing (seccomp/gVisor) — PARTIALLY RELEVANT, NOT USED
ON THE WASM SPINE.**
The WASM design arguably *exceeds* a syscall sandbox for the host-protection
goal: the component literally cannot issue host syscalls (no WASI I/O linked),
so there is no syscall attack surface against the host kernel from inside a
component. gVisor (Track G, retained) matters only for *native* unported apps.
Neither seccomp nor gVisor addresses layer (b) — see Q3.

**(d) Hardware / VM isolation — NOT PROVIDED in-process.**
Separate VMs (or separate bare-metal machines, or dedicated cores with cache
partitioning) are the only layers that begin to address (b), and even VMs do
not fully stop LLC/SMT channels without additional hardware controls.

**Summary table:**

| Isolation layer | In-process WASM (today) | Provided? |
|---|---|---|
| (a) Memory safety between instances | separate linear memories + Spectre-hardened bounds | **Yes** |
| (b) Microarch side channels (cache/TLB/BP/SMT) | shared core, shared LLC | **No** |
| (c) Host-syscall containment | no WASI I/O linked at all | **Yes (by removal)** |
| (d) Hardware/VM isolation | one process, one core set | **No** |

---

## Q2 — Feasibility against *this* secret specifically

The general result "co-resident code can extract a crypto key via cache
timing" is real and, in 2024–2025, demonstrated cross-tenant on commercial
clouds (Flush+Reload/Prime+Probe variants on the shared LLC, with cross-tenant
leakage shown on Google Cloud Run). So the *mechanism* exists here: tenant A's
component and tenant B's component on the same host share the LLC and possibly
an SMT sibling.

**The decisive question is what the victim does with the secret.** Cache
attacks observe **memory-access patterns**, not values. They recover a key
*because the victim's code branches or indexes on key bits* — table-driven
AES, square-and-multiply RSA, secret-dependent control flow. The leakage is of
*addresses touched*, which the attacker correlates to key bits.

In Tangram today, the victim component's interaction with its secret is:
1. read `CALORIENINJAS_API_KEY` from its WASI env at instantiation
   (`runtime.rs:187`, value injected by the host from `${VAR}` expansion);
2. store it in the component's heap as a `String`;
3. on an action, copy it into an HTTP header object and pass the request JSON
   to `http-fetch` (`runtime.rs:57-108` is the host side; the guest just
   builds a JSON string).

There is **no secret-dependent table lookup, no secret-dependent branch, no
cryptographic computation on the key** inside the victim. The key is moved
around as opaque bytes. A Prime+Probe attacker watching cache-set occupancy
sees the *same* access pattern regardless of the key's value — the bytes are
copied, not branched on. There is essentially no signal that varies with the
secret. Recovering the actual key bytes would require a *value*-revealing
channel (e.g., the victim doing a secret-indexed memory access per byte),
which this code path does not contain.

**Quantifying realism for the current design:**
- *Co-location* — both tenants must land on the same physical host (the same
  process here, so guaranteed if they're both running) and ideally the same
  core / SMT pair to get L1/L2/port channels; LLC is package-wide so same-core
  is not required for LLC attacks.
- *Noise & samples* — even against a *good* target (crypto with
  secret-dependent accesses), cross-core LLC attacks need many thousands of
  probe samples and must repeatedly preempt/observe the victim during the
  secret operation; reported recoveries range from seconds to minutes under
  favorable conditions and are noisy. Against a *bad* target (a stored token
  copied once into a header), there is no repeated secret-dependent operation
  to sample at all — the attacker cannot manufacture signal that isn't there.
- *Co-scheduling* — the sharpest channels (port contention/MDS, L1) require
  the attacker thread to run on the **SMT sibling** of the victim during the
  secret operation; with SMT off, those collapse to the slower, noisier
  cross-core LLC channel.

**What Wasmtime mitigates vs. what co-residency inherently exposes:**
- *Mitigated by Wasmtime:* speculative escape from the sandbox bounds checks
  (Spectre v1 on `call_indirect`/`br_table`/memory), guard-page-based memory
  isolation, no shared linear memory between instances. So the attacker cannot
  use a Spectre gadget *inside its own module* to read B's memory directly.
- *Inherently exposed by co-residency (Wasmtime cannot fix):* shared L1/L2/LLC
  occupancy, shared branch predictor/BTB state, shared TLB, and SMT-sibling
  port/store-buffer contention. These are properties of the silicon, not the
  runtime.

**Bottom line for Q2:** Against the current "stored API key" the practical
timing-extraction risk is **low** — not because co-residency is safe in
general, but because this particular victim performs almost no
secret-dependent computation, which is the prerequisite for cache key
recovery. The risk would be materially higher for any app that does crypto on
a long-lived secret in component memory (e.g., signing JWTs, HMACs, deriving
keys) — that is the workload class to watch.

---

## Q3 — Would gVisor help?

**Precisely: gVisor interposes the *System ABI* (syscalls + page faults) to
protect the host kernel from a malicious guest.** It does not change what the
CPU does microarchitecturally while guest instructions execute.

gVisor's own security model states it directly:

> "In general, gVisor does not provide protection against hardware side
> channels, although it may make exploits that rely on direct access to the
> host System API more difficult to use."

and

> "gVisor relies on the host operating system and the platform for defense
> against hardware-based attacks."

So gVisor **does not stop cross-tenant microarchitectural timing leaks.** Two
gVisor sandboxes scheduled on the same core (or SMT pair, or sharing the LLC)
still contend for the same caches and predictors. gVisor's value is orthogonal
to this threat: it contains *syscall-level* kernel exploits and limits the
host API surface — which, for the WASM spine, is already near-zero because
components have no WASI I/O linked at all (`runtime.rs:179-188`). gVisor would
be the right tool for the *retained Track-G native-app path* (untrusted native
binaries that do issue syscalls), not as a side-channel barrier.

**Spectrum of isolation strength against side channels (weak → strong):**

| Mechanism | Memory-disclosure bugs | LLC cache channel | SMT/port + L1 channel | Notes |
|---|---|---|---|---|
| In-process WASM (today) | strong (SFI) | none | none | one process, shared everything microarch |
| Process-per-tenant | strong (separate addr spaces) | none | none | helps against runtime memory bugs, not microarch |
| gVisor per tenant | strong + kernel containment | none | none | syscall barrier; vendor says NOT a side-channel barrier |
| Separate VMs | strong | weak/none | weak/none | VM boundary doesn't partition LLC or SMT siblings |
| VM + **disable SMT** | strong | weak | **removed** | kills the sibling-thread channels |
| VM + **CAT (LLC partitioning)** + no SMT | strong | **strongly reduced** | removed | partitions the shared LLC by ways |
| **Dedicated cores / core scheduling** (no cross-tenant co-residency on a core) + no SMT | strong | reduced | removed | attacker never shares a core with victim |
| **Separate physical hosts** | strong | **removed** | removed | no shared silicon — the only complete answer |

The honest hierarchy: separate processes (and gVisor) help for
*memory-disclosure* bugs and host-kernel containment but **do essentially
nothing extra for SMT/LLC side channels**; only hardware-level controls
(disable SMT, partition the LLC with Intel CAT, pin tenants to non-shared
cores via core scheduling) or **physical separation** address layer (b). The
strongest *practical* answer for genuinely untrusted multi-tenancy is:
dedicated cores per trust domain with SMT disabled and LLC partitioning, or
simply separate hosts per tenant — exactly the posture serious clouds adopt
for their most sensitive isolation tiers.

---

## Q4 — The ADR-0005 angle (host-injected credentials)

ADR-0004 is explicit that today the `env://` resolver still injects the
plaintext **value** into the component, and that the orthogonal axis — "does
the component ever see the plaintext at all" — is ADR-0005 (which has since
shipped: `docs/adr/0005-egress-credential-injection.md`; this review predates
it). The host code at review time confirmed the gap: `spec.resolved_env`
is passed into `WasiCtx::env` at instantiation (`app.rs:123`,
`runtime.rs:185-187`), so the tenant's linear memory **holds its own API key
in plaintext** for the instance's lifetime.

**What ADR-0005 changes for the side-channel surface:**

If the credential is injected by the host *at the egress boundary* — i.e., the
component calls `http-fetch` with a placeholder and the host fills in the real
`Authorization`/API-key header inside `HostState::http_fetch`
(`runtime.rs:57-108`) before the request leaves the host — then **the
plaintext secret is never in the tenant's address space.** Consequences:

- There is no co-resident victim secret to leak. Cache/timing/Spectre attacks
  observe a tenant's *own* memory and microarch footprint; if tenant B's key
  is only ever in the *host's* memory (and only momentarily, in host code that
  does no secret-dependent branching either), tenant A has nothing co-resident
  to attack. The attack reduces to "leak a secret out of the host process,"
  which is the same as attacking any privileged broker — a much smaller,
  better-controlled surface than "leak it out of an untrusted peer tenant."
- This is true **independent of process/VM/core isolation.** Even if all
  tenants stay in one process on one core, removing the plaintext from tenant
  memory removes the thing worth stealing. That is why it is the
  highest-leverage mitigation for the *credential-theft* threat specifically.

**Is it the single most important mitigation here? Yes, for credentials.**
For the concrete worry in the prompt ("can tenant A extract tenant B's API
key"), ADR-0005 is more impactful than moving to process-per-tenant or VMs,
because it eliminates the asset rather than hardening the channel. It does
**not** address side channels in general (an app that does crypto on
*non*-credential secret data still leaks), and it does not help if the host's
egress injection itself becomes value-dependent in a way an attacker can
observe — but for opaque bearer tokens / API keys handed to an HTTP client, it
is decisive. It should be sequenced *before* opening the host to untrusted
third-party tenants.

Caveats worth noting in implementation:
- The host must scope which credential goes to which egress host (the
  allowlist in `runtime.rs:70-81` already binds a request to an allowed host;
  ADR-0005 should bind *which secret* is injected to *which (tenant, host)*
  pair, so a tenant can't trigger injection of a credential it isn't entitled
  to by targeting an allowed host).
- Response bodies still flow back into the component (`runtime.rs:126-135`);
  ADR-0005 protects the *credential*, not data the API returns. If the
  upstream response echoes the key, the protection is undone — the host should
  avoid handing back auth-bearing response headers.

---

## Q5 — Recommendation for Tangram's tenancy, by trust tier

The right isolation tier is a function of *who writes the code* and *what
secret is co-resident*. Three tiers, increasing in distrust:

### Tier 1 — First-party apps (today: notes, nutrition, registry, marketplace)
All app code is authored/curated by the operator (D4: "own/trusted apps for
months"; Phase 8 marketplace is operator-curated, third-party submission
explicitly deferred). There is no malicious peer.

- **In-process WASM is acceptable.** The memory-safety boundary (a) is the
  relevant one and it's solid; side channels require a malicious co-tenant,
  which does not exist in this tier.
- Keep: empty WASI ctx, allowlist egress, per-app data confinement
  (`tenant.rs:68-141`), bearer auth on mutating registry/marketplace tools.
- Recommended now: land ADR-0005 so first-party secrets aren't sitting in
  component memory regardless — cheap insurance and a prerequisite for Tier 2.
- Residual risk: low. A bug in a first-party app is an availability/integrity
  issue, not a cross-tenant confidentiality one.

### Tier 2 — Semi-trusted tenants (Phase 5/6 multi-tenancy: known accounts, OAuth/PAT)
Multiple real accounts (`/t/<tenant>/`), each running the operator's bundled
apps or vetted installs, but tenants don't fully trust *each other*. The
identity/confinement work is already strong (constant-time token lookup, no
existence oracle, data-tree confinement, allow_hosts ∩ ceiling, registry
specs blocked from host-env expansion — `tenant.rs:92-141`, Phase 5 notes).

- **In-process WASM is acceptable *if* ADR-0005 ships first** so no tenant's
  plaintext credential lives in tenant memory. With the asset removed, the
  residual side-channel risk is the generic "co-resident code can observe
  microarch state," which for non-crypto Tangram apps yields little of value.
- Configuration hardening to add at this tier:
  - **Disable SMT on the host** (`nosmt` / offline sibling threads) — removes
    the sharpest L1/port/MDS channels for near-zero cost on a doc-CRDT
    workload.
  - Keep Wasmtime's defaults that matter: Spectre mitigations on, guard pages
    on (the defaults; `runtime.rs` uses a stock `Config` plus cache). Do **not**
    expose a high-resolution timer to guests beyond the coarse `now-ms`
    (`wit/tangram.wit:30-31` is already coarse — keep it).
  - Per-tenant resource limits (fuel/epoch interruption, memory caps) to stop
    a tenant monopolizing a core to sharpen timing — Wasmtime supports epoch
    interruption / `StoreLimits`; not currently set in `runtime.rs` and worth
    adding.
- Residual risk: low-to-moderate. Acceptable for semi-trusted accounts;
  document that strong cross-tenant *confidentiality of secret-dependent
  computation* is not guaranteed in-process.

### Tier 3 — Fully untrusted third-party code (the marketplace TODO)
Arbitrary submitted components (`apps/marketplace` "third-party submissions"
TODO). The attacker *is* a tenant and will probe co-residents.

- **In-process WASM is NOT sufficient on its own** for strong cross-tenant
  secret confidentiality, because layer (b) is unaddressed and the attacker is
  hostile by assumption. SFI still prevents direct memory reads, so this is
  about timing/microarch leakage, not arbitrary memory disclosure.
- Required posture:
  - **ADR-0005 is mandatory** — never inject any third-party-relevant
    credential into a third-party component. The host owns all egress secrets.
  - **No cross-tenant co-residency on a core**: process-per-tenant *plus* core
    scheduling so two tenants never share a physical core; **SMT disabled** on
    the host.
  - For high-assurance separation, **dedicated cores or separate VMs per trust
    domain with LLC partitioning (Intel CAT)**; for the strongest tier,
    **separate physical hosts** (or the Cloudflare-per-tenant-DO path, Phase
    6, where the cloud provider owns the hardware-isolation problem).
  - Layer on Phase 8's planned capability verification (manifest ⊆ audited
    imports), sandboxed smoke-run, and behavioral check before listing.
- Residual risk even with all of the above: cache/LLC channels are reduced,
  not eliminated, without physical separation. Be honest in any marketplace
  threat-model doc that "we run your blob next to others" carries inherent
  microarchitectural risk, and that the platform's defense is *removing the
  secrets worth stealing* (ADR-0005) plus *not co-scheduling tenants on shared
  cores*, not a claim of perfect timing isolation.

### Tier recommendation summary

| Trust tier | In-proc WASM? | Process/VM | Core/SMT | Secret handling | Residual |
|---|---|---|---|---|---|
| 1 — first-party (today) | OK | shared process fine | default | ADR-0005 nice-to-have | low |
| 2 — semi-trusted accounts | OK *iff* ADR-0005 | shared process OK | **disable SMT**, add fuel/mem limits | **ADR-0005 (host-injected)** | low–moderate |
| 3 — untrusted 3rd-party | not alone | process-per-tenant; VM/dedicated-core for high assurance | **no cross-tenant core co-residency + SMT off + CAT** | **ADR-0005 mandatory** | reduced, not zero without physical separation |

---

## Synthesis

The Tangram WASM spine gets the *memory-safety* story right and arguably
better than a syscall sandbox (the component can't even name I/O). What it does
not — and cannot, by being in-process — provide is microarchitectural
isolation between co-resident tenants. gVisor would not close that gap; it is a
syscall barrier, and its maintainers say so. The good news specific to this
codebase is that the one obvious high-value secret (a stored API key copied
into an HTTP header) is a poor timing target, and the pending ADR-0005 removes
it from tenant memory entirely — which is the highest-leverage move for the
credential-theft threat, independent of process/VM/core isolation. The
disciplined path is: ship ADR-0005 before semi-trusted tenancy; disable SMT
and add per-tenant resource limits at that tier; and reserve genuinely
untrusted third-party code for process/VM/dedicated-core isolation with LLC
partitioning, while being candid that only physical separation fully closes
the cache channel.

---

## Sources

- gVisor — Security Model (explicit "does not provide protection against
  hardware side channels"; relies on host/platform for hardware attacks):
  https://gvisor.dev/docs/architecture_guide/security/
- gVisor — Security Basics / Intro (syscall + page-fault interposition as the
  boundary): https://gvisor.dev/docs/architecture_guide/intro/ ;
  https://gvisor.dev/blog/2019/11/18/gvisor-security-basics-part-1/
- Wasmtime — Security (Spectre mitigations on `call_indirect`/`br_table`/memory
  bounds checks, guard pages; mitigation "ongoing", no side-channel claim):
  https://docs.wasmtime.dev/security.html
- Bytecode Alliance — "Security and Correctness in Wasmtime":
  https://bytecodealliance.org/articles/security-and-correctness-in-wasmtime
- Wasmtime `Config` (guard pages, bounds-check / Spectre-related knobs):
  https://docs.wasmtime.dev/api/wasmtime/struct.Config.html
- Swivel: Hardening WebAssembly against Spectre (USENIX Security '21) — WASM
  is not inherently Spectre-safe; sandbox-poisoning/breakout classes:
  https://www.usenix.org/system/files/sec21fall-narayan.pdf
- WaSCR: A WebAssembly Instruction-Timing Side Channel Repairer (WWW '25) —
  instruction-timing side channels in WASM modules:
  https://chaowang-vt.github.io/pubDOC/HuangHWW25_WaSCR.pdf
- A Study of Timing Side-Channel Attacks and Countermeasures on JavaScript and
  WebAssembly:
  https://www.researchgate.net/publication/358890607
- "Last-Level Cache Side-Channel Attacks are Practical" (cross-core/cross-VM
  LLC attacks; requires secret-dependent victim accesses):
  https://www.cse.iitb.ac.in/~biswa/courses/CS773/lectures/primeprobe.pdf
- "Cache Attacks Enable Bulk Key Recovery on the Cloud" (Prime+Probe key
  recovery depends on key-dependent memory access): https://eprint.iacr.org/2016/596.pdf
- "From Co-location to Exfiltration: Practical Cache Side-Channel Attacks in
  the Modern Public Cloud" (IEEE 2024–2025; cross-tenant leakage demonstrated
  on Google Cloud Run; co-location + noise challenges):
  https://ieeexplore.ieee.org/document/11018321/
- "Lord of the Ring(s): Side Channel Attacks on the CPU On-Chip Ring
  Interconnect Are Practical": https://arxiv.org/pdf/2103.03443
- CATalyst: Defeating Last-Level Cache Side Channel Attacks in Cloud Computing
  (HPCA '16; Intel CAT for LLC partitioning):
  http://class.ece.iastate.edu/tyagi/cpre581/papers/HPCA16Catalyst.pdf
- "A Novel Scheduling Framework Leveraging Hardware Cache Partitioning for
  Cache-Side-Channel Elimination in Clouds" (CAT + scheduling):
  https://arxiv.org/pdf/1708.09538
- "Comparing Security and Efficiency of WebAssembly and Linux Containers in
  Kubernetes Cloud Computing": https://arxiv.org/pdf/2411.03344
- WebAssembly and Security: a review (2024): https://arxiv.org/pdf/2407.12297

### Codebase references grounding this review
- `crates/tangram-host/src/runtime.rs` — engine/instance setup, empty WASI ctx,
  env-injected secrets (`:185-188`), `http-fetch` allowlist (`:57-108`).
- `crates/tangram-host/src/app.rs` — `resolved_env` injection at instantiate
  (`:123`).
- `crates/tangram-host/src/tenant.rs` — multi-tenant confinement, ceiling
  intersection, registry env-expansion blocking.
- `crates/tangram-host/wit/tangram.wit` — closed component world (imports:
  `http-fetch`, `log`, `now-ms` only).
- `docs/RUNTIME_PLAN.md` (Phases 2, 5, 9), `docs/adr/0001-...md`,
  `docs/adr/0004-secret-resolution-interface.md`,
  `docs/adr/0005-egress-credential-injection.md` (referenced as pending at
  review time; has since shipped).
