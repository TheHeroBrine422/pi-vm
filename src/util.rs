//! Host-level helpers shared by commands.

/// Render a multi-line shell script as a `bash -c '...'` runcmd entry.
///
/// The entry is emitted as a YAML multi-line plain scalar — YAML folds
/// the newlines into spaces, so the guest receives the same one-liner
/// `bash -c '...'` command. The source script carries the `;`
/// separators: after complete commands, `done`, `fi`, `esac` — but NOT
/// after `do`, `then`, `else`, `)`, `in`, `;;` (a `;` there is a
/// syntax error). Constraints on the script text: no `: ` sequences and
/// no single quotes (YAML plain scalar), no blank lines (they fold to
/// nothing and would glue tokens together).
pub fn runcmd_block(script: &str) -> String {
    let lines: Vec<&str> = script
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();
    let (first, rest) = lines.split_first().expect("empty runcmd script");
    if rest.is_empty() {
        format!("  - bash -c '{first}'")
    } else {
        let body = rest.join("\n    ");
        format!("  - bash -c '{first}\n    {body}'")
    }
}

/// True if the process runs as root.
pub fn is_root() -> bool {
    run("id", &["-u"]).map(|u| u == "0").unwrap_or(true)
}

/// Run a privileged command (auto-sudo when not root — v1's $SUDO pattern).
pub fn run_priv(cmd: &str, args: &[&str]) -> Result<String, String> {
    if is_root() {
        run(cmd, args)
    } else {
        let mut a: Vec<&str> = vec!["sudo"];
        a.push(cmd);
        a.extend(args);
        run(a[0], &a[1..])
    }
}

/// Privileged run_ok (auto-sudo when not root).
pub fn run_ok_priv(cmd: &str, args: &[&str]) -> bool {
    if is_root() {
        run_ok(cmd, args)
    } else {
        let mut a: Vec<&str> = vec!["sudo"];
        a.push(cmd);
        a.extend(args);
        run_ok(a[0], &a[1..])
    }
}

/// Resolve the virtiofsd binary (Fedora ships it in /usr/libexec, not PATH).
pub fn virtiofsd_path() -> Option<String> {
    if let Ok(o) = std::process::Command::new("which").arg("virtiofsd").output()
        && o.status.success() {
            let p = String::from_utf8_lossy(&o.stdout).trim().to_string();
            if !p.is_empty() {
                return Some(p);
            }
        }
    let libexec = std::path::Path::new("/usr/libexec/virtiofsd");
    libexec.exists().then(|| libexec.to_string_lossy().to_string())
}

/// Human-readable byte size.
pub fn human(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut v = n as f64;
    let mut i = 0;
    while v >= 1024.0 && i < 4 {
        v /= 1024.0;
        i += 1;
    }
    format!("{v:.1} {}", UNITS[i])
}

/// Run a command, returning trimmed stdout or an error string.
pub fn run(cmd: &str, args: &[&str]) -> Result<String, String> {
    let o = std::process::Command::new(cmd)
        .args(args)
        .output()
        .map_err(|e| format!("{cmd} {args:?}: {e}"))?;
    if !o.status.success() {
        return Err(format!(
            "{cmd} {args:?} failed ({}): {}",
            o.status,
            String::from_utf8_lossy(&o.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&o.stdout).trim().to_string())
}

/// Run a command, ignoring output (success only).
pub fn run_ok(cmd: &str, args: &[&str]) -> bool {
    std::process::Command::new(cmd)
        .args(args)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}
