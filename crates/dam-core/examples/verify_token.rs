//! Verifies a delivery token from the command line, for checking a second implementation.
//!
//! The companion to `signing_vectors`. Those vectors prove another language can *produce* the right bytes
//! for a fixed claim; this proves a token that language produced from its own configuration and its own
//! clock is accepted by the code that will actually verify it. Different question, and the one that catches
//! a signer whose claim assembly is wrong rather than whose encoding is — a wrong tenant, a TTL applied to
//! the wrong field, a key id that never made it out of config.
//!
//! ```text
//! cargo run -p dam-core --example verify_token -- <secret> <key-id> <token>
//! ```
//!
//! Prints the decoded claim and exits non-zero if verification fails, so it composes into a shell check.

use chrono::Utc;
use dam_core::Secret;
use dam_core::signed_url::{Keyring, verify};

fn main() -> std::process::ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [secret, key_id, token] = args.as_slice() else {
        eprintln!("usage: verify_token <secret> <key-id> <token>");
        return std::process::ExitCode::from(2);
    };

    let keyring = Keyring::single(key_id.clone(), Secret::new(secret.clone()));
    match verify(&keyring, token, Utc::now()) {
        Ok(claim) => {
            println!("verified");
            println!("  purpose      {:?}", claim.purpose);
            println!("  tenant       {}", claim.tenant_id);
            println!("  asset        {}", claim.asset_id);
            println!("  transform    {:?}", claim.transform);
            println!("  channel      {:?}", claim.channel);
            println!("  territory    {:?}", claim.territory);
            println!("  identity     {:?}", claim.identity_id);
            println!("  share link   {:?}", claim.share_link_id);
            println!("  expires      {}", claim.expires_at);
            println!("  key id       {:?}", claim.key_id);
            std::process::ExitCode::SUCCESS
        }
        Err(error) => {
            // The variant, because this is a developer tool. The HTTP layer deliberately collapses these to
            // one refusal so a forger learns nothing from a response; here the whole point is to learn
            // which half is wrong.
            eprintln!("refused: {error}");
            std::process::ExitCode::FAILURE
        }
    }
}
