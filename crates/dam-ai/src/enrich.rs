//! What to ask a model about one asset, and how to read the answer (M5b).
//!
//! §8.3's list of uses starts with alt text, captions and "rich descriptions mapped onto the tenant's
//! controlled vocabulary". This is that ask, built so the two halves fall on the right side of the cache
//! breakpoint: the instructions and the vocabulary are identical for every asset in a tenant and go in
//! [`Ask::instructions`]; the picture and its filename are per-asset and go after.
//!
//! ## The vocabulary is offered, and inventions are recorded rather than dropped silently
//!
//! Tags are `taxonomy_terms`, so a suggestion that is not a term is not a tag. The model is given the terms it
//! may choose from and its answers are matched back by slug and label. Anything unmatched is kept in
//! [`Suggestion::unknown_tags`] — a model reaching for a word a tenant does not have is the single most useful
//! signal about a vocabulary with a hole in it, and throwing it away would waste a paid call's most interesting
//! output.
//!
//! ## Confidence is recorded, never trusted
//!
//! The model reports its own confidence and it is stored as *claimed* — that is what
//! `asset_metadata.provenance` and `asset_tags.confidence` are for. It does not gate anything: a self-reported
//! number is not calibrated, and `taxonomy_terms.ai_threshold` exists for the probe paths (M4) where the
//! confidence comes from a measured precision curve. Every LLM tag lands `suggested` and waits for a person,
//! which is also what makes `tag_feedback` a training set rather than a record of what the model already did.
//!
//! ## Alt text is an accessibility artefact
//!
//! §8.3: Opus-grade for anything user-visible, because bad alt text is worse than none — a screen reader
//! announcing "image of a JPEG file" is noise that displaces the caption a person would otherwise have
//! written. The instructions say what alt text is *for*, and the length limit is in them rather than enforced
//! afterwards: truncating a sentence mid-word produces exactly the artefact the limit was meant to prevent.

use crate::model::{Ask, Effort, Model, ModelError, Part, Usage};
use serde::{Deserialize, Serialize};

/// The pipeline name written to `enrichment_runs.pipeline`.
pub const PIPELINE: &str = "llm_describe";

/// Bumped when the prompt or the schema changes in a way that makes old output non-comparable.
///
/// Stored on the run, so "re-run everything the old prompt touched" is expressible — the same reason
/// `ai_models` exists for the model itself. A prompt is as much a version of the system as the weights are.
pub const PIPELINE_VERSION: i32 = 1;

/// How long an alt text may be, in characters, as told to the model.
///
/// 125 is the figure screen-reader guidance converges on: longer and a listener loses the thread before the
/// page moves on.
const ALT_TEXT_LIMIT: usize = 125;

/// One term the model may choose.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TermOffer {
    /// The stable name the answer is matched on.
    pub slug: String,
    /// What a person calls it. Offered because a model matches meaning better against a label than a slug.
    pub label: String,
    /// Alternative wordings, from `taxonomy_terms.synonyms`. They widen matching at no extra cost.
    pub synonyms: Vec<String>,
}

/// Everything about the ask that does not change between assets.
///
/// Held separately because it *is* the cached prefix. Anything per-asset in here is a cache miss on every call,
/// and at §8.3's volumes that is the difference between the $6–8k row of the cost table and the $23k one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Brief {
    /// The tenant's own guidance: house style, what to avoid, what a description is for here.
    pub guidance: String,
    /// The vocabulary. Empty is legitimate — a tenant with no taxonomy still wants alt text.
    pub vocabulary: Vec<TermOffer>,
    /// Which locale to write in. A DAM serving one market should not get English by accident of the prompt.
    pub language: String,
}

impl Brief {
    /// The instruction text. Deterministic in the inputs, because prompt caching matches on bytes: a set
    /// iterated in a different order is a different prefix and a cache miss.
    pub fn instructions(&self) -> String {
        let mut text = String::with_capacity(512 + self.vocabulary.len() * 48);
        text.push_str(
            "You describe images for a digital asset library. Answer only with the JSON the schema asks for.\n\n",
        );
        text.push_str(&format!(
            "`alt_text`: what a screen reader should say. At most {ALT_TEXT_LIMIT} characters, one sentence, no \
             lead-in like \"image of\" or \"photo of\". Describe what matters about the picture, not that it is \
             a picture. If the image carries text that a reader needs, include it.\n",
        ));
        text.push_str(
            "`description`: two or three sentences for somebody searching the library — subject, setting, \
             notable detail. No speculation about who somebody is, where exactly it was taken, or when.\n",
        );
        if self.vocabulary.is_empty() {
            text.push_str(
                "`tags`: an empty array. This library has no controlled vocabulary, so there is nothing to \
                 choose from and invented tags are not wanted.\n",
            );
        } else {
            text.push_str(
                "`tags`: choose only from the vocabulary below, by slug, and only what the image actually \
                 shows. Fewer correct tags are worth more than many plausible ones. If nothing fits, answer \
                 with an empty array.\n",
            );
        }
        text.push_str(
            "`confidence`: 0 to 1, how much of this you would stand behind. A person reviews it either way.\n",
        );
        text.push_str(&format!("\nWrite in {}.\n", self.language));
        if !self.guidance.trim().is_empty() {
            text.push_str("\nThe library's own guidance:\n");
            text.push_str(self.guidance.trim());
            text.push('\n');
        }
        if !self.vocabulary.is_empty() {
            text.push_str("\nVocabulary (slug — label — other wordings):\n");
            for term in &self.vocabulary {
                text.push_str(&format!("- {} — {}", term.slug, term.label));
                if !term.synonyms.is_empty() {
                    text.push_str(&format!(" — {}", term.synonyms.join(", ")));
                }
                text.push('\n');
            }
        }
        text
    }

    /// The schema the answer must satisfy.
    ///
    /// `additionalProperties: false` with every field required, which is not decoration: it is what makes the
    /// schema eligible for the OpenAI family's strict mode, where the shape is guaranteed rather than requested.
    /// See `openai_compatible::body`.
    pub fn schema() -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "alt_text": {"type": "string"},
                "description": {"type": "string"},
                "tags": {"type": "array", "items": {"type": "string"}},
                "confidence": {"type": "number"},
            },
            "required": ["alt_text", "description", "tags", "confidence"],
        })
    }

    /// Matches a slug or a label the model answered with back onto a term.
    ///
    /// Case-insensitive, and labels and synonyms count: a model told the slug is what to answer with will still
    /// occasionally answer with the label, and refusing that would throw away a correct tag on a formality.
    pub fn resolve(&self, answered: &str) -> Option<&TermOffer> {
        let wanted = answered.trim().to_lowercase();
        if wanted.is_empty() {
            return None;
        }
        self.vocabulary.iter().find(|term| {
            term.slug.to_lowercase() == wanted
                || term.label.to_lowercase() == wanted
                || term
                    .synonyms
                    .iter()
                    .any(|synonym| synonym.to_lowercase() == wanted)
        })
    }
}

/// What the model answered, before any matching.
#[derive(Debug, Clone, PartialEq, Deserialize)]
struct Answer {
    alt_text: String,
    description: String,
    tags: Vec<String>,
    confidence: f64,
}

/// What the model answered, after matching against the vocabulary.
#[derive(Debug, Clone, PartialEq)]
pub struct Suggestion {
    pub alt_text: String,
    pub description: String,
    /// Slugs that exist in this tenant's taxonomy, deduplicated, in the order the model gave them.
    pub tags: Vec<String>,
    /// Words the model reached for that are not terms here. Kept — see the module note.
    pub unknown_tags: Vec<String>,
    /// The model's own claim, clamped to 0..=1 because a provider that answers 4 has answered nonsense and a
    /// stored 4 would sort the review queue wrongly forever.
    pub confidence: f32,
    /// The model that actually answered, for `ai_models` and for the provenance record.
    pub model: String,
    pub usage: Usage,
}

/// Asks one model about one asset.
///
/// `image` is the *proxy*, never the original: `enrichment_runs.used_original` exists to catch a stage that
/// reads a master, because at library scale that is a restore storm rather than a slow job. Passing the proxy is
/// the caller's job; this only records what it was given.
pub async fn describe(
    model: &dyn Model,
    brief: &Brief,
    image: Part,
    filename: &str,
) -> Result<Suggestion, ModelError> {
    let ask = Ask {
        instructions: brief.instructions(),
        parts: vec![
            image,
            // After the image, and after the cache breakpoint. A filename is weak evidence and occasionally
            // the only evidence — "SS26_lookbook_cover.jpg" says more about intent than the picture does.
            Part::Text(format!("The file is named {filename}. Describe the image.")),
        ],
        schema: Some(Brief::schema()),
        // §8.3's rule, and the reason it is not configurable per call here: alt text is an accessibility
        // artefact and the cheap setting shows. Bulk classification that wants `Low` is a different pipeline.
        max_tokens: 1024,
        effort: Effort::High,
    };

    let completion = model.ask(&ask).await?;
    let structured = completion.structured.ok_or_else(|| {
        ModelError::Unreadable("the answer carried no structured output".to_owned())
    })?;
    let answer: Answer = serde_json::from_value(structured).map_err(|error| {
        ModelError::Unreadable(format!("the answer did not match the schema: {error}"))
    })?;

    let mut tags = Vec::new();
    let mut unknown_tags = Vec::new();
    for answered in &answer.tags {
        match brief.resolve(answered) {
            Some(term) => {
                if !tags.contains(&term.slug) {
                    tags.push(term.slug.clone());
                }
            }
            None => {
                let word = answered.trim().to_owned();
                if !word.is_empty() && !unknown_tags.contains(&word) {
                    unknown_tags.push(word);
                }
            }
        }
    }

    Ok(Suggestion {
        alt_text: answer.alt_text.trim().to_owned(),
        description: answer.description.trim().to_owned(),
        tags,
        unknown_tags,
        confidence: answer.confidence.clamp(0.0, 1.0) as f32,
        model: completion.model,
        usage: completion.usage,
    })
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;
    use crate::anthropic::AnthropicModel;
    use crate::testing::Recorded;
    use dam_core::Secret;
    use std::sync::Arc;

    fn brief() -> Brief {
        Brief {
            guidance: "Say 'trainers', not 'sneakers'.".to_owned(),
            vocabulary: vec![
                TermOffer {
                    slug: "footwear".to_owned(),
                    label: "Footwear".to_owned(),
                    synonyms: vec!["shoes".to_owned()],
                },
                TermOffer {
                    slug: "outdoor".to_owned(),
                    label: "Outdoor".to_owned(),
                    synonyms: vec![],
                },
            ],
            language: "British English".to_owned(),
        }
    }

    fn answered(json: &str) -> Arc<Recorded> {
        Arc::new(Recorded::always(
            200,
            crate::testing::anthropic_answer(json, "claude-opus-5", (900, 120, 800, 0)),
        ))
    }

    async fn describe_with(transport: Arc<Recorded>) -> Result<Suggestion, ModelError> {
        let model = AnthropicModel::new(
            transport,
            Secret::new("test-key-not-a-credential".to_owned()),
            None,
            "claude-opus-5",
        );
        describe(
            &model,
            &brief(),
            Part::Image {
                media_type: "image/jpeg".to_owned(),
                base64: "AAECAw==".to_owned(),
            },
            "SS26_lookbook_cover.jpg",
        )
        .await
    }

    #[test]
    fn the_instructions_are_the_cacheable_half_and_do_not_move() {
        // Byte-identical for two assets in the same tenant, or prompt caching never hits. Nothing per-asset,
        // no timestamp — `shared/prompt-caching.md`'s commonest silent invalidator.
        let first = brief().instructions();
        let second = brief().instructions();
        assert_eq!(first, second);
        assert!(first.contains("footwear — Footwear — shoes"), "{first}");
        assert!(
            first.contains("Say 'trainers'"),
            "the tenant's guidance travels"
        );
        assert!(first.contains("British English"));
        assert!(!first.contains("SS26"), "nothing per-asset in the prefix");
    }

    #[test]
    fn a_library_with_no_vocabulary_is_told_not_to_invent_one() {
        let mut brief = brief();
        brief.vocabulary.clear();
        let text = brief.instructions();
        assert!(text.contains("empty array"), "{text}");
        assert!(!text.contains("Vocabulary"), "no empty heading");
    }

    #[test]
    fn the_schema_is_strict_mode_eligible() {
        // Every property required and no extras, which is what the OpenAI family's `strict` demands — and
        // `openai_compatible` only claims strict when the schema says so.
        let schema = Brief::schema();
        assert_eq!(schema["additionalProperties"], false);
        let required = schema["required"].as_array().expect("required");
        let properties = schema["properties"].as_object().expect("properties");
        assert_eq!(required.len(), properties.len());
    }

    #[tokio::test]
    async fn a_suggestion_keeps_the_tags_that_exist_and_the_words_that_do_not() {
        let suggestion = describe_with(answered(
            r#"{"alt_text":"A runner on a wet path","description":"Two sentences.","tags":["footwear","Outdoor","shoes","streetwear"],"confidence":0.72}"#,
        ))
        .await
        .expect("a suggestion");

        // `Outdoor` came back as a label and `shoes` as a synonym; both are the terms they name, and `shoes`
        // must not become a second copy of footwear.
        assert_eq!(suggestion.tags, vec!["footwear", "outdoor"]);
        // The word the tenant has no term for is the most useful thing in the answer.
        assert_eq!(suggestion.unknown_tags, vec!["streetwear"]);
        assert_eq!(suggestion.alt_text, "A runner on a wet path");
        assert!((suggestion.confidence - 0.72).abs() < 0.001);
        assert_eq!(suggestion.model, "claude-opus-5");
        assert_eq!(suggestion.usage.cached_input_tokens, 800);
    }

    #[tokio::test]
    async fn a_nonsense_confidence_is_clamped_rather_than_stored() {
        let suggestion = describe_with(answered(
            r#"{"alt_text":"A","description":"B","tags":[],"confidence":4}"#,
        ))
        .await
        .expect("a suggestion");
        // A stored 4 would sit at the top of the review queue forever.
        assert!((suggestion.confidence - 1.0).abs() < f32::EPSILON);
    }

    #[tokio::test]
    async fn an_answer_missing_a_field_is_an_error_rather_than_a_default() {
        // A description that silently became "" would be written to the asset as though the model had said it.
        let error = describe_with(answered(r#"{"alt_text":"A","tags":[],"confidence":0.5}"#))
            .await
            .expect_err("an incomplete answer");
        assert!(matches!(error, ModelError::Unreadable(_)), "{error:?}");
    }

    #[tokio::test]
    async fn the_filename_travels_after_the_image_and_after_the_breakpoint() {
        let transport =
            answered(r#"{"alt_text":"A","description":"B","tags":[],"confidence":0.5}"#);
        describe_with(Arc::clone(&transport))
            .await
            .expect("a suggestion");
        let sent = transport.only();
        let content = sent.body["messages"][0]["content"]
            .as_array()
            .expect("content");
        assert_eq!(content[0]["type"], "image");
        assert!(
            content[1]["text"]
                .as_str()
                .expect("text")
                .contains("SS26_lookbook_cover.jpg")
        );
        // And the expensive setting, because this is the path that writes alt text.
        assert_eq!(sent.body["output_config"]["effort"], "high");
        assert_eq!(sent.body["system"][0]["cache_control"]["type"], "ephemeral");
    }
}
