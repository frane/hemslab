//! The web UI: one page, a JSON state endpoint and two controls.

use std::net::SocketAddr;
use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::cem::{self, Cem};
use crate::view::{Control, Registry};

const PAGE: &str = include_str!("ui.html");

#[derive(Clone)]
struct App {
    registry: Arc<Registry>,
    cem: Arc<Cem>,
}

pub async fn serve(
    address: SocketAddr,
    registry: Arc<Registry>,
    cem: Cem,
) -> Result<SocketAddr, String> {
    let app = App {
        registry,
        cem: Arc::new(cem),
    };
    let router = Router::new()
        .route("/", get(page))
        .route("/api/state", get(state))
        .route("/api/devices/{name}/mode", post(mode))
        .route("/api/devices/{name}/limit", post(limit))
        .with_state(app);
    let listener = tokio::net::TcpListener::bind(address)
        .await
        .map_err(|e| format!("UI on {address}: {e}"))?;
    let bound = listener.local_addr().map_err(|e| e.to_string())?;
    tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });
    Ok(bound)
}

async fn page() -> Response {
    ([(header::CONTENT_TYPE, "text/html; charset=utf-8")], PAGE).into_response()
}

async fn state(State(app): State<App>) -> Json<Value> {
    let devices: Vec<Value> = app
        .registry
        .all()
        .iter()
        .map(|d| {
            let view = d.view.lock().map(|v| v.clone()).unwrap_or_default();
            json!({
                "instance": d.instance,
                "profile": d.profile,
                "description": d.description,
                "kind": d.kind,
                "ski": d.ski,
                "port": d.port,
                "view": view,
            })
        })
        .collect();
    let cem = app.cem.status.lock().map(|s| s.clone()).unwrap_or_default();
    Json(json!({ "devices": devices, "cem": cem }))
}

#[derive(Deserialize)]
struct ModeBody {
    mode: String,
}

async fn mode(State(app): State<App>, Path(name): Path<String>, Json(body): Json<ModeBody>) -> StatusCode {
    match app.registry.get(&name) {
        Some(device) if device.control.send(Control::Mode(body.mode)).is_ok() => StatusCode::NO_CONTENT,
        _ => StatusCode::NOT_FOUND,
    }
}

#[derive(Deserialize)]
struct LimitBody {
    watts: Option<f64>,
}

async fn limit(State(app): State<App>, Path(name): Path<String>, Json(body): Json<LimitBody>) -> StatusCode {
    if app.registry.get(&name).is_none() {
        return StatusCode::NOT_FOUND;
    }
    if body.watts.is_some_and(|w| !w.is_finite() || w < 0.0) {
        return StatusCode::UNPROCESSABLE_ENTITY;
    }
    match app.cem.commands.send(cem::Command::Limit {
        instance: name,
        watts: body.watts,
    }) {
        Ok(()) => StatusCode::NO_CONTENT,
        Err(_) => StatusCode::SERVICE_UNAVAILABLE,
    }
}
