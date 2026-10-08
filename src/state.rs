//! VM state model: per-VM directory layout + meta.json.
//!
//! Layout (mirrors the v1 scripts so existing VMs are adoptable):
//!   ~/.local/share/pi-vm/
//!     assets/            cloud-hypervisor, kernel, initramfs, kernel-cmdline
//!     images/            fedora44-cloud.qcow2, fedora44-cloud-prepped.qcow2
//!     vms/<12-hex-id>/   meta.json, disk.qcow2, ssh_key, console.log, .lock
//!
//! The directory is the source of truth; there is no database. `rm -rf` of a
//! VM dir = delete VM. The .lock (flock) is the source of truth for "in use".

use std::fs;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use rand::Rng;
use serde::{Deserialize, Serialize};

use crate::util::run_ok_priv;

/// Root of all pi-vm state. Defaults to ~/.local/share/pi-vm, overridable
/// via PI_VM_HOME (testability + non-standard installs).
#[derive(Clone, Debug)]
pub struct VmHome {
    pub root: PathBuf,
}

impl VmHome {
    pub fn new() -> Self {
        let root = std::env::var("PI_VM_HOME").map(PathBuf::from).unwrap_or_else(|_| {
            let home = std::env::var("HOME").expect("HOME is set");
            PathBuf::from(home).join(".local/share/pi-vm")
        });
        Self { root }
    }

    pub fn assets_dir(&self) -> PathBuf {
        self.root.join("assets")
    }

    pub fn images_dir(&self) -> PathBuf {
        self.root.join("images")
    }

    pub fn vms_dir(&self) -> PathBuf {
        self.root.join("vms")
    }

    pub fn ch_binary(&self) -> PathBuf {
        self.assets_dir().join("cloud-hypervisor")
    }

    /// Direct-boot assets (extracted by `bake` from the pre-baked image):
    /// the guest kernel + initramfs + kernel cmdline. CH boots the VM
    /// straight from these (no UEFI, no GRUB).
    pub fn kernel(&self) -> PathBuf {
        self.assets_dir().join("kernel")
    }

    pub fn initramfs(&self) -> PathBuf {
        self.assets_dir().join("initramfs")
    }

    pub fn kernel_cmdline(&self) -> PathBuf {
        self.assets_dir().join("kernel-cmdline")
    }

    pub fn cloud_image(&self) -> PathBuf {
        self.images_dir().join("fedora44-cloud.qcow2")
    }

    pub fn prepped_image(&self) -> PathBuf {
        self.images_dir().join("fedora44-cloud-prepped.qcow2")
    }

    /// All VM ids (12-hex dir names) under vms/.
    pub fn list_vm_ids(&self) -> Vec<String> {
        let Ok(entries) = fs::read_dir(self.vms_dir()) else {
            return Vec::new();
        };
        entries
            .flatten()
            .filter_map(|e| {
                let name = e.file_name().to_string_lossy().to_string();
                if name.len() == 12 && name.chars().all(|c| c.is_ascii_hexdigit()) {
                    Some(name)
                } else {
                    None
                }
            })
            .collect()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum VmState {
    Stopped,
    Running,
}

impl std::fmt::Display for VmState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            VmState::Stopped => "stopped",
            VmState::Running => "running",
        };
        f.write_str(s)
    }
}

/// Per-VM metadata, persisted as meta.json in the VM directory.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct VmMeta {
    /// 12-hex unique id (also the directory name).
    pub id: String,
    /// Optional unique human name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// RFC3339-ish timestamps (seconds precision is enough).
    pub created: i64,
    pub updated: i64,
    /// Host path shared into the VM at /workspace (None = --no-mount VM).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    /// e.g. 172.16.55.0/24
    pub subnet: String,
    /// e.g. 172.16.55.2
    pub ip: String,
    /// e.g. 52:54:00:00:00:55
    pub mac: String,
    pub vcpus: u32,
    pub memory_mb: u32,
    pub disk_gb: u32,
    pub extra_disk: bool,
    pub kvm: bool,
    /// The uid all guest uids are squashed to (the ownership guarantee).
    pub squash_uid: u32,
    pub state: VmState,
    /// Set while a pi-vm process is supervising this VM (reconciled on load).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ch_pid: Option<i32>,
}

impl VmMeta {
    pub fn vm_dir(&self, home: &VmHome) -> PathBuf {
        home.vms_dir().join(&self.id)
    }

    pub fn disk_path(&self, home: &VmHome) -> PathBuf {
        self.vm_dir(home).join("disk.qcow2")
    }

    pub fn ssh_key_path(&self, home: &VmHome) -> PathBuf {
        self.vm_dir(home).join("ssh_key")
    }

    pub fn console_path(&self, home: &VmHome) -> PathBuf {
        self.vm_dir(home).join("console.log")
    }

    pub fn lock_path(&self, home: &VmHome) -> PathBuf {
        self.vm_dir(home).join(".lock")
    }

    /// Interface names derived from the id (first 8 hex chars — keeps both
    /// under the 15-char ifname limit: br-<8> = 11, tap-<8> = 12).
    pub fn bridge_name(&self) -> String {
        format!("br-{}", &self.id[..8])
    }

    pub fn tap_name(&self) -> String {
        format!("tap-{}", &self.id[..8])
    }

    pub fn meta_path(&self, home: &VmHome) -> PathBuf {
        self.vm_dir(home).join("meta.json")
    }

    pub fn save(&self, home: &VmHome) -> Result<(), String> {
        let dir = self.vm_dir(home);
        fs::create_dir_all(&dir).map_err(|e| format!("create {}: {e}", dir.display()))?;
        let json = serde_json::to_string_pretty(self).map_err(|e| e.to_string())?;
        // write-then-rename: a torn meta.json must never clobber a good one
        let tmp = dir.join("meta.json.tmp");
        fs::write(&tmp, json).map_err(|e| format!("write {}: {e}", tmp.display()))?;
        fs::rename(&tmp, self.meta_path(home)).map_err(|e| format!("rename: {e}"))
    }

    /// Load meta.json. A stale "running" state is reconciled to "stopped":
    /// there is no daemon, so a running record without a live supervisor is
    /// just a record of the last session. (If ch_pid is alive, the VM is
    /// genuinely running — the flock is the real guard; keep the record.)
    pub fn load(home: &VmHome, id: &str) -> Result<Self, String> {
        let path = home.vms_dir().join(id).join("meta.json");
        let text = fs::read_to_string(&path).map_err(|e| format!("read {}: {e}", path.display()))?;
        let mut meta: VmMeta = serde_json::from_str(&text).map_err(|e| format!("parse {}: {e}", path.display()))?;
        if meta.state == VmState::Running {
            let pid_alive = meta.ch_pid
                .map(|pid| std::process::Command::new("kill").args(["-0", &pid.to_string()]).output().map(|o| o.status.success()).unwrap_or(false))
                .unwrap_or(false);
            if !pid_alive {
                meta.state = VmState::Stopped;
                meta.ch_pid = None;
            }
        }
        Ok(meta)
    }
}

/// Generate a 12-hex unique id (docker-style).
pub fn new_id() -> String {
    let mut b = [0u8; 6];
    rand::rng().fill_bytes(&mut b);
    let id: String = b.iter().map(|x| format!("{x:02x}")).collect();
    id
}

/// Resolve a user-supplied id/name argument to a VM: exact id, unique id
/// prefix, or exact name (docker-style).
pub fn resolve(home: &VmHome, arg: &str) -> Result<VmMeta, String> {
    let ids = home.list_vm_ids();
    if let Some(id) = ids.iter().find(|i| i.as_str() == arg) {
        return VmMeta::load(home, id);
    }
    // unique prefix
    let matches: Vec<&String> = ids.iter().filter(|i| i.starts_with(arg)).collect();
    if matches.len() == 1 {
        return VmMeta::load(home, matches[0]);
    }
    if matches.len() > 1 {
        return Err(format!("ambiguous id prefix '{arg}': {}", matches.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(", ")));
    }
    // by name
    for id in &ids {
        let Ok(meta) = VmMeta::load(home, id) else { continue };
        if meta.name.as_deref() == Some(arg) {
            return Ok(meta);
        }
    }
    Err(format!("no VM matching '{arg}' (tried id, unique prefix, name)"))
}

/// Allocate a free 172.16.N.0/24 subnet (N in 2..=251, same range as v1).
///
/// Two guards (the second is v1's nested-VM fix, ported from vm/start.sh):
/// 1. Our own state: the persisted subnets of existing VMs.
/// 2. The host's LIVE interfaces: a nested pi-vm (a container or a VM sharing
///    this host's network namespace) has its own state and would otherwise
///    pick the same N. The duplicate bridge IP (.1) and the duplicate VM IP
///    (.2) break both sides' reachability (v1: "the nested NAT rules would
///    masquerade the parent's own traffic (breaking the parent's
///    networking)"). Check both .1 and .2 — the .2 check catches a nested
///    VM colliding with its parent's NIC (which is 172.16.N.2).
pub fn allocate_subnet(home: &VmHome) -> Result<(String, String, String), String> {
    let used: Vec<String> = home
        .list_vm_ids()
        .iter()
        .filter_map(|id| VmMeta::load(home, id).ok())
        .map(|m| m.subnet)
        .collect();
    // Live 172.16.x.x addresses (bridges, other VMs, a nested pi-vm's parent
    // NIC). Read-only; runs unprivileged (any user can read interface state).
    // Degrades to the state-only check if `ip` is unavailable.
    let live = std::process::Command::new("ip")
        .args(["-o", "addr", "show"])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).to_string())
        .unwrap_or_default();
    let free: Vec<u8> = (2..=251)
        .filter(|n| {
            let subnet = format!("172.16.{n}.0/24");
            !used.contains(&subnet)
                && !live.contains(&format!("172.16.{n}.1/"))
                && !live.contains(&format!("172.16.{n}.2/"))
        })
        .collect();
    if free.is_empty() {
        return Err("no free 172.16.N.0/24 subnet (N in 2..=251)".to_string());
    }
    // RANDOM pick from the free set: sequential scanning made every concurrent
    // `create` race for the same first-free N (duplicate subnets -> all VMs
    // get .2 -> ssh unreachable). Random makes a collision ~1/250 per pair.
    let n = free[rand_below(free.len())];
    let ip = format!("172.16.{n}.2");
    let mac = format!("52:54:00:00:00:{n:02x}");
    Ok((format!("172.16.{n}.0/24"), ip, mac))
}

fn rand_below(len: usize) -> usize {
    use std::io::Read;
    let mut buf = [0u8; 8];
    if let Ok(mut f) = std::fs::File::open("/dev/urandom") {
        if f.read_exact(&mut buf).is_ok() {
            return (u64::from_ne_bytes(buf) % len as u64) as usize;
        }
    }
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as usize % len)
        .unwrap_or(0)
}

/// Current unix timestamp in seconds.
pub fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Format a unix timestamp for display.
pub fn fmt_ts(ts: i64) -> String {
    // avoid a datetime dep: seconds -> "YYYY-MM-DD HH:MM:SS" UTC via civil-from-days
    let days = ts.div_euclid(86400);
    let secs = ts.rem_euclid(86400);
    let (h, m, s) = (secs / 3600, (secs % 3600) / 60, secs % 60);
    let (y, mo, d) = civil_from_days(days);
    format!("{y:04}-{mo:02}-{d:02} {h:02}:{m:02}:{s:02} UTC")
}

/// Howard Hinnant's civil-from-days algorithm.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 3652400) / 365;
    let y = yoe + era * 400;
    let doy = doe - (yoe * 365 + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let mo = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if mo <= 2 { y + 1 } else { y };
    (y, mo as u32, d as u32)
}

/// Acquire an exclusive flock on the VM's .lock file. The returned guard
/// releases on drop.
pub struct VmLock {
    #[allow(dead_code)]
    file: fs::File,
}

impl VmLock {
    pub fn acquire(meta: &VmMeta, home: &VmHome) -> Result<Self, String> {
        let path = meta.lock_path(home);
        let file = fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&path)
            .map_err(|e| format!("open {}: {e}", path.display()))?;
        use fs2::FileExt;
        match file.try_lock_exclusive() {
            Ok(()) => Ok(Self { file }),
            Err(e) => Err(format!("VM {} is in use by another pi-vm process ({e})", meta.id)),
        }
    }
}

/// Delete a VM's bridge + tap + iptables rules (best-effort, like v1
/// delete.sh). Privileged (auto-sudo) — the setup path in vm.rs is
/// privileged too, and a non-root `ip link del` would fail silently.
/// Deleting the bridge also removes the enslaved tap; the explicit tap
/// del is the fallback for a tap that was never enslaved.
pub fn cleanup_network(meta: &VmMeta) {
    let br = meta.bridge_name();
    let tap = meta.tap_name();
    let subnet = meta.subnet.clone();
    for cmd in [
        vec!["ip", "link", "del", &br],
        vec!["ip", "link", "del", &tap],
        vec!["iptables", "-t", "nat", "-D", "POSTROUTING", "-s", &subnet, "-j", "MASQUERADE"],
        vec!["iptables", "-D", "FORWARD", "-i", &br, "-j", "ACCEPT"],
        vec!["iptables", "-D", "FORWARD", "-o", &br, "-j", "ACCEPT"],
    ] {
        let _ = run_ok_priv(cmd[0], &cmd[1..]);
    }
}

/// Remove a VM directory (after locking).
pub fn remove_vm(meta: &VmMeta, home: &VmHome) -> Result<(), String> {
    let dir = meta.vm_dir(home);
    fs::remove_dir_all(&dir).map_err(|e| format!("remove {}: {e}", dir.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn id_format() {
        let id = new_id();
        assert_eq!(id.len(), 12);
        assert!(id.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn interface_names_fit() {
        let home = VmHome::new();
        let id = new_id();
        let meta = VmMeta {
            id: id.clone(),
            name: None,
            created: 0,
            updated: 0,
            project: Some("/tmp".into()),
            subnet: "172.16.2.0/24".into(),
            ip: "172.16.2.2".into(),
            mac: "52:54:00:00:00:02".into(),
            vcpus: 4,
            memory_mb: 8192,
            disk_gb: 80,
            extra_disk: false,
            kvm: true,
            squash_uid: 1000,
            state: VmState::Stopped,
            ch_pid: None,
        };
        assert!(meta.bridge_name().len() <= 15);
        assert!(meta.tap_name().len() <= 15);
        let _ = home;
    }

    #[test]
    fn meta_roundtrip() {
        let dir = std::env::temp_dir().join(format!("pi-vm-test-{}", new_id()));
        let home = VmHome { root: dir.clone() };
        let id = new_id();
        let meta = VmMeta {
            id: id.clone(),
            name: Some("test".into()),
            created: 1700000000,
            updated: 1700000001,
            project: Some("/tmp/proj".into()),
            subnet: "172.16.3.0/24".into(),
            ip: "172.16.3.2".into(),
            mac: "52:54:00:00:00:03".into(),
            vcpus: 2,
            memory_mb: 4096,
            disk_gb: 80,
            extra_disk: true,
            kvm: false,
            squash_uid: 1000,
            state: VmState::Stopped,
            ch_pid: None,
        };
        meta.save(&home).unwrap();
        let loaded = VmMeta::load(&home, &id).unwrap();
        assert_eq!(loaded.name.as_deref(), Some("test"));
        assert_eq!(loaded.subnet, "172.16.3.0/24");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn allocate_subnet_skips_used() {
        let dir = std::env::temp_dir().join(format!("pi-vm-test-{}", new_id()));
        let home = VmHome { root: dir.clone() };
        let id = new_id();
        let meta = VmMeta {
            id: id.clone(),
            name: None,
            created: 0,
            updated: 0,
            project: Some("/tmp".into()),
            subnet: "172.16.2.0/24".into(),
            ip: "172.16.2.2".into(),
            mac: "52:54:00:00:00:02".into(),
            vcpus: 1,
            memory_mb: 1024,
            disk_gb: 80,
            extra_disk: false,
            kvm: true,
            squash_uid: 1000,
            state: VmState::Stopped,
            ch_pid: None,
        };
        meta.save(&home).unwrap();
        let (subnet, ip, mac) = allocate_subnet(&home).unwrap();
        assert_ne!(subnet, "172.16.2.0/24"); // the state-based skip
        let n: u8 = subnet
            .trim_start_matches("172.16.")
            .trim_end_matches(".0/24")
            .parse()
            .unwrap();
        assert!((3..=251).contains(&n));
        assert_eq!(ip, format!("172.16.{n}.2"));
        assert_eq!(mac, format!("52:54:00:00:00:{n:02x}"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn resolve_by_name_and_prefix() {
        let dir = std::env::temp_dir().join(format!("pi-vm-test-{}", new_id()));
        let home = VmHome { root: dir.clone() };
        let id = new_id();
        let meta = VmMeta {
            id: id.clone(),
            name: Some("myvm".into()),
            created: 0,
            updated: 0,
            project: Some("/tmp".into()),
            subnet: "172.16.4.0/24".into(),
            ip: "172.16.4.2".into(),
            mac: "52:54:00:00:00:04".into(),
            vcpus: 1,
            memory_mb: 1024,
            disk_gb: 80,
            extra_disk: false,
            kvm: true,
            squash_uid: 1000,
            state: VmState::Stopped,
            ch_pid: None,
        };
        meta.save(&home).unwrap();
        assert_eq!(resolve(&home, &id).unwrap().id, id);
        assert_eq!(resolve(&home, &id[..4]).unwrap().id, id);
        assert_eq!(resolve(&home, "myvm").unwrap().id, id);
        assert!(resolve(&home, "nope").is_err());
        std::fs::remove_dir_all(&dir).ok();
    }
}
