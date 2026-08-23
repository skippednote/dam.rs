//! Writes a JPEG carrying a signed C2PA manifest, for driving the ingest path by hand.
//!
//! An example rather than a test fixture: it exists so somebody can produce a credentialed file to upload
//! through a running deployment, which is the only way to see the ingest verification actually fire.
//!
//! The identity is ephemeral, so the result verifies as `untrusted` — cryptographically sound and chaining to
//! nobody. That is the correct outcome for a self-signed certificate and exactly what a real deployment's
//! `provenance_manifests` row should say about one.
// `expect` throughout, and deliberately: an operator tool that panics with the reason is more useful than
// one that returns an opaque exit code, and there is no caller to propagate an error to.
#![allow(clippy::expect_used)]

fn main() {
    let path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "credentialed.jpg".to_owned());

    let mut plain = std::io::Cursor::new(Vec::new());
    let mut image = image::RgbImage::new(320, 240);
    for (x, y, pixel) in image.enumerate_pixels_mut() {
        *pixel = image::Rgb([(x % 256) as u8, (y % 256) as u8, 128]);
    }
    image
        .write_to(&mut plain, image::ImageFormat::Jpeg)
        .expect("encode");

    let identity = dam_media::provenance::SigningIdentity::ephemeral(
        dam_core::config::Environment::Development,
        "damrs-example.local",
    )
    .expect("ephemeral identity");

    let signed = dam_media::provenance::sign(
        &identity,
        &plain.into_inner(),
        "image/jpeg",
        dam_media::provenance::Claim {
            claim_generator: "damrs example 1.0".to_owned(),
            provenance: dam_media::provenance::Provenance::Created(
                dam_media::provenance::Origin::DigitalCapture,
            ),
            actions: vec![],
        },
    )
    .expect("sign");

    std::fs::write(&path, &signed.bytes).expect("write");
    println!("{path}\t{} bytes", signed.bytes.len());
}
