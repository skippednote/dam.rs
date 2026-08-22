//! A question, turned into the search language the DAM already has (M5d).
//!
//! §8.3 calls this "natural-language search → structured query IR". It produces **shorthand**, not an IR, and
//! that choice is the whole design:
//!
//! - `dam_core::shorthand` is already the one validated entry point for a query, and §12's argument is that a
//!   second representation is a second place for the access filter and the field validation to be applied
//!   differently. A model emitting shorthand goes through the same parser as a person typing it.
//! - It is **visible and editable**. The answer goes in the search box, so somebody can see what was understood,
//!   correct it, and keep it. An IR would be invisible, and a wrong one would look like a broken search.
//! - It cannot widen anything. The parsed query is composed with the caller's predicate exactly like any other,
//!   so the worst a hallucinated clause can do is match less.
//!
//! ## What the model is given
//!
//! The tenant's own field keys with their kinds and aliases, the category paths, and the three reserved
//! selectors. Nothing else: a model told to invent field names produces queries that fail to parse, and the
//! parser's error would name a field the tenant has never heard of.
//!
//! ## Where today's date goes
//!
//! In the question, not the instructions. "Photos from last week" needs a date, and a date in the cached prefix
//! would invalidate it every midnight — the commonest silent cache invalidator there is. The prefix is the
//! vocabulary, which changes when the schema does.

use crate::model::{Ask, Completion, Effort, Model, ModelError, Part, Usage};
use serde::Deserialize;

/// What a spend row calls this pipeline.
pub const PIPELINE: &str = "nl_query";

/// Bumped when the instructions change in a way that makes old answers non-comparable.
pub const PIPELINE_VERSION: i32 = 1;

/// One field the model may use.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FieldHint {
    pub key: String,
    /// `text`, `date`, `number`, and so on — the model needs it to know whether a range is legal.
    pub kind: String,
    /// The short alias a person would type, when the tenant defined one.
    pub alias: Option<String>,
}

/// What the tenant's search language actually contains.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Vocabulary {
    pub fields: Vec<FieldHint>,
    /// Category paths, for `in:`.
    pub categories: Vec<String>,
}

impl Vocabulary {
    /// The instruction text: the cacheable half, and stable while the schema is.
    pub fn instructions(&self) -> String {
        let mut text = String::with_capacity(1024 + self.fields.len() * 48);
        text.push_str(
            "You turn a question about a media library into that library's own search syntax. Answer only with \
             the JSON the schema asks for.\n\n",
        );
        text.push_str("The syntax:\n");
        text.push_str(
            "- Bare words search the text: `harbour dawn` finds assets mentioning both.\n",
        );
        text.push_str("- `\"a phrase\"` searches the exact phrase.\n");
        text.push_str("- `field:value` filters on a field below, by key or alias. Quote values with spaces.\n");
        text.push_str(
            "- `field:>=value` and `field:<value` compare, for dates and numbers only.\n",
        );
        text.push_str(
            "- `-term` excludes. `OR` (capitals) alternates; space means AND. Parentheses group.\n",
        );
        text.push_str("- `in:path` filters by category, using a path below.\n");
        text.push_str(
            "- `is:favourite`, `is:watched`, `is:rated` filter by what the person asking has marked.\n",
        );
        text.push_str("- `stars:>=4` filters by the library's average rating, 1 to 5.\n");
        // `YYYY-MM-DD` rather than a specimen date: a real one in the prefix reads as stale within a year, and
        // the test below treats any year here as the cache invalidator it usually is.
        text.push_str(
            "\nDates are ISO, YYYY-MM-DD. Write a relative date out: \"last week\" becomes a `>=` comparison \
             against the date given with the question.\n",
        );
        text.push_str(
            "\nIf the question cannot be expressed — it asks for something the library does not record — answer \
             with the plain words to search for and say so in the explanation. A query that returns nothing is \
             worse than a broader one that returns something.\n",
        );

        if self.fields.is_empty() {
            text.push_str(
                "\nThis library defines no fields, so use text and the selectors above only.\n",
            );
        } else {
            text.push_str("\nFields (key — kind — alias):\n");
            for field in &self.fields {
                text.push_str(&format!("- {} — {}", field.key, field.kind));
                if let Some(alias) = &field.alias {
                    text.push_str(&format!(" — {alias}"));
                }
                text.push('\n');
            }
        }
        if !self.categories.is_empty() {
            text.push_str("\nCategories, for `in:`:\n");
            for path in &self.categories {
                text.push_str(&format!("- {path}\n"));
            }
        }
        text
    }

    /// The schema the answer must satisfy. Strict-mode eligible, like the enrichment one.
    pub fn schema() -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "shorthand": {"type": "string"},
                "explanation": {"type": "string"},
                "confidence": {"type": "number"},
            },
            "required": ["shorthand", "explanation", "confidence"],
        })
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
struct Answer {
    shorthand: String,
    explanation: String,
    confidence: f64,
}

/// What the model made of a question.
#[derive(Debug, Clone, PartialEq)]
pub struct Asked {
    /// The query, in the library's own syntax. Unparsed — the caller runs it through
    /// `dam_core::shorthand::parse`, which is what makes this safe.
    pub shorthand: String,
    /// One sentence for the person who asked, so a wrong query is correctable rather than mysterious.
    pub explanation: String,
    pub confidence: f32,
    pub model: String,
    pub usage: Usage,
}

/// The ask for one question.
pub fn ask_for(vocabulary: &Vocabulary, question: &str, today: &str) -> Ask {
    Ask {
        instructions: vocabulary.instructions(),
        // Both per-question, and therefore after the cache breakpoint. The date is here rather than in the
        // instructions precisely so the prefix survives midnight.
        parts: vec![Part::Text(format!(
            "Today is {today}. Question: {question}"
        ))],
        schema: Some(Vocabulary::schema()),
        // Small: the answer is one line of syntax and one sentence. A large budget here buys nothing and pays
        // for the thinking a search box cannot wait for.
        max_tokens: 400,
        // Somebody is waiting for this, unlike a backfill — and a query is a small, well-specified translation
        // rather than a judgement.
        effort: Effort::Low,
    }
}

/// Asks one model to translate one question.
pub async fn translate(
    model: &dyn Model,
    vocabulary: &Vocabulary,
    question: &str,
    today: &str,
) -> Result<Asked, ModelError> {
    read(model.ask(&ask_for(vocabulary, question, today)).await?)
}

/// Reads a completion into an [`Asked`].
pub fn read(completion: Completion) -> Result<Asked, ModelError> {
    let structured = completion.structured.ok_or_else(|| {
        ModelError::Unreadable("the answer carried no structured output".to_owned())
    })?;
    let answer: Answer = serde_json::from_value(structured).map_err(|error| {
        ModelError::Unreadable(format!("the answer did not match the schema: {error}"))
    })?;
    Ok(Asked {
        shorthand: answer.shorthand.trim().to_owned(),
        explanation: answer.explanation.trim().to_owned(),
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
    use crate::testing::{Recorded, anthropic_answer};
    use dam_core::Secret;
    use std::sync::Arc;

    fn vocabulary() -> Vocabulary {
        Vocabulary {
            fields: vec![
                FieldHint {
                    key: "brand".to_owned(),
                    kind: "text".to_owned(),
                    alias: Some("bra".to_owned()),
                },
                FieldHint {
                    key: "shot_on".to_owned(),
                    kind: "date".to_owned(),
                    alias: None,
                },
            ],
            categories: vec!["exterior.harbour".to_owned()],
        }
    }

    #[test]
    fn the_instructions_are_the_vocabulary_and_nothing_per_question() {
        let text = vocabulary().instructions();
        // The lines themselves, not the heading: a prompt with a heading and no fields is the failure that
        // matters, and it is the one a heading-only assertion would miss.
        assert!(text.contains("- brand — text — bra\n"), "{text}");
        assert!(text.contains("- shot_on — date\n"), "{text}");
        assert!(text.contains("exterior.harbour"), "{text}");
        // No date, no question: the prefix has to survive midnight and every other question.
        assert!(
            !text.contains("2026"),
            "a date in the prefix invalidates the cache daily"
        );
        assert!(!text.contains("Question"), "{text}");
        assert_eq!(
            text,
            vocabulary().instructions(),
            "byte-identical between calls"
        );
    }

    #[test]
    fn a_library_with_no_fields_is_told_to_use_text() {
        let text = Vocabulary::default().instructions();
        assert!(text.contains("defines no fields"), "{text}");
        assert!(!text.contains("Categories"), "no empty heading");
    }

    #[test]
    fn the_question_and_the_date_travel_together_after_the_breakpoint() {
        let ask = ask_for(
            &vocabulary(),
            "photos of the harbour from last week",
            "2026-08-20",
        );
        assert_eq!(ask.parts.len(), 1);
        match &ask.parts[0] {
            Part::Text(text) => {
                assert!(text.contains("2026-08-20"), "{text}");
                assert!(text.contains("harbour from last week"), "{text}");
            }
            other => panic!("expected text, got {other:?}"),
        }
        // Cheap and quick: somebody is waiting for this.
        assert_eq!(ask.effort, Effort::Low);
        assert_eq!(ask.max_tokens, 400);
    }

    #[tokio::test]
    async fn a_translation_comes_back_as_shorthand_and_a_sentence() {
        let transport = Arc::new(Recorded::always(
            200,
            anthropic_answer(
                r#"{"shorthand":"in:exterior.harbour shot_on:>=2026-08-13","explanation":"Assets filed under the harbour category, shot in the last week.","confidence":0.8}"#,
                "claude-opus-5",
                (400, 30, 350, 0),
            ),
        ));
        let model = AnthropicModel::new(
            transport,
            Secret::new("test-key-not-a-credential".to_owned()),
            None,
            "claude-opus-5",
        );
        let asked = translate(
            &model,
            &vocabulary(),
            "harbour photos from last week",
            "2026-08-20",
        )
        .await
        .expect("a translation");

        assert_eq!(asked.shorthand, "in:exterior.harbour shot_on:>=2026-08-13");
        assert!(asked.explanation.starts_with("Assets filed"));
        assert!((asked.confidence - 0.8).abs() < 0.001);
        assert_eq!(asked.usage.cached_input_tokens, 350);
    }

    #[test]
    fn a_nonsense_confidence_is_clamped_rather_than_stored() {
        // A stored 4 would make every "how sure was it" display wrong, and a stored -1 would sort below
        // everything forever.
        let completion = Completion {
            text: String::new(),
            structured: Some(serde_json::json!({
                "shorthand": "harbour",
                "explanation": "x",
                "confidence": 4,
            })),
            model: "claude-opus-5".to_owned(),
            usage: Usage::default(),
        };
        let asked = read(completion).expect("an answer");
        assert!((asked.confidence - 1.0).abs() < f32::EPSILON);
    }

    #[test]
    fn structured_output_of_the_wrong_shape_is_an_error_rather_than_a_default() {
        // Distinct from prose: the model *did* answer with JSON, and the wrong JSON. Defaulting would produce an
        // empty query and an empty explanation, which looks like a search that found nothing.
        let completion = Completion {
            text: String::new(),
            structured: Some(serde_json::json!({"shorthand": 5})),
            model: "claude-opus-5".to_owned(),
            usage: Usage::default(),
        };
        let error = read(completion).expect_err("the wrong shape");
        assert!(matches!(error, ModelError::Unreadable(_)), "{error:?}");
    }

    #[tokio::test]
    async fn an_answer_that_ignored_the_schema_is_an_error() {
        let transport = Arc::new(Recorded::always(
            200,
            anthropic_answer("in:exterior.harbour", "claude-opus-5", (10, 5, 0, 0)),
        ));
        let model = AnthropicModel::new(
            transport,
            Secret::new("test-key-not-a-credential".to_owned()),
            None,
            "claude-opus-5",
        );
        let error = translate(&model, &vocabulary(), "harbour", "2026-08-20")
            .await
            .expect_err("prose is not an answer");
        assert!(matches!(error, ModelError::Unreadable(_)), "{error:?}");
    }
}
