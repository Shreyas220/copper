//! libvirt backend. Every copper VM is a persistent domain named
//! `copper-<instance id>`; anything else on the host is invisible to us.

use std::path::Path;

use anyhow::{anyhow, Context};
use copper_api::*;
use virt::connect::Connect;
use virt::domain::Domain;
use virt::sys;

const PREFIX: &str = "copper-";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DomState {
    Running,
    /// Defined but not running (shut off, crashed, or never started).
    Off,
    /// Mid-transition, e.g. shutting down.
    Other,
}

pub struct Libvirt {
    conn: Connect,
}

fn name_of(id: InstanceId) -> String {
    format!("{PREFIX}{id}")
}

fn err(e: virt::error::Error) -> anyhow::Error {
    anyhow!("libvirt: {}", e.message())
}

impl Libvirt {
    pub fn connect(uri: &str) -> anyhow::Result<Self> {
        // libvirt prints every error to stderr by default, including the
        // expected "domain not found" from our lookups. We report errors ourselves.
        virt::error::clear_error_callback();
        let conn = Connect::open(Some(uri)).map_err(err).with_context(|| format!("connecting to {uri}"))?;
        Ok(Self { conn })
    }

    /// Host CPU threads and memory as libvirt sees them.
    pub fn node_capacity(&self) -> anyhow::Result<(u32, u64)> {
        let info = self.conn.get_node_info().map_err(err)?;
        Ok((info.cpus, info.memory / 1024))
    }

    fn lookup(&self, id: InstanceId) -> Option<Domain> {
        Domain::lookup_by_name(&self.conn, &name_of(id)).ok()
    }

    pub fn exists(&self, id: InstanceId) -> bool {
        self.lookup(id).is_some()
    }

    pub fn state(&self, id: InstanceId) -> Option<DomState> {
        let d = self.lookup(id)?;
        let (s, _) = d.get_state().ok()?;
        Some(match s {
            sys::VIR_DOMAIN_RUNNING | sys::VIR_DOMAIN_BLOCKED | sys::VIR_DOMAIN_PAUSED => DomState::Running,
            sys::VIR_DOMAIN_SHUTOFF | sys::VIR_DOMAIN_CRASHED | sys::VIR_DOMAIN_NOSTATE => DomState::Off,
            _ => DomState::Other,
        })
    }

    /// Instance ids of every copper domain on this host.
    pub fn list(&self) -> anyhow::Result<Vec<InstanceId>> {
        let doms = self.conn.list_all_domains(0).map_err(err)?;
        Ok(doms
            .iter()
            .filter_map(|d| d.get_name().ok())
            .filter_map(|n| n.strip_prefix(PREFIX).and_then(|s| s.parse().ok()))
            .collect())
    }

    pub fn define(&self, id: InstanceId, spec: &InstanceSpec, disk: &Path, seed: &Path) -> anyhow::Result<()> {
        if self.exists(id) {
            return Ok(());
        }
        let xml = domain_xml(id, spec, disk, seed);
        Domain::define_xml(&self.conn, &xml).map_err(err).context("defining domain")?;
        tracing::info!(instance = %id, "domain defined");
        Ok(())
    }

    pub fn start(&self, id: InstanceId) -> anyhow::Result<()> {
        let d = self.lookup(id).ok_or_else(|| anyhow!("domain missing"))?;
        if d.is_active().map_err(err)? {
            return Ok(());
        }
        d.create().map_err(err).context("starting domain")?;
        tracing::info!(instance = %id, "domain started");
        Ok(())
    }

    /// Ask the guest to shut down (ACPI power button).
    pub fn shutdown(&self, id: InstanceId) -> anyhow::Result<()> {
        let d = self.lookup(id).ok_or_else(|| anyhow!("domain missing"))?;
        if !d.is_active().map_err(err)? {
            return Ok(());
        }
        d.shutdown_flags(sys::VIR_DOMAIN_SHUTDOWN_ACPI_POWER_BTN).map_err(err).context("shutting down domain")?;
        Ok(())
    }

    /// Pull the plug.
    pub fn destroy(&self, id: InstanceId) -> anyhow::Result<()> {
        let d = self.lookup(id).ok_or_else(|| anyhow!("domain missing"))?;
        if !d.is_active().map_err(err)? {
            return Ok(());
        }
        d.destroy().map_err(err).context("destroying domain")?;
        tracing::info!(instance = %id, "domain destroyed");
        Ok(())
    }

    pub fn undefine(&self, id: InstanceId) -> anyhow::Result<()> {
        let Some(d) = self.lookup(id) else { return Ok(()) };
        if d.is_active().map_err(err)? {
            d.destroy().map_err(err)?;
        }
        d.undefine_flags(sys::VIR_DOMAIN_UNDEFINE_NVRAM | sys::VIR_DOMAIN_UNDEFINE_MANAGED_SAVE)
            .map_err(err)
            .context("undefining domain")?;
        tracing::info!(instance = %id, "domain undefined");
        Ok(())
    }

    /// IPv4 addresses of the domain's NICs. The guest agent is authoritative
    /// once cloud-init has installed it; until then the host ARP table is the
    /// fallback, which only knows a guest the host has exchanged traffic with
    /// (bridged VMs don't get libvirt DHCP leases).
    pub fn addresses(&self, id: InstanceId) -> Vec<String> {
        let Some(d) = self.lookup(id) else { return vec![] };
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
    let nics: String = spec
        .nics
        .iter()
        .map(|n| {
            format!(
                "    <interface type='bridge'>\n      <source bridge='{}'/>\n      <mac address='{}'/>\n      <model type='virtio'/>\n    </interface>\n",
                n.bridge, n.mac
            )
        })
        .collect();
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
    <memballoon model='virtio'/>
  </devices>
</domain>
"#,
        name = name_of(id),
        mem = spec.memory_mb,
        vcpus = spec.vcpus,
        disk = disk.display(),
        seed = seed.display(),
    )
}
