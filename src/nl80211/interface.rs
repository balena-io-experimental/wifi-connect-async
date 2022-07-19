#![allow(clippy::upper_case_acronyms)]

use std::convert::{TryFrom, TryInto};

use macaddr::MacAddr6;

use neli::genl::Genlmsghdr;

use crate::nl80211::consts;
use crate::nl80211::enums::{Nl80211Attr, Nl80211Cmd};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum InterfaceType {
    Unspecified = 0,
    Adhoc,
    Station,
    AP,
    APVlan,
    WDS,
    Monitor,
    MeshPoint,
    P2PClient,
    P2PGo,
    P2PDevice,
    Ocb,
    Nan,
}

impl From<::std::os::raw::c_uint> for InterfaceType {
    fn from(orig: ::std::os::raw::c_uint) -> Self {
        match orig {
            consts::NL80211_IFTYPE_UNSPECIFIED => Self::Unspecified,
            consts::NL80211_IFTYPE_ADHOC => Self::Adhoc,
            consts::NL80211_IFTYPE_STATION => Self::Station,
            consts::NL80211_IFTYPE_AP => Self::AP,
            consts::NL80211_IFTYPE_AP_VLAN => Self::APVlan,
            consts::NL80211_IFTYPE_WDS => Self::WDS,
            consts::NL80211_IFTYPE_MONITOR => Self::Monitor,
            consts::NL80211_IFTYPE_MESH_POINT => Self::MeshPoint,
            consts::NL80211_IFTYPE_P2P_CLIENT => Self::P2PClient,
            consts::NL80211_IFTYPE_P2P_GO => Self::P2PGo,
            consts::NL80211_IFTYPE_P2P_DEVICE => Self::P2PDevice,
            consts::NL80211_IFTYPE_OCB => Self::Ocb,
            consts::NL80211_IFTYPE_NAN => Self::Nan,
            _ => Self::Unspecified,
        }
    }
}

#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct Interface {
    pub name: String,
    pub index: u32,
    pub iftype: InterfaceType,
    pub wiphy: u32,
    pub wdev: u64,
    pub mac_address: MacAddr6,
}

impl TryFrom<&Genlmsghdr<Nl80211Cmd, Nl80211Attr>> for Interface {
    type Error = anyhow::Error;

    fn try_from(payload: &Genlmsghdr<Nl80211Cmd, Nl80211Attr>) -> Result<Self, Self::Error> {
        let attrs = payload.get_attr_handle();
        let name = attrs.get_attr_payload_as_with_len(Nl80211Attr::Ifname)?;
        let index = attrs.get_attr_payload_as(Nl80211Attr::Ifindex)?;
        let iftype = attrs
            .get_attr_payload_as::<u32>(Nl80211Attr::Iftype)?
            .into();
        let wiphy = attrs.get_attr_payload_as(Nl80211Attr::Wiphy)?;
        let wdev = attrs.get_attr_payload_as(Nl80211Attr::Wdev)?;
        let mac_bytes: [u8; 6] = attrs
            .get_attr_payload_as_with_len::<&[u8]>(Nl80211Attr::Mac)?
            .try_into()?;
        let mac_address = mac_bytes.into();
        Ok(Self {
            name,
            index,
            iftype,
            wiphy,
            wdev,
            mac_address,
        })
    }
}
