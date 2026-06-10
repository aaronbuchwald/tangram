//! Web frontend: a JSON API plus static UI files served from `ui/`.
//!
//! The UI is designed to work both standalone in a browser and embedded as
//! an iframe inside hosts like Obsidian or the Tangram shell (see the
//! frame-ancestors handling in main.rs).

use axum::{Json, Router, extract::State, routing::get};
use serde::Deserialize;
use tower_http::{cors::CorsLayer, services::ServeDir};

use crate::state::{AppState, Note};

#[derive(Debug, Deserialize)]
pub struct AddNoteBody {
    pub text: String,
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        .route(
            "/api/notes",
            get(list_notes).post(add_note).delete(clear_notes),
        )
        // NOTE: permissive CORS so embedding hosts can call the API from any
        // origin. Tighten this for apps that hold sensitive data.
        .layer(CorsLayer::permissive())
        .fallback_service(ServeDir::new("ui").append_index_html_on_directories(true))
        .with_state(state)
}

async fn healthz() -> &'static str {
    "ok"
}

async fn list_notes(State(state): State<AppState>) -> Json<Vec<Note>> {
    Json(state.list_notes().await)
}

async fn add_note(State(state): State<AppState>, Json(body): Json<AddNoteBody>) -> Json<Note> {
    Json(state.add_note(body.text).await)
}

async fn clear_notes(State(state): State<AppState>) -> Json<usize> {
    Json(state.clear_notes().await)
}
