//! Deterministic, in-process retrieval over the bounded memory corpus.

use super::{MemoryCandidate, StoredMemory};
use std::collections::{BTreeSet, HashMap, HashSet};

const BM25_K1: f64 = 1.2;
const BM25_B: f64 = 0.75;
const PREVIEW_MAX_BYTES: usize = 64;

pub(super) fn rank(query: &str, memories: &[StoredMemory], limit: usize) -> Vec<MemoryCandidate> {
    if limit == 0 || memories.is_empty() {
        return Vec::new();
    }

    let query_terms = tokenize(query).into_iter().collect::<BTreeSet<_>>();
    if query_terms.is_empty() {
        return Vec::new();
    }

    let documents = memories
        .iter()
        .map(|memory| Document::new(memory, tokenize(&memory.content)))
        .collect::<Vec<_>>();
    let average_document_length = documents
        .iter()
        .map(|document| document.length as f64)
        .sum::<f64>()
        / documents.len() as f64;
    let inverse_document_frequencies = inverse_document_frequencies(&query_terms, &documents);

    let mut candidates = documents
        .into_iter()
        .filter_map(|document| {
            let score = bm25_score(
                &document,
                &query_terms,
                &inverse_document_frequencies,
                average_document_length,
            );
            if score == 0.0 {
                return None;
            }
            Some(MemoryCandidate {
                key: document.memory.key(),
                preview: preview(&document.memory.content),
                score,
            })
        })
        .collect::<Vec<_>>();

    candidates.sort_by(|left, right| {
        right
            .score
            .total_cmp(&left.score)
            .then_with(|| left.key.id.cmp(&right.key.id))
    });
    candidates.truncate(limit);
    candidates
}

struct Document<'a> {
    memory: &'a StoredMemory,
    term_frequencies: HashMap<String, usize>,
    length: usize,
}

impl<'a> Document<'a> {
    fn new(memory: &'a StoredMemory, tokens: Vec<String>) -> Self {
        let length = tokens.len();
        let mut term_frequencies = HashMap::new();
        for token in tokens {
            *term_frequencies.entry(token).or_default() += 1;
        }
        Self {
            memory,
            term_frequencies,
            length,
        }
    }
}

fn inverse_document_frequencies(
    query_terms: &BTreeSet<String>,
    documents: &[Document<'_>],
) -> HashMap<String, f64> {
    query_terms
        .iter()
        .map(|term| {
            let document_frequency = documents
                .iter()
                .filter(|document| document.term_frequencies.contains_key(term))
                .count() as f64;
            let document_count = documents.len() as f64;
            let idf = (1.0
                + (document_count - document_frequency + 0.5) / (document_frequency + 0.5))
                .ln();
            (term.clone(), idf)
        })
        .collect()
}

fn bm25_score(
    document: &Document<'_>,
    query_terms: &BTreeSet<String>,
    inverse_document_frequencies: &HashMap<String, f64>,
    average_document_length: f64,
) -> f64 {
    query_terms
        .iter()
        .filter_map(|term| {
            let term_frequency = *document.term_frequencies.get(term)? as f64;
            let length_ratio = if average_document_length == 0.0 {
                0.0
            } else {
                document.length as f64 / average_document_length
            };
            let denominator = term_frequency + BM25_K1 * (1.0 - BM25_B + BM25_B * length_ratio);
            Some(
                inverse_document_frequencies[term] * term_frequency * (BM25_K1 + 1.0) / denominator,
            )
        })
        .sum()
}

fn tokenize(content: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut identifier = String::new();

    for character in content.chars() {
        if character.is_alphanumeric() || character == '_' {
            identifier.push(character);
            continue;
        }
        append_identifier_tokens(&identifier, &mut tokens);
        identifier.clear();
    }
    append_identifier_tokens(&identifier, &mut tokens);
    tokens
}

fn append_identifier_tokens(identifier: &str, tokens: &mut Vec<String>) {
    if identifier.is_empty() {
        return;
    }

    let lowercase = identifier.to_lowercase();
    tokens.push(lowercase.clone());

    let mut components = HashSet::new();
    for underscore_component in identifier
        .split('_')
        .filter(|component| !component.is_empty())
    {
        for component in split_camel_case(underscore_component) {
            let component = component.to_lowercase();
            if component != lowercase && components.insert(component.clone()) {
                tokens.push(component);
            }
        }
    }
}

fn split_camel_case(identifier: &str) -> Vec<&str> {
    let mut components = Vec::new();
    let mut start = 0;
    let mut previous_was_lowercase_or_digit = false;

    for (index, character) in identifier.char_indices() {
        if index > start && character.is_uppercase() && previous_was_lowercase_or_digit {
            components.push(&identifier[start..index]);
            start = index;
        }
        previous_was_lowercase_or_digit = character.is_lowercase() || character.is_ascii_digit();
    }
    components.push(&identifier[start..]);
    components
}

fn preview(content: &str) -> String {
    if content.len() <= PREVIEW_MAX_BYTES {
        return content.to_owned();
    }

    let mut end = PREVIEW_MAX_BYTES;
    while !content.is_char_boundary(end) {
        end -= 1;
    }
    content[..end].to_owned()
}

#[cfg(test)]
mod tests {
    use super::{preview, rank, tokenize};
    use crate::core::extensions::memory::StoredMemory;

    fn memory(id: i64, content: &str) -> StoredMemory {
        StoredMemory::for_test(id, content)
    }

    #[test]
    fn tokenizer_supports_paths_and_code_identifiers() {
        assert_eq!(
            tokenize("src/core/httpServer.rs parse_request"),
            [
                "src",
                "core",
                "httpserver",
                "http",
                "server",
                "rs",
                "parse_request",
                "parse",
                "request"
            ]
        );
    }

    #[test]
    fn ranks_term_frequency_and_document_length() {
        let memories = [
            memory(1, "rust sqlite"),
            memory(2, "rust rust sqlite"),
            memory(3, "rust sqlite unrelated padding words"),
        ];

        let candidates = rank("rust sqlite", &memories, 5);

        assert_eq!(
            candidates
                .iter()
                .map(|candidate| candidate.key.id)
                .collect::<Vec<_>>(),
            [2, 1, 3]
        );
    }

    #[test]
    fn ranks_complete_matches_above_partial_matches() {
        let memories = [
            memory(1, "common"),
            memory(2, "common rare"),
            memory(3, "common"),
        ];

        let candidates = rank("common rare", &memories, 5);

        assert_eq!(
            candidates
                .iter()
                .map(|candidate| candidate.key.id)
                .collect::<Vec<_>>(),
            [2, 1, 3]
        );
    }

    #[test]
    fn broad_preference_query_returns_coherent_subset_matches() {
        let memories = [
            memory(
                1,
                "The user prefers invariant-first code review and implementation.",
            ),
            memory(
                2,
                "The user expects task scope to be followed. Read-only requests authorize no edits.",
            ),
            memory(3, "An unrelated repository fact."),
        ];

        let candidates = rank(
            "user preferences code review actionable defects read only repository",
            &memories,
            2,
        );
        let mut ids = candidates
            .iter()
            .map(|candidate| candidate.key.id)
            .collect::<Vec<_>>();
        ids.sort_unstable();

        assert_eq!(ids, [1, 2]);
    }

    #[test]
    fn broad_repository_query_returns_partial_topic_match() {
        let memories = [
            memory(
                1,
                "For Commonware storage reviews, checkpoints are trusted and paired with their database.",
            ),
            memory(2, "For Commonware networking reviews, peers are untrusted."),
            memory(
                3,
                "Durability requires an explicit synchronization boundary.",
            ),
        ];

        let candidates = rank(
            "Commonware runtime storage buffer durability review",
            &memories,
            5,
        );

        assert!(
            candidates.iter().any(|candidate| candidate.key.id == 1),
            "the storage-specific memory should survive unrelated query terms"
        );
    }

    #[test]
    fn no_overlap_abstains_and_scores_tie_by_id() {
        let memories = [memory(2, "same"), memory(1, "same")];

        assert!(rank("different", &memories, 5).is_empty());
        assert_eq!(
            rank("same", &memories, 5)
                .iter()
                .map(|candidate| candidate.key.id)
                .collect::<Vec<_>>(),
            [1, 2]
        );
    }

    #[test]
    fn preview_returns_short_content_and_truncates_at_a_utf8_boundary() {
        let short = "Use early returns.";
        assert_eq!(preview(short), short);

        let exact = "a".repeat(64);
        assert_eq!(preview(&exact), exact);

        let long = "a".repeat(65);
        assert_eq!(preview(&long), "a".repeat(64));

        let crossing = format!("{}é-tail", "a".repeat(63));
        let preview = preview(&crossing);
        assert_eq!(preview, "a".repeat(63));
        assert!(preview.is_char_boundary(preview.len()));
    }
}
