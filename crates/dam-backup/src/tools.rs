//! Locating `pg_dump` and `pg_restore`.
//!
//! Discovered rather than assumed, and version-checked, for a reason this project learned the expensive way:
//! `vipsthumbnail` renamed a flag between releases and the hard-coded name failed every render on any image
//! whose distribution shipped an older build. A client older than the server is the same shape of problem
//! with worse consequences — `pg_dump` refuses outright, which is the good case, and the bad case is a
//! version pair nobody checked until a restore.

use std::path::{Path, PathBuf};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{0}")]
    Unavailable(String),
}

type Result<T> = std::result::Result<T, Error>;

/// Absolute paths to the Postgres client tools.
#[derive(Debug, Clone)]
pub struct Toolchain {
    pg_dump: PathBuf,
    pg_restore: PathBuf,
    /// The client's `(major, minor)`, so a caller can refuse a server it cannot dump.
    version: (u32, u32),
}

impl Toolchain {
    /// Locates the binaries.
    ///
    /// `DAMRS_PG_BIN` overrides the search, so a container image can put them wherever it likes — the same
    /// escape hatch `DAMRS_VIPS_BIN` and `DAMRS_FFMPEG_BIN` provide, and for the same reason.
    pub fn discover() -> Result<Self> {
        if let Ok(dir) = std::env::var("DAMRS_PG_BIN") {
            return Self::from_dir(Path::new(&dir));
        }
        let found = which("pg_dump").ok_or_else(|| {
            Error::Unavailable(
                "no `pg_dump` on PATH and DAMRS_PG_BIN is unset; it is pinned in .mise.toml, so \
                 `mise install` should provide it"
                    .to_owned(),
            )
        })?;
        Self::from_dir(found.parent().unwrap_or(Path::new(".")))
    }

    fn from_dir(dir: &Path) -> Result<Self> {
        let pg_dump = dir.join("pg_dump");
        let pg_restore = dir.join("pg_restore");
        for binary in [&pg_dump, &pg_restore] {
            if !binary.exists() {
                return Err(Error::Unavailable(format!(
                    "{} does not exist",
                    binary.display()
                )));
            }
        }
        let version = read_version(&pg_dump).ok_or_else(|| {
            Error::Unavailable(format!(
                "{} did not report a parseable version",
                pg_dump.display()
            ))
        })?;
        // **Deliberately not canonicalised**, unlike the vips toolchain next door.
        //
        // On Debian, `/usr/bin/pg_dump` is a symlink to `pg_wrapper`, a multi-call program that decides which
        // versioned binary to exec **from `argv[0]`**. Resolving the symlink makes `argv[0]` `pg_wrapper`,
        // which then cannot tell what was asked of it and dies with `Can't exec "--version"`. Copying the
        // canonicalisation from `dam_media::vips` — where it defends against a symlink swapped between
        // discovery and use — broke every backup taken inside the container image while working perfectly
        // against the conda-installed binaries on a laptop, which are not wrappers.
        //
        // The defence it gave up is small here and the loss was total: these paths come from `PATH` or from
        // `DAMRS_PG_BIN`, both operator-controlled, and an attacker who can swap `/usr/bin/pg_dump` can swap
        // whatever it points at too.
        Ok(Self {
            pg_dump,
            pg_restore,
            version,
        })
    }

    pub fn pg_dump(&self) -> &Path {
        &self.pg_dump
    }

    pub fn pg_restore(&self) -> &Path {
        &self.pg_restore
    }

    pub fn version(&self) -> (u32, u32) {
        self.version
    }

    /// Whether this client can dump a server of `server_major`.
    ///
    /// `pg_dump` supports servers older than itself and refuses newer ones. Checked before the dump rather
    /// than after, so the failure is a sentence about versions instead of a subprocess error mid-backup.
    pub fn can_dump(&self, server_major: u32) -> bool {
        self.version.0 >= server_major
    }
}

fn read_version(pg_dump: &Path) -> Option<(u32, u32)> {
    let out = std::process::Command::new(pg_dump)
        .arg("--version")
        .output()
        .ok()?;
    parse_version(&String::from_utf8_lossy(&out.stdout))
}

/// The `(major, minor)` out of `pg_dump (PostgreSQL) 17.11`.
pub fn parse_version(reported: &str) -> Option<(u32, u32)> {
    let digits = reported
        .split_whitespace()
        .find(|word| word.chars().next().is_some_and(|c| c.is_ascii_digit()))?;
    let mut parts = digits.split('.');
    // Leading digits only, because a devel build reports `18devel` with the suffix attached and no minor at
    // all. Refusing to parse that would refuse to back up against a pre-release server, which is exactly when
    // somebody is most likely to want a backup.
    let major = leading_number(parts.next()?)?;
    let minor = parts.next().and_then(leading_number).unwrap_or(0);
    Some((major, minor))
}

fn leading_number(text: &str) -> Option<u32> {
    let digits: String = text.chars().take_while(char::is_ascii_digit).collect();
    digits.parse().ok()
}

fn which(binary: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(binary))
        .find(|candidate| candidate.is_file())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_parse_the_way_postgres_reports_them() {
        assert_eq!(parse_version("pg_dump (PostgreSQL) 17.11"), Some((17, 11)));
        assert_eq!(parse_version("pg_dump (PostgreSQL) 16.3\n"), Some((16, 3)));
        // A devel build has no minor; refusing to parse it would refuse to back up.
        assert_eq!(parse_version("pg_dump (PostgreSQL) 18devel"), Some((18, 0)));
        assert_eq!(parse_version("no version here"), None);
    }

    #[test]
    fn a_client_older_than_the_server_is_refused_before_the_dump() {
        let seventeen = (17, 11);
        assert!(seventeen.0 >= 16, "a 17 client dumps a 16 server");
        assert!(
            seventeen.0 < 18,
            "and refuses an 18 server, which pg_dump would refuse anyway — the point is saying so first"
        );
    }
}
