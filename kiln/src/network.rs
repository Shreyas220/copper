//! Copper's network: one /16, a /24 per node, a fixed address per VM.
//!
//! VMs live here rather than on the LAN of whatever site their node is at,
//! so the same plan holds for a node on the home router and one anywhere on
//! the tailnet. The node is `.1` of its /24, the VMs' router and DNS server;
//! VMs get `.10` to `.250`, lowest free first.

use copper_api::NodeNetwork;

const FIRST_VM: u8 = 10;
const LAST_VM: u8 = 250;

/// `10.42.0.0/16`, as `kiln --vm-network` gives it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VmNet {
    a: u8,
    b: u8,
}

impl VmNet {
    pub fn parse(s: &str) -> anyhow::Result<Self> {
        let (addr, prefix) = s.split_once('/').ok_or_else(|| anyhow::anyhow!("{s}: give it as a /16, like 10.42.0.0/16"))?;
        let octets: Vec<u8> = addr.split('.').map(str::parse).collect::<Result<_, _>>().map_err(|_| anyhow::anyhow!("{s}: not an IPv4 address"))?;
        anyhow::ensure!(prefix == "16" && octets.len() == 4 && octets[2] == 0 && octets[3] == 0, "{s}: give it as a /16, like 10.42.0.0/16");
        let private = octets[0] == 10 || (octets[0] == 172 && (16..32).contains(&octets[1])) || (octets[0] == 192 && octets[1] == 168);
        anyhow::ensure!(private, "{s}: must be a private range");
        Ok(Self { a: octets[0], b: octets[1] })
    }

    /// Node `n`'s part: `10.42.n.0/24`, with the node at `.1`.
    pub fn node(&self, n: u8) -> NodeNetwork {
        NodeNetwork { subnet: format!("{}.{}.{n}.0/24", self.a, self.b), gateway: format!("{}.{}.{n}.1", self.a, self.b) }
    }

    /// The lowest address in node `n`'s /24 that none of `taken` (addresses
    /// like `10.42.1.10/24`) holds, or `None` when it's full.
    pub fn allocate<'a>(&self, n: u8, taken: impl Iterator<Item = &'a str>) -> Option<String> {
        let prefix = format!("{}.{}.{n}.", self.a, self.b);
        let used: std::collections::HashSet<u8> = taken
            .filter_map(|t| t.split('/').next()?.strip_prefix(&prefix)?.parse().ok())
            .collect();
        (FIRST_VM..=LAST_VM).find(|h| !used.contains(h)).map(|h| format!("{prefix}{h}/24"))
    }
}

impl Default for VmNet {
    fn default() -> Self {
        Self { a: 10, b: 42 }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_private_sixteens() {
        assert_eq!(VmNet::parse("10.42.0.0/16").unwrap(), VmNet::default());
        assert!(VmNet::parse("10.42.0.0/24").is_err());
        assert!(VmNet::parse("10.42.1.0/16").is_err());
        assert!(VmNet::parse("8.8.0.0/16").is_err());
        assert!(VmNet::parse("172.20.0.0/16").is_ok());
    }

    #[test]
    fn addresses_fill_from_ten_and_come_back() {
        let net = VmNet::default();
        assert_eq!(net.node(3), NodeNetwork { subnet: "10.42.3.0/24".into(), gateway: "10.42.3.1".into() });
        assert_eq!(net.allocate(1, [].into_iter()).as_deref(), Some("10.42.1.10/24"));
        let taken = ["10.42.1.10/24", "10.42.1.11/24", "10.42.2.12/24"];
        assert_eq!(net.allocate(1, taken.into_iter()).as_deref(), Some("10.42.1.12/24"), "another node's addresses don't count");
        let gap = ["10.42.1.10/24", "10.42.1.12/24"];
        assert_eq!(net.allocate(1, gap.into_iter()).as_deref(), Some("10.42.1.11/24"), "a deleted VM's address is reused");
        let full: Vec<String> = (10..=250).map(|h| format!("10.42.1.{h}/24")).collect();
        assert_eq!(net.allocate(1, full.iter().map(String::as_str)), None);
    }
}
