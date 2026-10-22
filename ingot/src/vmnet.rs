//! The node's side of copper's network. Kiln gives the node a /24 and each
//! VM a fixed address in it; this names the pieces and writes the guest's
//! network config. The host side (NAT to the internet, nothing private) is
//! `copper-net.service`, installed by `install.sh ingot`, because it needs
//! root and ingot doesn't run as root.

use std::net::Ipv4Addr;

use copper_api::{Nic, NodeNetwork};

/// Written by copper-net.service once its rules are in place.
pub const READY_FILE: &str = "/run/copper/net-ready";

/// `copper-10.42.1.0-24`: the libvirt network for a node's /24.
pub fn libvirt_name(subnet: &str) -> String {
    format!("copper-{}", subnet.replace('/', "-"))
}

/// `cu-42-1`: the host bridge, short enough for Linux's 15-character limit.
/// copper-net.service matches every `cu-*` interface.
pub fn bridge_name(net: &NodeNetwork) -> Option<String> {
    net.octets().map(|(b, n)| format!("cu-{b}-{n}"))
}

/// `10.42.1.10/24` taken apart: the host address and its /24's network.
pub fn parse(address: &str) -> Option<(Ipv4Addr, NodeNetwork)> {
    let (ip, prefix) = address.split_once('/')?;
    let ip: Ipv4Addr = ip.parse().ok()?;
    if prefix != "24" {
        return None;
    }
    let [a, b, c, _] = ip.octets();
    Some((ip, NodeNetwork { subnet: format!("{a}.{b}.{c}.0/24"), gateway: format!("{a}.{b}.{c}.1") }))
}

/// The VM's copper address, for the NIC that has one.
pub fn address_of(nics: &[Nic]) -> Option<Ipv4Addr> {
    nics.iter().find_map(|n| parse(n.address.as_deref()?)).map(|(ip, _)| ip)
}

/// cloud-init network config (v2) giving the guest its fixed address, with
/// the node as router and DNS server. `None` for NICs without an address.
pub fn network_config(nics: &[Nic]) -> Option<String> {
    let nic = nics.iter().find(|n| n.address.is_some())?;
    let address = nic.address.as_deref()?;
    let (_, net) = parse(address)?;
    Some(format!(
        "version: 2\nethernets:\n  copper:\n    match:\n      macaddress: {mac}\n    addresses: [{address}]\n    routes:\n      - to: default\n        via: {gw}\n    nameservers:\n      addresses: [{gw}]\n",
        mac = serde_json::to_string(&nic.mac).unwrap(),
        address = serde_json::to_string(address).unwrap(),
        gw = net.gateway,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nic(address: Option<&str>) -> Nic {
        Nic { bridge: String::new(), mac: "52:54:00:12:34:56".into(), address: address.map(str::to_string) }
    }

    #[test]
    fn names_follow_the_subnet() {
        let net = NodeNetwork { subnet: "10.42.7.0/24".into(), gateway: "10.42.7.1".into() };
        assert_eq!(libvirt_name(&net.subnet), "copper-10.42.7.0-24");
        assert_eq!(bridge_name(&net).as_deref(), Some("cu-42-7"));
        assert!(bridge_name(&NodeNetwork { subnet: "10.255.254.0/24".into(), gateway: String::new() }).unwrap().len() <= 15);
    }

    #[test]
    fn addresses_parse_only_as_slash_24s() {
        let (ip, net) = parse("10.42.1.10/24").unwrap();
        assert_eq!((ip, net.gateway.as_str(), net.subnet.as_str()), (Ipv4Addr::new(10, 42, 1, 10), "10.42.1.1", "10.42.1.0/24"));
        assert!(parse("10.42.1.10").is_none());
        assert!(parse("10.42.1.10/16").is_none());
        assert_eq!(address_of(&[nic(None), nic(Some("10.42.1.11/24"))]), Some(Ipv4Addr::new(10, 42, 1, 11)));
    }

    #[test]
    fn guests_get_their_address_router_and_dns() {
        let config = network_config(&[nic(Some("10.42.1.10/24"))]).unwrap();
        assert!(config.contains("macaddress: \"52:54:00:12:34:56\""));
        assert!(config.contains("addresses: [\"10.42.1.10/24\"]"));
        assert!(config.contains("via: 10.42.1.1"));
        assert!(config.contains("addresses: [10.42.1.1]"));
        assert!(network_config(&[nic(None)]).is_none(), "bridged VMs keep the router's DHCP");
    }
}
