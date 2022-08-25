use anyhow::{Context, Result};

use actix_web::web::{resource, Data};
use actix_web::{middleware, App, HttpResponse, HttpServer};

use tokio::sync::oneshot;

use serde::Serialize;

use crate::network::{Command, CommandRequest, CommandResponce};
use crate::nl80211;

pub enum AppResponse {
    Network(CommandResponce),
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

pub async fn run_web_loop(glib_sender: Sender) {
    println!("Web server starting...");

    HttpServer::new(move || {
        App::new()
            .app_data(Data::new(glib_sender.clone()))
            .wrap(middleware::Logger::default())
            .service(resource("/").to(usage))
            .service(resource("/check-connectivity").to(check_connectivity))
            .service(resource("/list-connections").to(list_connections))
            .service(resource("/list-wifi-networks").to(list_wifi_networks))
            .service(resource("/stop").to(stop))
            .service(resource("/scan").to(scan))
    })
    .bind(("127.0.0.1", 3000))
    .unwrap()
    .run()
    .await
    .unwrap();
}

async fn usage() -> &'static str {
    "Use /check-connectivity or /list-connections\n"
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
    let stations = nl80211::scan::scan("wlan0").await.unwrap();
    HttpResponse::Ok().json(stations)
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
        .unwrap();

    receive_network_thread_response(receiver, action)
        .await
        .into()
}

async fn receive_network_thread_response(
    receiver: oneshot::Receiver<Result<CommandResponce>>,
    action: &str,
) -> Result<CommandResponce> {
    let result = receiver
        .await
        .context("Failed to receive network thread response");

    result
        .and_then(|r| r)
        .or_else(|e| Err(e).context(format!("Failed to {}", action)))
}

impl From<Result<CommandResponce>> for AppResponse {
    fn from(result: Result<CommandResponce>) -> Self {
        match result {
            Ok(network_response) => Self::Network(network_response),
            Err(err) => Self::Error(err),
        }
    }
}

impl Into<HttpResponse> for AppResponse {
    fn into(self) -> HttpResponse {
        match self {
            AppResponse::Error(err) => {
                let errors: Vec<String> = err.chain().map(|e| format!("{}", e)).collect();
                let app_errors = AppErrors::new(errors);
                HttpResponse::InternalServerError().json(app_errors)
            }
            AppResponse::Network(network_response) => match network_response {
                CommandResponce::ListConnections(connections) => {
                    HttpResponse::Ok().json(connections)
                }
                CommandResponce::CheckConnectivity(connectivity) => {
                    HttpResponse::Ok().json(connectivity)
                }
                CommandResponce::ListWiFiNetworks(networks) => HttpResponse::Ok().json(networks),
                CommandResponce::Stop(stop) => HttpResponse::Ok().json(stop),
            },
        }
    }
}
