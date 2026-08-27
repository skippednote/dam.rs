//! Emits the delivery-token test vectors the Drupal module is held to.
//!
//! §11.3 requires the Drupal connector to sign transform URLs **in PHP**, from the shared secret, with no
//! API call in the render path — otherwise a damrs outage is a white screen on somebody's site rather than
//! a stale page. That means a second implementation of the canonical form exists, in another language, and
//! the two have to agree byte for byte forever.
//!
//! A PHP test that only checks "the server accepted my token" would pass against a server running the same
//! wrong assumption, and would need a live server to run at all. These vectors pin the *bytes*: run this,
//! commit the output, and the PHP suite compares against it offline. Change the canonical form and this
//! output changes, the diff is visible in review, and the PHP test fails until it is updated too — which is
//! the whole point, because the alternative is discovering the mismatch when a customer's images stop
//! rendering.
//!
//! The vectors deliberately include the cases a reimplementation gets wrong: an absent optional field
//! (which must be a zero-length field and not an omission), a non-ASCII transform (so the length is bytes
//! and not characters), and an empty string next to an absent value (so the two cannot collide).
//!
//! Regenerate with:
//!
//! ```text
//! cargo run -p dam-core --example signing_vectors > integrations/drupal/tests/fixtures/signing_vectors.json
//! ```

use chrono::DateTime;
use dam_core::Secret;
use dam_core::signed_url::{DeliveryClaim, Keyring, Purpose, sign};
use uuid::Uuid;

/// The secret every vector is signed with. A fixed value, because a vector signed with a random key proves
/// nothing to a second implementation that cannot know the key.
const SECRET: &str = "vector-secret-do-not-use-in-production";
const KEY_ID: &str = "k1";

fn uuid(hex: &str) -> Uuid {
    #[allow(clippy::expect_used)]
    Uuid::parse_str(hex).expect("a literal uuid in this file")
}

struct Case {
    name: &'static str,
    why: &'static str,
    claim: DeliveryClaim,
}

fn main() {
    let tenant = uuid("11111111-2222-3333-4444-555555555555");
    let asset = uuid("66666666-7777-8888-9999-aaaaaaaaaaaa");
    let identity = uuid("bbbbbbbb-cccc-dddd-eeee-ffffffffffff");
    let share = uuid("01234567-89ab-cdef-0123-456789abcdef");
    #[allow(clippy::expect_used)]
    let expires = DateTime::from_timestamp(1_800_000_000, 0).expect("a literal timestamp");

    let base = DeliveryClaim {
        tenant_id: tenant,
        asset_id: asset,
        purpose: Purpose::Distribution,
        transform: "w=800,fmt=webp".to_owned(),
        channel: "web".to_owned(),
        territory: "GB".to_owned(),
        identity_id: None,
        expires_at: expires,
        share_link_id: None,
        key_id: KEY_ID.to_owned(),
    };

    let cases = [
        Case {
            name: "minimal",
            why: "both optional fields absent, which must encode as zero-length fields rather than nothing",
            claim: base.clone(),
        },
        Case {
            name: "internal_preview",
            why: "the purpose byte is 2, and a reader that defaults it would serve a preview as a download",
            claim: DeliveryClaim {
                purpose: Purpose::InternalPreview,
                ..base.clone()
            },
        },
        Case {
            name: "with_identity",
            why: "an optional uuid present, so its 16 raw bytes sit where the empty field was",
            claim: DeliveryClaim {
                identity_id: Some(identity),
                ..base.clone()
            },
        },
        Case {
            name: "with_share_link",
            why: "the second optional, to catch an implementation that writes them in the wrong order",
            claim: DeliveryClaim {
                identity_id: Some(identity),
                share_link_id: Some(share),
                ..base.clone()
            },
        },
        Case {
            name: "empty_transform_and_territory",
            why: "an empty string beside an absent optional; if they collided one signature would cover both",
            claim: DeliveryClaim {
                transform: String::new(),
                territory: String::new(),
                ..base.clone()
            },
        },
        Case {
            name: "non_ascii_transform",
            why: "the length prefix counts bytes, not characters — 'é' is two and a char count would shift \
                  every field after it",
            claim: DeliveryClaim {
                transform: "w=800,label=café,fmt=avif".to_owned(),
                ..base.clone()
            },
        },
        Case {
            name: "long_transform",
            why: "over 255 bytes, which is why the length is a u32; a u8 length would truncate and collide",
            claim: DeliveryClaim {
                transform: "w=800,".repeat(60),
                ..base.clone()
            },
        },
    ];

    let keyring = Keyring::single(KEY_ID, Secret::new(SECRET.to_owned()));

    println!("{{");
    println!(
        "  \"comment\": \"Generated by `cargo run -p dam-core --example signing_vectors`. Do not edit by hand.\","
    );
    println!("  \"secret\": {},", json_string(SECRET));
    println!("  \"key_id\": {},", json_string(KEY_ID));
    println!("  \"cases\": [");
    let last = cases.len() - 1;
    for (index, case) in cases.iter().enumerate() {
        #[allow(clippy::expect_used)]
        let token = sign(&keyring, &case.claim).expect("the keyring has a signing key");
        println!("    {{");
        println!("      \"name\": {},", json_string(case.name));
        println!("      \"why\": {},", json_string(case.why));
        println!("      \"claim\": {{");
        println!("        \"purpose\": {},", purpose_name(case.claim.purpose));
        println!(
            "        \"tenant_id\": {},",
            json_string(&case.claim.tenant_id.to_string())
        );
        println!(
            "        \"asset_id\": {},",
            json_string(&case.claim.asset_id.to_string())
        );
        println!(
            "        \"transform\": {},",
            json_string(&case.claim.transform)
        );
        println!("        \"channel\": {},", json_string(&case.claim.channel));
        println!(
            "        \"territory\": {},",
            json_string(&case.claim.territory)
        );
        println!(
            "        \"identity_id\": {},",
            json_optional_uuid(case.claim.identity_id)
        );
        println!(
            "        \"share_link_id\": {},",
            json_optional_uuid(case.claim.share_link_id)
        );
        println!(
            "        \"expires_at\": {},",
            case.claim.expires_at.timestamp()
        );
        println!("        \"key_id\": {}", json_string(&case.claim.key_id));
        println!("      }},");
        println!("      \"token\": {}", json_string(&token));
        println!("    }}{}", if index == last { "" } else { "," });
    }
    println!("  ]");
    println!("}}");
}

fn purpose_name(purpose: Purpose) -> String {
    // Named rather than numbered in the fixture, so the PHP side has to map the name to the byte itself and
    // a wrong mapping fails rather than being copied across.
    match purpose {
        Purpose::Distribution => json_string("distribution"),
        Purpose::InternalPreview => json_string("internal_preview"),
    }
}

fn json_optional_uuid(value: Option<Uuid>) -> String {
    match value {
        Some(id) => json_string(&id.to_string()),
        None => "null".to_owned(),
    }
}

/// Minimal JSON string escaping, so this example needs no serde dependency for six fields.
fn json_string(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for ch in value.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}
