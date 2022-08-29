use anyhow::{Context, Result};

use actix_web::web::{resource, Data};
use actix_web::{middleware, App, HttpResponse, HttpServer};

use tokio::sync::oneshot;

use serde::Serialize;

use crate::network::{Command, CommandRequest, CommandResponse};
use crate::nl80211;

#[derive(Debug)]
pub enum AppResponse {
    Network(CommandResponse),
    Error(anyhow::Error),
}

#[derive(Serialize)]
pub struct AppErrors {
    pub errors: Vec<String>,
}

impl AppErrors {
    fn new(errors: Vec<String>) -> Self {
        Self { errors }
    }
}

type Sender = glib::Sender<CommandRequest>;

pub async fn run_web_loop(glib_sender: Sender) -> Result<()> {
    println!("Web server starting...");

    HttpServer::new(move || {
        App::new()
            .app_data(Data::new(glib_sender.clone()))
            .wrap(middleware::Logger::default())
            .service(resource("/").to(index))
            .service(resource("/check-connectivity").to(check_connectivity))
            .service(resource("/list-connections").to(list_connections))
            .service(resource("/list-wifi-networks").to(list_wifi_networks))
            .service(resource("/stop").to(stop))
            .service(resource("/scan").to(scan))
    })
    .bind(("127.0.0.1", 3000))
    .context("Failed to bind listening socket")?
    .run()
    .await
    .context("Failed to run HTTP server")
}

#[allow(clippy::unused_async)]
async fn index() -> &'static str {
    "WiFi Connect"
}

async fn check_connectivity(sender: Data<Sender>) -> HttpResponse {
    send_command(sender.get_ref(), Command::CheckConnectivity)
        .await
        .into()
}

async fn list_connections(sender: Data<Sender>) -> HttpResponse {
    send_command(sender.get_ref(), Command::ListConnections)
        .await
        .into()
}

async fn list_wifi_networks(sender: Data<Sender>) -> HttpResponse {
    send_command(sender.get_ref(), Command::ListWiFiNetworks)
        .await
        .into()
}

async fn stop(sender: Data<Sender>) -> HttpResponse {
    send_command(sender.get_ref(), Command::Stop).await.into()
}

async fn scan() -> HttpResponse {
    let scan_result = nl80211::scan::scan("wlan0")
        .await
        .context("Failed to scan for networks");

    match scan_result {
        Ok(stations) => HttpResponse::Ok().json(stations),
        Err(err) => to_http_error_response(&err),
    }
}

async fn send_command(glib_sender: &glib::Sender<CommandRequest>, command: Command) -> AppResponse {
    let (responder, receiver) = oneshot::channel();

    let action = match command {
        Command::CheckConnectivity => "check connectivity",
        Command::ListConnections => "list actions",
        Command::ListWiFiNetworks => "list WiFi networks",
        Command::Stop => "stop",
    };

    glib_sender
        .send(CommandRequest::new(responder, command))
        .expect("Failed to send command request");

    receive_network_thread_response(receiver, action)
        .await
        .into()
}

async fn receive_network_thread_response(
    receiver: oneshot::Receiver<Result<CommandResponse>>,
    action: &str,
) -> Result<CommandResponse> {
    let result = receiver
        .await
        .context("Failed to receive network thread response");

    result
        .and_then(|r| r)
        .or_else(|e| Err(e).context(format!("Failed to {}", action)))
}

impl From<Result<CommandResponse>> for AppResponse {
    fn from(result: Result<CommandResponse>) -> Self {
        match result {
            Ok(network_response) => Self::Network(network_response),
            Err(err) => Self::Error(err),
        }
    }
}

impl From<AppResponse> for HttpResponse {
    fn from(response: AppResponse) -> Self {
        match response {
            AppResponse::Error(err) => to_http_error_response(&err),
            AppResponse::Network(network_response) => match network_response {
                CommandResponse::ListConnections(connections) => Self::Ok().json(connections),
                CommandResponse::CheckConnectivity(connectivity) => Self::Ok().json(connectivity),
                CommandResponse::ListWiFiNetworks(networks) => Self::Ok().json(networks),
                CommandResponse::Stop(stop) => Self::Ok().json(stop),
            },
        }
    }
}

fn to_http_error_response(err: &anyhow::Error) -> HttpResponse {
    let errors: Vec<String> = err.chain().map(|e| format!("{}", e)).collect();
    let app_errors = AppErrors::new(errors);
    HttpResponse::InternalServerError().json(app_errors)
}
