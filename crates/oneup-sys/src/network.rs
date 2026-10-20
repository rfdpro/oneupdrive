//! Which network this PC is on, as Windows knows it, and its changes.
//!
//! A network is told apart by the ID Windows' Network List Manager gives
//! it (`NetworkItem.NetworkId`, the same GUID behind the network profiles
//! in Settings), not by a name, which two networks may share. Changes come
//! as Windows' own `NetworkStatusChanged` event: switching networks, a
//! connection becoming metered, the internet going or coming back. No
//! polling.
//!
//! WinRT through the `windows` crate's safe wrappers: no `unsafe` here.

use windows::Networking::Connectivity::{
    ConnectionProfile, NetworkConnectivityLevel, NetworkCostType, NetworkInformation, NetworkStatusChangedEventHandler,
};

/// How far the connection reaches.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Connectivity {
    /// Not connected.
    None,
    /// Connected to a network without the internet.
    LocalOnly,
    /// The internet only after signing in to the network (a hotel's or a
    /// train's captive portal), or otherwise limited.
    Constrained,
    Internet,
    /// Windows couldn't tell: taken as the internet, so nothing stops for
    /// want of an answer.
    Unknown,
}

impl Connectivity {
    /// Whether OneDrive can be reached, as far as Windows can tell.
    pub fn reaches_internet(self) -> bool {
        matches!(self, Connectivity::Internet | Connectivity::Unknown)
    }

    pub fn name(self) -> &'static str {
        match self {
            Connectivity::None => "none",
            Connectivity::LocalOnly => "local",
            Connectivity::Constrained => "constrained",
            Connectivity::Internet => "internet",
            Connectivity::Unknown => "unknown",
        }
    }
}

/// The network the internet connection goes through.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Network {
    /// Windows' ID for the network (`{GUID}`); `None` when not connected
    /// or not told.
    pub id: Option<String>,
    /// Its name as Windows shows it (a Wi-Fi name, "Network 2").
    pub name: String,
    /// A cost per byte or a data limit, roaming, or over the limit.
    pub metered: bool,
    pub connectivity: Connectivity,
}

impl Network {
    fn none() -> Self {
        Network { id: None, name: String::new(), metered: false, connectivity: Connectivity::None }
    }
}

fn level(p: &ConnectionProfile) -> Connectivity {
    match p.GetNetworkConnectivityLevel() {
        Ok(NetworkConnectivityLevel::InternetAccess) => Connectivity::Internet,
        Ok(NetworkConnectivityLevel::ConstrainedInternetAccess) => Connectivity::Constrained,
        Ok(NetworkConnectivityLevel::LocalAccess) => Connectivity::LocalOnly,
        Ok(_) => Connectivity::None,
        Err(_) => Connectivity::Unknown,
    }
}

fn metered(p: &ConnectionProfile) -> bool {
    let check = || -> windows::core::Result<bool> {
        let cost = p.GetConnectionCost()?;
        let kind = cost.NetworkCostType()?;
        Ok(kind == NetworkCostType::Fixed
            || kind == NetworkCostType::Variable
            || cost.Roaming()?
            || cost.OverDataLimit()?)
    };
    check().unwrap_or(false)
}

/// `{XXXXXXXX-XXXX-XXXX-XXXX-XXXXXXXXXXXX}`, as Windows writes network IDs.
pub fn guid_text(g: &windows::core::GUID) -> String {
    let d = g.data4;
    format!(
        "{{{:08X}-{:04X}-{:04X}-{:02X}{:02X}-{:02X}{:02X}{:02X}{:02X}{:02X}{:02X}}}",
        g.data1, g.data2, g.data3, d[0], d[1], d[2], d[3], d[4], d[5], d[6], d[7]
    )
}

fn describe(p: &ConnectionProfile) -> Network {
    let id = p.NetworkAdapter().and_then(|a| a.NetworkItem()).and_then(|i| i.NetworkId()).ok().map(|g| guid_text(&g));
    Network {
        id,
        name: p.ProfileName().map(|h| h.to_string()).unwrap_or_default(),
        metered: metered(p),
        connectivity: level(p),
    }
}

/// The network now. Without an internet connection, the connected network
/// that reaches furthest (a captive portal's shows as
/// [`Connectivity::Constrained`]), or [`Connectivity::None`].
pub fn current() -> Network {
    if let Ok(p) = NetworkInformation::GetInternetConnectionProfile() {
        return describe(&p);
    }
    let Ok(all) = NetworkInformation::GetConnectionProfiles() else { return Network::none() };
    let rank = |c: Connectivity| match c {
        Connectivity::Internet => 4,
        Connectivity::Constrained => 3,
        Connectivity::LocalOnly => 2,
        Connectivity::Unknown => 1,
        Connectivity::None => 0,
    };
    let mut best = Network::none();
    for i in 0..all.Size().unwrap_or(0) {
        if let Ok(p) = all.GetAt(i) {
            let n = describe(&p);
            if rank(n.connectivity) > rank(best.connectivity) {
                best = n;
            }
        }
    }
    best
}

/// Windows' network-change event, registered while this lives.
pub struct Watch {
    token: i64,
}

impl Drop for Watch {
    fn drop(&mut self) {
        let _ = NetworkInformation::RemoveNetworkStatusChanged(self.token);
    }
}

/// Calls `changed` (on a Windows thread-pool thread) whenever the network
/// changes: another network, its cost, its connectivity. Keep it short:
/// pinging a channel or a `Notify`.
pub fn watch(changed: impl Fn() + Send + 'static) -> crate::Result<Watch> {
    let handler = NetworkStatusChangedEventHandler::new(move |_| {
        changed();
        Ok(())
    });
    let token = NetworkInformation::NetworkStatusChanged(&handler)
        .map_err(|source| crate::Error::Win32 { op: "NetworkStatusChanged", source })?;
    Ok(Watch { token })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn network_ids_read_as_windows_writes_them() {
        let g = windows::core::GUID::from_u128(0x0123_4567_89ab_cdef_0011_2233_4455_6677);
        assert_eq!(guid_text(&g), "{01234567-89AB-CDEF-0011-223344556677}");
    }

    /// Reads this PC's network and registers for its changes; asserts only
    /// what holds on any machine (offline, too).
    #[test]
    fn the_network_is_read_and_watched() {
        let n = current();
        if let Some(id) = &n.id {
            assert!(id.starts_with('{') && id.ends_with('}') && id.len() == 38, "{id}");
        }
        let w = watch(|| {}).expect("register for network changes");
        drop(w);
    }
}
