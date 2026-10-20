use std::path::Path;

#[derive(Debug, Clone, PartialEq)]
pub struct KnowledgeChunk {
    pub id: String,
    pub header_path: Vec<String>,
    pub text: String,
    // Pre-tokenised so search does no allocation per chunk.
    tokens: Vec<String>,
    header_tokens: Vec<String>,
}

pub struct KnowledgeBase {
    chunks: Vec<KnowledgeChunk>,
    /// Inverse document frequency per token: rare words (place names, "eicr") count for more.
    idf: std::collections::HashMap<String, f64>,
}

const DEFAULT_CHUNK_WORDS: usize = 512;
const DEFAULT_OVERLAP_WORDS: usize = 75;

/// Most chunks handed to the model per retrieval.
pub const MAX_CONTEXT_CHUNKS: usize = 6;
/// Total chunk text (bytes) handed to the model per retrieval. The best match is always kept
/// even if it alone exceeds this.
pub const MAX_CONTEXT_CHARS: usize = 6_000;
/// How much a query term counts once an already-selected chunk covers it. Low, so the next
/// pick prefers a chunk that answers a different part of the question.
const COVERED_TERM_WEIGHT: f64 = 0.1;
/// Penalty for a chunk under the same heading as one already selected that adds no new query
/// term (e.g. the same section copied on two pages, or overlapping splits of one section).
const NEAR_DUPLICATE_WEIGHT: f64 = 0.1;

fn tokens(text: &str) -> Vec<String> {
    let cleaned: String = text
        .to_lowercase()
        .chars()
        .map(|c| {
            if c.is_ascii_lowercase() || c.is_ascii_digit() || c.is_whitespace() || c == '-' {
                c
            } else {
                ' '
            }
        })
        .collect();
    cleaned
        .split_whitespace()
        // Two-character terms such as "ev" or "uk" carry meaning; two-letter function words are
        // dropped by the stopword list instead.
        .filter(|t| t.len() >= 2 && !is_stopword(t))
        .map(stem)
        .collect()
}

/// Very light English stemming so "rank", "ranks", "ranked" and "ranking" match each other.
fn stem(word: &str) -> String {
    let w = word;
    let stripped = if w.len() > 5 && w.ends_with("ing") {
        &w[..w.len() - 3]
    } else if w.len() > 4 && w.ends_with("ed") {
        &w[..w.len() - 2]
    } else if w.len() > 3 && w.ends_with('s') && !w.ends_with("ss") {
        &w[..w.len() - 1]
    } else {
        w
    };
    stripped.to_owned()
}

fn chunk(id: String, header_path: Vec<String>, text: String) -> KnowledgeChunk {
    let tokens = tokens(&format!("{} {}", header_path.join(" "), text));
    let header_tokens = self::tokens(&header_path.join(" "));
    KnowledgeChunk {
        id,
        header_path,
        text,
        tokens,
        header_tokens,
    }
}

/// Common words that say nothing about which section answers a question.
const STOPWORDS: &[&str] = &[
    "the", "and", "for", "you", "your", "are", "can", "what", "which", "who", "how", "when",
    "where", "why", "does", "did", "with", "this", "that", "there", "have", "has", "had", "was",
    "were", "will", "would", "should", "could", "about", "from", "into", "our", "out", "any",
    "all", "get", "got", "need", "want", "please", "help", "tell", "know", "find", "some", "them",
    "they", "their", "then", "than", "also", "just", "not", "but", "its", "it's", "i'm", "use",
    // Question filler: on their own these matched unrelated sections ("how long" hit the
    // privacy notice's "How long we keep it").
    "long", "take", "takes", "roughly", "much", "many", "often", "quick", "quickly", "soon", "like",
    "do", "in", "on", "to", "of", "is", "it", "me", "my", "we", "us", "an", "or", "if", "at", "by",
    "be", "so", "no", "up", "as", "am", "i", "a",
];

fn is_stopword(token: &str) -> bool {
    STOPWORDS.contains(&token)
}

pub fn parse_markdown_knowledge(
    markdown: &str,
    chunk_words: usize,
    overlap_words: usize,
) -> Vec<KnowledgeChunk> {
    let overlap_words = overlap_words.min(chunk_words.saturating_sub(1));
    let mut sections: Vec<(Vec<String>, String)> = Vec::new();
    let mut header_path: Vec<String> = Vec::new();
    let mut buffer: Vec<&str> = Vec::new();

    let flush = |buffer: &mut Vec<&str>,
                 header_path: &Vec<String>,
                 sections: &mut Vec<(Vec<String>, String)>| {
        let text = buffer.join("\n").trim().to_owned();
        if !text.is_empty() {
            sections.push((header_path.clone(), text));
        }
        buffer.clear();
    };

    for line in markdown.lines() {
        let hashes = line.bytes().take_while(|b| *b == b'#').count();
        let rest = &line[hashes..];
        let is_heading = (1..=6).contains(&hashes)
            && rest.starts_with(|c: char| c.is_whitespace())
            && !rest.trim().is_empty();
        if is_heading {
            flush(&mut buffer, &header_path, &mut sections);
            header_path.truncate(hashes - 1);
            header_path.push(rest.trim().to_owned());
        } else {
            buffer.push(line);
        }
    }
    flush(&mut buffer, &header_path, &mut sections);

    let mut out = Vec::new();
    for (section_index, (header_path, text)) in sections.into_iter().enumerate() {
        let words: Vec<&str> = text.split_whitespace().collect();
        if words.len() <= chunk_words {
            out.push(chunk(format!("kb-{section_index}-0"), header_path, text));
            continue;
        }
        let step = chunk_words - overlap_words;
        let mut start = 0;
        let mut chunk_index = 0;
        while start < words.len() {
            let end = (start + chunk_words).min(words.len());
            out.push(chunk(
                format!("kb-{section_index}-{chunk_index}"),
                header_path.clone(),
                words[start..end].join(" "),
            ));
            start += step;
            chunk_index += 1;
        }
    }
    out
}

impl KnowledgeBase {
    pub fn new(chunks: Vec<KnowledgeChunk>) -> Self {
        let mut df: std::collections::HashMap<String, usize> = Default::default();
        for c in &chunks {
            let unique: std::collections::HashSet<&String> = c.tokens.iter().collect();
            for t in unique {
                *df.entry(t.clone()).or_default() += 1;
            }
        }
        let n = chunks.len() as f64;
        let idf = df
            .into_iter()
            .map(|(t, d)| (t, (1.0 + n / d as f64).ln()))
            .collect();
        Self { chunks, idf }
    }

    pub fn from_markdown(markdown: &str) -> Self {
        Self::new(parse_markdown_knowledge(
            markdown,
            DEFAULT_CHUNK_WORDS,
            DEFAULT_OVERLAP_WORDS,
        ))
    }

    pub fn load(path: &Path) -> std::io::Result<Self> {
        Ok(Self::from_markdown(&std::fs::read_to_string(path)?))
    }

    /// Context for one chat turn: up to [`MAX_CONTEXT_CHUNKS`] chunks within
    /// [`MAX_CONTEXT_CHARS`], chosen so each part of a multi-part question gets a chunk. When
    /// something matched and room remains, the knowledge base's top-level overview (its first
    /// chunk) is appended last; it never displaces a matching chunk.
    pub fn retrieve(&self, query: &str) -> Vec<&KnowledgeChunk> {
        let mut hits = self.search_within(query, MAX_CONTEXT_CHUNKS, MAX_CONTEXT_CHARS);
        if let Some(overview) = self.chunks.first().filter(|c| c.header_path.len() <= 1) {
            let used: usize = hits.iter().map(|c| c.text.len()).sum();
            if !hits.is_empty()
                && hits.len() < MAX_CONTEXT_CHUNKS
                && used + overview.text.len() <= MAX_CONTEXT_CHARS
                && !hits.iter().any(|c| std::ptr::eq(*c, overview))
            {
                hits.push(overview);
            }
        }
        hits
    }

    /// Up to `limit` chunks, coverage-aware, with no text budget.
    pub fn search(&self, query: &str, limit: usize) -> Vec<&KnowledgeChunk> {
        self.search_within(query, limit, usize::MAX)
    }

    /// Scores each chunk by the IDF-weighted query words it contains (stopwords ignored, bonus
    /// for heading matches), then picks greedily: the best chunk first, then whichever chunk adds
    /// the most weight for query terms not yet covered. Ties go to the higher plain score, then
    /// to document order, so results are deterministic. A chunk that would take the total text
    /// past `max_chars` is skipped (the first pick is always kept).
    fn search_within(&self, query: &str, limit: usize, max_chars: usize) -> Vec<&KnowledgeChunk> {
        // Sorted and deduplicated so float sums run in a fixed order.
        let query_tokens: std::collections::BTreeSet<String> = tokens(query)
            .into_iter()
            .filter(|t| !is_stopword(t))
            .collect();
        let query_tokens: Vec<String> = query_tokens.into_iter().collect();

        struct Candidate {
            index: usize,
            weights: Vec<f64>,
            score: f64,
        }
        let mut candidates: Vec<Candidate> = self
            .chunks
            .iter()
            .enumerate()
            .map(|(index, c)| {
                let weights: Vec<f64> = query_tokens
                    .iter()
                    .map(|q| self.term_weight(c, q))
                    .collect();
                let score = weights.iter().sum();
                Candidate {
                    index,
                    weights,
                    score,
                }
            })
            .filter(|c| c.score > 0.0)
            .collect();

        let mut covered = vec![false; query_tokens.len()];
        let mut selected: Vec<&KnowledgeChunk> = Vec::new();
        let mut used_chars = 0usize;
        while selected.len() < limit && !candidates.is_empty() {
            let gain = |c: &Candidate| -> f64 {
                let mut adds_new_term = false;
                let gain: f64 = c
                    .weights
                    .iter()
                    .zip(&covered)
                    .map(|(w, done)| {
                        if *w > 0.0 && !done {
                            adds_new_term = true;
                        }
                        if *done {
                            w * COVERED_TERM_WEIGHT
                        } else {
                            *w
                        }
                    })
                    .sum();
                let leaf = self.chunks[c.index].header_path.last();
                let duplicate_heading = leaf.is_some()
                    && selected.iter().any(|s| {
                        s.header_path
                            .last()
                            .zip(leaf)
                            .is_some_and(|(a, b)| a.eq_ignore_ascii_case(b))
                    });
                if duplicate_heading && !adds_new_term {
                    gain * NEAR_DUPLICATE_WEIGHT
                } else {
                    gain
                }
            };
            let best = candidates
                .iter()
                .enumerate()
                .map(|(pos, c)| (gain(c), c.score, c.index, pos))
                .max_by(|a, b| {
                    a.0.total_cmp(&b.0)
                        .then(a.1.total_cmp(&b.1))
                        .then(b.2.cmp(&a.2))
                })
                .map(|(_, _, _, pos)| pos)
                .expect("candidates is non-empty");
            let pick = candidates.remove(best);
            let chunk = &self.chunks[pick.index];
            if !selected.is_empty() && used_chars.saturating_add(chunk.text.len()) > max_chars {
                continue;
            }
            used_chars = used_chars.saturating_add(chunk.text.len());
            for (done, w) in covered.iter_mut().zip(&pick.weights) {
                *done |= *w > 0.0;
            }
            selected.push(chunk);
        }
        selected
    }

    fn term_weight(&self, chunk: &KnowledgeChunk, term: &str) -> f64 {
        let tf = chunk.tokens.iter().filter(|t| *t == term).count();
        if tf == 0 {
            return 0.0;
        }
        let idf = self.idf.get(term).copied().unwrap_or(0.0);
        let heading = if chunk.header_tokens.iter().any(|t| t == term) {
            2.0
        } else {
            1.0
        };
        idf * (1.0 + (tf as f64).ln()) * heading
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stemming_matches_word_forms() {
        let kb = KnowledgeBase::from_markdown(
            "# How listings are ranked\nListings are ordered editorially.\n# Other\nNothing here.",
        );
        assert_eq!(
            kb.search("how do you rank electricians", 1)[0].header_path,
            vec!["How listings are ranked"]
        );
        assert_eq!(
            kb.search("ranking", 1)[0].header_path,
            vec!["How listings are ranked"]
        );
        assert_eq!(stem("boxes"), "boxe");
        assert_eq!(stem("address"), "address");
    }

    #[test]
    fn rare_words_and_headings_beat_common_words() {
        let kb = KnowledgeBase::from_markdown(
            "# Getting quotes\nYou can get quotes. You tell us what you need and you get quotes from you know who.\n\
             # Electricians in Leeds\nWe list electricians in Leeds.\n\
             # Electricians in York\nWe list electricians in York.",
        );
        assert_eq!(
            kb.search("Do you cover Leeds?", 1)[0].header_path,
            vec!["Electricians in Leeds"]
        );
        assert_eq!(
            kb.search("how do I get quotes", 1)[0].header_path,
            vec!["Getting quotes"]
        );
        assert!(
            kb.search("you and the", 3).is_empty(),
            "stopwords alone match nothing"
        );
    }

    #[test]
    fn keeps_header_path_per_section() {
        let chunks = parse_markdown_knowledge("# A\nintro\n## B\nbody text\n# C\nother", 512, 75);
        let paths: Vec<_> = chunks.iter().map(|c| c.header_path.join(">")).collect();
        assert_eq!(paths, vec!["A", "A>B", "C"]);
        assert_eq!(chunks[1].id, "kb-1-0");
    }

    #[test]
    fn splits_long_sections_with_overlap() {
        let body = (0..10)
            .map(|i| format!("w{i}"))
            .collect::<Vec<_>>()
            .join(" ");
        let chunks = parse_markdown_knowledge(&format!("# H\n{body}"), 4, 1);
        assert_eq!(chunks[0].text, "w0 w1 w2 w3");
        assert_eq!(chunks[1].text, "w3 w4 w5 w6");
        assert_eq!(chunks.last().unwrap().text, "w9");
    }

    #[test]
    fn search_ranks_by_token_overlap_and_skips_misses() {
        let kb = KnowledgeBase::from_markdown(
            "# Fees\nAccreditation fees cost money\n# Logos\nUse the accreditation logo",
        );
        let hits = kb.search("accreditation fees", 3);
        assert_eq!(hits[0].header_path, vec!["Fees"]);
        assert_eq!(hits.len(), 2);
        assert!(kb.search("zzz", 3).is_empty());
    }

    /// Modelled on the demo electrician site where "EV chargers in Wakefield ... how long" missed
    /// Wakefield and pulled in the privacy notice instead.
    const DEMO_KB: &str = "# Homepage\n\
Bright Spark Electrical is a family-run electrician covering Leeds, Wakefield, Bradford and the surrounding towns.\n\
## EV charger installation\n\
We fit home EV chargers from all major brands, with OZEV-approved installers.\n\
# Services\n\
## Rewires\n\
Full and partial rewires for houses and flats.\n\
## EV charger installation\n\
A typical EV charger installation takes half a day. We install tethered and untethered chargers.\n\
# Areas we cover\n\
## Towns and areas\n\
- Leeds\n- Wakefield\n- Bradford\n- Huddersfield\n\
# Privacy notice\n\
## How long we keep it\n\
We keep enquiry details for as long as needed and no longer than six years.\n";

    #[test]
    fn multi_part_question_covers_every_part_and_skips_filler_matches() {
        let kb = KnowledgeBase::from_markdown(DEMO_KB);
        let hits = kb.retrieve(
            "Do you install EV chargers in Wakefield, and roughly how long does it take?",
        );
        let paths: Vec<String> = hits.iter().map(|c| c.header_path.join(" > ")).collect();
        assert!(
            hits.iter().any(|c| c.text.contains("Wakefield")),
            "no Wakefield chunk: {paths:?}"
        );
        assert!(
            hits.iter()
                .any(|c| c.header_path.last().unwrap() == "EV charger installation"),
            "no EV chunk: {paths:?}"
        );
        assert!(
            !paths.iter().any(|p| p.contains("Privacy")),
            "filler word 'long' pulled in privacy: {paths:?}"
        );
    }

    #[test]
    fn coverage_beats_near_duplicates_even_with_a_small_limit() {
        let kb = KnowledgeBase::from_markdown(DEMO_KB);
        let hits = kb.search("install EV chargers in Wakefield", 2);
        assert_eq!(hits.len(), 2);
        assert!(
            hits.iter().any(|c| c.text.contains("Wakefield")),
            "second slot went to a duplicate EV chunk"
        );
    }

    #[test]
    fn two_letter_terms_are_kept_but_function_words_are_not() {
        assert_eq!(tokens("EV charger in UK"), vec!["ev", "charger", "uk"]);
        assert!(tokens("do it on me if so").is_empty());
        let kb = KnowledgeBase::from_markdown("# Cars\nWe fit EV points.\n# Boilers\nGas boilers.");
        assert_eq!(kb.search("ev", 3)[0].header_path, vec!["Cars"]);
        assert!(kb.search("how long does it take", 3).is_empty());
    }

    #[test]
    fn retrieve_respects_chunk_limit_and_text_budget() {
        let section = |i: usize| format!("# Area {i}\nWe cover Leeds. {}\n", "word ".repeat(300));
        let md: String = (0..12).map(section).collect();
        let kb = KnowledgeBase::from_markdown(&md);
        let hits = kb.retrieve("Leeds");
        assert!(!hits.is_empty());
        assert!(hits.len() <= MAX_CONTEXT_CHUNKS);
        let total: usize = hits.iter().map(|c| c.text.len()).sum();
        assert!(total <= MAX_CONTEXT_CHARS, "{total} chars over budget");

        let small = KnowledgeBase::from_markdown(
            &(0..12)
                .map(|i| format!("# Area {i}\nWe cover Leeds.\n"))
                .collect::<String>(),
        );
        assert_eq!(small.retrieve("Leeds").len(), MAX_CONTEXT_CHUNKS);
    }

    #[test]
    fn retrieve_always_returns_the_best_chunk_even_if_oversized() {
        // 400 long words stay in one chunk but exceed the whole text budget on their own.
        let md = format!(
            "# Big\nLeeds Leeds {}\n# Small\nLeeds too.",
            "electrification ".repeat(400)
        );
        let kb = KnowledgeBase::from_markdown(&md);
        let hits = kb.retrieve("Leeds");
        assert!(hits[0].text.len() > MAX_CONTEXT_CHARS);
        assert_eq!(hits[0].header_path, vec!["Big"]);
        assert_eq!(hits.len(), 1, "nothing else fits once the budget is spent");
    }

    #[test]
    fn ties_are_broken_by_document_order_deterministically() {
        let kb = KnowledgeBase::from_markdown("# A\nLeeds.\n# B\nLeeds.\n# C\nLeeds.");
        for _ in 0..5 {
            let ids: Vec<&str> = kb
                .search("Leeds", 3)
                .iter()
                .map(|c| c.id.as_str())
                .collect();
            assert_eq!(ids, vec!["kb-0-0", "kb-1-0", "kb-2-0"]);
        }
    }

    #[test]
    fn retrieve_adds_the_overview_only_when_room_remains_and_something_matched() {
        let kb = KnowledgeBase::from_markdown(
            "# Home\nFamily electricians since 1990.\n# Services\n## Rewires\nFull rewires.",
        );
        let hits = kb.retrieve("rewires");
        assert_eq!(hits[0].header_path, vec!["Services", "Rewires"]);
        assert_eq!(hits.last().unwrap().header_path, vec!["Home"]);
        assert!(kb.retrieve("zzz").is_empty());
    }
}
