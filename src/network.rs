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

use nm::*;

const WIFI_SCAN_TIMEOUT_SECONDS: usize = 45;

type TokioResponder = oneshot::Sender<Result<NetworkResponse>>;

#[derive(Debug)]
pub enum NetworkCommand {
    CheckConnectivity,
    ListConnections,
    ListWiFiNetworks,
}

pub struct NetworkRequest {
    responder: TokioResponder,
    command: NetworkCommand,
}

impl NetworkRequest {
    pub fn new(responder: TokioResponder, command: NetworkCommand) -> Self {
        NetworkRequest { responder, command }
    }
}

pub enum NetworkResponse {
    CheckConnectivity(Connectivity),
    ListConnections(ConnectionList),
    ListWiFiNetworks(NetworkList),
}

#[derive(Serialize)]
pub struct Connectivity {
    pub connectivity: String,
}

impl Connectivity {
    fn new(connectivity: String) -> Self {
        Connectivity { connectivity }
    }
}

#[derive(Serialize)]
pub struct ConnectionList {
    pub connections: Vec<ConnectionDetails>,
}

impl ConnectionList {
    fn new(connections: Vec<ConnectionDetails>) -> Self {
        ConnectionList { connections }
    }
}

#[derive(Serialize)]
pub struct ConnectionDetails {
    pub id: String,
    pub uuid: String,
}

impl ConnectionDetails {
    fn new(id: String, uuid: String) -> Self {
        ConnectionDetails { id, uuid }
    }
}

#[derive(Serialize)]
pub struct NetworkList {
    pub networks: Vec<NetworkDetails>,
}

impl NetworkList {
    fn new(networks: Vec<NetworkDetails>) -> Self {
        NetworkList { networks }
    }
}

#[derive(Serialize)]
pub struct NetworkDetails {
    pub ssid: String,
    pub strength: u8,
}

impl NetworkDetails {
    fn new(ssid: String, strength: u8) -> Self {
        NetworkDetails { ssid, strength }
    }
}

pub fn create_channel() -> (glib::Sender<NetworkRequest>, glib::Receiver<NetworkRequest>) {
    MainContext::channel(glib::PRIORITY_DEFAULT)
}

pub fn run_network_manager_loop(
    opts: Opts,
    initialized_sender: oneshot::Sender<Result<()>>,
    glib_receiver: glib::Receiver<NetworkRequest>,
) {
    let context = MainContext::new();
    let loop_ = MainLoop::new(Some(&context), false);

    context
        .with_thread_default(|| {
            glib_receiver.attach(None, dispatch_command_requests);

            context.spawn_local(init_network_respond(opts, initialized_sender));

            loop_.run();
        })
        .unwrap();
}

async fn init_network_respond(opts: Opts, initialized_sender: oneshot::Sender<Result<()>>) {
    let init_result = init_network(opts).await;

    let _ = initialized_sender.send(init_result);
}

async fn init_network(opts: Opts) -> Result<()> {
    let client = create_client().await?;

    delete_exising_wifi_connect_ap_profile(&client, &opts.ssid).await?;

    let device = find_device(&client, &opts.interface)?;

    println!("Device: {:?}", device);

    scan_wifi(&device).await?;

    let access_points = get_nearby_access_points(&device);

    let _networks = access_points
        .iter()
        .map(|ap| NetworkDetails::new(ssid_to_string(ap.ssid()).unwrap(), ap.strength()))
        .collect::<Vec<_>>();

    let _portal_connection = Some(create_portal(&client, &device, &opts).await?);

    Ok(())
}

fn dispatch_command_requests(command_request: NetworkRequest) -> glib::Continue {
    let NetworkRequest { responder, command } = command_request;
    match command {
        NetworkCommand::CheckConnectivity => spawn(check_connectivity(), responder),
        NetworkCommand::ListConnections => spawn(list_connections(), responder),
        NetworkCommand::ListWiFiNetworks => spawn(list_wifi_networks(), responder),
    };
    glib::Continue(true)
}

fn spawn(
    command_future: impl Future<Output = Result<NetworkResponse>> + 'static,
    responder: TokioResponder,
) {
    let context = MainContext::ref_thread_default();
    context.spawn_local(execute_and_respond(command_future, responder));
}

async fn execute_and_respond(
    command_future: impl Future<Output = Result<NetworkResponse>> + 'static,
    responder: TokioResponder,
) {
    let result = command_future.await;
    let _ = responder.send(result);
}

async fn check_connectivity() -> Result<NetworkResponse> {
    let client = create_client().await?;

    let connectivity = client
        .check_connectivity_async_future()
        .await
        .context("Failed to execute check connectivity")?;

    Ok(NetworkResponse::CheckConnectivity(Connectivity::new(
        connectivity.to_string(),
    )))
}

async fn list_connections() -> Result<NetworkResponse> {
    let client = create_client().await?;

    let all_connections: Vec<_> = client
        .connections()
        .into_iter()
        .map(|c| c.upcast::<Connection>())
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

    Ok(NetworkResponse::ListConnections(ConnectionList::new(
        connections,
    )))
}

async fn list_wifi_networks() -> Result<NetworkResponse> {
    let client = create_client().await?;

    let device = find_any_wifi_device(&client)?;

    scan_wifi(&device).await?;

    let access_points = get_nearby_access_points(&device);

    let networks = access_points
        .iter()
        .map(|ap| NetworkDetails::new(ssid_to_string(ap.ssid()).unwrap(), ap.strength()))
        .collect::<Vec<_>>();

    Ok(NetworkResponse::ListWiFiNetworks(NetworkList::new(
        networks,
    )))
}

async fn scan_wifi(device: &DeviceWifi) -> Result<()> {
    let prescan = utils_get_timestamp_msec();

    device
        .request_scan_async_future()
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

fn get_nearby_access_points(device: &DeviceWifi) -> Vec<AccessPoint> {
    let mut access_points = device.access_points();

    // Purge non-string SSIDs
    access_points.retain(|ap| ssid_to_string(ap.ssid()).is_some());

    // Purge access points with duplicate SSIDs
    let mut inserted = HashSet::new();
    access_points.retain(|ap| inserted.insert(ssid_to_string(ap.ssid()).unwrap()));

    // Purge access points without SSID (hidden)
    access_points.retain(|ap| !ssid_to_string(ap.ssid()).unwrap().is_empty());

    // Sort access points by signal strength first and then ssid
    access_points.sort_by_key(|ap| (ap.strength(), ssid_to_string(ap.ssid()).unwrap()));
    access_points.reverse();

    access_points
}

fn ssid_to_string(ssid: Option<glib::Bytes>) -> Option<String> {
    // An access point SSID could be random bytes and not a UTF-8 encoded string
    std::str::from_utf8(&ssid?).ok().map(str::to_owned)
}

async fn create_client() -> Result<Client> {
    let client = Client::new_async_future()
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
        let c = connection.clone().upcast::<nm::Connection>();
        if is_access_point_connection(&c) && is_same_ssid(&c, ssid) {
            println!(
                "Deleting already created by WiFi Connect access point connection profile: {:?}",
                ssid,
            );
            connection.delete_async_future().await?;
        }
    }

    Ok(())
}

fn is_same_ssid(connection: &nm::Connection, ssid: &str) -> bool {
    connection_ssid_as_str(connection) == Some(ssid.to_string())
}

fn connection_ssid_as_str(connection: &nm::Connection) -> Option<String> {
    ssid_to_string(connection.setting_wireless()?.ssid())
}

fn is_access_point_connection(connection: &nm::Connection) -> bool {
    is_wifi_connection(connection) && is_access_point_mode(connection)
}

fn is_access_point_mode(connection: &nm::Connection) -> bool {
    if let Some(setting) = connection.setting_wireless() {
        if let Some(mode) = setting.mode() {
            return mode == *nm::SETTING_WIRELESS_MODE_AP;
        }
    }

    false
}

fn is_wifi_connection(connection: &nm::Connection) -> bool {
    if let Some(setting) = connection.setting_connection() {
        if let Some(connection_type) = setting.connection_type() {
            return connection_type == *nm::SETTING_WIRELESS_SETTING_NAME;
        }
    }

    false
}

pub fn find_device(client: &Client, interface: &Option<String>) -> Result<DeviceWifi> {
    if let Some(ref interface) = *interface {
        get_exact_device(client, interface)
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

    Ok(device.downcast().unwrap())
}

fn find_any_wifi_device(client: &Client) -> Result<DeviceWifi> {
    for device in client.devices() {
        if device.device_type() == DeviceType::Wifi && device.state() != DeviceState::Unmanaged {
            return Ok(device.downcast().unwrap());
        }
    }

    bail!("Failed to find a managed WiFi device")
}

async fn create_portal(
    client: &Client,
    device: &DeviceWifi,
    opts: &Opts,
) -> Result<nm::ActiveConnection> {
    let interface = device.clone().upcast::<Device>().iface().unwrap();

    let connection = create_ap_connection(
        interface.as_str(),
        &opts.ssid,
        crate::opts::DEFAULT_GATEWAY,
        &opts.password.as_ref().map(|p| p as &str),
    )?;

    let active_connection = client
        .add_and_activate_connection_async_future(Some(&connection), device, None)
        .await
        .context("Failed to add and activate connection")?;

    let (sender, receiver) = oneshot::channel::<Result<()>>();
    let sender = Rc::new(RefCell::new(Some(sender)));

    active_connection.connect_state_changed(move |active_connection, state, _| {
        let sender = sender.clone();
        let active_connection = active_connection.clone();
        spawn_local(async move {
            let state = unsafe { nm::ActiveConnectionState::from_glib(state as _) };
            println!("Active connection state: {:?}", state);

            let exit = match state {
                nm::ActiveConnectionState::Activated => {
                    println!("Successfully activated");
                    Some(Ok(()))
                }
                nm::ActiveConnectionState::Deactivated => {
                    println!("Connection deactivated");
                    if let Some(remote_connection) = active_connection.connection() {
                        Some(
                            remote_connection
                                .delete_async_future()
                                .await
                                .context("Failed to delete captive portal connection"),
                        )
                    } else {
                        Some(Err(anyhow!(
                            "Failed to get remote connection from active connection"
                        )))
                    }
                }
                _ => None,
            };
            if let Some(result) = exit {
                let sender = sender.borrow_mut().take();
                if let Some(sender) = sender {
                    let _ = sender.send(result);
                }
            }
        });
    });

    if let Err(err) = receiver.await? {
        Err(err)
    } else {
        Ok(active_connection)
    }
}

fn create_ap_connection(
    interface: &str,
    ssid: &str,
    address: &str,
    passphrase: &Option<&str>,
) -> Result<nm::SimpleConnection> {
    let connection = nm::SimpleConnection::new();

    let s_connection = nm::SettingConnection::new();
    s_connection.set_type(Some(&nm::SETTING_WIRELESS_SETTING_NAME));
    s_connection.set_id(Some(ssid));
    s_connection.set_autoconnect(false);
    s_connection.set_interface_name(Some(interface));
    connection.add_setting(&s_connection);

    let s_wireless = nm::SettingWireless::new();
    s_wireless.set_ssid(Some(&(ssid.as_bytes().into())));
    s_wireless.set_band(Some("bg"));
    s_wireless.set_hidden(false);
    s_wireless.set_mode(Some(&nm::SETTING_WIRELESS_MODE_AP));
    connection.add_setting(&s_wireless);

    if let Some(password) = passphrase {
        let s_wireless_security = nm::SettingWirelessSecurity::new();
        s_wireless_security.set_key_mgmt(Some("wpa-psk"));
        s_wireless_security.set_psk(Some(password));
        connection.add_setting(&s_wireless_security);
    }

    let s_ip4 = nm::SettingIP4Config::new();
    let address = nm::IPAddress::new(libc::AF_INET, address, 24).unwrap(); //context("Failed to parse address")?;
    s_ip4.add_address(&address);
    s_ip4.set_method(Some(&nm::SETTING_IP4_CONFIG_METHOD_MANUAL));
    connection.add_setting(&s_ip4);

    Ok(connection)
}

pub fn spawn_local<F: Future<Output = ()> + 'static>(f: F) {
    glib::MainContext::ref_thread_default().spawn_local(f);
}
