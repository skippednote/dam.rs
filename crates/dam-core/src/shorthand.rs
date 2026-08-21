//! The shorthand search syntax (2.5).
//!
//! What a person types into one box, parsed into the same [`Query`] the API accepts. One query language
//! and one set of semantics: a separate parser producing a separate representation would be a second
//! place for the access filter and the field validation to be applied differently, which is the shape
//! §12 warns about.
//!
//! ## Errors carry a column
//!
//! The requirement TASKS.md names, and it is about error reporting rather than parsing. `"beach holiday`
//! with no closing quote is what every hand-written search box treats as a search for the literal text
//! `"beach holiday` — it returns nothing, and the user has no way to know the quote was the problem. A
//! column lets a UI underline the character to fix.
//!
//! Columns are 1-based and counted in **characters**. Bytes would put the caret under the wrong character
//! the moment a query contains an accent.
//!
//! ## Case sensitivity for operators
//!
//! `OR` is an operator, `or` is a word. This is the cheapest way to keep ordinary English searchable: a
//! user typing `cats or dogs` overwhelmingly means the word, and a case-insensitive keyword would make it
//! unsearchable with no way to escape it short of quoting.
//!
//! ## What is not a field reference
//!
//! `key:value` is a field query only when `key` is a known field or alias. Two exceptions keep the
//! shorthand from becoming a trap: anything containing `://` is text, because pasting a URL into a search
//! box is common; and a key that does not match the `field_defs` key shape is text, so `9:30` and
//! `Ratio:16` search rather than fail. An unknown key that *does* look like a field is an error naming it
//! — a typo silently becoming free text returns nothing and explains nothing.

use crate::fields::{FieldDef, FieldKind};
use crate::query::{Comparison, Endpoint, Literal, Orientation, Personal, Query};
use std::collections::HashMap;
use uuid::Uuid;

/// Longest input accepted, in characters.
///
/// A search string arrives in a URL, so this is untrusted input. Bounding it here is cheaper than
/// bounding the parsed tree, and it puts the IR's node limit out of reach from this entry point.
pub const MAX_INPUT_CHARS: usize = 4096;

/// The selector that filters by category: `in:exterior.yellow`.
///
/// Reserved, so a field of this name cannot shadow the browse tree.
pub const CATEGORY_SELECTOR: &str = "in";

/// The selector that filters by the caller's own engagement: `is:favourite`, `is:watched`, `is:rated`.
///
/// Reserved for the same reason as `in:`: a tenant field called `is` would shadow it, and the rail's own links
/// would stop working for reasons nobody could see.
pub const PERSONAL_SELECTOR: &str = "is";

/// The selector that filters by the asset's average rating: `stars:>=4`.
///
/// Reserved. Unlike `is:`, this one is about the asset rather than the caller — the library's shared judgement,
/// which is what makes it a facet worth having.
pub const RATING_SELECTOR: &str = "stars";

/// The selector that filters by lifecycle status: `status:archived`.
///
/// Reserved, like the others here. `assets.status` is a column with a CHECK constraint behind it, so a tenant
/// field of the same name would shadow something that is not theirs to redefine.
pub const STATUS_SELECTOR: &str = "status";

/// The selector that filters by the shape of the frame: `orientation:landscape`.
pub const ORIENTATION_SELECTOR: &str = "orientation";

/// The selector that filters by filename: `filename:DSC_0043.jpg`, `filename:DSC*`, `filename:*0043*` (Q.16).
///
/// Reserved. `assets.filename` is a column, and a tenant field called `filename` would shadow the one thing
/// every asset has whether or not the tenant defined any schema at all.
pub const FILENAME_SELECTOR: &str = "filename";

/// The selector for what is attached to an asset: `has:attachment`.
///
/// One value today, and a selector rather than `is:attached` because the two read about different things:
/// `is:` is the caller ("things I marked"), `has:` is the asset ("things it carries").
pub const PRESENCE_SELECTOR: &str = "has";

/// Deepest parenthesis nesting.
///
/// The parser recurses over groups, so this is a stack bound. Sixteen is well past anything typed by
/// hand or produced by a filter rail.
pub const MAX_GROUP_DEPTH: usize = 16;

/// What the parser needs to know about the tenant's schema.
#[derive(Debug, Clone)]
pub struct Schema {
    fields: Vec<FieldDef>,
    /// `search_alias` → field key.
    aliases: HashMap<String, String>,
    /// Category ltree path → term id, so `in:exterior.yellow` resolves here rather than needing a second
    /// round trip after parsing.
    ///
    /// Categories live in the query string rather than in a separate parameter deliberately: the filter rail
    /// exists to edit *one* string, so that "copy this search" copies all of it. A `category=` alongside `q`
    /// would be exactly the two-filters-and-no-way-to-see-the-whole-thing split the rail was built to avoid.
    categories: HashMap<String, Uuid>,
}

impl Schema {
    pub fn new(fields: Vec<FieldDef>, aliases: HashMap<String, String>) -> Self {
        Self {
            fields,
            aliases,
            categories: HashMap::new(),
        }
    }

    /// The same schema, able to resolve `in:<path>`.
    #[must_use]
    pub fn with_categories(mut self, categories: HashMap<String, Uuid>) -> Self {
        self.categories = categories;
        self
    }

    /// The definitions, for a caller that needs to plan the query this schema parsed.
    ///
    /// Handing back the same list the parser resolved against is the point: planning against a separately
    /// loaded set could validate a query the parser built from a field the planner has never heard of.
    pub fn fields(&self) -> &[FieldDef] {
        &self.fields
    }

    /// Every category path this schema can resolve, sorted.
    ///
    /// For a caller that has to *describe* the vocabulary rather than parse with it — M5d gives the list to a
    /// model so it can only produce paths that exist. Sorted because that description is a cached prompt prefix,
    /// and a set iterated in a different order is a different prefix.
    pub fn category_paths(&self) -> Vec<String> {
        let mut paths: Vec<String> = self.categories.keys().cloned().collect();
        paths.sort();
        paths
    }

    /// Every alias, as `key -> alias`. Same purpose as [`Self::category_paths`].
    pub fn aliases_by_key(&self) -> std::collections::HashMap<String, String> {
        self.aliases
            .iter()
            .map(|(alias, key)| (key.clone(), alias.clone()))
            .collect()
    }

    /// Resolves a category path to its term id.
    fn resolve_category(&self, path: &str) -> Option<Uuid> {
        // Case-folded, like every other selector value: a path typed `Exterior.Yellow` is the same category,
        // and refusing it would make the rail's own links unusable if a label ever changed case.
        let lowered = path.to_ascii_lowercase();
        self.categories.get(&lowered).copied()
    }

    /// Resolves a key or alias to a definition.
    /// Every name a field clause may be written with: keys, aliases, and the reserved selectors.
    ///
    /// The selectors are in the list because `stars:` is as likely a typo target as `brand:` — somebody typing
    /// `star:4` has made the same kind of mistake, and a suggestion that only knew about fields would tell
    /// them nothing.
    fn nameable(&self) -> Vec<&str> {
        let mut names: Vec<&str> = self.fields.iter().map(|def| def.key.as_str()).collect();
        names.extend(self.aliases.keys().map(String::as_str));
        names.extend([
            CATEGORY_SELECTOR,
            PERSONAL_SELECTOR,
            RATING_SELECTOR,
            STATUS_SELECTOR,
            ORIENTATION_SELECTOR,
            PRESENCE_SELECTOR,
            FILENAME_SELECTOR,
        ]);
        names
    }

    fn resolve(&self, name: &str) -> Option<&FieldDef> {
        let key = self.aliases.get(name).map_or(name, String::as_str);
        self.fields.iter().find(|def| def.key == key)
    }
}

/// A parse failure, with the character to point at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseError {
    /// A stable code, for the same reason `fields::Rejection` has one: a client branches on it and a UI
    /// renders it in the user's language.
    pub code: &'static str,
    /// 1-based, in characters.
    pub column: usize,
    pub detail: String,
    /// The name somebody probably meant, when one is close enough (Q.17).
    ///
    /// A suggestion, not a correction: the query is still refused. Answering `brnad:acme` as `brand:acme`
    /// would be a filter nobody asked for, and the first wrong guess leaves a user with results they cannot
    /// explain. This is the one-click fix beside the refusal, and it is `None` rather than a stretch when
    /// nothing is close — suggesting `year` for `photographer` reads as a system that does not know what its
    /// own fields are called.
    pub suggestion: Option<String>,
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "column {}: {}", self.column, self.detail)
    }
}

impl std::error::Error for ParseError {}

impl ParseError {
    fn new(code: &'static str, column: usize, detail: impl Into<String>) -> Self {
        Self {
            code,
            column,
            detail: detail.into(),
            suggestion: None,
        }
    }

    /// The same error, carrying the name somebody probably meant.
    fn suggesting<'a>(
        mut self,
        candidates: impl IntoIterator<Item = &'a str>,
        typed: &str,
    ) -> Self {
        self.suggestion = crate::similar::closest(candidates, typed).map(str::to_owned);
        self
    }
}

/// One lexical unit, with the column it started at.
#[derive(Debug, Clone, PartialEq)]
struct Token {
    column: usize,
    kind: TokenKind,
}

#[derive(Debug, Clone, PartialEq)]
enum TokenKind {
    /// A bare or quoted run of text, with whether it was quoted — quoting suppresses operator meaning.
    Word {
        text: String,
        quoted: bool,
    },
    Not,
    Or,
    And,
    OpenGroup,
    CloseGroup,
}

/// Parses `input` into a [`Query`].
pub fn parse(input: &str, schema: &Schema) -> Result<Query, ParseError> {
    let chars: Vec<char> = input.chars().collect();
    if chars.len() > MAX_INPUT_CHARS {
        return Err(ParseError::new(
            "too_long",
            MAX_INPUT_CHARS,
            format!(
                "{} characters, maximum {MAX_INPUT_CHARS}; a search string arrives in a URL and is \
                 bounded before it is parsed",
                chars.len()
            ),
        ));
    }

    let tokens = tokenise(&chars)?;
    if tokens.is_empty() {
        // An empty search box is the default state of a search page, not an error. The answer is the
        // library — access-filtered, as everything is.
        return Ok(Query::All);
    }

    let mut parser = Parser {
        tokens: &tokens,
        position: 0,
        schema,
        depth: 0,
    };
    let query = parser.parse_or()?;
    if let Some(token) = parser.peek() {
        return Err(ParseError::new(
            "unexpected_token",
            token.column,
            "unexpected token; a closing parenthesis with no opening one, or two terms with no operator \
             between them that the parser could not join",
        ));
    }
    Ok(query)
}

/// Splits the input into tokens, tracking the column each began at.
fn tokenise(chars: &[char]) -> Result<Vec<Token>, ParseError> {
    let mut tokens = Vec::new();
    let mut index = 0usize;

    while index < chars.len() {
        let column = index + 1;
        let ch = chars[index];

        if ch.is_whitespace() {
            index += 1;
            continue;
        }

        match ch {
            '(' => {
                tokens.push(Token {
                    column,
                    kind: TokenKind::OpenGroup,
                });
                index += 1;
            }
            ')' => {
                tokens.push(Token {
                    column,
                    kind: TokenKind::CloseGroup,
                });
                index += 1;
            }
            '"' => {
                // The named case. Scanning to the end and reporting the *opening* quote's column is
                // deliberate: the missing character is at the end, but the one the user has to look at is
                // the quote they opened.
                let mut text = String::new();
                let mut cursor = index + 1;
                let mut closed = false;
                while cursor < chars.len() {
                    if chars[cursor] == '"' {
                        closed = true;
                        break;
                    }
                    text.push(chars[cursor]);
                    cursor += 1;
                }
                if !closed {
                    return Err(ParseError::new(
                        "unclosed_quote",
                        column,
                        "this quote is never closed; without the error the whole rest of the query \
                         would be searched as literal text, which returns nothing and explains nothing",
                    ));
                }
                tokens.push(Token {
                    column,
                    kind: TokenKind::Word { text, quoted: true },
                });
                index = cursor + 1;
            }
            _ => {
                // A run up to the next whitespace or grouping character. A quote inside the run ends it,
                // so `brand:"Acme Corp"` lexes as the key run then the quoted value.
                let mut text = String::new();
                let mut cursor = index;
                while cursor < chars.len() {
                    let c = chars[cursor];
                    if c.is_whitespace() || c == '(' || c == ')' {
                        break;
                    }
                    if c == '"' && !text.is_empty() {
                        break;
                    }
                    text.push(c);
                    cursor += 1;
                }
                index = cursor;

                // `-` only negates when it *starts* a term. Inside a word it is a hyphen, and treating it
                // as an operator would make hyphenated product names unsearchable.
                if text.len() > 1 && text.starts_with('-') {
                    tokens.push(Token {
                        column,
                        kind: TokenKind::Not,
                    });
                    tokens.push(Token {
                        column: column + 1,
                        kind: TokenKind::Word {
                            text: text[1..].to_owned(),
                            quoted: false,
                        },
                    });
                    continue;
                }

                let kind = match text.as_str() {
                    // Uppercase only. See the module docs.
                    "OR" => TokenKind::Or,
                    "AND" => TokenKind::And,
                    "NOT" => TokenKind::Not,
                    _ => TokenKind::Word {
                        text,
                        quoted: false,
                    },
                };
                tokens.push(Token { column, kind });
            }
        }
    }

    Ok(tokens)
}

struct Parser<'a> {
    tokens: &'a [Token],
    position: usize,
    schema: &'a Schema,
    depth: usize,
}

impl<'a> Parser<'a> {
    fn peek(&self) -> Option<&'a Token> {
        self.tokens.get(self.position)
    }

    /// The column just past the input, for an unexpected-end message.
    fn end_column(&self) -> usize {
        self.tokens.last().map_or(1, |token| token.column)
    }

    /// `a OR b OR c` — the loosest binding.
    fn parse_or(&mut self) -> Result<Query, ParseError> {
        let mut branches = vec![self.parse_and()?];
        while matches!(self.peek().map(|t| &t.kind), Some(TokenKind::Or)) {
            self.position += 1;
            branches.push(self.parse_and()?);
        }
        Ok(if branches.len() == 1 {
            branches.remove(0)
        } else {
            Query::Or(branches)
        })
    }

    /// Juxtaposition and an explicit `AND`, which mean the same thing.
    fn parse_and(&mut self) -> Result<Query, ParseError> {
        let mut terms = vec![self.parse_unary()?];
        loop {
            match self.peek().map(|t| &t.kind) {
                Some(TokenKind::And) => {
                    self.position += 1;
                    terms.push(self.parse_unary()?);
                }
                // Juxtaposition. Stops at `OR` and at a closing parenthesis, which is what gives AND the
                // tighter binding.
                Some(TokenKind::Word { .. } | TokenKind::Not | TokenKind::OpenGroup) => {
                    terms.push(self.parse_unary()?);
                }
                _ => break,
            }
        }
        Ok(if terms.len() == 1 {
            terms.remove(0)
        } else {
            Query::And(terms)
        })
    }

    fn parse_unary(&mut self) -> Result<Query, ParseError> {
        match self.peek() {
            Some(Token {
                kind: TokenKind::Not,
                ..
            }) => {
                self.position += 1;
                Ok(Query::Not(Box::new(self.parse_unary()?)))
            }
            Some(Token {
                kind: TokenKind::OpenGroup,
                column,
            }) => {
                let open_column = *column;
                self.position += 1;
                self.depth += 1;
                if self.depth > MAX_GROUP_DEPTH {
                    return Err(ParseError::new(
                        "too_deep",
                        open_column,
                        format!(
                            "parentheses nested deeper than {MAX_GROUP_DEPTH}; the parser recurses, so \
                             a deeper query is a stack overflow rather than a slow one"
                        ),
                    ));
                }
                let inner = self.parse_or()?;
                self.depth -= 1;
                match self.peek() {
                    Some(Token {
                        kind: TokenKind::CloseGroup,
                        ..
                    }) => {
                        self.position += 1;
                        Ok(inner)
                    }
                    _ => Err(ParseError::new(
                        "unclosed_group",
                        open_column,
                        "this parenthesis is never closed",
                    )),
                }
            }
            Some(Token {
                kind: TokenKind::Word { text, quoted },
                column,
            }) => {
                let term = self.term(text, *quoted, *column)?;
                self.position += 1;
                Ok(term)
            }
            Some(Token { column, .. }) => Err(ParseError::new(
                "unexpected_token",
                *column,
                "expected a term here",
            )),
            None => Err(ParseError::new(
                "unexpected_end",
                self.end_column(),
                "the query ends after an operator; a trailing operator silently ignored would answer a \
                 different question and look like it worked",
            )),
        }
    }

    /// One word: a field query if it names a field, otherwise free text.
    fn term(&mut self, text: &str, quoted: bool, column: usize) -> Result<Query, ParseError> {
        // Quoting suppresses every operator meaning, including `:`. Without that a user cannot search for
        // "9:30" or "sold-out" at all.
        if quoted {
            return Ok(Query::Text(text.to_owned()));
        }

        let Some((name, rest)) = text.split_once(':') else {
            return Ok(Query::Text(text.to_owned()));
        };

        // See the module docs for both exceptions.
        if rest.starts_with("//") {
            return Ok(Query::Text(text.to_owned()));
        }
        if !is_field_shaped(name) {
            return Ok(Query::Text(text.to_owned()));
        }

        // `is:` is the caller's own engagement. Before field resolution, for the same reason as `in:` below.
        if name.eq_ignore_ascii_case(PERSONAL_SELECTOR) {
            let what = rest.trim();
            let state = match what.to_ascii_lowercase().as_str() {
                "favourite" | "favorite" => Some(Personal::Favourite),
                "watched" | "watching" => Some(Personal::Watched),
                "rated" => Some(Personal::Rated),
                _ => None,
            };
            // Both spellings of "favourite", because the tenant is in Pune and the vendor is in Boston, and a
            // search box that accepts only one of them is a search box that is wrong for somebody every day.
            // "watching" likewise: the button says Watch, so both the state and the act read naturally.
            let Some(state) = state else {
                return Err(ParseError::new(
                    "unknown_personal",
                    column + name.chars().count() + 1,
                    format!(
                        "{PERSONAL_SELECTOR}: takes favourite, watched or rated — {what:?} is none of them"
                    ),
                )
                .suggesting(["favourite", "watched", "rated"], what));
            };
            return Ok(Query::Mine(state));
        }

        // `stars:` is the asset's average rating.
        if name.eq_ignore_ascii_case(RATING_SELECTOR) {
            let spec = rest.trim();
            if spec.is_empty() {
                return Err(ParseError::new(
                    "empty_rating",
                    column,
                    format!(
                        "{RATING_SELECTOR}: needs a number or a comparison, like {RATING_SELECTOR}:>=4"
                    ),
                ));
            }
            // Through the same comparison parser a field uses, so `>=4`, `2..4` and `none` behave identically
            // here and there. A second parser for the same syntax is a second set of edge cases.
            let op = self.operator(
                FieldKind::Int,
                RATING_SELECTOR,
                spec,
                false,
                column + name.chars().count() + 1,
            )?;
            return Ok(Query::Rating(op));
        }

        // `filename:` is the asset's own name, with wildcards (Q.16).
        if name.eq_ignore_ascii_case(FILENAME_SELECTOR) {
            let value_column = column + name.chars().count() + 1;
            // A quoted value lexes as the *next* token, exactly as it does for a field — and a filename is the
            // value most likely to contain a space, so this branch is the one that matters here.
            let (spec, quoted) = if rest.is_empty() {
                match self.tokens.get(self.position + 1) {
                    Some(Token {
                        kind: TokenKind::Word { text, quoted: true },
                        ..
                    }) => {
                        let value = text.clone();
                        self.position += 1;
                        (value, true)
                    }
                    _ => {
                        return Err(ParseError::new(
                            "empty_filename",
                            column,
                            format!(
                                "{FILENAME_SELECTOR}: needs a name, a prefix like \
                                 {FILENAME_SELECTOR}:DSC*, or a substring like {FILENAME_SELECTOR}:*0043*"
                            ),
                        ));
                    }
                }
            } else {
                (rest.to_owned(), false)
            };
            // Through the same operator parser a text field uses, so a wildcard means the same thing here as
            // there rather than being a second dialect for one column.
            let op = self.operator(
                FieldKind::Text,
                FILENAME_SELECTOR,
                &spec,
                quoted,
                value_column,
            )?;
            return Ok(Query::Filename(op));
        }

        // `status:` is the asset's lifecycle column.
        if name.eq_ignore_ascii_case(STATUS_SELECTOR) {
            let value = rest.trim().trim_matches('"');
            if value.is_empty() {
                return Err(ParseError::new(
                    "empty_status",
                    column,
                    format!("{STATUS_SELECTOR}: needs a value, like {STATUS_SELECTOR}:archived"),
                ));
            }
            // Not checked against the CHECK constraint's list. A status this build has never heard of matches
            // nothing, which is the honest answer for a query about something that does not exist — and the
            // list is a migration's business, not a parser's.
            return Ok(Query::Status(value.to_ascii_lowercase()));
        }

        // `orientation:` is derived from the stored dimensions.
        if name.eq_ignore_ascii_case(ORIENTATION_SELECTOR) {
            let value = rest.trim().trim_matches('"');
            let Some(shape) = Orientation::parse(value) else {
                return Err(ParseError::new(
                    "unknown_orientation",
                    column + name.chars().count() + 1,
                    format!(
                        "{ORIENTATION_SELECTOR}: takes landscape, portrait or square — {value:?} is none of \
                         them"
                    ),
                )
                .suggesting(["landscape", "portrait", "square"], value));
            };
            return Ok(Query::Orientation(shape));
        }

        // `has:` is what the asset carries.
        if name.eq_ignore_ascii_case(PRESENCE_SELECTOR) {
            let value = rest.trim().trim_matches('"').to_ascii_lowercase();
            return match value.as_str() {
                "attachment" | "attachments" => Ok(Query::HasAttachment),
                // Named, so somebody typing `has:comments` learns what this selector does hold rather than
                // getting a clause that silently matched everything.
                _ => Err(ParseError::new(
                    "unknown_presence",
                    column + name.chars().count() + 1,
                    format!(
                        "{PRESENCE_SELECTOR}: takes attachment — {value:?} is not something it knows"
                    ),
                )
                .suggesting(["attachment"], &value)),
            };
        }

        // `in:` is the category selector, not a field. Checked before field resolution because `in` is a
        // reserved name here: a tenant that defined a field called `in` would shadow the browse tree, and the
        // rail's own links would stop working for reasons nobody could see.
        if name.eq_ignore_ascii_case(CATEGORY_SELECTOR) {
            let path = rest.trim();
            if path.is_empty() {
                return Err(ParseError::new(
                    "empty_category",
                    column,
                    format!(
                        "{CATEGORY_SELECTOR}: needs a category path, like {CATEGORY_SELECTOR}:exterior.yellow"
                    ),
                ));
            }
            let known_paths = self.schema.category_paths();
            let Some(term_id) = self.schema.resolve_category(path) else {
                return Err(ParseError::new(
                    "unknown_category",
                    column + name.chars().count() + 1,
                    format!(
                        "no category at path {path:?}; treating a typo as free text would return \
                         nothing and explain nothing"
                    ),
                )
                .suggesting(
                    // Sorted, so the suggestion for an ambiguous typo is the same one every time.
                    known_paths.iter().map(String::as_str),
                    path,
                ));
            };
            return Ok(Query::Term {
                term_id,
                // Always. "In Exterior" colloquially includes everything filed beneath it, which is what
                // clicking a branch in a browse tree means and why the paths are `ltree` in the first place.
                include_descendants: true,
            });
        }

        let Some(def) = self.schema.resolve(name) else {
            return Err(ParseError::new(
                "unknown_field",
                column,
                format!(
                    "no field or alias named {name:?}; a typo treated as free text would return \
                     nothing and explain nothing"
                ),
            )
            .suggesting(self.schema.nameable(), name));
        };
        // Cloned so the borrow on `self.schema` ends before the value is parsed, which may consult the
        // next token for a quoted value.
        let def = def.clone();

        // The value's column: the key, the colon, then the value.
        let value_column = column + name.chars().count() + 1;

        // An empty value means the value was quoted and lexed as the next token.
        let value = if rest.is_empty() {
            match self.tokens.get(self.position + 1) {
                Some(Token {
                    kind: TokenKind::Word { text, quoted: true },
                    ..
                }) => {
                    let value = text.clone();
                    // Consume the value token; the caller advances past the key.
                    self.position += 1;
                    return self.comparison(&def, &value, true, value_column);
                }
                _ => {
                    return Err(ParseError::new(
                        "bad_value",
                        value_column,
                        format!("{name:?} has no value"),
                    ));
                }
            }
        } else {
            rest.to_owned()
        };

        self.comparison(&def, &value, false, value_column)
    }

    /// Builds the comparison for a field and its raw value text.
    fn comparison(
        &self,
        def: &FieldDef,
        value: &str,
        quoted: bool,
        column: usize,
    ) -> Result<Query, ParseError> {
        let op = self.operator(def.kind, &def.key, value, quoted, column)?;
        Ok(Query::Field {
            key: def.key.clone(),
            op,
        })
    }

    /// The comparison a value text expresses, for a value of `kind`.
    ///
    /// Split out from [`Self::comparison`] so `stars:>=4` parses through exactly the same syntax as an int
    /// field does. `label` names the thing being compared and only ever appears in an error message. A second
    /// parser for `>=`, `..` and `-` would be a second set of edge cases to keep in step, and the first one
    /// somebody found would be the one where the two disagreed.
    fn operator(
        &self,
        kind: FieldKind,
        label: &str,
        value: &str,
        quoted: bool,
        column: usize,
    ) -> Result<Comparison, ParseError> {
        let op = if quoted {
            // A quoted value is always an equality against the literal text — `brand:">2020"` asks for
            // the string, not a range.
            Comparison::Equals(parse_literal(kind, value, column)?)
        } else if value == "*" {
            Comparison::Exists
        } else if value == "-" {
            Comparison::Missing
        } else if let Some(rest) = value.strip_prefix(">=") {
            range(
                kind,
                label,
                Endpoint::Inclusive(parse_literal(kind, rest, column)?),
                Endpoint::Unbounded,
                column,
            )?
        } else if let Some(rest) = value.strip_prefix("<=") {
            range(
                kind,
                label,
                Endpoint::Unbounded,
                Endpoint::Inclusive(parse_literal(kind, rest, column)?),
                column,
            )?
        } else if let Some(rest) = value.strip_prefix('>') {
            range(
                kind,
                label,
                Endpoint::Exclusive(parse_literal(kind, rest, column)?),
                Endpoint::Unbounded,
                column,
            )?
        } else if let Some(rest) = value.strip_prefix('<') {
            range(
                kind,
                label,
                Endpoint::Unbounded,
                Endpoint::Exclusive(parse_literal(kind, rest, column)?),
                column,
            )?
        } else if let Some(pattern) = wildcard(value) {
            // Substring and prefix (Q.16). Text only: a wildcard over a date or a number has no meaning that
            // is not a coincidence of formatting, and answering one would return whatever the ISO spelling
            // happened to allow.
            if kind != FieldKind::Text {
                return Err(ParseError::new(
                    "not_matchable",
                    column,
                    format!(
                        "{label} is a {} field; a wildcard only means something over text",
                        kind.as_str()
                    ),
                ));
            }
            match pattern {
                Wildcard::Contains(inner) => Comparison::Contains(inner.to_owned()),
                Wildcard::Prefix(inner) => Comparison::StartsWith(inner.to_owned()),
                // A leading wildcard alone asks for a suffix, which no index and no `LIKE` prefix can serve
                // and which is almost always a substring search typed one star short. Named rather than
                // widened, because widening it would return more than was asked for.
                Wildcard::Suffix => {
                    return Err(ParseError::new(
                        "suffix_wildcard",
                        column,
                        format!(
                            "{label}:*text asks for a suffix; write *text* for a substring search"
                        ),
                    ));
                }
            }
        } else if let Some((lower, upper)) = split_range(value) {
            let lower = match lower {
                "" => Endpoint::Unbounded,
                text => Endpoint::Inclusive(parse_literal(kind, text, column)?),
            };
            let upper = match upper {
                "" => Endpoint::Unbounded,
                text => Endpoint::Inclusive(parse_literal(kind, text, column)?),
            };
            if matches!(lower, Endpoint::Unbounded) && matches!(upper, Endpoint::Unbounded) {
                return Err(ParseError::new(
                    "empty_range",
                    column,
                    "a range needs at least one bound",
                ));
            }
            range(kind, label, lower, upper, column)?
        } else {
            Comparison::Equals(parse_literal(kind, value, column)?)
        };

        Ok(op)
    }
}

/// Builds a range, refusing a field with no ordering.
///
/// Refused here as well as in the IR's validation, so the message can carry a column. Both refuse it;
/// this one refuses it usefully.
fn range(
    kind: FieldKind,
    label: &str,
    lower: Endpoint,
    upper: Endpoint,
    column: usize,
) -> Result<Comparison, ParseError> {
    if !matches!(
        kind,
        FieldKind::Int | FieldKind::Decimal | FieldKind::Date | FieldKind::DateTime
    ) {
        return Err(ParseError::new(
            "not_orderable",
            column,
            format!(
                "{label} is a {} field, which has no ordering to range over",
                kind.as_str()
            ),
        ));
    }
    Ok(Comparison::Range { lower, upper })
}

/// What a wildcard value asks for (Q.16).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Wildcard<'a> {
    /// `*text*`
    Contains(&'a str),
    /// `text*`
    Prefix(&'a str),
    /// `*text`
    Suffix,
}

/// Reads a wildcard value, or `None` when there is no wildcard to read.
///
/// A bare `*` is [`Comparison::Exists`] and is handled before this is reached, so a pattern here always has
/// something to match. An interior star — `a*b` — is not a pattern this supports and is left to fall through
/// to equality, where it matches a filename that really does contain a star.
fn wildcard(value: &str) -> Option<Wildcard<'_>> {
    let starts = value.starts_with('*');
    let ends = value.ends_with('*');
    if !starts && !ends {
        return None;
    }
    let inner = value.trim_matches('*');
    if inner.is_empty() {
        // `**`, `***`: a pattern with nothing in it, which equality will refuse more usefully than a match
        // over everything would.
        return None;
    }
    // An interior star is not a supported pattern, and treating `a*b` as a substring for "a" would answer a
    // different question than the one typed.
    if inner.contains('*') {
        return None;
    }
    Some(match (starts, ends) {
        (true, true) => Wildcard::Contains(inner),
        (false, true) => Wildcard::Prefix(inner),
        (true, false) => Wildcard::Suffix,
        (false, false) => return None,
    })
}

/// Splits `a..b` on the range separator.
///
/// Scans from the left but skips a leading `-`, so a negative lower bound still works — and a date
/// like `2026-01-01` contains no `..` so it is unaffected.
fn split_range(value: &str) -> Option<(&str, &str)> {
    let at = value.find("..")?;
    Some((&value[..at], &value[at + 2..]))
}

/// Whether `name` could be a `field_defs.key`.
///
/// The same shape `field_defs_key_shape` enforces, so the parser and the schema agree on what a field
/// reference looks like.
fn is_field_shaped(name: &str) -> bool {
    let mut chars = name.chars();
    match chars.next() {
        Some(first) if first.is_ascii_lowercase() => {}
        _ => return false,
    }
    name.len() <= 63 && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

/// Parses `text` as a literal of `kind`.
fn parse_literal(kind: FieldKind, text: &str, column: usize) -> Result<Literal, ParseError> {
    let bad = |expected: &str| {
        ParseError::new(
            "bad_value",
            column,
            format!("expected {expected}, got {text:?}"),
        )
    };

    let literal = match kind {
        FieldKind::Text
        | FieldKind::Textarea
        | FieldKind::LongText
        | FieldKind::Select
        | FieldKind::MultiSelect
        | FieldKind::Url => Literal::Text(text.to_owned()),
        FieldKind::Int => Literal::Int(text.parse().map_err(|_| bad("an integer"))?),
        FieldKind::Decimal => Literal::Decimal(text.parse().map_err(|_| bad("a number"))?),
        FieldKind::Bool => match text {
            "true" | "yes" => Literal::Bool(true),
            "false" | "no" => Literal::Bool(false),
            _ => return Err(bad("true or false")),
        },
        FieldKind::Date => Literal::Date(
            chrono::NaiveDate::parse_from_str(text, "%Y-%m-%d").map_err(|_| bad("YYYY-MM-DD"))?,
        ),
        FieldKind::DateTime => Literal::DateTime(
            chrono::DateTime::parse_from_rfc3339(text)
                .map_err(|_| bad("an RFC 3339 timestamp"))?
                .into(),
        ),
        FieldKind::TaxonomyRef | FieldKind::UserRef => {
            Literal::Uuid(uuid::Uuid::parse_str(text).map_err(|_| bad("a UUID"))?)
        }
        FieldKind::Geo => return Err(bad("nothing — a geo field is matched by presence only")),
    };
    Ok(literal)
}
