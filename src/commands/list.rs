//! `pi-vm list` — docker ps -a style (alias: `pi-vm ls`).

use clap::{Args, ValueEnum};

use crate::state::{fmt_ts, VmHome, VmMeta, VmState};
use crate::util::human;

#[derive(Args)]
pub struct List {
    /// Full details (all meta fields)
    #[arg(short, long)]
    pub verbose: bool,
    /// Filter VMs (repeatable, AND): state=running|stopped, name=<prefix>,
    /// project=<prefix>, id=<prefix>
    #[arg(long, value_name = "KEY=VALUE")]
    pub filter: Vec<String>,
    /// Sort rows by key (no --sort keeps the default order)
    #[arg(long, value_name = "KEY")]
    pub sort: Option<SortKey>,
    /// Reverse the --sort order
    #[arg(long)]
    pub reverse: bool,
}

/// `--sort` keys.
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum SortKey {
    /// Creation timestamp (oldest first)
    Created,
    /// Last-run timestamp (meta.updated, oldest first)
    #[value(name = "last-run")]
    LastRun,
    /// State (running first, then stopped)
    State,
    /// Name (nameless VMs first, then by name)
    Name,
    /// VM id (lexicographic)
    Id,
}

#[derive(Debug)]
enum FilterKey {
    State,
    Name,
    Project,
    Id,
}

/// One parsed `--filter KEY=VALUE` spec.
#[derive(Debug)]
struct FilterSpec {
    key: FilterKey,
    value: String,
}

impl FilterSpec {
    fn matches(&self, meta: &VmMeta) -> bool {
        match self.key {
            FilterKey::State => meta.state.to_string() == self.value,
            FilterKey::Name => meta
                .name
                .as_deref()
                .is_some_and(|n| n.starts_with(&self.value)),
            FilterKey::Project => meta
                .project
                .as_deref()
                .is_some_and(|p| p.starts_with(&self.value)),
            FilterKey::Id => meta.id.starts_with(&self.value),
        }
    }
}

fn parse_filter(s: &str) -> Result<FilterSpec, String> {
    let (key, value) = s
        .split_once('=')
        .ok_or_else(|| format!("invalid filter '{s}' (expected KEY=VALUE)"))?;
    match key {
        "state" => {
            if value != "running" && value != "stopped" {
                return Err(format!(
                    "invalid filter 'state={value}' (expected state=running or state=stopped)"
                ));
            }
            Ok(FilterSpec {
                key: FilterKey::State,
                value: value.to_string(),
            })
        }
        "name" => Ok(FilterSpec {
            key: FilterKey::Name,
            value: value.to_string(),
        }),
        "project" => Ok(FilterSpec {
            key: FilterKey::Project,
            value: value.to_string(),
        }),
        "id" => Ok(FilterSpec {
            key: FilterKey::Id,
            value: value.to_string(),
        }),
        other => Err(format!(
            "unknown filter key '{other}' (expected state, name, project, or id)"
        )),
    }
}

/// Sort comparator. `sort_by` is stable, so ties keep the default order.
fn cmp_meta(a: &VmMeta, b: &VmMeta, key: SortKey, reverse: bool) -> std::cmp::Ordering {
    let ord = match key {
        SortKey::Created => a.created.cmp(&b.created),
        SortKey::LastRun => a.updated.cmp(&b.updated),
        SortKey::State => state_rank(a.state).cmp(&state_rank(b.state)),
        SortKey::Name => a
            .name
            .as_deref()
            .unwrap_or("")
            .cmp(b.name.as_deref().unwrap_or("")),
        SortKey::Id => a.id.cmp(&b.id),
    };
    if reverse {
        ord.reverse()
    } else {
        ord
    }
}

/// running before stopped (active first, docker-like).
fn state_rank(s: VmState) -> u8 {
    match s {
        VmState::Running => 0,
        VmState::Stopped => 1,
    }
}

impl List {
    pub fn run(&self, home: &VmHome) -> i32 {
        let ids = home.list_vm_ids();
        if ids.is_empty() {
            println!("no VMs (run `pi-vm create <path>` or `pi-vm create --no-mount` first — see `pi-vm doctor`)");
            return 0;
        }
        let metas: Vec<VmMeta> = ids
            .iter()
            .filter_map(|id| VmMeta::load(home, id).ok())
            .collect();

        // --filter: parse first (usage error -> exit 2), then AND all specs.
        let specs: Vec<FilterSpec> = match self.filter.iter().map(|s| parse_filter(s)).collect() {
            Ok(specs) => specs,
            Err(e) => {
                eprintln!("error: {e}");
                return 2;
            }
        };
        let metas: Vec<VmMeta> = if specs.is_empty() {
            metas
        } else {
            metas
                .into_iter()
                .filter(|m| specs.iter().all(|s| s.matches(m)))
                .collect()
        };
        // --sort: stable sort (ties keep the default order); no --sort = unchanged.
        let metas: Vec<VmMeta> = match self.sort {
            Some(key) => {
                let mut v = metas;
                v.sort_by(|a, b| cmp_meta(a, b, key, self.reverse));
                v
            }
            None => metas,
        };

        if self.verbose {
            for meta in &metas {
                println!("=== {} {} ===", meta.id, meta.name.as_deref().unwrap_or("(no name)"));
                println!("  state:    {:?}", meta.state);
                println!("  project:  {}", meta.project.as_deref().unwrap_or("(no mount)"));
                println!("  network:  {} / {} / {}", meta.subnet, meta.ip, meta.mac);
                println!(
                    "  resources: {} vcpus, {} MB, {} GB disk, extra_disk={}, kvm={}, squash_uid={}",
                    meta.vcpus, meta.memory_mb, meta.disk_gb, meta.extra_disk, meta.kvm, meta.squash_uid
                );
                println!("  created:  {}", fmt_ts(meta.created));
                println!("  last run: {}", fmt_ts(meta.updated));
                println!(
                    "  disk:     {} ({})",
                    meta.disk_path(home).display(),
                    disk_size_str(&meta.disk_path(home))
                );
                println!();
            }
            return 0;
        }

        // state is converted to a String: this toolchain's width specifiers
        // only pad built-in string types, not custom Display impls — passing
        // the VmState enum directly printed "stopped" unpadded and shifted
        // every column after it one char left of the header. Variable widths
        // ({:name}) are also unsupported, so padding is done explicitly.
        let sep = "  ";
        let proj_w = metas
            .iter()
            .map(|m| m.project.as_deref().unwrap_or("-").len())
            .chain(std::iter::once("PROJECT".len()))
            .max()
            .unwrap();
        let disks: Vec<String> = metas.iter().map(|m| disk_size_str(&m.disk_path(home))).collect();
        let disk_w = disks
            .iter()
            .map(|d| d.len())
            .chain(std::iter::once("DISK".len()))
            .max()
            .unwrap();

        println!(
            "{}{}{}{}{}{}{}{}{}{}{}{}{}{}NAME",
            pad("ID", 12), sep, pad("STATE", 7), sep, pad("SUBNET", 15), sep,
            pad("CREATED", 23), sep, pad("LAST RUN", 23), sep, pad("PROJECT", proj_w), sep,
            pad("DISK", disk_w), sep
        );
        for (i, meta) in metas.iter().enumerate() {
            println!(
                "{}{}{}{}{}{}{}{}{}{}{}{}{}{}{}",
                pad(&meta.id, 12), sep,
                pad(&meta.state.to_string(), 7), sep,
                pad(&meta.subnet, 15), sep,
                pad(&fmt_ts(meta.created), 23), sep,
                pad(&fmt_ts(meta.updated), 23), sep,
                pad(meta.project.as_deref().unwrap_or("-"), proj_w), sep,
                pad(&disks[i], disk_w), sep,
                meta.name.as_deref().unwrap_or("-")
            );
        }
        0
    }
}

/// Pad `s` to at least `w` chars with trailing spaces (this toolchain's fmt
/// doesn't apply width specifiers to custom Display types, and variable
/// widths are unsupported).
fn pad(s: &str, w: usize) -> String {
    if s.len() >= w {
        s.to_string()
    } else {
        format!("{s}{}", " ".repeat(w - s.len()))
    }
}

fn disk_size_str(p: &std::path::Path) -> String {
    match std::fs::metadata(p) {
        Ok(m) => human(m.len()),
        Err(_) => "-".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_meta(
        id: &str,
        name: Option<&str>,
        project: Option<&str>,
        state: VmState,
        created: i64,
        updated: i64,
    ) -> VmMeta {
        VmMeta {
            id: id.to_string(),
            name: name.map(|s| s.to_string()),
            created,
            updated,
            project: project.map(|s| s.to_string()),
            subnet: "172.16.2.0/24".into(),
            ip: "172.16.2.2".into(),
            mac: "52:54:00:00:00:02".into(),
            vcpus: 1,
            memory_mb: 1024,
            disk_gb: 80,
            extra_disk: false,
            kvm: true,
            squash_uid: 1000,
            state,
            ch_pid: None,
            base_image_sha256: None,
        }
    }

    #[test]
    fn parse_filter_accepts_known_keys() {
        for s in ["state=running", "state=stopped", "name=web", "project=/home", "id=aa"] {
            assert!(parse_filter(s).is_ok(), "{s} should parse");
        }
    }

    #[test]
    fn parse_filter_rejects_bad_input() {
        assert!(parse_filter("state").is_err()); // no '='
        assert!(parse_filter("foo=bar").is_err()); // unknown key
        assert!(parse_filter("state=paused").is_err()); // bad state value
        let e = parse_filter("foo=bar").unwrap_err();
        assert!(e.contains("unknown filter key 'foo'"));
    }

    #[test]
    fn filter_prefix_semantics() {
        let web = test_meta("aabbccddeeff", Some("web"), Some("/home/u/site"), VmState::Running, 1, 2);
        let api = test_meta("112233445566", Some("api"), Some("/home/u/site2"), VmState::Stopped, 3, 4);
        let anon = test_meta("deadbeef0000", None, None, VmState::Stopped, 5, 6);

        let name_web = parse_filter("name=we").unwrap();
        assert!(name_web.matches(&web));
        assert!(!name_web.matches(&api));
        assert!(!name_web.matches(&anon)); // nameless never matches a name filter

        let proj_site2 = parse_filter("project=/home/u/site2").unwrap();
        assert!(!proj_site2.matches(&web)); // prefix, not substring
        assert!(proj_site2.matches(&api));
        assert!(!proj_site2.matches(&anon)); // projectless never matches a project filter

        let id_dead = parse_filter("id=dead").unwrap();
        assert!(id_dead.matches(&anon));
        assert!(!id_dead.matches(&web));

        let st_running = parse_filter("state=running").unwrap();
        assert!(st_running.matches(&web));
        assert!(!st_running.matches(&api));
    }

    #[test]
    fn filter_and_semantics() {
        let web = test_meta("aabbccddeeff", Some("web"), Some("/home/u/site"), VmState::Running, 1, 2);
        let api = test_meta("112233445566", Some("api"), Some("/home/u/site2"), VmState::Stopped, 3, 4);
        let specs = vec![parse_filter("state=stopped").unwrap(), parse_filter("name=we").unwrap()];
        assert!(!specs.iter().all(|s| s.matches(&web))); // running, not name=we
        assert!(!specs.iter().all(|s| s.matches(&api))); // stopped, but not name=we
    }

    #[test]
    fn sort_created_and_reverse() {
        let old = test_meta("1", None, None, VmState::Stopped, 100, 0);
        let new = test_meta("2", None, None, VmState::Stopped, 200, 0);
        let mut v = vec![new.clone(), old.clone()];
        v.sort_by(|a, b| cmp_meta(a, b, SortKey::Created, false));
        assert_eq!(v[0].created, 100);
        v.sort_by(|a, b| cmp_meta(a, b, SortKey::Created, true));
        assert_eq!(v[0].created, 200);
    }

    #[test]
    fn sort_last_run() {
        let a = test_meta("1", None, None, VmState::Stopped, 0, 500);
        let b = test_meta("2", None, None, VmState::Stopped, 0, 100);
        let mut v = vec![a.clone(), b.clone()];
        v.sort_by(|a, b| cmp_meta(a, b, SortKey::LastRun, false));
        assert_eq!(v[0].updated, 100);
    }

    #[test]
    fn sort_state_running_first() {
        let stopped = test_meta("1", None, None, VmState::Stopped, 0, 0);
        let running = test_meta("2", None, None, VmState::Running, 0, 0);
        let mut v = vec![stopped.clone(), running.clone()];
        v.sort_by(|a, b| cmp_meta(a, b, SortKey::State, false));
        assert_eq!(v[0].state, VmState::Running);
        v.sort_by(|a, b| cmp_meta(a, b, SortKey::State, true));
        assert_eq!(v[0].state, VmState::Stopped);
    }

    #[test]
    fn sort_name_nameless_first_then_by_name() {
        let anon = test_meta("1", None, None, VmState::Stopped, 0, 0);
        let web = test_meta("2", Some("web"), None, VmState::Stopped, 0, 0);
        let api = test_meta("3", Some("api"), None, VmState::Stopped, 0, 0);
        let mut v = vec![web.clone(), anon.clone(), api.clone()];
        v.sort_by(|a, b| cmp_meta(a, b, SortKey::Name, false));
        assert_eq!(v[0].name, None);
        assert_eq!(v[1].name.as_deref(), Some("api"));
        v.sort_by(|a, b| cmp_meta(a, b, SortKey::Name, true));
        assert_eq!(v[0].name.as_deref(), Some("web"));
    }

    #[test]
    fn sort_id_lexicographic() {
        let b = test_meta("b", None, None, VmState::Stopped, 0, 0);
        let a = test_meta("a", None, None, VmState::Stopped, 0, 0);
        let mut v = vec![b.clone(), a.clone()];
        v.sort_by(|a, b| cmp_meta(a, b, SortKey::Id, false));
        assert_eq!(v[0].id, "a");
    }

    #[test]
    fn sort_is_stable_on_ties() {
        // equal keys keep the input order (default/directory order)
        let x = test_meta("x", None, None, VmState::Stopped, 100, 0);
        let y = test_meta("y", None, None, VmState::Stopped, 100, 0);
        let mut v = vec![x.clone(), y.clone()];
        v.sort_by(|a, b| cmp_meta(a, b, SortKey::Created, false));
        assert_eq!(v[0].id, "x");
        assert_eq!(v[1].id, "y");
    }
}
