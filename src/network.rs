use anyhow::{anyhow, bail, Context, Result};

use tokio::sync::oneshot;

use glib::translate::FromGlib;
use glib::{MainContext, MainLoop};

use std::cell::RefCell;
use std::collections::HashSet;
use std::future::Future;
use std::rc::Rc;

use serde::Serialize;

use crate::opts::Opts;

use nm::{
    utils_get_timestamp_msec, AccessPoint, ActiveConnection, ActiveConnectionExt,
    ActiveConnectionState, Cast, Client, Connection, ConnectionExt, Device, DeviceExt, DeviceState,
    DeviceType, DeviceWifi, IPAddress, SettingConnection, SettingIP4Config, SettingIPConfigExt,
    SettingWireless, SettingWirelessSecurity, SimpleConnection, SETTING_IP4_CONFIG_METHOD_MANUAL,
    SETTING_WIRELESS_MODE_AP, SETTING_WIRELESS_SETTING_NAME,
};

const WIFI_SCAN_TIMEOUT_SECONDS: usize = 45;

type TokioResponder = oneshot::Sender<Result<CommandResponse>>;

#[derive(Debug)]
pub enum Command {
    CheckConnectivity,
    ListConnections,
    ListWiFiNetworks,
    Stop,
}

pub struct CommandRequest {
    responder: TokioResponder,
    command: Command,
}

impl CommandRequest {
    pub fn new(responder: TokioResponder, command: Command) -> Self {
        Self { responder, command }
    }
}

#[derive(Debug)]
pub enum CommandResponse {
    CheckConnectivity(Connectivity),
    ListConnections(Vec<ConnectionDetails>),
    ListWiFiNetworks(Vec<Station>),
    Stop(Stop),
}

#[derive(Serialize, Debug)]
pub struct Connectivity {
    pub connectivity: String,
}

impl Connectivity {
    const fn new(connectivity: String) -> Self {
        Self { connectivity }
    }
}

#[derive(Serialize, Debug)]
pub struct ConnectionDetails {
    pub id: String,
    pub uuid: String,
}

impl ConnectionDetails {
    const fn new(id: String, uuid: String) -> Self {
        Self { id, uuid }
    }
}

#[derive(Serialize, Debug, Clone, PartialEq, Eq, Hash)]
pub struct Station {
    pub ssid: String,
    pub quality: u8,
}

impl Station {
    const fn new(ssid: String, quality: u8) -> Self {
        Self { ssid, quality }
    }
}

impl TryFrom<&AccessPoint> for Station {
    type Error = anyhow::Error;

    fn try_from(ap: &AccessPoint) -> Result<Self, Self::Error> {
        if let Some(ssid) = ssid_to_str(ap.ssid().as_deref()) {
            Ok(Self::new(ssid.to_owned(), ap.strength()))
        } else {
            bail!("SSID not a string")
        }
    }
}

#[derive(Serialize, Debug)]
pub struct Stop {
    pub stop: String,
}

impl Stop {
    fn new(status: &str) -> Self {
        Self {
            stop: status.to_owned(),
        }
    }
}

#[allow(dead_code)]
struct NetworkState {
    client: Client,
    device: DeviceWifi,
    stations: Vec<Station>,
    portal_connection: Option<ActiveConnection>,
}

impl NetworkState {
    fn new(
        client: Client,
        device: DeviceWifi,
        stations: Vec<Station>,
        portal_connection: Option<ActiveConnection>,
    ) -> Self {
        Self {
            client,
            device,
            stations,
            portal_connection,
        }
    }
}

pub fn create_channel() -> (glib::Sender<CommandRequest>, glib::Receiver<CommandRequest>) {
    MainContext::channel(glib::PRIORITY_DEFAULT)
}

pub fn run_network_manager_loop(
    opts: Opts,
    initialized_sender: oneshot::Sender<Result<()>>,
    glib_receiver: glib::Receiver<CommandRequest>,
) {
    let context = MainContext::new();
    let loop_ = MainLoop::new(Some(&context), false);

    context
        .with_thread_default(|| {
            let state = context
                .block_on(init_network_respond(opts, initialized_sender))
                .expect("Network not initialized");

            glib_receiver.attach(None, move |command_request| {
                let CommandRequest { responder, command } = command_request;
                let _ = &state;
                match command {
                    Command::CheckConnectivity => {
                        spawn(responder, check_connectivity(state.client.clone()));
                    }
                    Command::ListConnections => {
                        respond(responder, Ok(list_connections(&state.client)));
                    }
                    Command::ListWiFiNetworks => {
                        respond(responder, Ok(list_wifi_networks(state.stations.clone())));
                    }
                    Command::Stop => {
                        spawn(
                            responder,
                            stop(state.client.clone(), state.portal_connection.clone()),
                        );
                    }
                };
                glib::Continue(true)
            });

            loop_.run();
        })
        .expect("Main context is owned already by another thread");
}

async fn init_network_respond(
    opts: Opts,
    initialized_sender: oneshot::Sender<Result<()>>,
) -> Option<NetworkState> {
    match init_network(opts).await {
        Ok(state) => {
            initialized_sender.send(Ok(())).ok();
            Some(state)
        }
        Err(err) => {
            initialized_sender.send(Err(err)).ok();
            None
        }
    }
}

async fn init_network(opts: Opts) -> Result<NetworkState> {
    let client = create_client().await?;

    delete_exising_wifi_connect_ap_profile(&client, &opts.ssid).await?;

    let device = find_device(&client, opts.interface.as_deref())?;

    let interface = get_wifi_device_interface(&device);

    println!("Interface: {}", interface);

    scan_wifi(&device).await?;

    let stations = get_nearby_stations(&device);

    let portal_connection = Some(
        create_portal(&client, &device, &opts)
            .await
            .context("Failed to create captive portal")?,
    );

    println!("Network initilized");

    Ok(NetworkState::new(
        client,
        device,
        stations,
        portal_connection,
    ))
}

fn spawn(
    responder: TokioResponder,
    command_future: impl Future<Output = Result<CommandResponse>> + 'static,
) {
    let context = MainContext::ref_thread_default();
    context.spawn_local(execute_and_respond(responder, command_future));
}

async fn execute_and_respond(
    responder: TokioResponder,
    command_future: impl Future<Output = Result<CommandResponse>>,
) {
    let result = command_future.await;
    respond(responder, result);
}

fn respond(responder: TokioResponder, response: Result<CommandResponse>) {
    let _res = responder.send(response);
}

async fn check_connectivity(client: Client) -> Result<CommandResponse> {
    let connectivity = client
        .check_connectivity_future()
        .await
        .context("Failed to execute check connectivity")?;

    Ok(CommandResponse::CheckConnectivity(Connectivity::new(
        connectivity.to_string(),
    )))
}

fn list_connections(client: &Client) -> CommandResponse {
    let all_connections: Vec<_> = client
        .connections()
        .into_iter()
        .map(glib::Cast::upcast::<Connection>)
        .collect();

    let mut connections = Vec::new();

    for connection in all_connections {
        if let Some(setting_connection) = connection.setting_connection() {
            if let Some(id) = setting_connection.id() {
                if let Some(uuid) = setting_connection.uuid() {
                    connections.push(ConnectionDetails::new(id.to_string(), uuid.to_string()));
                }
            }
        }
    }

    CommandResponse::ListConnections(connections)
}

const fn list_wifi_networks(stations: Vec<Station>) -> CommandResponse {
    CommandResponse::ListWiFiNetworks(stations)
}

async fn stop(
    client: Client,
    portal_connection: Option<ActiveConnection>,
) -> Result<CommandResponse> {
    if let Some(active_connection) = portal_connection {
        stop_portal(&client, &active_connection).await?;
    }

    Ok(CommandResponse::Stop(Stop::new("ok")))
}

async fn scan_wifi(device: &DeviceWifi) -> Result<()> {
    println!("Scanning for networks...");

    let prescan = utils_get_timestamp_msec();

    device
        .request_scan_future()
        .await
        .context("Failed to request WiFi scan")?;

    for _ in 0..WIFI_SCAN_TIMEOUT_SECONDS {
        if prescan < device.last_scan() {
            break;
        }

        glib::timeout_future_seconds(1).await;
    }

    Ok(())
}

fn get_nearby_stations(device: &DeviceWifi) -> Vec<Station> {
    let mut stations = device
        .access_points()
        .iter()
        .filter_map(|ap| Station::try_from(ap).ok())
        .collect::<Vec<_>>();

    // Sort access points by signal strength first and then ssid
    stations.sort_by_key(|station| (station.quality, station.ssid.clone()));
    stations.reverse();

    // Purge access points with duplicate SSIDs
    let mut inserted = HashSet::new();
    stations.retain(|station| inserted.insert(station.ssid.clone()));

    // Purge access points without SSID (hidden)
    stations.retain(|station| !station.ssid.is_empty());

    stations
}

fn ssid_to_str(ssid: Option<&[u8]>) -> Option<&str> {
    // An access point SSID could be random bytes and not a UTF-8 encoded string
    std::str::from_utf8(ssid.as_ref()?).ok()
}

async fn create_client() -> Result<Client> {
    let client = Client::new_future()
        .await
        .context("Failed to create NetworkManager client")?;

    if !client.is_nm_running() {
        return Err(anyhow!("NetworkManager daemon is not running"));
    }

    Ok(client)
}

async fn delete_exising_wifi_connect_ap_profile(client: &Client, ssid: &str) -> Result<()> {
    let connections = client.connections();

    for connection in connections {
        let c = connection.clone().upcast::<Connection>();
        if is_access_point_connection(&c) && is_same_ssid(&c, ssid) {
            println!(
                "Deleting already created by WiFi Connect access point connection profile: {:?}",
                ssid,
            );
            connection.delete_future().await?;
        }
    }

    Ok(())
}

fn is_same_ssid(connection: &Connection, ssid: &str) -> bool {
    connection_ssid_to_string(connection).as_deref() == Some(ssid)
}

fn connection_ssid_to_string(connection: &Connection) -> Option<String> {
    ssid_to_str(connection.setting_wireless()?.ssid().as_deref()).map(str::to_owned)
}

fn is_access_point_connection(connection: &Connection) -> bool {
    is_wifi_connection(connection) && is_access_point_mode(connection)
}

fn is_access_point_mode(connection: &Connection) -> bool {
    if let Some(setting) = connection.setting_wireless() {
        if let Some(mode) = setting.mode() {
            return mode == *SETTING_WIRELESS_MODE_AP;
        }
    }

    false
}

fn is_wifi_connection(connection: &Connection) -> bool {
    if let Some(setting) = connection.setting_connection() {
        if let Some(connection_type) = setting.connection_type() {
            return connection_type == *SETTING_WIRELESS_SETTING_NAME;
        }
    }

    false
}

pub fn find_device(client: &Client, interface: Option<&str>) -> Result<DeviceWifi> {
    if let Some(iface) = interface {
        get_exact_device(client, iface)
    } else {
        find_any_wifi_device(client)
    }
}

fn get_exact_device(client: &Client, interface: &str) -> Result<DeviceWifi> {
    let device = client
        .device_by_iface(interface)
        .context(format!("Failed to find interface '{}'", interface))?;

    if device.device_type() != DeviceType::Wifi {
        bail!("Not a WiFi interface '{}'", interface);
    }

    if device.state() == DeviceState::Unmanaged {
        bail!("Interface is not managed by NetworkManager '{}'", interface);
    }

    Ok(device.downcast().expect("Cannot downcast to DeviceWifi"))
}

fn find_any_wifi_device(client: &Client) -> Result<DeviceWifi> {
    for device in client.devices() {
        if device.device_type() == DeviceType::Wifi && device.state() != DeviceState::Unmanaged {
            return Ok(device.downcast().expect("Cannot downcast to DeviceWifi"));
        }
    }

    bail!("Failed to find a managed WiFi device")
}

async fn create_portal(
    client: &Client,
    device: &DeviceWifi,
    opts: &Opts,
) -> Result<ActiveConnection> {
    let interface = get_wifi_device_interface(device);

    let connection = create_ap_connection(
        interface.as_str(),
        &opts.ssid,
        &opts.gateway,
        opts.password.as_deref(),
    )?;

    let active_connection = client
        .add_and_activate_connection_future(Some(&connection), device, None)
        .await
        .context("Failed to add and activate connection")?;

    let state = finalize_active_connection_state(&active_connection).await?;

    if state == ActiveConnectionState::Deactivated {
        if let Some(remote_connection) = active_connection.connection() {
            remote_connection
                .delete_future()
                .await
                .context("Failed to delete captive portal connection after failing to activate")?;
        }
        Err(anyhow!("Failed to activate captive portal connection"))
    } else {
        Ok(active_connection)
    }
}

async fn stop_portal(client: &Client, active_connection: &ActiveConnection) -> Result<()> {
    client
        .deactivate_connection_future(active_connection)
        .await?;

    finalize_active_connection_state(active_connection).await?;

    if let Some(remote_connection) = active_connection.connection() {
        remote_connection
            .delete_future()
            .await
            .context("Failed to delete captive portal connection profile")?;
    }

    Ok(())
}

async fn finalize_active_connection_state(
    active_connection: &ActiveConnection,
) -> Result<ActiveConnectionState> {
    println!("Monitoring connection state...");

    let (sender, receiver) = oneshot::channel::<ActiveConnectionState>();
    let sender_cell = Rc::new(RefCell::new(Some(sender)));

    let handler_id = active_connection.connect_state_changed(move |_, state_u32, _| {
        // SAFETY: conversion from u32 is guaranteed
        let state = unsafe {
            ActiveConnectionState::from_glib(
                state_u32.try_into().expect("Unknown connection state"),
            )
        };
        println!("Connection: {:?}", state);

        let exit = match state {
            ActiveConnectionState::Activated => Some(ActiveConnectionState::Activated),
            ActiveConnectionState::Deactivated => Some(ActiveConnectionState::Deactivated),
            _ => None,
        };
        if let Some(result) = exit {
            if let Some(inner_sender) = sender_cell.borrow_mut().take() {
                inner_sender.send(result).ok();
            }
        }
    });

    let state = receiver
        .await
        .context("Failed to receive active connection state change")?;

    glib::signal_handler_disconnect(active_connection, handler_id);

    Ok(state)
}

fn create_ap_connection(
    interface: &str,
    ssid: &str,
    address: &str,
    passphrase: Option<&str>,
) -> Result<SimpleConnection> {
    let connection = SimpleConnection::new();

    let s_connection = SettingConnection::new();
    s_connection.set_type(Some(&SETTING_WIRELESS_SETTING_NAME));
    s_connection.set_id(Some(ssid));
    s_connection.set_autoconnect(false);
    s_connection.set_interface_name(Some(interface));
    connection.add_setting(&s_connection);

    let s_wireless = SettingWireless::new();
    s_wireless.set_ssid(Some(&(ssid.as_bytes().into())));
    s_wireless.set_band(Some("bg"));
    s_wireless.set_hidden(false);
    s_wireless.set_mode(Some(&SETTING_WIRELESS_MODE_AP));
    connection.add_setting(&s_wireless);

    if let Some(password) = passphrase {
        let s_wireless_security = SettingWirelessSecurity::new();
        s_wireless_security.set_key_mgmt(Some("wpa-psk"));
        s_wireless_security.set_psk(Some(password));
        connection.add_setting(&s_wireless_security);
    }

    let s_ip4 = SettingIP4Config::new();
    let ip_address =
        IPAddress::new(libc::AF_INET, address, 24).context("Failed to parse gateway address")?;
    s_ip4.add_address(&ip_address);
    s_ip4.set_method(Some(&SETTING_IP4_CONFIG_METHOD_MANUAL));
    connection.add_setting(&s_ip4);

    Ok(connection)
}

fn get_wifi_device_interface(device: &DeviceWifi) -> String {
    device
        .clone()
        .upcast::<Device>()
        .iface()
        .expect("No interface associated with device")
        .to_string()
}
