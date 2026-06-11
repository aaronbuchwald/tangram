//! Thin wrapper so the Cloudflare relay e2e is reachable from `cargo test`:
//!
//! ```sh
//! cargo test -p tangram-host -- --ignored e2e_cloudflare
//! ```
//!
//! All the substance lives in `scripts/e2e-cloudflare-sync.sh`: it builds
//! `tangram-notes`, runs the relay under `wrangler dev` (miniflare) on an
//! isolated state dir, and asserts genesis convergence, bidirectional sync,
//! and restart persistence (see the script header and docs/SYNC_PROTOCOL.md).
//! Ignored by default because it needs node/npm and spawns real processes.

use std::path::Path;
use std::process::Command;

#[test]
#[ignore = "spawns wrangler dev (miniflare) + two native instances; needs node >= 20.3 and npm"]
fn e2e_cloudflare_sync() {
    let script = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../scripts/e2e-cloudflare-sync.sh")
        .canonicalize()
        .expect("scripts/e2e-cloudflare-sync.sh exists");
    let status = Command::new("bash")
        .arg(&script)
        .status()
        .expect("spawn bash for the e2e script");
    assert!(
        status.success(),
        "e2e-cloudflare-sync.sh failed ({status}); its output above has the failing assertion \
         and the wrangler/native log tails"
    );
}
