//! Observability O2: pin that the principal-identity telemetry field the host
//! renders (`x-tangram-principal` referenced via a CEL expression in BOTH
//! `config.logging` and `config.tracing`) is ACCEPTED by the installed
//! agentgateway — the mechanism is verified against the binary, not assumed.
//!
//! `tangram-host` is a binary-only crate, so the render itself is unit-tested in
//! `src/gateway.rs`; this test pins the OTHER half: that the exact CEL
//! expression those fields map to (`request.headers["x-tangram-principal"]`) is
//! a valid expression for THIS agentgateway version. agentgateway evaluates
//! `logging`/`tracing` field values as CEL over the per-request context, and
//! `--validate-only` parses the config AND every CEL expression — so a passing
//! run proves the identity fields are wire-correct (a missing header → null, not
//! a parse error).
//!
//! The CEL literal here is kept byte-identical to `gateway::PRINCIPAL_FIELD_CEL`
//! (a `src/gateway.rs` unit test asserts that constant references
//! `auth::PRINCIPAL_HEADER`); a control case with a deliberately-broken
//! expression confirms the validator actually rejects bad CEL (so a green run is
//! meaningful, not vacuous).
//!
//! SKIPS with a notice when no `agentgateway` binary is on $PATH.

use std::io::Write as _;
use std::process::Command;

/// MUST match `tangram_host::gateway::PRINCIPAL_FIELD_CEL` (binary crate ⇒ can't
/// import it; the gateway unit test ties that constant to the header name).
const PRINCIPAL_FIELD_CEL: &str = r#"request.headers["x-tangram-principal"]"#;

fn agentgateway_on_path() -> Option<std::path::PathBuf> {
    std::env::var_os("PATH").and_then(|path| {
        std::env::split_paths(&path)
            .map(|dir| dir.join("agentgateway"))
            .find(|p| p.is_file())
    })
}

/// Run `agentgateway -f <cfg> --validate-only`; returns (valid, combined output).
/// `--validate-only` prints "Configuration is valid!" on success and an
/// "Error:" line (schema or CEL parse) on failure.
fn validate(bin: &std::path::Path, config: &serde_json::Value) -> (bool, String) {
    let mut tmp = tempfile::NamedTempFile::new().expect("temp config");
    tmp.write_all(serde_json::to_string_pretty(config).unwrap().as_bytes())
        .unwrap();
    let out = Command::new(bin)
        .arg("-f")
        .arg(tmp.path())
        .arg("--validate-only")
        .output()
        .expect("run agentgateway --validate-only");
    let combined = format!(
        "STDOUT: {}\nSTDERR: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let valid = combined.contains("Configuration is valid!") && !combined.contains("Error:");
    (valid, combined)
}

/// A config carrying the principal-identity CEL in both telemetry blocks,
/// shaped like the host's real render (an MCP route + the logging/tracing
/// fields). `field_cel` is the CEL the identity fields map to.
fn identity_config(field_cel: &str) -> serde_json::Value {
    serde_json::json!({
        "config": {
            "adminAddr": "127.0.0.1:0",
            "statsAddr": "127.0.0.1:0",
            "readinessAddr": "127.0.0.1:0",
            "logging": { "fields": { "add": {
                "total_tokens": "llm.totalTokens",
                "principal": field_cel
            } } },
            "tracing": {
                "otlpEndpoint": "http://127.0.0.1:3000/api/public/otel",
                "otlpProtocol": "http",
                "randomSampling": true,
                "fields": { "add": {
                    "gen_ai.usage.total_tokens": "llm.totalTokens",
                    "tangram.principal": field_cel
                } }
            }
        },
        "binds": [{
            "port": 19998,
            "listeners": [{ "routes": [
                { "name": "notes-mcp",
                  "policies": { "authorization": { "rules": [
                      "string(source.address).startsWith(\"127.\")"] } },
                  "matches": [{ "path": { "pathPrefix": "/notes/mcp" } }],
                  "backends": [{ "mcp": { "targets": [] } }] }
            ] }]
        }]
    })
}

#[test]
fn principal_identity_cel_validates_against_agentgateway() {
    let Some(bin) = agentgateway_on_path() else {
        eprintln!("SKIP: no `agentgateway` on $PATH — render shape is unit-tested in gateway.rs");
        return;
    };

    // The real identity CEL is accepted in BOTH the access log and the trace.
    let (valid, out) = validate(&bin, &identity_config(PRINCIPAL_FIELD_CEL));
    assert!(
        valid,
        "agentgateway rejected the principal-identity CEL {PRINCIPAL_FIELD_CEL:?}:\n{out}"
    );

    // Control: a deliberately-broken CEL IS rejected — so the green run above is
    // a real signal that the validator parses these expressions, not a no-op.
    let (valid_bad, _) = validate(&bin, &identity_config("this is (((not cel"));
    assert!(
        !valid_bad,
        "control: agentgateway must reject a malformed CEL expression"
    );
}
