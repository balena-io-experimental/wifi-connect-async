use clap::Parser;

pub const DEFAULT_GATEWAY: &str = "192.168.42.1";
const DEFAULT_SSID: &str = "WiFiConnect";

#[derive(Parser)]
pub struct Opts {
    #[clap(short, long, default_value = DEFAULT_SSID)]
    pub ssid: String,

    #[clap(short, long)]
    pub interface: Option<String>,
}
