//! libvirt backend. Every copper VM is a persistent domain named
//! `copper-<instance id>`; anything else on the host is invisible to us.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Mutex;
use std::time::Instant;

use anyhow::{anyhow, Context};
use copper_api::*;
use virt::connect::Connect;
use virt::domain::Domain;
use virt::network::Network;
use virt::sys;

use crate::disk::Disks;
use crate::vmnet;
use crate::host::HostUsage;
use crate::hv::{name_of, DomState, Hypervisor};

pub struct Libvirt {
    conn: Connect,
    pub disks: Disks,
    host: HostUsage,
    /// The last CPU time reading per VM, to turn the counter into a rate.
    cpu_seen: Mutex<HashMap<InstanceId, (u64, Instant)>>,
}

fn err(e: virt::error::Error) -> anyhow::Error {
    anyhow!("libvirt: {}", e.message())
}

impl Libvirt {
    pub fn connect(uri: &str, disks: Disks) -> anyhow::Result<Self> {
        // libvirt prints every error to stderr by default, including the
        // expected "domain not found" from our lookups. We report errors ourselves.
        virt::error::clear_error_callback();
        let conn = Connect::open(Some(uri)).map_err(err).with_context(|| format!("connecting to {uri}"))?;
        let host = HostUsage::new(disks.dir.clone());
        Ok(Self { conn, disks, host, cpu_seen: Mutex::default() })
    }

    /// CPU as a share of the VM's own vCPUs, memory as the guest sees it
    /// (balloon stats; the QEMU process's resident size before the guest
    /// agent reports), disk as blocks the overlay really holds.
    fn usage(&self, id: InstanceId) -> Option<Usage> {
        let d = self.lookup(id).ok()??;
        let info = d.get_info().ok()?;
        let now = Instant::now();
        let cpu_permille = {
            let mut seen = self.cpu_seen.lock().unwrap();
            let rate = match seen.get(&id) {
                Some((t0, at)) if info.cpu_time >= *t0 && info.nr_virt_cpu > 0 => {
                    let wall = now.duration_since(*at).as_nanos() as u64 * info.nr_virt_cpu as u64;
                    if wall == 0 { 0 } else { ((info.cpu_time - t0) * 1000 / wall).min(1000) as u32 }
                }
                _ => 0,
            };
            seen.insert(id, (info.cpu_time, now));
            rate
        };
        let total = info.max_mem * 1024;
        let stats: HashMap<u32, u64> = d.memory_stats(0).map(|s| s.into_iter().map(|m| (m.tag, m.val * 1024)).collect()).unwrap_or_default();
        let used = match (stats.get(&(sys::VIR_DOMAIN_MEMORY_STAT_AVAILABLE as u32)), stats.get(&(sys::VIR_DOMAIN_MEMORY_STAT_USABLE as u32))) {
            (Some(available), Some(usable)) => available.saturating_sub(*usable),
            _ => stats.get(&(sys::VIR_DOMAIN_MEMORY_STAT_RSS as u32)).copied().unwrap_or(0).min(total),
        };
        let disk = d.get_block_info("vda", 0).ok();
        Some(Usage {
            cpu_permille,
            memory_used_bytes: used,
            memory_total_bytes: total,
            disk_used_bytes: disk.as_ref().map_or(0, |b| b.allocation),
            disk_total_bytes: disk.as_ref().map_or(0, |b| b.capacity),
        })
    }

    /// Host CPU threads and memory as libvirt sees them.
    pub fn node_capacity(&self) -> anyhow::Result<(u32, u64)> {
        let info = self.conn.get_node_info().map_err(err)?;
        Ok((info.cpus, info.memory / 1024))
    }

    fn lookup(&self, id: InstanceId) -> anyhow::Result<Option<Domain>> {
        match Domain::lookup_by_name(&self.conn, &name_of(id)) {
            Ok(domain) => Ok(Some(domain)),
            Err(e) if e.code() == virt::error::ErrorNumber::NoDomain => Ok(None),
            Err(e) => Err(err(e)),
        }
    }

    pub fn state(&self, id: InstanceId) -> anyhow::Result<Option<DomState>> {
        let Some(d) = self.lookup(id)? else { return Ok(None) };
        let (s, _) = d.get_state().map_err(err)?;
        Ok(Some(match s {
            sys::VIR_DOMAIN_RUNNING | sys::VIR_DOMAIN_BLOCKED | sys::VIR_DOMAIN_PAUSED => DomState::Running,
            sys::VIR_DOMAIN_SHUTOFF | sys::VIR_DOMAIN_CRASHED | sys::VIR_DOMAIN_NOSTATE => DomState::Off,
            _ => DomState::Other,
        }))
    }

    pub fn define(&self, id: InstanceId, spec: &InstanceSpec, disk: &Path, seed: &Path) -> anyhow::Result<()> {
        if self.lookup(id)?.is_some() {
            return Ok(());
        }
        let xml = domain_xml(id, spec, disk, seed);
        Domain::define_xml(&self.conn, &xml).map_err(err).context("defining domain")?;
        tracing::info!(instance = %id, "domain defined");
        Ok(())
    }

    pub fn start(&self, id: InstanceId) -> anyhow::Result<()> {
        let d = self.lookup(id)?.ok_or_else(|| anyhow!("domain missing"))?;
        if d.is_active().map_err(err)? {
            return Ok(());
        }
        d.create().map_err(err).context("starting domain")?;
        tracing::info!(instance = %id, "domain started");
        Ok(())
    }

    /// Ask the guest to shut down (ACPI power button).
    pub fn shutdown(&self, id: InstanceId) -> anyhow::Result<()> {
        let d = self.lookup(id)?.ok_or_else(|| anyhow!("domain missing"))?;
        if !d.is_active().map_err(err)? {
            return Ok(());
        }
        d.shutdown_flags(sys::VIR_DOMAIN_SHUTDOWN_ACPI_POWER_BTN).map_err(err).context("shutting down domain")?;
        Ok(())
    }

    /// Pull the plug.
    pub fn destroy(&self, id: InstanceId) -> anyhow::Result<()> {
        let d = self.lookup(id)?.ok_or_else(|| anyhow!("domain missing"))?;
        if !d.is_active().map_err(err)? {
            return Ok(());
        }
        d.destroy().map_err(err).context("destroying domain")?;
        tracing::info!(instance = %id, "domain destroyed");
        Ok(())
    }

    /// Redefine an inactive domain whose vCPUs or memory differ from the
    /// spec, then grow its disk. Redefining only when needed keeps libvirt's
    /// device addresses stable.
    pub fn reconfigure(&self, inst: &DesiredInstance) -> anyhow::Result<()> {
        let d = self.lookup(inst.id)?.ok_or_else(|| anyhow!("domain missing"))?;
        if d.is_active().map_err(err)? {
            anyhow::bail!("domain is running; resize applies after it stops");
        }
        let info = d.get_info().map_err(err)?;
        if info.max_mem != inst.spec.memory_mb * 1024 || info.nr_virt_cpu != inst.spec.vcpus {
            let xml = domain_xml(inst.id, &inst.spec, &self.disks.disk_path(inst.id), &self.disks.seed_path(inst.id));
            Domain::define_xml(&self.conn, &xml).map_err(err).context("redefining domain")?;
            tracing::info!(instance = %inst.id, vcpus = inst.spec.vcpus, memory_mb = inst.spec.memory_mb, "domain resized");
        }
        self.disks.grow(inst.id, inst.spec.boot_disk.size_gb)
    }

    pub fn undefine(&self, id: InstanceId) -> anyhow::Result<()> {
        self.cpu_seen.lock().unwrap().remove(&id);
        let Some(d) = self.lookup(id)? else { return Ok(()) };
        if d.is_active().map_err(err)? {
            d.destroy().map_err(err)?;
        }
        d.undefine_flags(sys::VIR_DOMAIN_UNDEFINE_NVRAM | sys::VIR_DOMAIN_UNDEFINE_MANAGED_SAVE)
            .map_err(err)
            .context("undefining domain")?;
        tracing::info!(instance = %id, "domain undefined");
        Ok(())
    }

    /// The node's copper network, defined, running and set to start at boot.
    pub fn ensure_network(&self, net: &NodeNetwork) -> anyhow::Result<()> {
        let name = vmnet::libvirt_name(&net.subnet);
        let network = match Network::lookup_by_name(&self.conn, &name) {
            Ok(n) => n,
            Err(e) if e.code() == virt::error::ErrorNumber::NoNetwork => {
                let n = Network::define_xml(&self.conn, &network_xml(net)?).map_err(err).context("defining the copper network")?;
                tracing::info!(network = %name, subnet = %net.subnet, "copper network defined");
                n
            }
            Err(e) => return Err(err(e)),
        };
        if !network.is_active().map_err(err)? {
            network.create().map_err(err).context("starting the copper network")?;
            tracing::info!(network = %name, "copper network started");
        }
        if !network.get_autostart().map_err(err)? {
            network.set_autostart(true).map_err(err)?;
        }
        Ok(())
    }

    /// IPv4 addresses of the domain's NICs. The guest agent is authoritative
    /// once cloud-init has installed it; until then the host ARP table is the
    /// fallback, which only knows a guest the host has exchanged traffic with
    /// (bridged VMs don't get libvirt DHCP leases).
    pub fn addresses(&self, id: InstanceId) -> Vec<String> {
        let Ok(Some(d)) = self.lookup(id) else { return vec![] };
        for source in [sys::VIR_DOMAIN_INTERFACE_ADDRESSES_SRC_AGENT, sys::VIR_DOMAIN_INTERFACE_ADDRESSES_SRC_ARP] {
            let Ok(ifaces) = d.interface_addresses(source, 0) else { continue };
            let mut out: Vec<String> = ifaces
                .iter()
                .filter(|i| i.name != "lo")
                .flat_map(|i| i.addrs.iter())
                .filter(|a| a.typed == sys::VIR_IP_ADDR_TYPE_IPV4 as i64 && !a.addr.starts_with("127."))
                .map(|a| a.addr.clone())
                .collect();
            out.sort();
            out.dedup();
            if !out.is_empty() {
                return out;
            }
        }
        vec![]
    }
}

fn domain_xml(id: InstanceId, spec: &InstanceSpec, disk: &Path, seed: &Path) -> String {
    let arch = match spec.arch {
        Arch::X86_64 => "x86_64",
        Arch::Aarch64 => "aarch64",
    };
    let machine = match spec.arch {
        Arch::X86_64 => "q35",
        Arch::Aarch64 => "virt",
    };
    let nics: String = spec.nics.iter().map(nic_xml).collect();
    format!(
        r#"<domain type='kvm'>
  <name>{name}</name>
  <uuid>{id}</uuid>
  <metadata>
    <copper:instance xmlns:copper="https://github.com/Shreyas220/copper" id="{id}"/>
  </metadata>
  <memory unit='MiB'>{mem}</memory>
  <vcpu>{vcpus}</vcpu>
  <os>
    <type arch='{arch}' machine='{machine}'>hvm</type>
    <boot dev='hd'/>
  </os>
  <features><acpi/><apic/></features>
  <cpu mode='host-passthrough'/>
  <clock offset='utc'/>
  <on_poweroff>destroy</on_poweroff>
  <on_reboot>restart</on_reboot>
  <on_crash>destroy</on_crash>
  <devices>
    <disk type='file' device='disk'>
      <driver name='qemu' type='qcow2' discard='unmap'/>
      <source file='{disk}'/>
      <target dev='vda' bus='virtio'/>
    </disk>
    <disk type='file' device='cdrom'>
      <driver name='qemu' type='raw'/>
      <source file='{seed}'/>
      <target dev='sda' bus='sata'/>
      <readonly/>
    </disk>
{nics}    <serial type='pty'><target port='0'/></serial>
    <console type='pty'><target type='serial' port='0'/></console>
    <channel type='unix'>
      <target type='virtio' name='org.qemu.guest_agent.0'/>
    </channel>
    <rng model='virtio'><backend model='random'>/dev/urandom</backend></rng>
    <memballoon model='virtio'><stats period='10'/></memballoon>
  </devices>
</domain>
"#,
        name = name_of(id),
        mem = spec.memory_mb,
        vcpus = spec.vcpus,
        disk = xml_escape(&disk.to_string_lossy()),
        seed = xml_escape(&seed.to_string_lossy()),
    )
}

/// On the node's copper network, with libvirt's own clean-traffic filter so a
/// VM can only send as its own MAC and address. Older VMs stay on their bridge.
fn nic_xml(n: &Nic) -> String {
    match n.address.as_deref().and_then(vmnet::parse) {
        Some((ip, net)) => format!(
            "    <interface type='network'>\n      <source network='{}'/>\n      <mac address='{}'/>\n      <model type='virtio'/>\n      \
             <filterref filter='clean-traffic'>\n        <parameter name='IP' value='{ip}'/>\n      </filterref>\n    </interface>\n",
            xml_escape(&vmnet::libvirt_name(&net.subnet)), xml_escape(&n.mac)
        ),
        None => format!(
            "    <interface type='bridge'>\n      <source bridge='{}'/>\n      <mac address='{}'/>\n      <model type='virtio'/>\n    </interface>\n",
            xml_escape(&n.bridge), xml_escape(&n.mac)
        ),
    }
}

/// `open` mode: libvirt runs DNS on the gateway and adds no firewall rules.
/// copper-net.service owns those.
fn network_xml(net: &NodeNetwork) -> anyhow::Result<String> {
    let bridge = vmnet::bridge_name(net).ok_or_else(|| anyhow!("bad subnet {}", net.subnet))?;
    Ok(format!(
        "<network>\n  <name>{}</name>\n  <forward mode='open'/>\n  <bridge name='{}' stp='off' delay='0'/>\n  <ip address='{}' netmask='255.255.255.0'/>\n</network>\n",
        xml_escape(&vmnet::libvirt_name(&net.subnet)), xml_escape(&bridge), xml_escape(&net.gateway)
    ))
}

fn xml_escape(value: &str) -> String {
    value.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
        .replace('\"', "&quot;").replace('\'', "&apos;")
}

impl Hypervisor for Libvirt {
    fn node_capacity(&self) -> anyhow::Result<(u32, u64)> { self.node_capacity() }
    fn allocatable_gb(&self, ids: &[InstanceId]) -> u64 { self.disks.allocatable_gb(ids.iter().copied()) }
    fn prepare(&self, inst: &DesiredInstance) -> anyhow::Result<()> {
        self.disks.ensure_disk(inst.id, &inst.spec)?;
        self.disks.ensure_seed(inst.id, &inst.name, &inst.spec)?;
        Ok(())
    }
    fn has_storage(&self, id: InstanceId) -> bool { self.disks.disk_exists(id) }
    fn remove_storage(&self, id: InstanceId) -> anyhow::Result<()> { self.disks.remove(id) }
    fn define(&self, inst: &DesiredInstance) -> anyhow::Result<()> {
        self.define(inst.id, &inst.spec, &self.disks.disk_path(inst.id), &self.disks.seed_path(inst.id))
    }
    fn start(&self, id: InstanceId) -> anyhow::Result<()> { self.start(id) }
    fn shutdown(&self, id: InstanceId) -> anyhow::Result<()> { self.shutdown(id) }
    fn destroy(&self, id: InstanceId) -> anyhow::Result<()> { self.destroy(id) }
    fn undefine(&self, id: InstanceId) -> anyhow::Result<()> { self.undefine(id) }
    fn reconfigure(&self, inst: &DesiredInstance) -> anyhow::Result<()> { self.reconfigure(inst) }
    fn state(&self, id: InstanceId) -> anyhow::Result<Option<DomState>> { self.state(id) }
    fn addresses(&self, id: InstanceId) -> Vec<String> { self.addresses(id) }
    fn copper_network(&self) -> bool { true }
    fn ensure_network(&self, net: &NodeNetwork) -> anyhow::Result<()> { self.ensure_network(net) }
    fn network_ready(&self) -> Option<bool> { Some(Path::new(vmnet::READY_FILE).exists()) }
    fn usage(&self, id: InstanceId) -> Option<Usage> { self.usage(id) }
    fn host_usage(&self) -> Option<Usage> { self.host.read() }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nic(address: Option<&str>, bridge: &str) -> Nic {
        Nic { bridge: bridge.into(), mac: "52:54:00:12:34:56".into(), address: address.map(str::to_string) }
    }

    #[test]
    fn vms_join_their_nodes_copper_network_and_only_as_themselves() {
        let x = nic_xml(&nic(Some("10.42.1.10/24"), ""));
        assert!(x.contains("<interface type='network'>"));
        assert!(x.contains("<source network='copper-10.42.1.0-24'/>"));
        assert!(x.contains("<filterref filter='clean-traffic'>"));
        assert!(x.contains("<parameter name='IP' value='10.42.1.10'/>"));
        let older = nic_xml(&nic(None, "br0"));
        assert!(older.contains("<source bridge='br0'/>") && !older.contains("filterref"), "bridged VMs stay as they were");
    }

    #[test]
    fn the_network_adds_no_firewall_rules_of_its_own() {
        let x = network_xml(&NodeNetwork { subnet: "10.42.1.0/24".into(), gateway: "10.42.1.1".into() }).unwrap();
        assert!(x.contains("<name>copper-10.42.1.0-24</name>"));
        assert!(x.contains("<forward mode='open'/>"));
        assert!(x.contains("<bridge name='cu-42-1'"));
        assert!(x.contains("<ip address='10.42.1.1' netmask='255.255.255.0'/>"));
    }
}
