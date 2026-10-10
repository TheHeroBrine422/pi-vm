//! `pi-vm send` — type a message into a running VM's agent (the tmux
//! session 'agent'). The message lands in pi's input box as keystrokes
//! (Enter included); a newline in the message becomes a space (a newline
//! in the pty would be an Enter and submit early). Only works for agent
//! VMs (created or resumed with --prompt-file) while the VM is running.

use clap::Args;

use crate::state::{resolve, VmHome, VmState};

/// Escape a string for embedding in a single-quoted shell argument.
/// Inside single quotes backslashes are literal, so only `'` needs
/// escaping (close the quote, an escaped quote, reopen the quote).
fn escape_single_quoted(s: &str) -> String {
    s.replace('\'', "'\\''")
}

#[derive(Args)]
pub struct Send {
    /// VM id, unique id prefix, or name
    pub target: String,
    /// Message to type into the agent's input box
    #[arg(short, long)]
    pub message: String,
}

impl Send {
    pub fn run(&self, home: &VmHome) -> i32 {
        let meta = match resolve(home, &self.target) {
            Ok(m) => m,
            Err(e) => {
                eprintln!("error: {e}");
                return 2;
            }
        };
        if meta.state != VmState::Running {
            eprintln!("error: VM {} is not running (state: {})", meta.id, meta.state);
            return 1;
        }
        if let Some(pid) = meta.ch_pid
            && !std::path::Path::new(&format!("/proc/{pid}")).exists()
        {
            eprintln!("error: VM {} is not running (supervisor pid {pid} is gone)", meta.id);
            return 1;
        }
        // Sanitize: a newline in pty input is an Enter (early submit).
        let msg = self.message.replace('\n', " ").replace('\r', " ");
        // Escape for the remote shell (single-quote context): inside
        // single quotes backslashes are LITERAL, so only the quote
        // itself needs escaping.
        let q = escape_single_quoted(&msg);
        let cmd = format!("tmux send-keys -t agent '{q}' Enter");
        let key = meta.ssh_key_path(home);
        let o = std::process::Command::new("ssh")
            .args([
                "-i", &key.to_string_lossy(),
                "-o", "StrictHostKeyChecking=no",
                "-o", "UserKnownHostsFile=/dev/null",
                "-o", "BatchMode=yes",
                "-o", "ConnectTimeout=10",
                &format!("root@{}", meta.ip),
                &cmd,
            ])
            .output();
        match o {
            Ok(o) if o.status.success() => {
                println!(
                    "sent to {} ({})",
                    meta.id,
                    meta.name.as_deref().unwrap_or("(no name)")
                );
                0
            }
            Ok(o) => {
                let err = String::from_utf8_lossy(&o.stderr);
                eprintln!("error: send failed: {} (is the VM an agent VM, with the tmux session 'agent' running?)", err.trim());
                1
            }
            Err(e) => {
                eprintln!("error: ssh: {e}");
                1
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::escape_single_quoted;

    #[test]
    fn backslashes_are_literal_not_doubled() {
        // P1 regression: a backslash must be typed as a single backslash
        // (the old code doubled it inside the single-quote context).
        assert_eq!(escape_single_quoted("C:\\temp\\x"), "C:\\temp\\x");
    }

    #[test]
    fn quotes_are_escaped() {
        assert_eq!(escape_single_quoted("it's"), "it'\\''s");
        assert_eq!(escape_single_quoted("a'b'c"), "a'\\''b'\\''c");
    }

    #[test]
    fn mixed_backslash_and_quote() {
        // backslash stays single; quote gets the close/escape/reopen.
        assert_eq!(escape_single_quoted("a\\'b"), "a\\'\\''b");
    }

    #[test]
    fn plain_text_unchanged() {
        assert_eq!(escape_single_quoted("hello world /quit"), "hello world /quit");
    }
}
