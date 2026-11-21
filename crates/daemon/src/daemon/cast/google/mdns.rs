//! Finding Cast devices: an mDNS browse for `_googlecast._tcp` on each address this machine has
//! on the home network, and what each device's TXT record says about it.
//!
//! The browse uses mdns-sd, which shares port 5353 with the system's own responder (Avahi,
//! mDNSResponder, Windows), so the multicast answers a firewall lets in reach it. Asking from
//! another port, as tvpipe does, gets answers sent to that port, which a host firewall drops.

use std::collections::BTreeMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::time::Duration;

pub const SERVICE: &str = "_googlecast._tcp.local.";
/// How long a browse listens. Devices answer within a second; a lost answer is asked again.
const LISTEN: Duration = Duration::from_secs(3);
/// The `ca` bits, as pychromecast names them.
const VIDEO_OUT: u64 = 1 << 0;
const GROUP: u64 = 1 << 5;
/// The most devices a browse keeps.
const MOST: usize = 32;

/// A Cast device that answered.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Heard {
    /// The device's own ID, from its TXT record.
    pub id: String,
    /// The name its owner gave it, such as "Living Room TV".
    pub name: String,
    /// Its model, such as "Chromecast" or "Google Home Mini".
    pub model: String,
    pub at: SocketAddr,
    /// Its `ca` bits.
    pub capabilities: u64,
}

impl Heard {
    /// Reads a device's TXT record. `None` for a record without an ID or a name.
    pub fn parse(txt: &[(String, String)], address: IpAddr, port: u16) -> Option<Heard> {
        let get = |key: &str| {
            txt.iter().find(|(k, _)| k.eq_ignore_ascii_case(key)).map(|(_, v)| v.trim()).filter(|v| !v.is_empty())
        };
        Some(Heard {
            id: get("id")?.to_ascii_lowercase(),
            name: get("fn")?.to_string(),
            model: get("md").unwrap_or("Cast device").to_string(),
            at: SocketAddr::new(address, port),
            capabilities: get("ca").and_then(|ca| ca.parse().ok()).unwrap_or(0),
        })
    }

    /// Whether it shows video. A speaker such as a Google Home Mini, or a group of speakers, does
    /// not, so it is not listed.
    pub fn shows_video(&self) -> bool {
        self.capabilities & VIDEO_OUT != 0 && self.capabilities & GROUP == 0 && self.model != "Google Cast Group"
    }
}

/// Browses from `sources`, this machine's addresses on the home network, and returns the devices
/// that answered, by ID.
pub async fn browse(sources: &[Ipv4Addr]) -> Vec<Heard> {
    if sources.is_empty() {
        return Vec::new();
    }
    let sources = sources.to_vec();
    tokio::task::spawn_blocking(move || browse_blocking(&sources)).await.unwrap_or_default()
}

fn browse_blocking(sources: &[Ipv4Addr]) -> Vec<Heard> {
    use mdns_sd::{IfKind, ServiceDaemon, ServiceEvent};
    let Ok(daemon) = ServiceDaemon::new() else { return Vec::new() };
    let _ = daemon.disable_interface(IfKind::All);
    for source in sources {
        let _ = daemon.enable_interface(IfKind::Addr(IpAddr::V4(*source)));
    }
    let Ok(events) = daemon.browse(SERVICE) else {
        let _ = daemon.shutdown();
        return Vec::new();
    };
    let deadline = std::time::Instant::now() + LISTEN;
    let mut found = BTreeMap::new();
    while let Some(left) = deadline.checked_duration_since(std::time::Instant::now()) {
        let Ok(event) = events.recv_timeout(left) else { break };
        let ServiceEvent::ServiceResolved(service) = event else { continue };
        let txt: Vec<(String, String)> =
            service.txt_properties.iter().map(|p| (p.key().to_string(), p.val_str().to_string())).collect();
        // The address on the network it answered on.
        let address = service.addresses.iter().map(|ip| ip.to_ip_addr()).find(|ip| ip.is_ipv4());
        if let Some(heard) = address.and_then(|address| Heard::parse(&txt, address, service.port))
            && found.len() < MOST
        {
            found.insert(heard.id.clone(), heard);
        }
    }
    let _ = daemon.stop_browse(SERVICE);
    let _ = daemon.shutdown();
    found.into_values().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn txt(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    #[test]
    fn a_device_is_known_by_its_txt_record_and_only_screens_are_listed() {
        let address: IpAddr = "192.168.86.92".parse().unwrap();
        // As the bedroom Google TV, a Google Home Mini, and a speaker group answered.
        let tv = txt(&[
            ("id", "5580BF78572B8F4BBA82C645AFF2E322"),
            ("md", "Chromecast"),
            ("fn", "Augie's Bedroom Google TV"),
            ("ca", "464901"),
        ]);
        let mini = txt(&[("id", "8a30d2d2"), ("md", "Google Home Mini"), ("fn", "Kitchen speaker"), ("ca", "199172")]);
        let group =
            txt(&[("id", "ea887fcf-5317"), ("md", "Google Cast Group"), ("fn", "Home Speakers"), ("ca", "199204")]);
        let heard = Heard::parse(&tv, address, 8009).unwrap();
        assert_eq!(
            heard,
            Heard {
                id: "5580bf78572b8f4bba82c645aff2e322".into(),
                name: "Augie's Bedroom Google TV".into(),
                model: "Chromecast".into(),
                at: SocketAddr::new(address, 8009),
                capabilities: 464901,
            }
        );
        assert!(heard.shows_video());
        assert!(!Heard::parse(&mini, address, 8009).unwrap().shows_video());
        assert!(!Heard::parse(&group, address, 32172).unwrap().shows_video());
        assert_eq!(Heard::parse(&txt(&[("id", "x"), ("fn", " ")]), address, 8009), None, "a device needs a name");
    }
}
