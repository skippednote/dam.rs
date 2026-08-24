//! The subprocess sandbox (task 1.7).
//!
//! Derivative generation runs ffmpeg, LibreOffice and friends on **files a stranger uploaded**.
//! Every one of those has had a CVE reachable from a malformed input, so the runner around them
//! is not boilerplate — it is the containment. Four things it has to do:
//!
//! - stop a hang from occupying a worker forever;
//! - stop a runaway from eating the host's CPU, memory or disk;
//! - stop an argument from being reinterpreted as a shell command;
//! - stop the child from inheriting the parent's environment, which holds storage credentials.
//!
//! Everything here is tested with `/bin/sh` and `dd`, so the suite needs no media tools
//! installed and tests the *runner* rather than the tools.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use dam_media::sandbox::{self, Limits, Outcome, Sandbox};
use std::time::{Duration, Instant};

fn quick() -> Limits {
    Limits {
        wall_clock: Duration::from_secs(2),
        cpu_seconds: Some(5),
        address_space_bytes: None,
        file_size_bytes: None,
        max_output_bytes: 64 * 1024,
    }
}

#[tokio::test]
async fn a_hanging_process_is_killed_at_the_wall_clock_deadline() {
    // The limit that always applies. CPU limits do not fire on a process that is asleep, and a
    // stalled network read is asleep — so a wall clock is the only thing that reclaims a worker
    // occupied by a hung LibreOffice.
    let sandbox = Sandbox::new(Limits {
        wall_clock: Duration::from_millis(600),
        ..quick()
    })
    .expect("sandbox");

    let started = Instant::now();
    let outcome = sandbox
        .run("/bin/sh", &["-c", "sleep 30"])
        .await
        .expect("run");
    let elapsed = started.elapsed();

    assert!(
        matches!(outcome, Outcome::TimedOut { .. }),
        "expected a timeout, got {outcome:?}"
    );
    assert!(
        elapsed < Duration::from_secs(5),
        "the deadline must actually fire, took {elapsed:?}"
    );
}

#[tokio::test]
async fn a_cpu_bound_spin_is_killed_by_the_cpu_limit() {
    // Distinct from the wall clock: a CPU limit is what stops a decoder that is *working* — a
    // 100-megapixel decompression bomb — from burning a core for the whole wall-clock budget.
    let sandbox = Sandbox::new(Limits {
        wall_clock: Duration::from_secs(30),
        cpu_seconds: Some(1),
        ..quick()
    })
    .expect("sandbox");

    let outcome = sandbox
        .run("/bin/sh", &["-c", "while :; do :; done"])
        .await
        .expect("run");
    assert!(
        !matches!(outcome, Outcome::Ok { .. }),
        "a spin must not be reported as success: {outcome:?}"
    );
    assert!(
        matches!(outcome, Outcome::Killed { .. } | Outcome::Failed { .. }),
        "expected the CPU limit to end it, got {outcome:?}"
    );
}

#[tokio::test]
async fn a_runaway_write_is_stopped_by_the_file_size_limit() {
    // A malformed video that makes ffmpeg write until the disk fills takes down every other
    // worker on the host, not just its own job.
    let sandbox = Sandbox::new(Limits {
        file_size_bytes: Some(64 * 1024),
        ..quick()
    })
    .expect("sandbox");

    let outcome = sandbox
        .run(
            "/bin/sh",
            &["-c", "dd if=/dev/zero of=./runaway bs=1k count=10000"],
        )
        .await
        .expect("run");
    assert!(
        !matches!(outcome, Outcome::Ok { .. }),
        "a write past the cap must not report success: {outcome:?}"
    );

    let written = std::fs::metadata(sandbox.path().join("runaway"))
        .map(|m| m.len())
        .unwrap_or(0);
    // `ulimit -f` counts blocks, and the block size is 512 on busybox but **1024 on bash**, which
    // is `/bin/sh` on macOS — `POSIXLY_CORRECT` does not change it (measured both ways). So the
    // shell's own ceiling can be twice what was asked for, and the ulimit is the coarse bound
    // that stops unbounded growth rather than an exact byte count.
    assert!(
        written <= 2 * 64 * 1024,
        "the write must be bounded within the shell's block-size slack, {written} bytes landed"
    );
    assert!(
        written < 10_000 * 1024,
        "and nowhere near the 10 MB the command asked for, got {written}"
    );

    // The invariant is that a runaway write is *bounded and accounted for*, by whichever of the two
    // mechanisms applies — not that the scan always has something to report.
    //
    // Which one fires is decided by the shell's block size. `ulimit -f` is set to
    // `65536 / 512 = 128` blocks: on a 512-byte shell that is exactly the cap, so the file stops at
    // 65536 and there is genuinely nothing over the limit for `oversized` to find. On bash's
    // 1024-byte blocks the same 128 means 131072, so the file overshoots and the scan is what
    // catches it. Asserting the scan is non-empty encodes the second platform's slack as a
    // universal rule, and it failed on the first Linux CI run for exactly that reason while passing
    // on macOS.
    let over = sandbox.oversized();
    let written_over_cap = written > 64 * 1024;
    assert_eq!(
        written_over_cap,
        over.iter().any(|(p, _)| p.ends_with("runaway")),
        "a file past the cap must be caught by the scan, and a file within it must not be \
         reported: {written} bytes written, scan says {over:?}"
    );
}

#[tokio::test]
async fn an_argument_is_never_reinterpreted_as_a_shell_command() {
    // The whole reason the runner uses positional parameters rather than building a command
    // line: a filename is attacker-controlled, and a DAM stores files whose names were chosen by
    // whoever uploaded them.
    let sandbox = Sandbox::new(quick()).expect("sandbox");
    let hostile = "; touch /tmp/damrs-pwned; echo $(whoami) `id` $HOME";

    let outcome = sandbox.run("/bin/echo", &[hostile]).await.expect("run");
    match outcome {
        Outcome::Ok { stdout, .. } => {
            let text = String::from_utf8_lossy(&stdout);
            assert!(
                text.contains(hostile),
                "the argument must arrive literally, got {text:?}"
            );
            assert!(
                !text.contains("root") && !text.contains("uid="),
                "no substitution may have happened: {text:?}"
            );
        }
        other => panic!("expected success, got {other:?}"),
    }
    assert!(
        !std::path::Path::new("/tmp/damrs-pwned").exists(),
        "the injected command must not have run"
    );
}

#[tokio::test]
async fn the_childs_environment_is_exactly_the_allowlist() {
    // The parent holds storage credentials — `.mise.toml` alone puts DATABASE_URL and
    // AWS_SECRET_ACCESS_KEY in this process. A subprocess that inherits them turns an RCE in
    // ffmpeg into a bucket compromise, which is a much worse day than a crashed worker.
    //
    // Asserted as a whitelist rather than by looking for specific leaks: naming the variables we
    // fear only catches the ones we thought of, and the set of secrets in a deployed environment
    // is not ours to predict. (It also avoids mutating this process's environment, which is
    // `unsafe` in edition 2024 and forbidden workspace-wide.)
    let sandbox = Sandbox::new(quick()).expect("sandbox");
    let outcome = sandbox.run("/bin/sh", &["-c", "env"]).await.expect("run");
    let seen = match outcome {
        Outcome::Ok { stdout, .. } => String::from_utf8_lossy(&stdout).to_string(),
        other => panic!("expected success, got {other:?}"),
    };

    // What the runner sets deliberately, plus what a POSIX shell adds to its own environment.
    const ALLOWED: &[&str] = &[
        "PATH", "HOME", "TMPDIR", "LC_ALL", "PWD", "SHLVL", "OLDPWD", "_",
    ];
    let leaked: Vec<&str> = seen
        .lines()
        .filter_map(|line| line.split('=').next())
        .filter(|key| !key.is_empty() && !ALLOWED.contains(key))
        .collect();
    assert!(
        leaked.is_empty(),
        "these variables reached the subprocess and should not have: {leaked:?}\nfull env:\n{seen}"
    );

    // And the parent genuinely had something worth leaking, so the assertion above is not
    // vacuous.
    assert!(
        std::env::vars().count() > ALLOWED.len(),
        "the test process has almost no environment, so this proves little"
    );
}

#[tokio::test]
async fn output_is_capped_so_a_chatty_process_cannot_exhaust_memory() {
    // ffmpeg on a malformed input can emit warnings indefinitely. Buffering all of it is how a
    // worker dies of OOM while the subprocess is behaving exactly as designed.
    let sandbox = Sandbox::new(Limits {
        max_output_bytes: 4096,
        wall_clock: Duration::from_secs(5),
        ..quick()
    })
    .expect("sandbox");

    // Finite: a genuinely endless printer keeps the readers busy until the wall clock fires, which
    // is correct behaviour but tests the deadline rather than the cap. The next test covers that.
    let outcome = sandbox
        .run(
            "/bin/sh",
            &[
                "-c",
                "i=0; while [ $i -lt 20000 ]; do echo chatter; i=$((i+1)); done",
            ],
        )
        .await
        .expect("run");
    let (stdout, truncated) = match &outcome {
        Outcome::Ok {
            stdout, truncated, ..
        }
        | Outcome::Failed {
            stdout, truncated, ..
        }
        | Outcome::Killed {
            stdout, truncated, ..
        }
        | Outcome::TimedOut {
            stdout, truncated, ..
        } => (stdout.clone(), *truncated),
    };
    assert!(
        stdout.len() <= 4096 * 2,
        "captured {} bytes against a 4096-byte cap",
        stdout.len()
    );
    assert!(truncated, "the caller must be told output was cut");
}

#[tokio::test]
async fn a_timeout_still_returns_what_the_tool_printed_before_it_stalled() {
    // "Timed out" with no output is unactionable. ffmpeg's last lines before a stall are what say
    // which stream it choked on, so the partial capture is the diagnosis.
    let sandbox = Sandbox::new(Limits {
        wall_clock: Duration::from_millis(700),
        ..quick()
    })
    .expect("sandbox");

    let outcome = sandbox
        .run(
            "/bin/sh",
            &["-c", "echo starting; echo detail >&2; sleep 30"],
        )
        .await
        .expect("run");
    match outcome {
        Outcome::TimedOut { stdout, stderr, .. } => {
            assert!(
                String::from_utf8_lossy(&stdout).contains("starting"),
                "stdout before the stall must survive"
            );
            assert!(
                String::from_utf8_lossy(&stderr).contains("detail"),
                "and so must stderr — that is where the diagnosis is"
            );
        }
        other => panic!("expected TimedOut, got {other:?}"),
    }
}

#[tokio::test]
async fn a_failing_command_reports_its_status_and_stderr() {
    // "Deriving failed" with no detail is unactionable; ffmpeg's stderr is the whole diagnosis.
    let sandbox = Sandbox::new(quick()).expect("sandbox");
    let outcome = sandbox
        .run("/bin/sh", &["-c", "echo boom >&2; exit 3"])
        .await
        .expect("run");
    match outcome {
        Outcome::Failed { code, stderr, .. } => {
            assert_eq!(code, Some(3));
            assert!(
                String::from_utf8_lossy(&stderr).contains("boom"),
                "stderr must be preserved"
            );
        }
        other => panic!("expected Failed, got {other:?}"),
    }
}

#[tokio::test]
async fn a_path_outside_the_sandbox_is_refused() {
    let sandbox = Sandbox::new(quick()).expect("sandbox");
    for hostile in [
        "../escape",
        "a/../../escape",
        "/etc/passwd",
        "/tmp/absolute",
        "",
    ] {
        assert!(
            sandbox.resolve(hostile).is_err(),
            "{hostile:?} must not resolve inside the sandbox"
        );
    }
    let inside = sandbox
        .resolve("frame-001.png")
        .expect("a plain name is fine");
    assert!(inside.starts_with(sandbox.path()));
}

#[tokio::test]
async fn a_symlink_pointing_out_of_the_sandbox_is_refused() {
    // The interesting case: the *name* is innocent and the target is not. A media tool told to
    // write to `out.png` would follow the link and overwrite whatever it points at.
    let sandbox = Sandbox::new(quick()).expect("sandbox");
    let link = sandbox.path().join("out.png");
    std::os::unix::fs::symlink("/etc/hosts", &link).expect("symlink");

    assert!(
        sandbox.resolve("out.png").is_err(),
        "a symlink out of the sandbox must be refused, not followed"
    );
}

#[tokio::test]
async fn the_working_directory_is_the_sandbox_so_relative_writes_land_inside() {
    let sandbox = Sandbox::new(quick()).expect("sandbox");
    sandbox
        .run("/bin/sh", &["-c", "echo hello > relative.txt"])
        .await
        .expect("run");
    assert!(
        sandbox.path().join("relative.txt").exists(),
        "a tool writing a relative path must land in the sandbox, not in the worker's cwd"
    );
}

#[tokio::test]
async fn the_directory_is_removed_when_the_sandbox_is_dropped() {
    let path = {
        let sandbox = Sandbox::new(quick()).expect("sandbox");
        sandbox
            .run("/bin/sh", &["-c", "echo x > leftover"])
            .await
            .expect("run");
        sandbox.path().to_path_buf()
    };
    assert!(
        !path.exists(),
        "a per-job temp dir that outlives its job fills the disk one derivative at a time"
    );
}

#[test]
fn unenforceable_limits_are_declared_rather_than_silently_ignored() {
    // Measured, not assumed: on darwin `ulimit -v` is rejected by the shell outright and a
    // 200 MB allocation runs unimpeded under a 50 MB cap, while on Linux the same cap makes
    // `dd` fail with "out of memory". A runner that accepted the limit on both would leave a
    // protection that exists only in production — and nobody would notice until it mattered.
    let caps = sandbox::capabilities();
    if cfg!(target_os = "linux") {
        assert!(caps.address_space_limit, "Linux enforces RLIMIT_AS");
    } else {
        assert!(
            !caps.address_space_limit,
            "no non-Linux platform here enforces it, so it must not be claimed"
        );
    }
    assert!(caps.cpu_limit, "POSIX ulimit -t works everywhere tested");
    assert!(caps.file_size_limit, "as does -f");
}

#[test]
fn requesting_a_limit_the_platform_cannot_enforce_is_reported() {
    let sandbox = Sandbox::new(Limits {
        address_space_bytes: Some(64 * 1024 * 1024),
        ..quick()
    })
    .expect("sandbox");

    let unenforced = sandbox.unenforced();
    if cfg!(target_os = "linux") {
        assert!(unenforced.is_empty(), "Linux can honour all of these");
    } else {
        assert!(
            unenforced.iter().any(|u| u.contains("address space")),
            "the caller must be able to see which limits are not real here: {unenforced:?}"
        );
    }
}
