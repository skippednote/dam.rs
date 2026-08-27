//! Signs a webhook body from the command line, for driving a receiver.
//!
//! The companion to `webhook_vectors`. Those pin the bytes offline; this posts a live delivery at the
//! current time, which is the only way to exercise a receiver's freshness window and its header handling
//! together with its digest check. A fixture cannot test that a receiver reads the right header.
//!
//! ```text
//! cargo run -p dam-connect --example sign_webhook -- <secret> <body>
//! ```
//!
//! Prints the timestamp and the signature, one per line, so a shell can read them into variables.

use dam_connect::webhooks::sign;
use dam_core::Secret;

fn main() -> std::process::ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [secret, body] = args.as_slice() else {
        eprintln!("usage: sign_webhook <secret> <body>");
        return std::process::ExitCode::from(2);
    };

    let timestamp = chrono::Utc::now().timestamp();
    let signature = sign(&Secret::new(secret.clone()), timestamp, body.as_bytes());

    println!("{timestamp}");
    println!("{signature}");
    std::process::ExitCode::SUCCESS
}
