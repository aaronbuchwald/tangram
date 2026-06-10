//! Tangram shell — several Tangram apps on one server, each under its own
//! path prefix. Every mounted app keeps its full surface (web UI + JSON API,
//! `/mcp`, `/sync`) and its own document; per-app sync remotes come from
//! `TANGRAM_REMOTE_<NAME>` (e.g. `TANGRAM_REMOTE_NOTES`).

use std::future::IntoFuture;

use anyhow::Context;
use axum::Router;
use axum::response::{Html, Redirect};
use axum::routing::get;
use tracing_subscriber::EnvFilter;

/// The apps this shell hosts, as (prefix, title, blurb).
const APPS: &[(&str, &str, &str)] = &[
    ("notes", "Notes", "a replicated notes list"),
    ("nutrition", "Nutrition", "a replicated nutrition tracker"),
];

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let _ = dotenvy::dotenv();
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .try_init();

    let bind_addr = std::env::var("BIND_ADDR").unwrap_or_else(|_| "127.0.0.1:8080".into());

    let app = Router::new()
        .route("/", get(|| async { Html(index_html()) }))
        // The app UIs fetch relative paths (api/events, api/actions/...), so
        // the prefix must end with a slash for them to resolve.
        .route("/notes", get(|| async { Redirect::permanent("/notes/") }))
        .route(
            "/nutrition",
            get(|| async { Redirect::permanent("/nutrition/") }),
        )
        // Mounted with a trailing slash so the prefix root (`/notes/`) reaches
        // the app's static-UI fallback (`Router::nest` would leave it
        // unroutable: a nested fallback only matches non-empty tails).
        .nest_service("/notes/", notes::app().build()?)
        .nest_service("/nutrition/", nutrition::app().build()?);

    let listener = tokio::net::TcpListener::bind(&bind_addr)
        .await
        .with_context(|| format!("failed to bind {bind_addr}"))?;
    tracing::info!("shell — index http://{bind_addr}/");
    for (prefix, _, _) in APPS {
        tracing::info!("{prefix} — web UI http://{bind_addr}/{prefix}/");
        tracing::info!("{prefix} — mcp    http://{bind_addr}/{prefix}/mcp");
        tracing::info!("{prefix} — sync   ws://{bind_addr}/{prefix}/sync");
    }

    // Race the server against Ctrl-C instead of using graceful shutdown: the
    // hosted apps hold connections that never close on their own (SSE state
    // streams, sync WebSockets, MCP sessions), so graceful shutdown would
    // hang until every client disconnected. Aborting them is safe because
    // each app's store persists synchronously on every change.
    tokio::select! {
        result = axum::serve(listener, app).into_future() => result?,
        _ = tokio::signal::ctrl_c() => {
            tracing::info!("shell — shutting down");
        }
    }
    Ok(())
}

fn index_html() -> String {
    let cards: String = APPS
        .iter()
        .map(|(prefix, title, blurb)| {
            format!(
                r#"    <li>
      <a class="app" href="/{prefix}/"><strong>{title}</strong><span>{blurb}</span></a>
      <div class="endpoints">
        <code>/{prefix}/mcp</code>
        <code>ws://&hellip;/{prefix}/sync</code>
      </div>
    </li>
"#
            )
        })
        .collect();
    format!(
        r#"<!doctype html>
<html lang="en">
<head>
  <meta charset="utf-8">
  <meta name="viewport" content="width=device-width, initial-scale=1">
  <title>Tangram shell</title>
  <style>
    :root {{ color-scheme: dark; }}
    body {{
      margin: 0; min-height: 100vh; display: grid; place-content: center;
      background: #14161a; color: #e6e8eb;
      font: 16px/1.5 system-ui, -apple-system, sans-serif;
    }}
    main {{ padding: 3rem 1.5rem; max-width: 36rem; }}
    h1 {{ font-size: 1.4rem; margin: 0 0 0.25rem; }}
    p.sub {{ color: #9aa0a8; margin: 0 0 2rem; }}
    ul {{ list-style: none; margin: 0; padding: 0; display: grid; gap: 1rem; }}
    a.app {{
      display: block; padding: 1rem 1.25rem; border-radius: 10px;
      background: #1d2026; border: 1px solid #2a2e36;
      color: inherit; text-decoration: none;
    }}
    a.app:hover {{ border-color: #4a90d9; }}
    a.app strong {{ display: block; font-size: 1.1rem; }}
    a.app span {{ color: #9aa0a8; font-size: 0.9rem; }}
    .endpoints {{ margin: 0.4rem 0.25rem 0; display: flex; gap: 0.75rem; }}
    .endpoints code {{
      font-size: 0.78rem; color: #7d8590; background: #1a1d22;
      padding: 0.1rem 0.45rem; border-radius: 5px;
    }}
  </style>
</head>
<body>
  <main>
    <h1>Tangram shell</h1>
    <p class="sub">Installed apps on this server</p>
    <ul>
{cards}    </ul>
  </main>
</body>
</html>
"#
    )
}
