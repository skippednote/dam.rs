//! Scanning uploads, by talking to `clamd` directly (M1's "virus scan").
//!
//! ## Why the socket rather than the `clamscan` binary
//!
//! `clamscan` loads the whole signature database on every invocation — seconds of CPU and hundreds of
//! megabytes of RSS per file. `clamd` holds it in memory and answers over a socket in milliseconds, and its
//! `INSTREAM` command takes the bytes directly so nothing has to be written to a shared filesystem first.
//! Shelling out would also mean a temporary file containing content we have not yet decided to accept.
//!
//! The protocol is four lines of framing, which is why there is no dependency here: `zINSTREAM\0`, then
//! length-prefixed chunks, then a zero length, then one line of reply.
//!
//! ## An unreachable scanner refuses the upload
//!
//! Fail closed, and as a **transient** error so the queue retries. The upload stays in staging and finalises
//! when `clamd` comes back; nothing is lost and nothing unscanned becomes an asset. A configurable fail-open
//! was the alternative and it is a footgun: the setting that lets ingest continue during an outage is the
//! setting that is still switched on a year later.
//!
//! ## Files too large to scan are accepted, and that is a real hole
//!
//! `clamd` has a `StreamMaxLength` (100 MB by default) and refuses anything past it. A DAM whose whole purpose
//! is video masters and layered PSDs cannot refuse every large file, so those are accepted with the reason
//! recorded. Stated plainly rather than buried: **large uploads are not virus-scanned.** The mitigations that
//! actually apply to them are elsewhere — nothing is executed, derivatives are rendered in a sandbox with the
//! environment cleared, and delivery is a signed redirect rather than a served file.

use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

/// The default `clamd` stream ceiling. Matches `StreamMaxLength` out of the box.
pub const DEFAULT_MAX_SCAN_BYTES: u64 = 100 * 1024 * 1024;

/// How long to wait on the scanner before treating it as unreachable.
///
/// Generous, because a first scan after `clamd` reloads its signatures legitimately blocks. Shorter than the
/// job lease, so a stuck scanner surfaces as a retryable failure rather than a lost lease.
const TIMEOUT: Duration = Duration::from_secs(30);

/// `clamd` accepts chunks up to `StreamMaxLength`; 64 KiB keeps memory flat and syscalls reasonable.
const CHUNK: usize = 64 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The scanner could not be reached or did not answer. **Retry this.**
    #[error("clamd at {address} is unreachable: {source}")]
    Unreachable {
        address: String,
        source: std::io::Error,
    },
    /// It answered something this code does not understand, which is a version or configuration mismatch
    /// rather than a verdict — and must not be read as "clean".
    #[error("clamd said {0:?}, which is not a verdict this understands")]
    Unintelligible(String),
}

/// What the scanner concluded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    Clean,
    /// The signature name, which is what belongs in an audit trail and a support conversation.
    Infected(String),
    /// Past the scanner's stream ceiling. Accepted by policy; see the module docs.
    TooLarge {
        bytes: u64,
        limit: u64,
    },
}

/// A `clamd` endpoint.
#[derive(Debug, Clone)]
pub struct Scanner {
    address: String,
    max_bytes: u64,
}

impl Scanner {
    pub fn new(address: impl Into<String>, max_bytes: u64) -> Self {
        Self {
            address: address.into(),
            max_bytes,
        }
    }

    pub fn address(&self) -> &str {
        &self.address
    }

    /// Scans `bytes`.
    ///
    /// Takes a slice rather than a reader because the caller already holds the object: the store's `get`
    /// returns the whole body, and a streaming variant would only matter above the size ceiling where this
    /// declines to scan at all.
    pub async fn scan(&self, bytes: &[u8]) -> Result<Verdict, Error> {
        let size = bytes.len() as u64;
        if size > self.max_bytes {
            return Ok(Verdict::TooLarge {
                bytes: size,
                limit: self.max_bytes,
            });
        }

        let reply = tokio::time::timeout(TIMEOUT, self.instream(bytes))
            .await
            .map_err(|_| Error::Unreachable {
                address: self.address.clone(),
                source: std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    format!("no reply within {}s", TIMEOUT.as_secs()),
                ),
            })?
            .map_err(|source| Error::Unreachable {
                address: self.address.clone(),
                source,
            })?;

        parse(&reply)
    }

    async fn instream(&self, bytes: &[u8]) -> std::io::Result<String> {
        let mut stream = TcpStream::connect(&self.address).await?;
        // The `z` prefix asks for NUL-terminated replies, which is what makes the response framing
        // unambiguous — the newline form (`n`) cannot be distinguished from a signature name containing one.
        stream.write_all(b"zINSTREAM\0").await?;

        for chunk in bytes.chunks(CHUNK) {
            stream
                .write_all(&(chunk.len() as u32).to_be_bytes())
                .await?;
            stream.write_all(chunk).await?;
        }
        // A zero-length chunk is end of stream. Without it clamd waits forever and this times out, which is
        // the one framing mistake that presents as "the scanner is down".
        stream.write_all(&0u32.to_be_bytes()).await?;
        stream.flush().await?;

        let mut reply = Vec::new();
        stream.read_to_end(&mut reply).await?;
        Ok(String::from_utf8_lossy(&reply)
            .trim_matches(['\0', '\n', ' '])
            .to_owned())
    }
}

/// Reads one `clamd` reply.
///
/// Anything unrecognised is an error rather than a default. A parser that fell through to `Clean` would turn
/// every future protocol change into a silent bypass of the only thing standing between an upload and the
/// library — which is the worst possible direction for this particular function to be wrong in.
fn parse(reply: &str) -> Result<Verdict, Error> {
    if reply.ends_with("OK") && !reply.contains("FOUND") {
        return Ok(Verdict::Clean);
    }
    if let Some(rest) = reply.strip_suffix(" FOUND") {
        // `stream: Eicar-Signature FOUND` — the signature is between the colon and the suffix.
        let signature = rest
            .rsplit_once(": ")
            .map_or(rest, |(_, name)| name)
            .trim()
            .to_owned();
        return Ok(Verdict::Infected(signature));
    }
    Err(Error::Unintelligible(reply.to_owned()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncReadExt;
    use tokio::net::TcpListener;

    #[test]
    fn a_clean_reply_is_clean() {
        assert_eq!(parse("stream: OK").expect("parse"), Verdict::Clean);
    }

    #[test]
    fn an_infection_carries_its_signature() {
        assert_eq!(
            parse("stream: Win.Test.EICAR_HDB-1 FOUND").expect("parse"),
            Verdict::Infected("Win.Test.EICAR_HDB-1".to_owned())
        );
    }

    /// The case that matters most: an unrecognised reply must never read as clean.
    #[test]
    fn anything_else_is_an_error_rather_than_a_pass() {
        for reply in ["", "stream: ERROR", "INSTREAM size limit exceeded", "wat"] {
            assert!(
                matches!(parse(reply), Err(Error::Unintelligible(_))),
                "{reply:?} must not be read as a verdict"
            );
        }
    }

    /// A reply containing both words is an infection, not a pass.
    ///
    /// Deliberate ordering in `parse`: a naive `ends_with("OK")` check placed first would mis-read a
    /// hypothetical signature name ending in OK.
    #[test]
    fn found_wins_over_ok() {
        assert!(matches!(
            parse("stream: Something.OK FOUND").expect("parse"),
            Verdict::Infected(_)
        ));
    }

    #[tokio::test]
    async fn oversized_input_is_declined_without_contacting_the_scanner() {
        // Address is deliberately unroutable: reaching it would be the failure.
        let scanner = Scanner::new("127.0.0.1:1", 10);
        let verdict = scanner
            .scan(&[0u8; 64])
            .await
            .expect("no connection needed");
        assert_eq!(
            verdict,
            Verdict::TooLarge {
                bytes: 64,
                limit: 10
            }
        );
    }

    #[tokio::test]
    async fn an_unreachable_scanner_is_an_error_not_a_pass() {
        let scanner = Scanner::new("127.0.0.1:1", DEFAULT_MAX_SCAN_BYTES);
        assert!(matches!(
            scanner.scan(b"hello").await,
            Err(Error::Unreachable { .. })
        ));
    }

    /// A fake `clamd`, so the framing is tested rather than assumed.
    ///
    /// Asserts the wire format from the server's side: the command, the length prefixes, and the zero
    /// terminator. Getting the terminator wrong is the mistake that presents as "the scanner is down".
    async fn fake_clamd(reply: &'static str) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let address = listener.local_addr().expect("addr").to_string();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("accept");
            let mut command = [0u8; 10];
            socket.read_exact(&mut command).await.expect("command");
            assert_eq!(&command, b"zINSTREAM\0");

            // Read length-prefixed chunks until the zero terminator.
            loop {
                let mut length = [0u8; 4];
                socket.read_exact(&mut length).await.expect("length");
                let length = u32::from_be_bytes(length) as usize;
                if length == 0 {
                    break;
                }
                let mut body = vec![0u8; length];
                socket.read_exact(&mut body).await.expect("body");
            }
            socket
                .write_all(format!("{reply}\0").as_bytes())
                .await
                .expect("reply");
        });
        address
    }

    #[tokio::test]
    async fn the_wire_protocol_round_trips() {
        let address = fake_clamd("stream: OK").await;
        let scanner = Scanner::new(address, DEFAULT_MAX_SCAN_BYTES);
        // Larger than one chunk, so the chunking loop is exercised rather than just the single-write case.
        let payload = vec![7u8; CHUNK + 1234];
        assert_eq!(scanner.scan(&payload).await.expect("scan"), Verdict::Clean);
    }

    #[tokio::test]
    async fn an_infected_stream_is_reported_with_its_signature() {
        let address = fake_clamd("stream: Win.Test.EICAR_HDB-1 FOUND").await;
        let scanner = Scanner::new(address, DEFAULT_MAX_SCAN_BYTES);
        assert_eq!(
            scanner
                .scan(b"X5O!P%@AP[4\\PZX54(P^)7CC)7}$EICAR")
                .await
                .expect("scan"),
            Verdict::Infected("Win.Test.EICAR_HDB-1".to_owned())
        );
    }
}
