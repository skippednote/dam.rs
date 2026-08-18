//! C2PA content credentials: verify, preserve, re-sign (D13, closing GAPS G1).
//!
//! Every tool in the derivative pipeline — libvips, ffmpeg, pdfium — strips embedded metadata by
//! default. So the pipeline does not accidentally lose provenance; it destroys it reliably, and a DAM is
//! the system of record and the worst place in the chain for that to happen. This module is what puts it
//! back, and it is why D13 calls a credential-stripping pipeline *wrong* rather than incomplete.
//!
//! ## The state mapping, which is easy to get backwards
//!
//! c2pa-rs reports three validation states and our schema stores four. The correspondence is not
//! one-to-one and the difference is a security property:
//!
//! | c2pa-rs | ours | meaning |
//! |---|---|---|
//! | — (no manifest) | `absent` | never had credentials. The common case. |
//! | `Trusted` | `valid` | verifies **and** chains to a known root. |
//! | `Valid` | `untrusted` | signature verifies; nobody recognises the signer. |
//! | `Invalid` | `invalid` | the binding fails. Possible tampering. |
//!
//! Collapsing `Valid` into our `valid` would display "credentials verified" for a manifest anyone can
//! mint. Collapsing `absent` into `invalid` would bury every real tamper signal under every ordinary
//! photograph that never met a C2PA-aware tool.
//!
//! ## What is deliberately not here
//!
//! Remote manifest fetching. The `fetch_remote_manifests` feature is off (see the workspace manifest):
//! it makes the reader dereference a URL found inside an uploaded file, which on an ingest path is a
//! server-side request forgery primitive handed to anyone who can upload.
//!
//! ## Signing runs in this process, not the sandbox
//!
//! Unlike libvips and ffmpeg, this is a Rust library parsing into Rust types with no `unsafe` in the
//! path and no shell-out, so §16's subprocess containment buys much less here. Verification does read
//! hostile bytes, so it is bounded by the caller's own limits — an oversized or malformed manifest fails
//! to parse rather than being handed to a C library.

use c2pa::{Builder, Context, Reader, Signer, ValidationState, assertions};
use dam_core::config::Environment;
use std::io::Cursor;
use std::sync::{Arc, LazyLock};

/// The verification and signing context, built once and shared.
///
/// Explicit rather than ambient. The convenience constructors that read *thread-local* settings are
/// deprecated for a reason that matters more in a server than in a CLI: on a thread pool, one request's
/// settings become the next request's defaults, so a trust list configured for one tenant's verification
/// could silently apply to another's. An `Arc<Context>` shared by every call has no such coupling.
///
/// Defaults today. When a trust list arrives it is configured here, in one place, rather than at each
/// call site.
static CONTEXT: LazyLock<Arc<Context>> = LazyLock::new(|| Arc::new(Context::new()));

#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The file could not be read at all — distinct from "has no credentials", which is
    /// [`ProvenanceState::Absent`] and not an error.
    #[error("could not read content credentials: {0}")]
    Unreadable(String),

    /// Refused rather than failed. Today this is only decision C2PA 2's test-certificate rule, and it
    /// is a separate variant because it is a *policy* refusal: it must never be retried, logged as a
    /// transient fault, or fall back to signing with something else.
    #[error("refusing to sign: {0}")]
    Refused(String),

    #[error("signing failed: {0}")]
    Signing(String),
}

type Result<T> = std::result::Result<T, Error>;

/// The `claim_generator` this deployment signs with.
///
/// One identity per deployment (decision C2PA 1): a signature attests to who performed the transform,
/// and that is this service rather than the customer whose asset it was. Per-tenant certificates would
/// also mean a CA-issued certificate per tenant, which is operationally infeasible.
pub fn claim_generator() -> String {
    format!("damrs/{}", env!("CARGO_PKG_VERSION"))
}

/// The provenance verdict, matching `assets.provenance_state`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProvenanceState {
    /// No credentials. Not a problem — most assets have never met a C2PA-aware tool.
    Absent,
    /// Verifies and chains to a root the trust list knows.
    Valid,
    /// The signature verifies; the signer is not recognised. Displayed differently from [`Self::Valid`]
    /// on purpose — a manifest anyone can mint must not read as one an authority vouched for.
    Untrusted,
    /// The binding fails. Possible tampering, and the state the suspect index exists to surface.
    Invalid,
}

impl ProvenanceState {
    /// The value stored in `assets.provenance_state` / `provenance_manifests.validation_state`.
    ///
    /// Spelled out rather than derived from `Debug`, because a rename of a variant must not silently
    /// change a database value that a CHECK constraint and a partial index both depend on.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Absent => "absent",
            Self::Valid => "valid",
            Self::Untrusted => "untrusted",
            Self::Invalid => "invalid",
        }
    }
}

/// What verification found.
#[derive(Debug, Clone)]
pub struct Verification {
    pub state: ProvenanceState,
    /// The detached manifest, for storage as its own object.
    ///
    /// Separate from the asset because §2 keeps metadata hot while masters tier to Deep Archive: a
    /// credential that lived only inside the original's bytes would become unverifiable the moment the
    /// original was archived, which is the whole reason the schema stores an `object_key` here.
    ///
    /// Present even when the state is [`ProvenanceState::Invalid`] — decision C2PA 3, and D13's
    /// prohibition on stripping. A broken chain is the customer's evidence of what broke.
    pub manifest: Option<Vec<u8>>,
    pub signer_cn: Option<String>,
    pub claim_generator: Option<String>,
    pub spec_version: Option<String>,
    /// Ingredients on the active manifest. Non-zero is what makes a derivative's credential a
    /// continuation of a chain rather than the start of a new one.
    pub ingredient_count: usize,
    /// Action labels on the active manifest, in order.
    pub actions: Vec<String>,
    /// Any `digitalSourceType` values found on those actions.
    ///
    /// Read back rather than assumed because this field carries the EU AI Act Article 50 marking (D15):
    /// the obligation is that synthetic content is marked *machine-readably*, and a mark nobody can
    /// read back is not a mark.
    pub source_types: Vec<String>,
    /// The validation codes, verbatim, for `provenance_manifests.validation_detail`.
    ///
    /// Stored because `invalid` on its own is unactionable: somebody has to be able to see *which*
    /// assertion failed before deciding whether an asset was tampered with or merely re-saved by a tool
    /// that mangled the manifest.
    pub detail: serde_json::Value,
}

impl Verification {
    fn absent() -> Self {
        Self {
            state: ProvenanceState::Absent,
            manifest: None,
            signer_cn: None,
            claim_generator: None,
            spec_version: None,
            ingredient_count: 0,
            actions: Vec::new(),
            source_types: Vec::new(),
            detail: serde_json::Value::Null,
        }
    }
}

/// Reads and validates the credentials embedded in `bytes`.
///
/// `Ok(state = Absent)` means there are none, which is normal. An `Err` means the file could not be
/// read at all — those are different, and conflating them would report "no credentials" for a file
/// nobody managed to parse.
pub fn verify(format: &str, bytes: &[u8]) -> Result<Verification> {
    match Reader::from_shared_context(&CONTEXT).with_stream(format, Cursor::new(bytes)) {
        Ok(reader) => Ok(from_reader(&reader, bytes, format)),
        Err(c2pa::Error::JumbfNotFound) => Ok(Verification::absent()),
        Err(error) => Err(Error::Unreadable(format!("from a {format}: {error}"))),
    }
}

/// Validates a detached manifest against the asset it was taken from.
///
/// The path an archived asset takes: the manifest is hot, the master may be in Deep Archive, and the
/// two are re-associated on demand. If this could not work, storing the manifest separately would have
/// made an archived asset's provenance permanently unverifiable.
pub fn verify_detached(format: &str, asset: &[u8], manifest: &[u8]) -> Result<Verification> {
    let reader = Reader::from_shared_context(&CONTEXT)
        .with_manifest_data_and_stream(manifest, format, Cursor::new(asset))
        .map_err(|error| Error::Unreadable(format!("reattaching a manifest: {error}")))?;
    Ok(from_reader(&reader, asset, format))
}

fn from_reader(reader: &Reader, bytes: &[u8], format: &str) -> Verification {
    let state = match reader.validation_state() {
        // See the table in the module docs. `Valid` is *not* our `valid`.
        ValidationState::Trusted => ProvenanceState::Valid,
        ValidationState::Valid => ProvenanceState::Untrusted,
        ValidationState::Invalid => ProvenanceState::Invalid,
    };

    let active = reader.active_manifest();
    let signer_cn = active
        .and_then(|m| m.signature_info())
        .and_then(|info| info.common_name.clone());
    // `claim_generator_info`, with the flat `claim_generator` as a fallback. Claim v2 moved the
    // generator into a structured list and leaves the old field empty, so reading only the old one
    // reports `None` for everything this system signs — which is how that was found.
    let claim_generator = active
        .and_then(|m| {
            m.claim_generator_info.as_ref().and_then(|infos| {
                infos.first().map(|info| match &info.version {
                    Some(version) => format!("{} {version}", info.name),
                    None => info.name.clone(),
                })
            })
        })
        .or_else(|| active.and_then(|m| m.claim_generator().map(str::to_owned)));
    let ingredient_count = active.map_or(0, |m| m.ingredients().len());
    let parsed_actions: Vec<assertions::Action> = active
        .map(|m| {
            m.assertions()
                .iter()
                .filter(|a| a.label().starts_with(assertions::labels::ACTIONS))
                .filter_map(|a| a.to_assertion::<assertions::Actions>().ok())
                .flat_map(|actions| actions.actions().to_vec())
                .collect()
        })
        .unwrap_or_default();
    let actions = parsed_actions
        .iter()
        .map(|action| action.action().to_owned())
        .collect();
    let source_types = parsed_actions
        .iter()
        .filter_map(|action| action.source_type())
        .filter_map(|source| serde_json::to_value(source).ok())
        .filter_map(|value| value.as_str().map(str::to_owned))
        .collect();

    Verification {
        state,
        // Re-extracted rather than taken from the reader's own copy, so the bytes stored are the ones
        // that were actually in the file.
        manifest: detach(bytes, format),
        signer_cn,
        claim_generator,
        spec_version: reader
            .validation_results()
            .and_then(|_| active.and_then(|m| m.label().map(str::to_owned))),
        ingredient_count,
        actions,
        source_types,
        detail: reader
            .validation_results()
            .and_then(|r| serde_json::to_value(r).ok())
            .unwrap_or(serde_json::Value::Null),
    }
}

/// Pulls the raw manifest store out of a file, for storage as a detached object.
///
/// Via `Ingredient`, which is the public route to the raw bytes — `Reader` exposes the *parsed* manifest
/// and there is nothing to be gained from re-serialising it: what has to be stored is the byte sequence
/// that was actually in the file, or a later re-validation would be checking our round trip rather than
/// the customer's evidence.
fn detach(bytes: &[u8], format: &str) -> Option<Vec<u8>> {
    // Through a throwaway builder, which is the only public non-deprecated route to an ingredient's
    // raw manifest data — `Reader` exposes the parsed manifest and never the bytes. Nothing is signed;
    // the builder is a means of reading, discarded immediately.
    let mut builder = Builder::from_shared_context(&CONTEXT);
    builder
        .add_ingredient_from_stream("{}", format, &mut Cursor::new(bytes))
        .ok()
        .and_then(|ingredient| ingredient.manifest_data().map(|data| data.to_vec()))
}

/// This deployment's signing identity.
///
/// Holds the signer rather than the key material, so nothing here can print a private key — which
/// matters because a `Debug` on a request context is how credentials reach logs.
pub struct SigningIdentity {
    signer: Box<dyn Signer>,
    common_name: String,
    ephemeral: bool,
}

impl std::fmt::Debug for SigningIdentity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SigningIdentity")
            .field("common_name", &self.common_name)
            .field("ephemeral", &self.ephemeral)
            .finish_non_exhaustive()
    }
}

impl SigningIdentity {
    /// A self-signed identity generated on the spot, for development only.
    ///
    /// Refused anywhere else (decision C2PA 2, recorded as irreversible). A test-signed credential in
    /// production is worse than no credential: it *looks* like provenance and verifies against nothing,
    /// so a downstream consumer would believe a chain had been checked when none had. The check is here
    /// rather than at the call site because a call site is exactly where it would be forgotten.
    pub fn ephemeral(environment: Environment, common_name: &str) -> Result<Self> {
        if environment != Environment::Development {
            return Err(Error::Refused(format!(
                "refusing to sign with an ephemeral certificate in {environment:?}: it would \
                 produce credentials that look valid and verify against nothing. Configure a real \
                 signing certificate, or leave signing disabled."
            )));
        }
        let signer = c2pa::EphemeralSigner::new(common_name)
            .map_err(|e| Error::Signing(format!("generating an ephemeral identity: {e}")))?;
        Ok(Self {
            signer: Box::new(signer),
            common_name: common_name.to_owned(),
            ephemeral: true,
        })
    }
}

/// One entry in the action chain.
///
/// C2PA's own vocabulary where it has one, `damrs.*` where it does not — the schema's `action` column
/// documents the same split. Using an invented label for something C2PA already names would make our
/// manifests unreadable to every other tool, which defeats the point of a standard.
#[derive(Debug, Clone)]
pub struct Action {
    label: String,
    parameters: Vec<(String, serde_json::Value)>,
}

/// How a file came into existence, for `c2pa.created`.
///
/// A separate type because the C2PA 2.x specification makes `digitalSourceType` **mandatory** on
/// `c2pa.created` — a manifest without it is rejected as a malformed action, which is how this was
/// found. Making it a required argument means that cannot be forgotten at a call site.
///
/// [`Self::AlgorithmicMedia`] is also the mechanism D15 and GAPS G2 use for EU AI Act Article 50
/// marking: the obligation is a *machine-readable* mark, and this field is the machine-readable mark
/// C2PA already defines. The disclosure record and its review workflow are M5's; the field it will
/// write to is this one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    /// A camera or recording device captured it from life.
    DigitalCapture,
    /// Multiple frames merged by signal processing or non-generative AI — smartphone HDR.
    ComputationalCapture,
    /// Generated by a trained model. Article 50 applies.
    AlgorithmicMedia,
    /// Assembled from multiple sources.
    Composite,
}

impl From<Origin> for assertions::DigitalSourceType {
    fn from(origin: Origin) -> Self {
        match origin {
            Origin::DigitalCapture => Self::DigitalCapture,
            Origin::ComputationalCapture => Self::ComputationalCapture,
            Origin::AlgorithmicMedia => Self::TrainedAlgorithmicMedia,
            Origin::Composite => Self::Composite,
        }
    }
}

impl Action {
    /// `c2pa.resized`, carrying the dimensions.
    ///
    /// The parameters matter as much as the label: "resized" without a size records that something
    /// happened without recording what, which is not provenance.
    pub fn resized(width: u32, height: u32) -> Self {
        Self {
            label: "c2pa.resized".to_owned(),
            parameters: vec![
                ("width".to_owned(), serde_json::json!(width)),
                ("height".to_owned(), serde_json::json!(height)),
            ],
        }
    }

    /// `c2pa.transcoded`, carrying the target codec.
    pub fn transcoded(codec: &str) -> Self {
        Self {
            label: "c2pa.transcoded".to_owned(),
            parameters: vec![("codec".to_owned(), serde_json::json!(codec))],
        }
    }

    /// `c2pa.converted`, carrying the target format.
    pub fn converted(format: &str) -> Self {
        Self {
            label: "c2pa.converted".to_owned(),
            parameters: vec![("format".to_owned(), serde_json::json!(format))],
        }
    }

    pub fn label(&self) -> &str {
        &self.label
    }
}

/// The asset a derivative was made from.
///
/// Recorded as a C2PA *ingredient*, which is the mechanism that makes a derivative's credential a
/// continuation rather than a new chain. Its bytes are needed because an ingredient carries a hash of
/// the thing it refers to — a title alone would be an unverifiable assertion.
#[derive(Debug, Clone)]
pub struct Parent {
    pub bytes: Vec<u8>,
    pub format: String,
    pub title: String,
}

/// Where a file being signed came from.
///
/// An enum rather than an `Option<Parent>` plus a hand-assembled first action, because the C2PA
/// specification constrains the two together and getting them out of step produces a manifest that
/// verifies as **invalid** — indistinguishable, to any consumer, from a tampered file. Specifically:
/// the action chain must begin with `c2pa.created` or `c2pa.opened`; `c2pa.created` must carry a
/// `digitalSourceType`; `c2pa.opened` must reference the ingredient it opened by hashed URI; and
/// `c2pa.opened` requires a `parentOf` ingredient to exist. Each of those was a separate validation
/// failure found by testing, and none of them can go wrong through this type.
///
/// The hashed URI is also the reason the first action is not the caller's to build: it is not known
/// until the manifest is assembled, so only the builder can write it.
#[derive(Debug, Clone)]
pub enum Provenance {
    /// An original. `Origin` says how it came to exist.
    Created(Origin),
    /// A derivative of `Parent` — the case D13 is about.
    DerivedFrom(Parent),
}

/// What to assert about a file being signed.
#[derive(Debug, Clone)]
pub struct Claim {
    pub claim_generator: String,
    pub provenance: Provenance,
    /// The transforms performed, in order. The opening or creating action is not included: see
    /// [`Provenance`].
    pub actions: Vec<Action>,
}

/// A signed file and its detached manifest.
#[derive(Debug, Clone)]
pub struct Signed {
    pub bytes: Vec<u8>,
    pub manifest: Option<Vec<u8>>,
}

/// Signs `bytes`, embedding a manifest that chains to `claim.parent` when there is one.
///
/// The chain is the requirement. Appending an action and terminating the chain both produce a signed
/// file, and they are indistinguishable unless you check for the ingredient — which is why one exists in
/// the tests rather than only a "the derivative has a manifest" assertion.
pub fn sign(
    identity: &SigningIdentity,
    bytes: &[u8],
    format: &str,
    claim: Claim,
) -> Result<Signed> {
    let mut builder = Builder::from_shared_context(&CONTEXT);
    builder.set_claim_generator_info(c2pa::ClaimGeneratorInfo::new(claim.claim_generator));
    builder.set_format(format);

    // The intent is what makes the chain correct. Given `Edit` and a `parentOf` ingredient the builder
    // inserts `c2pa.opened` itself, carrying the ingredient's hashed URI — which cannot be computed
    // before the manifest is assembled, so this genuinely is not something a caller could do. Given
    // `Create` it inserts `c2pa.created` with the source type.
    match &claim.provenance {
        Provenance::Created(origin) => {
            builder.set_intent(c2pa::BuilderIntent::Create(
                assertions::DigitalSourceType::from(*origin),
            ));
        }
        Provenance::DerivedFrom(parent) => {
            // `parentOf`, not `componentOf`: this derivative *is* that asset transformed, rather than
            // something the asset was placed into. Getting the relationship wrong makes the chain claim
            // something untrue about how the file came to be — and `Edit` requires a `parentOf`, so a
            // mistake here is refused rather than silently recorded.
            let ingredient = serde_json::json!({
                "title": parent.title,
                "relationship": "parentOf",
            })
            .to_string();
            builder
                .add_ingredient_from_stream(
                    ingredient,
                    &parent.format,
                    &mut Cursor::new(parent.bytes.clone()),
                )
                .map_err(|e| Error::Unreadable(format!("the parent asset: {e}")))?;
            builder.set_intent(c2pa::BuilderIntent::Edit);
        }
    }

    let mut actions = assertions::Actions::new();
    for action in &claim.actions {
        let mut built =
            assertions::Action::new(&action.label).set_software_agent(claim_generator().as_str());
        for (key, value) in &action.parameters {
            built = built
                .set_parameter(key.clone(), value.clone())
                .map_err(|e| Error::Signing(format!("setting {key} on {}: {e}", action.label)))?;
        }
        actions = actions.add_action(built);
    }
    // Skipped when there are no transforms: the builder supplies the created/opened action from the
    // intent, and adding an empty `Actions` assertion alongside it would suppress that (it only fires
    // when no created/opened action is present) and leave the manifest with no first action at all.
    if !claim.actions.is_empty() {
        builder
            .add_assertion(assertions::Actions::LABEL, &actions)
            .map_err(|e| Error::Signing(format!("adding the action chain: {e}")))?;
    }

    let mut source = Cursor::new(bytes);
    let mut dest = Cursor::new(Vec::new());
    let manifest = builder
        .sign(identity.signer.as_ref(), format, &mut source, &mut dest)
        .map_err(|e| Error::Signing(format!("a {format}: {e}")))?;

    Ok(Signed {
        bytes: dest.into_inner(),
        manifest: Some(manifest),
    })
}
