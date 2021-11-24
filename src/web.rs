use std::convert::Infallible;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};

use axum::{
    body::{Bytes, Full},
    extract,
    handler::get,
    http::{Response, StatusCode},
    response::IntoResponse,
    AddExtensionLayer, Json, Router,
};

use tokio::signal::unix::{signal, SignalKind};
use tokio::sync::oneshot;

use serde::Serialize;

use crate::network::{NetworkCommand, NetworkRequest, NetworkResponse};

pub enum AppResponse {
    Network(NetworkResponse),
    Error(anyhow::Error),
}

#[derive(Serialize)]
pub struct AppErrors {
    pub errors: Vec<String>,
}

impl AppErrors {
    fn new(errors: Vec<String>) -> Self {
        AppErrors { errors }
    }
}

struct State {
    glib_sender: glib::Sender<NetworkRequest>,
    shutdown_opt: Mutex<Option<oneshot::Sender<()>>>,
}

pub async fn run_web_loop(glib_sender: glib::Sender<NetworkRequest>) {
    let (shutdown_tx, shutdown_rx) = oneshot::channel();

    let shared_state = Arc::new(State {
        glib_sender: glib_sender.clone(),
        shutdown_opt: Mutex::new(Some(shutdown_tx)),
    });

    let app = Router::new()
        .route("/", get(usage))
        .route("/check-connectivity", get(check_connectivity))
        .route("/list-connections", get(list_connections))
        .route("/list-wifi-networks", get(list_wifi_networks))
        .route("/shutdown", get(shutdown))
        .route("/stop", get(stop))
        .layer(AddExtensionLayer::new(shared_state));

    let server =
        axum::Server::bind(&"0.0.0.0:3000".parse().unwrap()).serve(app.into_make_service());

    let graceful = server.with_graceful_shutdown(shutdown_signal(shutdown_rx, glib_sender));

    graceful.await.unwrap();
}

async fn shutdown_signal(
    shutdown_rx: oneshot::Receiver<()>,
    glib_sender: glib::Sender<NetworkRequest>,
) {
    let mut interrupt = signal(SignalKind::interrupt()).unwrap();
    let mut terminate = signal(SignalKind::terminate()).unwrap();
    let mut quit = signal(SignalKind::quit()).unwrap();
    let mut hangup = signal(SignalKind::hangup()).unwrap();

    tokio::select! {
        _ = shutdown_rx => {},
        _ = interrupt.recv() => println!("SIGINT received"),
        _ = terminate.recv() => println!("SIGTERM received"),
        _ = quit.recv() => println!("SIGQUIT received"),
        _ = hangup.recv() => println!("SIGHUP received"),
    }

    send_command(&glib_sender, NetworkCommand::Stop).await;

    println!("Shut down")
}

async fn usage() -> &'static str {
    "Use /check-connectivity or /list-connections\n"
}

async fn check_connectivity(state: extract::Extension<Arc<State>>) -> impl IntoResponse {
    send_command(&state.0.glib_sender, NetworkCommand::CheckConnectivity)
        .await
        .into_response()
}

async fn list_connections(state: extract::Extension<Arc<State>>) -> impl IntoResponse {
    send_command(&state.0.glib_sender, NetworkCommand::ListConnections)
        .await
        .into_response()
}

async fn list_wifi_networks(state: extract::Extension<Arc<State>>) -> impl IntoResponse {
    send_command(&state.0.glib_sender, NetworkCommand::ListWiFiNetworks)
        .await
        .into_response()
}

async fn shutdown(mut state: extract::Extension<Arc<State>>) -> impl IntoResponse {
    let response = send_command(&state.0.glib_sender, NetworkCommand::Shutdown)
        .await
        .into_response();

    issue_shutdwon(&mut state.0).await;

    response
}

async fn stop(state: extract::Extension<Arc<State>>) -> impl IntoResponse {
    send_command(&state.0.glib_sender, NetworkCommand::Stop)
        .await
        .into_response()
}

async fn issue_shutdwon(state: &mut Arc<State>) {
    if let Some(shutdown_tx) = state.shutdown_opt.lock().unwrap().take() {
        shutdown_tx.send(()).ok();
    }
}

async fn send_command(
    glib_sender: &glib::Sender<NetworkRequest>,
    command: NetworkCommand,
) -> AppResponse {
    let (responder, receiver) = oneshot::channel();

    let action = match command {
        NetworkCommand::CheckConnectivity => "check connectivity",
        NetworkCommand::ListConnections => "list actions",
        NetworkCommand::ListWiFiNetworks => "list WiFi networks",
        NetworkCommand::Shutdown => "shutdown",
        NetworkCommand::Stop => "stop",
    };

    glib_sender
        .send(NetworkRequest::new(responder, command))
        .unwrap();

    receive_network_thread_response(receiver, action).await
}

async fn receive_network_thread_response(
    receiver: oneshot::Receiver<Result<NetworkResponse>>,
    action: &str,
) -> AppResponse {
    let received = receiver
        .await
        .context("Failed to receive network thread response");

    let result = received
        .and_then(|r| r)
        .or_else(|e| Err(e).context(format!("Failed to {}", action)));

    match result {
        Ok(network_response) => AppResponse::Network(network_response),
        Err(err) => AppResponse::Error(err),
    }
}

impl IntoResponse for AppResponse {
    type Body = Full<Bytes>;
    type BodyError = Infallible;

    fn into_response(self) -> Response<Self::Body> {
        match self {
            AppResponse::Error(err) => {
                let errors: Vec<String> = err.chain().map(|e| format!("{}", e)).collect();
                let app_errors = AppErrors::new(errors);
                (StatusCode::INTERNAL_SERVER_ERROR, Json(app_errors)).into_response()
            }
            AppResponse::Network(network_response) => match network_response {
                NetworkResponse::ListConnections(connections) => {
                    (StatusCode::OK, Json(connections)).into_response()
                }
                NetworkResponse::CheckConnectivity(connectivity) => {
                    (StatusCode::OK, Json(connectivity)).into_response()
                }
                NetworkResponse::ListWiFiNetworks(networks) => {
                    (StatusCode::OK, Json(networks)).into_response()
                }
                NetworkResponse::Shutdown(shutdown) => {
                    (StatusCode::OK, Json(shutdown)).into_response()
                }
                NetworkResponse::Stop(stop) => (StatusCode::OK, Json(stop)).into_response(),
            },
        }
    }
}
