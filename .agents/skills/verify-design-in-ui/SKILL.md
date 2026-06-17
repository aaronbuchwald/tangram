---
name: verify-design-in-ui
description: After landing a frontend-verifiable unit of work, confirm the design doc's intended behaviors actually shipped in the running UI. Reviews the design doc, uses the LLM to isolate the concrete observable behaviors the feature should exhibit, then drives the live UI with the browser tools to verify each one PASS / DIVERGES / MISSING with screenshot evidence. Use after merging + deploying any feature whose acceptance is visible in the frontend.
argument-hint: <design-doc-path> [--section <checkpoint>] [--url http://localhost:8080] [--commit <sha>]
allowed-tools: Read, Bash, mcp__playwright__browser_navigate, mcp__playwright__browser_snapshot, mcp__playwright__browser_click, mcp__playwright__browser_type, mcp__playwright__browser_press_key, mcp__playwright__browser_take_screenshot, mcp__playwright__browser_evaluate, mcp__playwright__browser_close
---

# Verify a design doc landed in the running UI

Close the loop between a design doc and the shipped feature: extract the
behaviors the doc *promises*, then prove against the **running UI** that each
one actually landed — or pinpoint exactly where reality diverges. This is an
acceptance check, not a code review; the evidence is what the UI does.

Run this **after** the feature has been merged AND deployed (a green build of
the relevant commit is live). Best run as a dedicated subagent, because the
browser tools write screenshots into the repo — keep them isolated and clean
up at the end.

Inputs (`$ARGUMENTS`): the design-doc path (required); `--section <checkpoint>`
to scope to one slice (e.g. an `R1`/`Build 1` checkpoint); `--url` of the
running UI (default `http://localhost:8080`); `--commit <sha>` the landing
commit (default: current `origin/main` HEAD).

## Step 1 — Isolate the observable behaviors (the LLM step)

`Read` the design doc (and the named `--section`). Reason over it and produce a
**checklist of concrete, observable, UI-verifiable behaviors** that must be TRUE
now that the feature has landed. Derive them from the doc's **FIRM / user-facing
decisions and acceptance criteria** — the things a user can see or do.

For each behavior write: a one-line **testable claim**, and **how to observe
it** (the exact interaction + the expected result). Keep them atomic.

**Exclude and list separately** (do not test): backend-only mechanics, anything
the doc marks deferred to a later checkpoint, and non-UI concerns. Verifying
only what the doc actually scoped to *this* landing is the point — don't fail a
feature for not doing a deferred thing.

## Step 2 — Confirm it's actually deployed

Don't verify a stale build. Check the running service corresponds to the
landing commit:

```bash
git -C <repo> log --oneline -1 <commit>            # the feature commit
systemctl is-active tangram-host                    # service up
curl -sf -o /dev/null -w '%{http_code}\n' <url>/    # reachable
```

If the running build predates the commit (UI dist not rebuilt / service not
restarted), STOP and report "not deployed — verification skipped" rather than
testing the old UI.

## Step 3 — Walk the running UI

`browser_navigate` to `<url>`. For EACH behavior from Step 1:

- Perform the interaction (`browser_click` / `browser_type` /
  `browser_press_key`), then `browser_snapshot` and/or
  `browser_take_screenshot` to capture the result.
- For keyboard/atomicity behaviors (e.g. "cursor steps over a widget, can't
  text-edit it"), drive it with `browser_press_key` (arrow/click into the
  token) and inspect cursor position / DOM via `browser_evaluate`.
- Record a verdict per behavior:
  - **PASS** — present and matches the spec.
  - **DIVERGES** — present but differs; describe the exact delta (what the doc
    said vs what the UI does).
  - **MISSING** — not present at all.
- Cite concrete evidence (the visible text, the screenshot, the DOM/cursor
  state). When unsure, say so rather than guessing PASS.

Don't destructively mutate persistent/shared state; if a check requires
creating data, prefer throwaway/clearly-labeled test data and note it.

## Step 4 — Report

Output a table — **Behavior | Verdict | Evidence/Notes** — followed by:

- a one-line **summary** (e.g. "6/7 landed; 1 diverges"),
- the **divergences and gaps** spelled out (these are the actionable items),
- the **deferred/out-of-UI items** you intentionally skipped (so the reader
  knows coverage), and
- **recommended follow-ups** (fix-forward items, or "ship as-is").

## Cleanup (always)

`browser_close`, then remove any screenshot files / `.playwright-mcp/`
directory the browser tools wrote into the repo so the working tree stays
clean (`rm -rf <repo>/.playwright-mcp <repo>/*.png` as applicable). Report that
the tree is clean.
