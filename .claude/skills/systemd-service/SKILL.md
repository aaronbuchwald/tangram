---
name: systemd-service
description: Install, deploy, rebuild, or restart a Tangram binary as a systemd service. Use when asked to install or deploy tangram-shell/tangram-notes/tangram-nutrition as a service, or to rebuild and restart an existing tangram systemd service after code changes.
argument-hint: install|rebuild [--dir <repo>] [--name <svc>] [--binary <pkg>] [--bind <addr:port>] [--env K=V]
allowed-tools: Bash
---

Manage a Tangram binary as a systemd service via the bundled script. All logic
lives in `service.sh`; run it and interpret the output.

## Commands

Install (build + write unit + enable + start + verify):

```bash
bash ${CLAUDE_SKILL_DIR}/service.sh install \
  [--dir <repo path>] [--name <service name>] [--binary <cargo package>] \
  [--bind <addr:port>] [--env KEY=VALUE ...]
```

Rebuild (build + restart + verify):

```bash
bash ${CLAUDE_SKILL_DIR}/service.sh rebuild \
  [--dir <repo path>] [--name <service name>] [--binary <cargo package>]
```

Pass the user's arguments through: $ARGUMENTS

## Defaults

- `--dir`: the repo root of the current working directory (`git rev-parse
  --show-toplevel`). Pass it explicitly to manage a service for a different
  checkout (e.g. `--dir /home/ubuntu/tangram`).
- `--name`: `tangram-shell` (unit file `/etc/systemd/system/<name>.service`).
- `--binary`: `tangram-shell`. Also works for `tangram-notes` and
  `tangram-nutrition`. ExecStart is `<dir>/target/release/<binary>`.
- `--bind`: optional; sets `Environment=BIND_ADDR=<addr:port>` in the unit
  (the apps default to 127.0.0.1:8080).
- `--env KEY=VALUE`: repeatable; each becomes an `Environment=` line (e.g.
  `--env NUTRITION_STRATEGY=calorieninjas`). Secrets in `<dir>/.env` are
  loaded by the app itself (dotenvy) via WorkingDirectory, not by systemd.

Install is idempotent: re-running overwrites the unit, daemon-reloads, and
restarts. The script uses `sudo` for the unit file and systemctl calls.

## Verification

The script verifies on its own: it waits for `systemctl is-active` to report
`active`, then curls `http://<bind>/healthz` falling back to `/` and requires
a 200. On failure it prints `journalctl -u <name> -n 20` and exits nonzero.

After running, confirm the final `OK:` line is present and report the service
name, unit path, and the URL that returned 200. If it fails, read the printed
journal lines to diagnose (common causes: port already in use, missing env
vars, binary name vs package name mismatch).
