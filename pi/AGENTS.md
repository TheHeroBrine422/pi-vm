# Environment

You are running as root inside a full-featured Fedora Cloud 44 VM sandbox:

- `/dev` access, nested KVM (`/dev/kvm`), block devices, kernel modules —
  you can build and test real kernel-level workloads.
- docker + compose and the full toolchain are preinstalled; anything else
  can be installed on demand — when something you need to run is blocked
  by a missing tool, install the tools that are necessary.
- It is safe to install packages, create files, and run destructive commands.
- The VM doesn't have git creds, so pushing is impossible and fetch/pull 
  are often impossible due to private repos. Commit locally and ask 
  the user to run remote git operations on the host; do not work around 
  it (tokens, HTTPS, new keys).
- If you need to check code, docs, or information about dependencies
  and related projects, clone the entire repo to `/tmp`. Verifying
  the source implementation is usually better and easier than trying
  to use web search to get information.
- Long-running commands (expected >60 seconds): launch in the background
  (nohup) with output redirected to a log in /workspace/logs/, sleep 5 s
  in the same call, and read the log — confirm a healthy start (an
  empty log is fine if the process is alive). Tell the user what
  started and the expected runtime before further polls.
- Poll on a ramp: every minute for the first few checks (most failures
  happen early), then ~5% of the expected runtime (clamped 60 s –
  10 min; fixed 2–5 min if it can't be estimated). An error or stalled
  log aborts the ramp: re-check closely and diagnose. When the job
  finishes, report the result in one line.
- If a job runs ~2–3x longer than expected, or you realize your own
  approach is suboptimal (naive algorithm, sequential steps that could
  be parallel), don't just wait out the estimate: fix and restart when
  it clearly pays off — never disturb work that's nearly done. If the
  slowness is inherent, re-estimate and resume.
- Copying multi-line text out of this TUI is unreliable (shell commands
  in particular). When the user needs to copy something, present it on
  a single line or write it to a file instead.
- The user almost always has the workspace open in an IDE (Zed): don't
  dump diffs or repeat file contents — they can see the changes
  themselves. Name the file and summarize what changed. Short targeted
  excerpts of errors and key output are still fine.

The VM is disposable. Its disk persists across stop/resume (files and
installed packages survive a reboot of the same VM), but do not assume the
VM will be resumed — if a result needs to survive, put it in `/workspace`
or state it explicitly instead of silently working around it. Do not
assume state from previous VMs: a fresh VM has no leftover files, no prior
installs, no cached credentials.

## /workspace (shared folder)

The `/workspace` folder is shared with the host (virtiofs) and is the
persistent project folder.

**Uid squashing:** ownership is remapped in both directions — don't rely on it:

- **Guest → host:** every guest uid maps to the host user. Files you
  write (even as a non-root user you create) land on the host owned by
  the host user.
- **Host → guest:** the host user's files (including ones you just
  created) appear as `fedora` (uid 1000), not root. Files owned by
  other host uids (e.g. made with `sudo`) appear as `nobody` (65534)
  and are read-only for everyone in the VM, even root.
- **Writes:** as root you can write the host user's files; a non-root
  user can only create new files (modifying any existing file — even
  one just created — is denied). Permission issues in `/workspace`
  mean this: chmod as root, or copy the file to a local path.
- If something needs real file ownership (e.g. container volume data),
  use a local path on the VM's disk instead (e.g. `/var/lib`).

## Autonomy

Operate autonomously. When you face ambiguity, make a reasonable assumption,
state it clearly, and proceed — do not ask me questions. Only pause to
confirm if an action is irreversible or high-risk (deleting data, force-push,
sending messages, spending money).
