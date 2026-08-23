//! Verifies a file's content credentials and prints what they say.
//!
//! An operator tool, and the answer to "does this derivative actually carry a chained credential" — which a
//! database row cannot answer, because a row records what we believe we wrote and this reads what is in the
//! bytes.
//!
//! `expect` throughout, deliberately: a tool that panics with the reason beats one returning an opaque exit
//! code, and there is no caller to propagate to.
#![allow(clippy::expect_used)]

fn main() {
    let path = std::env::args()
        .nth(1)
        .expect("usage: verify_file <path> [mime]");
    let mime = std::env::args()
        .nth(2)
        .unwrap_or_else(|| "image/jpeg".to_owned());
    let bytes = std::fs::read(&path).expect("read");

    let verified = dam_media::provenance::verify(&mime, &bytes).expect("verify");
    println!("state         {}", verified.state.as_validation_state());
    println!(
        "signer        {}",
        verified.signer_cn.unwrap_or_else(|| "-".to_owned())
    );
    println!(
        "generator     {}",
        verified.claim_generator.unwrap_or_else(|| "-".to_owned())
    );
    // The number that distinguishes a continued chain from a fresh claim about a file that appeared from
    // nowhere. Zero on an original; non-zero on a derivative that names its parent.
    println!("ingredients   {}", verified.ingredient_count);
    println!("actions       {}", verified.actions.join(", "));
    println!("source types  {}", verified.source_types.join(", "));
}
