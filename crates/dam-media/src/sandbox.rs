//! Running an external media tool on a stranger's file.
//!
//! ffmpeg, LibreOffice, libraw and pdfium have all had CVEs reachable from a malformed input,
//! and every file a DAM processes was chosen by whoever uploaded it. So this module is the
//! containment, not plumbing. It bounds four things:
//!
//! | Risk | Bound |
//! |---|---|
//! | A hang occupying a worker forever | wall clock, enforced in-process |
//! | A decompression bomb burning a core | `ulimit -t` |
//! | A runaway write filling the disk | `ulimit -f` |
//! | A memory bomb | `ulimit -v` — **Linux only**, see below |
//!
//! plus two things that are about the *runner* rather than the tool: an argument is never
//! reinterpreted as a shell command, and the child does not inherit the parent's environment.
//!
//! ## Why the shell is in the picture at all
//!
//! Applying an rlimit to a child normally means `pre_exec`, which is `unsafe`, and the whole
//! workspace is `#![forbid(unsafe_code)]`. The alternative is to let the shell do it:
//!
//! ```text
//! /bin/sh -c 'ulimit -t 5; ulimit -f 65536; exec "$0" "$@"' <program> <args…>
//! ```
//!
//! The limits go in the script — which is a fixed string this module builds from numbers, never
//! from caller input — and the program and its arguments arrive as **positional parameters**.
//! That is what makes injection structurally impossible rather than a quoting exercise: `$0` and
//! `"$@"` are values to the shell, not text to re-parse, so a filename containing `; rm -rf /`
//! is just a filename.
//!
//! ## The memory limit is not real on macOS
//!
//! Measured, not assumed. On darwin `ulimit -v` is rejected by the shell outright, and a 200 MB
//! allocation runs unimpeded under a 50 MB cap. On Linux the same cap makes `dd` fail with
//! "out of memory". A runner that accepted the limit on both would create a protection that
//! exists only in production, which is the worst place to discover a difference — so
//! [`capabilities`] states what the platform actually does and [`Sandbox::unenforced`] names
//! every requested limit that is decorative here.

use std::{
    path::{Path, PathBuf},
    process::Stdio,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tempfile::TempDir;
use tokio::{io::AsyncReadExt, process::Command, time};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("sandbox: {0}")]
    Setup(String),

    #[error("path {path:?} is outside the sandbox")]
    Escape { path: String },

    #[error("spawning {program}: {source}")]
    Spawn {
        program: String,
        #[source]
        source: std::io::Error,
    },
}

type Result<T> = std::result::Result<T, Error>;

/// What a subprocess is allowed to consume.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// Always enforced, in this process. The only bound that applies to a *sleeping* child — and
    /// a stalled read is sleeping, so this is what reclaims a worker from a hung tool.
    pub wall_clock: Duration,
    /// `ulimit -t`. Bounds work rather than elapsed time, which is what stops a decompression
    /// bomb from burning a core for the whole wall-clock budget.
    pub cpu_seconds: Option<u32>,
    /// `ulimit -v`. **Linux only** — see the module docs.
    pub address_space_bytes: Option<u64>,
    /// `ulimit -f`. A tool that writes until the disk fills takes down every worker on the host.
    pub file_size_bytes: Option<u64>,
    /// Cap on captured stdout and stderr. Buffering unbounded output is how a worker dies of OOM
    /// while the subprocess behaves exactly as designed.
    pub max_output_bytes: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            wall_clock: Duration::from_secs(120),
            cpu_seconds: Some(60),
            address_space_bytes: Some(2 * 1024 * 1024 * 1024),
            file_size_bytes: Some(8 * 1024 * 1024 * 1024),
            max_output_bytes: 256 * 1024,
        }
    }
}

/// Which limits the running platform actually enforces.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Capabilities {
    pub cpu_limit: bool,
    pub address_space_limit: bool,
    pub file_size_limit: bool,
}

/// Declared from the target rather than probed at runtime.
///
/// A probe would mean spawning a shell during startup and interpreting its refusal, and a
/// mis-read probe is worse than a stated fact: it would silently downgrade the sandbox.
pub const fn capabilities() -> Capabilities {
    Capabilities {
        cpu_limit: true,
        // Darwin rejects `ulimit -v` and does not constrain the allocation; Linux does both.
        address_space_limit: cfg!(target_os = "linux"),
        file_size_limit: true,
    }
}

/// How a run ended.
///
/// `TimedOut` and `Killed` are separate outcomes rather than error variants because the caller
/// treats them differently from a spawn failure: a timeout is a property of the *file*, worth
/// recording against the asset, while a missing binary is a deployment fault.
#[derive(Debug)]
pub enum Outcome {
    Ok {
        stdout: Vec<u8>,
        stderr: Vec<u8>,
        truncated: bool,
    },
    /// Exited non-zero.
    Failed {
        code: Option<i32>,
        stdout: Vec<u8>,
        stderr: Vec<u8>,
        truncated: bool,
    },
    /// Ended by a signal — which is how `ulimit -t` and `-f` present themselves.
    Killed {
        signal: Option<i32>,
        stdout: Vec<u8>,
        stderr: Vec<u8>,
        truncated: bool,
    },
    /// Hit the wall clock and was killed by us.
    ///
    /// Carries the partial output: when a tool hangs, what it printed before stalling is the
    /// whole diagnosis, and discarding it leaves an operator with "timed out" and nothing else.
    TimedOut {
        after: Duration,
        stdout: Vec<u8>,
        stderr: Vec<u8>,
        truncated: bool,
    },
}

impl Outcome {
    pub fn succeeded(&self) -> bool {
        matches!(self, Self::Ok { .. })
    }

    /// Captured stderr, for the log line that explains a failure.
    pub fn stderr(&self) -> &[u8] {
        match self {
            Self::Ok { stderr, .. }
            | Self::Failed { stderr, .. }
            | Self::Killed { stderr, .. }
            | Self::TimedOut { stderr, .. } => stderr,
        }
    }
}

/// A per-invocation working directory plus the limits applied to anything run in it.
///
/// Dropping it removes the directory. A per-job temp dir that outlives its job fills the disk one
/// derivative at a time, and a 200 GB video's intermediate files make that fast.
#[derive(Debug)]
pub struct Sandbox {
    dir: TempDir,
    limits: Limits,
    unenforced: Vec<String>,
}

impl Sandbox {
    pub fn new(limits: Limits) -> Result<Self> {
        let dir = tempfile::Builder::new()
            .prefix("damrs-job-")
            .tempdir()
            .map_err(|e| Error::Setup(format!("creating a working directory: {e}")))?;

        let caps = capabilities();
        let mut unenforced = Vec::new();
        if limits.address_space_bytes.is_some() && !caps.address_space_limit {
            unenforced.push(
                "address space (ulimit -v is not enforced on this platform; only Linux honours it)"
                    .to_owned(),
            );
        }
        if limits.cpu_seconds.is_some() && !caps.cpu_limit {
            unenforced.push("cpu seconds".to_owned());
        }
        if limits.file_size_bytes.is_some() && !caps.file_size_limit {
            unenforced.push("file size".to_owned());
        }
        if !unenforced.is_empty() {
            // Warned once per sandbox rather than returned as an error: refusing would make local
            // development impossible on macOS, and silence would hide the gap. The caller can
            // escalate via `unenforced()` if it needs to.
            tracing::warn!(
                unenforced = ?unenforced,
                "some subprocess limits are not enforced on this platform"
            );
        }

        Ok(Self {
            dir,
            limits,
            unenforced,
        })
    }

    pub fn path(&self) -> &Path {
        self.dir.path()
    }

    pub fn limits(&self) -> &Limits {
        &self.limits
    }

    /// Requested limits this platform will not actually apply.
    pub fn unenforced(&self) -> &[String] {
        &self.unenforced
    }

    /// Resolves a relative name inside the sandbox, refusing anything that leaves it.
    ///
    /// Three ways out, all closed: a `..` segment, an absolute path, and a **symlink whose target
    /// is outside**. The last is the interesting one — the name is innocent and the target is not,
    /// so a tool told to write `out.png` would follow the link and overwrite whatever it points
    /// at.
    pub fn resolve(&self, relative: &str) -> Result<PathBuf> {
        if relative.is_empty() {
            return Err(Error::Escape {
                path: relative.to_owned(),
            });
        }
        let candidate = Path::new(relative);
        if candidate.is_absolute()
            || candidate
                .components()
                .any(|c| matches!(c, std::path::Component::ParentDir))
        {
            return Err(Error::Escape {
                path: relative.to_owned(),
            });
        }

        let joined = self.dir.path().join(candidate);
        // Only an existing path can be canonicalised, and most call sites name a file the tool is
        // about to create — so an absent path is fine and a *present* one must canonicalise back
        // inside. That is what catches the symlink.
        if joined.symlink_metadata().is_ok() {
            let real = joined
                .canonicalize()
                .map_err(|e| Error::Setup(format!("canonicalising {joined:?}: {e}")))?;
            let root = self
                .dir
                .path()
                .canonicalize()
                .map_err(|e| Error::Setup(format!("canonicalising the sandbox root: {e}")))?;
            if !real.starts_with(&root) {
                return Err(Error::Escape {
                    path: relative.to_owned(),
                });
            }
        }
        Ok(joined)
    }

    /// Files in the sandbox that exceed the configured size limit.
    ///
    /// The authoritative disk bound. `ulimit -f` is the coarse one — it stops unbounded growth,
    /// but its block size differs between shells (512 on busybox, 1024 on bash) so the effective
    /// ceiling can be twice what was asked for. This closes that window: a derivative that
    /// overshot is detected and can be discarded rather than stored.
    pub fn oversized(&self) -> Vec<(PathBuf, u64)> {
        let Some(limit) = self.limits.file_size_bytes else {
            return Vec::new();
        };
        let mut over = Vec::new();
        let mut stack = vec![self.dir.path().to_path_buf()];
        while let Some(dir) = stack.pop() {
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                // `symlink_metadata`, not `metadata`: following a link out of the sandbox would
                // measure someone else's file and could be steered by the tool under test.
                let Ok(meta) = entry.path().symlink_metadata() else {
                    continue;
                };
                if meta.is_dir() {
                    stack.push(path);
                } else if meta.is_file() && meta.len() > limit {
                    over.push((path, meta.len()));
                }
            }
        }
        over
    }

    /// Runs `program` with `args` under the sandbox's limits.
    pub async fn run(&self, program: &str, args: &[&str]) -> Result<Outcome> {
        // Built from numbers only. `2>/dev/null || true` on the ulimit lines would hide a
        // rejection, so a limit the shell refuses is visible in stderr and already reported by
        // `unenforced()`.
        let mut script = String::new();
        if let Some(cpu) = self.limits.cpu_seconds {
            script.push_str(&format!("ulimit -t {cpu}; "));
        }
        if let Some(bytes) = self.limits.address_space_bytes
            && capabilities().address_space_limit
        {
            // ulimit -v is in kibibytes.
            script.push_str(&format!("ulimit -v {}; ", bytes / 1024));
        }
        if let Some(bytes) = self.limits.file_size_bytes {
            // POSIX says 512-byte blocks and busybox agrees, but bash — which is `/bin/sh` on
            // macOS — uses 1024-byte blocks, and `POSIXLY_CORRECT` does not change it (measured
            // both ways). So the same number means twice as many bytes on one platform as the
            // other. 512 is used because under-capping would fail legitimate large derivatives on
            // Linux, and the resulting slack (up to 2x on bash) is closed by
            // `Sandbox::oversized`, which is the authoritative check.
            script.push_str(&format!("ulimit -f {}; ", bytes.div_ceil(512)));
        }
        script.push_str("exec \"$0\" \"$@\"");

        let mut command = Command::new("/bin/sh");
        command
            .arg("-c")
            .arg(&script)
            // Positional parameters: the program and every argument are values, never text the
            // shell re-parses. This is the injection boundary.
            .arg(program)
            .args(args)
            .current_dir(self.dir.path())
            // The parent holds storage credentials. A subprocess that inherits them turns an RCE
            // in a media tool into a bucket compromise.
            .env_clear()
            .env("PATH", "/usr/bin:/bin:/usr/sbin:/sbin")
            .env("HOME", self.dir.path())
            .env("TMPDIR", self.dir.path())
            // Media tools read locale to decide number formatting, which has produced
            // comma-decimal output that downstream parsers rejected.
            .env("LC_ALL", "C")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        let mut child = command.spawn().map_err(|e| Error::Spawn {
            program: program.to_owned(),
            source: e,
        })?;

        let mut stdout_pipe = child.stdout.take();
        let mut stderr_pipe = child.stderr.take();
        let cap = self.limits.max_output_bytes;

        // Shared buffers, appended to as the pipes are read rather than returned at the end. The
        // timeout path has to be able to see what arrived *before* the stall — a hung tool's last
        // lines are the diagnosis, and a sink written only on completion is empty exactly when it
        // matters.
        let out_buf = Arc::new(Mutex::new(Vec::new()));
        let err_buf = Arc::new(Mutex::new(Vec::new()));
        let truncated = Arc::new(AtomicBool::new(false));

        let started = std::time::Instant::now();
        let collect = {
            let (o, e, t) = (
                Arc::clone(&out_buf),
                Arc::clone(&err_buf),
                Arc::clone(&truncated),
            );
            async move {
                // Both pipes are read concurrently with the wait. Draining one to EOF first
                // deadlocks as soon as the child fills the other pipe's buffer — a tool that is
                // chatty on stderr is exactly that case.
                tokio::join!(
                    read_capped(&mut stdout_pipe, cap, &o, &t),
                    read_capped(&mut stderr_pipe, cap, &e, &t)
                );
                child.wait().await
            }
        };

        let snapshot = |buf: &Arc<Mutex<Vec<u8>>>| match buf.lock() {
            Ok(g) => g.clone(),
            Err(p) => p.into_inner().clone(),
        };

        match time::timeout(self.limits.wall_clock, collect).await {
            // `kill_on_drop` reaps the child as `collect` is dropped here.
            Err(_) => Ok(Outcome::TimedOut {
                after: started.elapsed(),
                stdout: snapshot(&out_buf),
                stderr: snapshot(&err_buf),
                truncated: truncated.load(Ordering::Relaxed),
            }),
            Ok(status) => {
                let status = status.map_err(|e| Error::Spawn {
                    program: program.to_owned(),
                    source: e,
                })?;
                Ok(classify(
                    status,
                    snapshot(&out_buf),
                    snapshot(&err_buf),
                    truncated.load(Ordering::Relaxed),
                ))
            }
        }
    }
}

/// Reads at most `cap` bytes, reporting whether more was available.
///
/// The remainder is drained rather than left, so the child is not blocked writing into a full
/// pipe while we wait for it to exit — a truncated read that stalls the child turns an output cap
/// into a deadlock.
async fn read_capped<R>(
    pipe: &mut Option<R>,
    cap: usize,
    into: &Arc<Mutex<Vec<u8>>>,
    truncated: &Arc<AtomicBool>,
) where
    R: AsyncReadExt + Unpin,
{
    let Some(reader) = pipe.as_mut() else {
        return;
    };
    let mut buf = [0u8; 8192];
    loop {
        match reader.read(&mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                let mut guard = match into.lock() {
                    Ok(g) => g,
                    Err(p) => p.into_inner(),
                };
                if guard.len() < cap {
                    let room = cap - guard.len();
                    guard.extend_from_slice(&buf[..n.min(room)]);
                    if n > room {
                        truncated.store(true, Ordering::Relaxed);
                    }
                } else {
                    // Kept reading and discarded: leaving the pipe full would block the child
                    // forever, turning an output cap into a deadlock.
                    truncated.store(true, Ordering::Relaxed);
                }
            }
        }
    }
}

fn classify(
    status: std::process::ExitStatus,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    truncated: bool,
) -> Outcome {
    use std::os::unix::process::ExitStatusExt;
    if status.success() {
        return Outcome::Ok {
            stdout,
            stderr,
            truncated,
        };
    }
    // A signal is how `ulimit -t` and `-f` announce themselves (SIGXCPU, SIGXFSZ), so it is
    // distinguished from an ordinary non-zero exit: one means the file was hostile, the other
    // that the tool disagreed with it.
    if let Some(signal) = status.signal() {
        return Outcome::Killed {
            signal: Some(signal),
            stdout,
            stderr,
            truncated,
        };
    }
    Outcome::Failed {
        code: status.code(),
        stdout,
        stderr,
        truncated,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_limits_are_bounded_on_every_axis() {
        // An unset limit is not a generous limit, it is no limit — and this default is what most
        // callers will get.
        let d = Limits::default();
        assert!(d.cpu_seconds.is_some());
        assert!(d.file_size_bytes.is_some());
        assert!(d.address_space_bytes.is_some());
        assert!(d.max_output_bytes > 0);
        assert!(d.wall_clock > Duration::ZERO);
    }

    #[test]
    fn the_file_size_limit_rounds_up_to_whole_blocks() {
        // `ulimit -f` is in 512-byte blocks. Rounding *down* would silently cap below what the
        // caller asked for, so a derivative one byte over a block boundary would fail.
        assert_eq!(1u64.div_ceil(512), 1);
        assert_eq!(512u64.div_ceil(512), 1);
        assert_eq!(513u64.div_ceil(512), 2);
    }
}
