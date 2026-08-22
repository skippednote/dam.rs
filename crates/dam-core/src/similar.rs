//! Finding the name somebody meant (Q.17).
//!
//! ## Why a suggestion and not a silent correction
//!
//! The parser refuses an unknown field rather than treating it as free text, because a dropped clause returns
//! *more* than was asked for (2.1). That refusal is correct and, on its own, unhelpful: "no field named
//! `brnad`" leaves a user staring at a query they cannot see the flaw in. A suggestion makes the refusal
//! actionable without making it a guess — the query is still refused, and the correction is offered.
//!
//! Correcting silently would be the worst of both: `brnad:acme` quietly answered as `brand:acme` is a filter
//! nobody asked for, and the first time the guess is wrong the user has results they cannot explain.
//!
//! ## The distance cap
//!
//! Two edits for a short name, three for a long one. Without a cap the "closest" candidate is always
//! *something*, and suggesting `year` for `photographer` is worse than suggesting nothing: it reads as a
//! system that does not know what its own fields are called.

/// Levenshtein distance, bounded.
///
/// Returns `None` once the distance is known to exceed `cap`, which is what keeps this linear in practice —
/// and the caller has no use for a number it is going to reject anyway.
///
/// Case-insensitive over ASCII: a field key is lower case by construction, and somebody typing `Brand` has
/// made no spelling mistake at all.
#[must_use]
pub fn distance_within(a: &str, b: &str, cap: usize) -> Option<usize> {
    let left: Vec<char> = a.to_lowercase().chars().collect();
    let right: Vec<char> = b.to_lowercase().chars().collect();
    // A length difference is a lower bound on the distance, so this is the cheap rejection.
    if left.len().abs_diff(right.len()) > cap {
        return None;
    }

    let mut previous: Vec<usize> = (0..=right.len()).collect();
    let mut current = vec![0usize; right.len() + 1];
    for (i, from) in left.iter().enumerate() {
        current[0] = i + 1;
        for (j, to) in right.iter().enumerate() {
            let substitution = previous[j] + usize::from(from != to);
            current[j + 1] = substitution.min(previous[j + 1] + 1).min(current[j] + 1);
        }
        // Every distance in the finished table is at least the row's minimum, so a row that is entirely
        // beyond the cap cannot come back under it.
        if current.iter().min().is_some_and(|best| *best > cap) {
            return None;
        }
        std::mem::swap(&mut previous, &mut current);
    }
    let distance = previous[right.len()];
    (distance <= cap).then_some(distance)
}

/// How far off a name may be and still be the one somebody meant.
///
/// Scaled by length, because one wrong letter in `id` is a different word and one wrong letter in
/// `photographer` is a typo.
#[must_use]
pub fn cap_for(typed: &str) -> usize {
    match typed.chars().count() {
        0..=3 => 1,
        4..=7 => 2,
        _ => 3,
    }
}

/// The candidate closest to `typed`, or `None` when none is close enough.
///
/// Ties break on the candidate's own order, which for a field list is the tenant's ordering — so a suggestion
/// is stable across calls rather than depending on how a hash map iterated.
pub fn closest<'a, I>(candidates: I, typed: &str) -> Option<&'a str>
where
    I: IntoIterator<Item = &'a str>,
{
    let cap = cap_for(typed);
    let mut best: Option<(usize, &str)> = None;
    for candidate in candidates {
        // A prefix is the strongest signal there is: somebody typing `photog` has not misspelled anything,
        // they have stopped early, and no edit distance ranks that above a two-letter substitution.
        let scored = if candidate.len() > typed.len()
            && candidate.to_lowercase().starts_with(&typed.to_lowercase())
        {
            Some(0)
        } else {
            distance_within(typed, candidate, cap)
        };
        if let Some(distance) = scored
            && best.is_none_or(|(closest, _)| distance < closest)
        {
            best = Some((distance, candidate));
        }
    }
    best.map(|(_, candidate)| candidate)
}
