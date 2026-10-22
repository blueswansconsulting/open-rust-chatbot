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
    /// What the links and emails in the text allow a reply to carry (see `LinkPolicy::allowed_in`).
    link_hosts: Vec<String>,
}

const DEFAULT_CHUNK_WORDS: usize = 512;
const DEFAULT_OVERLAP_WORDS: usize = 75;

/// Most chunks handed to the model per retrieval.
pub const MAX_CONTEXT_CHUNKS: usize = 6;
/// Total excerpt text (bytes) handed to the model per retrieval. Chunks longer than their share
/// are trimmed to a window around their matching words rather than dropped, so every part of
/// the question keeps an excerpt.
pub const MAX_CONTEXT_CHARS: usize = 6_000;
/// How much a query term counts once an already-selected chunk covers it. Only breaks ties
/// between chunks that each add an uncovered term.
const COVERED_TERM_WEIGHT: f64 = 0.1;
/// Bytes reserved per trimmed excerpt for the "… " / " …" markers.
const ELLIPSIS_RESERVE: usize = 8;

/// A chunk, or a trimmed window of one, chosen as context for a turn.
#[derive(Debug, Clone, PartialEq)]
pub struct Excerpt<'a> {
    pub id: &'a str,
    pub header_path: &'a [String],
    pub text: std::borrow::Cow<'a, str>,
}

impl<'a> Excerpt<'a> {
    fn whole(chunk: &'a KnowledgeChunk) -> Self {
        Self {
            id: &chunk.id,
            header_path: &chunk.header_path,
            text: std::borrow::Cow::Borrowed(&chunk.text),
        }
    }
}

/// Contractions whose apostrophe-free form is also a real word or place ("Wells", "St Ives",
/// "Shell"), so they are dropped while the apostrophe is still present.
const AMBIGUOUS_CONTRACTIONS: &[&str] = &[
    "i've", "i'll", "i'd", "we'll", "we'd", "let's", "she'll", "he'll", "it'll", "she'd", "he'd",
    "we're",
];

fn tokens(text: &str) -> Vec<String> {
    let cleaned: String = text
        .to_lowercase()
        .chars()
        .map(|c| match c {
            '’' => '\'',
            c if c.is_alphanumeric() || c.is_whitespace() || c == '-' || c == '\'' => c,
            _ => ' ',
        })
        .collect();
    cleaned
        .split_whitespace()
        .filter(|w| !AMBIGUOUS_CONTRACTIONS.contains(w))
        // Apostrophes join rather than split, so "you're" is one (stop)word, not "you" + "re".
        .map(|w| w.replace('\'', ""))
        // Two-character terms such as "ev" or "uk" carry meaning; two-letter function words are
        // dropped by the stopword list instead. Stopwords are checked before stemming only, so
        // "Wells" (stem "well") or "Ives" (stem "ive") are never mistaken for one.
        .filter(|t| t.chars().count() >= 2 && !is_stopword(t))
        .map(|t| stem(&t))
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
    "they", "their", "then", "than", "also", "just", "not", "but", "its", "use",
    // Contractions, with the apostrophe removed by the tokeniser.
    // Ambiguous ones ("I've" -> "ive", "we'll" -> "well") are dropped earlier; see
    // AMBIGUOUS_CONTRACTIONS.
    "im", "youre", "youve", "youll", "youd", "were", "weve", "theyre", "theyve", "whats", "thats",
    "theres", "heres", "dont", "doesnt", "didnt", "cant", "wont", "isnt", "arent", "wasnt",
    "havent", "hasnt", "wouldnt", "couldnt", "shouldnt", "re", "ve", "ll", "hi", "ok", "st", "nd",
    "rd", "th",
    // Chat acknowledgements: "yes, how much for one?" is about the previous topic, not about
    // whichever section starts "Yes. Thanks ...".
    "yes", "yeah", "thanks", "thank", "great", "sure", "cool", "okay", "one",
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
        let mut link_hosts =
            crate::reply::LinkPolicy::allowed_in(chunks.iter().map(|c| c.text.as_str()));
        link_hosts.sort();
        link_hosts.dedup();
        Self {
            chunks,
            idf,
            link_hosts,
        }
    }

    /// What the links and emails in the knowledge base allow (hosts, or pages on shared hosts).
    pub fn link_hosts(&self) -> &[String] {
        &self.link_hosts
    }

    pub fn from_markdown(markdown: &str) -> Self {
        Self::new(parse_markdown_knowledge(
            markdown,
            DEFAULT_CHUNK_WORDS,
            DEFAULT_OVERLAP_WORDS,
        ))
    }

    /// The text of every chunk.
    pub fn texts(&self) -> impl Iterator<Item = &str> {
        self.chunks.iter().map(|c| c.text.as_str())
    }

    pub fn load(path: &Path) -> std::io::Result<Self> {
        Ok(Self::from_markdown(&std::fs::read_to_string(path)?))
    }

    /// Context for a single message. See [`Self::retrieve_for_turn`].
    pub fn retrieve(&self, question: &str) -> Vec<Excerpt<'_>> {
        self.retrieve_for_turn(question, "")
    }

    /// Context for one chat turn: up to [`MAX_CONTEXT_CHUNKS`] excerpts within
    /// [`MAX_CONTEXT_CHARS`], one per part of the question.
    ///
    /// `question` is the visitor's current message; `context` is earlier text (their previous
    /// message) so follow-ups like "how often do I need one?" still find the topic. Chunks for
    /// the current question's terms are always chosen before chunks that only match `context`.
    /// Selection stops once every query term is covered. Excerpts longer than their share of
    /// the budget are trimmed to a window around their matching words. When nothing was trimmed
    /// and room remains, the top-level overview chunk is appended last.
    pub fn retrieve_for_turn(&self, question: &str, context: &str) -> Vec<Excerpt<'_>> {
        let terms = query_terms(question, context);
        let picks = self.select(&terms, MAX_CONTEXT_CHUNKS);
        let (mut excerpts, remaining) = self.fit_to_budget(&picks, &terms);
        let all_whole = excerpts
            .iter()
            .all(|e| matches!(e.text, std::borrow::Cow::Borrowed(_)));
        if let Some(overview) = self.chunks.first() {
            if overview.header_path.len() == 1
                && all_whole
                && !picks.is_empty()
                && !picks.contains(&0)
                && picks.len() < MAX_CONTEXT_CHUNKS
                && overview.text.len() <= remaining
            {
                excerpts.push(Excerpt::whole(overview));
            }
        }
        excerpts
    }

    /// Greedy coverage: each pick is the chunk adding the most IDF weight for query terms not
    /// yet covered (already-covered terms only break ties), restricted to chunks with an
    /// uncovered current-question term while any remain. Ties go to the higher plain score,
    /// then document order. Stops when no chunk adds an uncovered term.
    fn select(&self, terms: &[(String, bool)], limit: usize) -> Vec<usize> {
        let lookup: std::collections::HashMap<&str, usize> = terms
            .iter()
            .enumerate()
            .map(|(i, (t, _))| (t.as_str(), i))
            .collect();
        if lookup.is_empty() {
            return Vec::new();
        }

        struct Candidate {
            index: usize,
            /// (term index, weight) in term order, so float sums run in a fixed order.
            weights: Vec<(usize, f64)>,
            score: f64,
        }
        let mut candidates: Vec<Candidate> = Vec::new();
        for (index, c) in self.chunks.iter().enumerate() {
            // One pass over the chunk's tokens, so cost does not grow with query length.
            let mut tf: std::collections::BTreeMap<usize, usize> = Default::default();
            for t in &c.tokens {
                if let Some(&i) = lookup.get(t.as_str()) {
                    *tf.entry(i).or_default() += 1;
                }
            }
            if tf.is_empty() {
                continue;
            }
            let weights: Vec<(usize, f64)> = tf
                .into_iter()
                .map(|(i, n)| {
                    let term = terms[i].0.as_str();
                    let idf = self.idf.get(term).copied().unwrap_or(0.0);
                    let heading = if c.header_tokens.iter().any(|t| t == term) {
                        2.0
                    } else {
                        1.0
                    };
                    (i, idf * (1.0 + (n as f64).ln()) * heading)
                })
                .filter(|(_, w)| *w > 0.0)
                .collect();
            let score = weights.iter().map(|(_, w)| w).sum();
            if score > 0.0 {
                candidates.push(Candidate {
                    index,
                    weights,
                    score,
                });
            }
        }

        let mut covered = vec![false; terms.len()];
        let mut selected = Vec::new();
        while selected.len() < limit && !candidates.is_empty() {
            let adds_question_term =
                |c: &Candidate| c.weights.iter().any(|(t, _)| terms[*t].1 && !covered[*t]);
            let question_first = candidates.iter().any(adds_question_term);
            let best = candidates
                .iter()
                .enumerate()
                .filter(|(_, c)| !question_first || adds_question_term(c))
                .map(|(pos, c)| {
                    let gain: f64 = c
                        .weights
                        .iter()
                        .map(|(t, w)| {
                            if covered[*t] {
                                w * COVERED_TERM_WEIGHT
                            } else {
                                *w
                            }
                        })
                        .sum();
                    (gain, c.score, c.index, pos)
                })
                .max_by(|a, b| {
                    a.0.total_cmp(&b.0)
                        .then(a.1.total_cmp(&b.1))
                        .then(b.2.cmp(&a.2))
                })
                .map(|(_, _, _, pos)| pos);
            let Some(best) = best else { break };
            let pick = candidates.swap_remove(best);
            for (t, _) in &pick.weights {
                covered[*t] = true;
            }
            selected.push(pick.index);
            // Chunks that add no uncovered term are never picked, so drop them now.
            candidates.retain(|c| c.weights.iter().any(|(t, _)| !covered[*t]));
        }
        selected
    }

    /// Splits [`MAX_CONTEXT_CHARS`] across the picks (short chunks keep their full text and
    /// their unused share goes to the others) and trims each longer chunk to a window around its
    /// matching words. Returns the excerpts and the unused budget.
    fn fit_to_budget(
        &self,
        picks: &[usize],
        terms: &[(String, bool)],
    ) -> (Vec<Excerpt<'_>>, usize) {
        let mut by_length: Vec<usize> = (0..picks.len()).collect();
        by_length.sort_by_key(|&i| (self.chunks[picks[i]].text.len(), i));
        let mut allowance = vec![0; picks.len()];
        let mut remaining = MAX_CONTEXT_CHARS;
        for (k, &i) in by_length.iter().enumerate() {
            let share = remaining / (picks.len() - k);
            allowance[i] = self.chunks[picks[i]].text.len().min(share);
            remaining -= allowance[i];
        }
        let excerpts = picks
            .iter()
            .zip(allowance)
            .map(|(&index, allowed)| {
                let chunk = &self.chunks[index];
                if chunk.text.len() <= allowed {
                    Excerpt::whole(chunk)
                } else {
                    Excerpt {
                        text: std::borrow::Cow::Owned(window(&chunk.text, allowed, terms)),
                        ..Excerpt::whole(chunk)
                    }
                }
            })
            .collect();
        (excerpts, remaining)
    }
}

/// Query terms sorted and deduplicated, each flagged true when it comes from the current
/// question rather than only from the earlier context.
fn query_terms(question: &str, context: &str) -> Vec<(String, bool)> {
    let mut terms: std::collections::BTreeMap<String, bool> = Default::default();
    for t in tokens(context) {
        terms.entry(t).or_insert(false);
    }
    for t in tokens(question) {
        terms.insert(t, true);
    }
    terms.into_iter().collect()
}

/// At most `allowed` bytes of `text`: the word-aligned window holding the most distinct query
/// terms (earliest on ties), marked with "…" where text was cut.
fn window(text: &str, allowed: usize, terms: &[(String, bool)]) -> String {
    let budget = allowed.saturating_sub(ELLIPSIS_RESERVE);
    let mut spans: Vec<(usize, usize)> = Vec::new();
    let mut word_start = None;
    for (i, c) in text.char_indices() {
        match (c.is_whitespace(), word_start) {
            (true, Some(s)) => {
                spans.push((s, i));
                word_start = None;
            }
            (false, None) => word_start = Some(i),
            _ => {}
        }
    }
    if let Some(s) = word_start {
        spans.push((s, text.len()));
    }
    let lookup: std::collections::HashMap<&str, usize> = terms
        .iter()
        .enumerate()
        .map(|(i, (t, _))| (t.as_str(), i))
        .collect();
    // (span index, term index) for every word that is a query term.
    let hits: Vec<(usize, usize)> = spans
        .iter()
        .enumerate()
        .flat_map(|(si, &(s, e))| {
            tokens(&text[s..e])
                .into_iter()
                .filter_map(|t| lookup.get(t.as_str()).map(|&ti| (si, ti)))
                .collect::<Vec<_>>()
        })
        .collect();

    // Candidate starts: the first word, and a little before each matching word.
    let lead = budget / 4;
    let mut starts = vec![0usize];
    for &(si, _) in &hits {
        let from = spans[si].0.saturating_sub(lead);
        starts.push(spans.partition_point(|&(s, _)| s < from).min(si));
    }
    starts.sort_unstable();
    starts.dedup();
    let mut seen = vec![false; terms.len()];
    let mut best = (0usize, 0usize); // (distinct terms, start span)
    for &start in &starts {
        let Some(&(start_byte, _)) = spans.get(start) else {
            break;
        };
        seen.iter_mut().for_each(|s| *s = false);
        let distinct = hits
            .iter()
            .filter(|&&(si, _)| si >= start && spans[si].1 - start_byte <= budget)
            .filter(|&&(_, ti)| !std::mem::replace(&mut seen[ti], true))
            .count();
        if distinct > best.0 {
            best = (distinct, start);
        }
    }

    let start_byte = spans.get(best.1).map_or(0, |s| s.0);
    let mut end_byte = spans[best.1..]
        .iter()
        .take_while(|&&(_, e)| e - start_byte <= budget)
        .last()
        .map_or(start_byte, |s| s.1);
    if end_byte == start_byte {
        // A single word longer than the budget: cut it on a char boundary.
        end_byte = (start_byte + budget).min(text.len());
        while !text.is_char_boundary(end_byte) {
            end_byte -= 1;
        }
    }
    let mut out = String::with_capacity(end_byte - start_byte + ELLIPSIS_RESERVE);
    if start_byte > 0 {
        out.push_str("… ");
    }
    out.push_str(&text[start_byte..end_byte]);
    if end_byte < text.len() {
        out.push_str(" …");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paths(hits: &[Excerpt<'_>]) -> Vec<String> {
        hits.iter().map(|c| c.header_path.join(" > ")).collect()
    }

    /// Filler prose so sections are page-sized (~450 words, ~3KB) like real website chunks.
    fn prose(n: usize) -> String {
        let s = "Our team of qualified electricians works carefully and tidily, explaining each step \
                 before we begin and making sure the property is safe at the end of every working day.";
        let words: Vec<&str> = s.split_whitespace().collect();
        (0..n)
            .map(|i| words[i % words.len()])
            .collect::<Vec<_>>()
            .join(" ")
    }

    #[test]
    fn stemming_matches_word_forms() {
        let kb = KnowledgeBase::from_markdown(
            "# Other\nNothing here.\n# How listings are ranked\nListings are ordered editorially.",
        );
        assert_eq!(
            kb.retrieve("how do you rank electricians")[0].header_path,
            vec!["How listings are ranked"]
        );
        assert_eq!(
            kb.retrieve("ranking")[0].header_path,
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
            kb.retrieve("Do you cover Leeds?")[0].header_path,
            vec!["Electricians in Leeds"]
        );
        assert_eq!(
            kb.retrieve("how do I get quotes")[0].header_path,
            vec!["Getting quotes"]
        );
        assert!(
            kb.retrieve("you and the").is_empty(),
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
    fn best_match_comes_first_and_misses_return_nothing() {
        let kb = KnowledgeBase::from_markdown(
            "# Logos\nUse the accreditation logo\n# Fees\nAccreditation fees cost money",
        );
        assert_eq!(
            kb.retrieve("accreditation fees")[0].header_path,
            vec!["Fees"]
        );
        assert!(kb.retrieve("zzz").is_empty());
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

    /// The real demo site's knowledge base, as built from its pages.
    const DEMO_SITE_KB: &str = include_str!("../tests/fixtures/demo-electrician-kb.md");

    #[test]
    fn multi_part_question_covers_every_part_and_skips_filler_matches() {
        let q = "Do you install EV chargers in Wakefield, and roughly how long does it take?";
        for md in [DEMO_KB, DEMO_SITE_KB] {
            let kb = KnowledgeBase::from_markdown(md);
            let hits = kb.retrieve(q);
            let p = paths(&hits);
            assert!(
                hits.iter().any(|c| c.text.contains("Wakefield")),
                "no Wakefield chunk: {p:?}"
            );
            assert!(
                hits.iter()
                    .any(|c| c.header_path.last().unwrap() == "EV charger installation"),
                "no EV chunk: {p:?}"
            );
            assert!(
                !p.iter().any(|p| p.contains("Privacy")),
                "filler word 'long' pulled in privacy: {p:?}"
            );
        }
    }

    #[test]
    fn near_duplicate_sections_do_not_crowd_out_other_terms() {
        let kb = KnowledgeBase::from_markdown(DEMO_KB);
        let hits = kb.retrieve("install EV chargers in Wakefield");
        let p = paths(&hits);
        assert!(hits.iter().any(|c| c.text.contains("Wakefield")), "{p:?}");
        assert_eq!(
            p.iter()
                .filter(|p| p.ends_with("EV charger installation"))
                .count(),
            1,
            "duplicate EV section added nothing new: {p:?}"
        );
    }

    #[test]
    fn two_letter_terms_are_kept_but_function_words_are_not() {
        assert_eq!(tokens("EV charger in UK"), vec!["ev", "charger", "uk"]);
        assert!(tokens("do it on me if so").is_empty());
        let kb = KnowledgeBase::from_markdown("# Boilers\nGas boilers.\n# Cars\nWe fit EV points.");
        assert_eq!(kb.retrieve("ev")[0].header_path, vec!["Cars"]);
        assert!(kb.retrieve("how long does it take").is_empty());
    }

    #[test]
    fn contraction_fragments_are_not_query_terms() {
        assert!(
            tokens("you're you've I'll we'd it’s hi ok what's don't").is_empty(),
            "{:?}",
            tokens("you're you've I'll we'd it’s hi ok what's don't")
        );
        let md = "# Rewires\nWe do rewires.\n# Areas\nWe're based in Leeds.\n# Privacy\nYou're entitled to see your data.\n";
        let kb = KnowledgeBase::from_markdown(md);
        let hits = kb.retrieve("you're doing rewires?");
        assert_eq!(paths(&hits), vec!["Rewires"]);
    }

    #[test]
    fn place_names_that_look_like_contractions_still_match() {
        let kb = KnowledgeBase::from_markdown(
            "# Home\nFamily electricians.\n# Somerset\nWe work in Wells and Glastonbury.\n\
             # Cornwall\nWe work in St Ives and Penzance.\n# Kent\nWe work in Lydd.",
        );
        assert_eq!(
            paths(&kb.retrieve("Wells?"))[0],
            "Somerset",
            "{:?}",
            tokens("Wells?")
        );
        assert_eq!(paths(&kb.retrieve("Are you near St Ives?"))[0], "Cornwall");
        // The contractions themselves are still dropped while the apostrophe is present.
        assert!(
            tokens("I've we'll I'll we'd let's she'll").is_empty(),
            "{:?}",
            tokens("I've we'll I'll we'd let's she'll")
        );
    }

    #[test]
    fn chat_acknowledgements_are_not_query_terms() {
        assert!(tokens("yes yeah thanks thank you great sure cool okay one").is_empty());
        let kb = KnowledgeBase::from_markdown(
            "# Home\nFamily electricians.\n# FAQ\n## Are you insured?\nYes. Thanks for asking, one of our great strengths.\n\
             # Prices\nA rewire costs from 3000 pounds.",
        );
        let hits = kb.retrieve_for_turn("Yes, how much for one?", "Do you do rewires?");
        let p = paths(&hits);
        assert_eq!(p[0], "Prices", "{p:?}");
        assert!(!p.iter().any(|p| p.starts_with("FAQ")), "{p:?}");
    }

    #[test]
    fn unicode_words_tokenise_whole() {
        assert_eq!(tokens("Kraków naïve café"), vec!["kraków", "naïve", "café"]);
        let kb = KnowledgeBase::from_markdown("# Poland\nWe work in Kraków.\n# Home\nHello.");
        assert_eq!(kb.retrieve("Kraków?")[0].header_path, vec!["Poland"]);
    }

    #[test]
    fn page_sized_chunks_still_cover_every_part_of_a_three_part_question() {
        let md = format!(
            "# Home\nWelcome.\n# Rewires\nRewires cost from 3000 pounds. {}\n# EV chargers\nWe install EV chargers. {}\n# Areas\nWe cover Wakefield. {}\n",
            prose(450),
            prose(450),
            prose(450)
        );
        let kb = KnowledgeBase::from_markdown(&md);
        let hits = kb.retrieve(
            "How much is a rewire, do you install EV chargers, and do you cover Wakefield?",
        );
        let p = paths(&hits);
        for want in ["Rewires", "EV chargers", "Areas"] {
            assert!(p.iter().any(|p| p == want), "missing {want}: {p:?}");
        }
        let total: usize = hits.iter().map(|c| c.text.len()).sum();
        assert!(total <= MAX_CONTEXT_CHARS, "{total} bytes over budget");
        assert!(hits.iter().any(|c| c.text.contains("3000 pounds")));
        assert!(hits.iter().any(|c| c.text.contains("Wakefield")));
    }

    #[test]
    fn retrieve_respects_chunk_limit_and_text_budget_and_keeps_each_match() {
        let md: String = (0..8)
            .map(|i| {
                format!(
                    "# Area {i}\n{} We cover town{i}x. {}\n",
                    prose(200),
                    prose(250)
                )
            })
            .collect();
        let kb = KnowledgeBase::from_markdown(&md);
        let q: String = (0..8).map(|i| format!("town{i}x ")).collect();
        let hits = kb.retrieve(&q);
        assert_eq!(hits.len(), MAX_CONTEXT_CHUNKS);
        let total: usize = hits.iter().map(|c| c.text.len()).sum();
        assert!(total <= MAX_CONTEXT_CHARS, "{total} bytes over budget");
        for hit in &hits {
            let n = hit.header_path[0].trim_start_matches("Area ");
            assert!(
                hit.text.contains(&format!("town{n}x")),
                "trimmed away its match: {}",
                hit.text
            );
        }
    }

    #[test]
    fn an_oversized_best_chunk_is_trimmed_around_its_match() {
        let md = format!(
            "# Big\n{} Wakefield is covered.\n# Small\nNothing.",
            "electrification ".repeat(400)
        );
        let kb = KnowledgeBase::from_markdown(&md);
        let hits = kb.retrieve("Wakefield");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].header_path, vec!["Big"]);
        assert!(hits[0].text.len() <= MAX_CONTEXT_CHARS);
        assert!(hits[0].text.contains("Wakefield is covered."));
        assert!(hits[0].text.starts_with('…'));
    }

    #[test]
    fn retrieval_stops_once_every_query_term_is_covered() {
        let kb = KnowledgeBase::from_markdown(DEMO_SITE_KB);
        let hits = kb.retrieve("what's your phone number?");
        let p = paths(&hits);
        assert!(!p.iter().any(|p| p.contains("Privacy")), "{p:?}");
        assert!(
            hits.iter().any(|c| c.text.contains("01632 960123")),
            "{p:?}"
        );
        assert!(hits.len() <= 2, "filler after full coverage: {p:?}");

        let small = KnowledgeBase::from_markdown(
            &(0..12)
                .map(|i| format!("# Area {i}\nWe cover Leeds.\n"))
                .collect::<String>(),
        );
        assert_eq!(small.retrieve("Leeds").len(), 1);
    }

    #[test]
    fn ties_are_broken_by_document_order_deterministically() {
        let kb = KnowledgeBase::from_markdown("# Home\nHi.\n# A\nLeeds.\n# B\nLeeds.\n# C\nLeeds.");
        for _ in 0..5 {
            let ids: Vec<&str> = kb.retrieve("Leeds").iter().map(|c| c.id).collect();
            assert_eq!(ids, vec!["kb-1-0", "kb-0-0"]);
        }
    }

    #[test]
    fn current_question_outranks_the_previous_turn() {
        let md = format!(
            "# Home\nWelcome.\n# EV chargers\nWe install EV chargers in Wakefield. {}\n# EICR\nEICR certificates for landlords. {}\n# Prices\nA rewire costs from 3000 pounds. {}\n",
            prose(480),
            prose(480),
            prose(480)
        );
        let kb = KnowledgeBase::from_markdown(&md);
        let hits = kb.retrieve_for_turn(
            "How much is a rewire?",
            "Do you install EV chargers in Wakefield and do EICR certificates?",
        );
        let p = paths(&hits);
        assert_eq!(p[0], "Prices", "{p:?}");
        assert!(hits[0].text.contains("3000 pounds"));
    }

    #[test]
    fn overview_is_appended_only_with_room_and_a_real_top_level_heading() {
        let kb = KnowledgeBase::from_markdown(
            "# Home\nFamily electricians since 1990.\n# Services\n## Rewires\nFull rewires.",
        );
        assert_eq!(
            paths(&kb.retrieve("rewires")),
            vec!["Services > Rewires", "Home"]
        );
        assert!(kb.retrieve("zzz").is_empty());

        let one = KnowledgeBase::from_markdown("# Only\nWe do rewires.");
        assert_eq!(
            one.retrieve("rewires").len(),
            1,
            "not duplicated as overview"
        );

        let preamble = KnowledgeBase::from_markdown(
            "Internal: owner mobile 07700 900123\n# Rewires\nWe do rewires.",
        );
        let hits = preamble.retrieve("rewires");
        assert!(
            !hits.iter().any(|c| c.text.contains("07700")),
            "{:?}",
            paths(&hits)
        );

        // Page-sized matches leave no room, so the overview never takes their budget.
        let md = format!(
            "# Home\nWelcome to us. {}\n# Rewires\nRewires. {}\n# Areas\nWakefield. {}\n",
            prose(300),
            prose(450),
            prose(450)
        );
        let kb = KnowledgeBase::from_markdown(&md);
        let hits = kb.retrieve("rewires in Wakefield");
        assert_eq!(paths(&hits), vec!["Rewires", "Areas"]);
    }

    #[test]
    fn degenerate_inputs() {
        assert!(KnowledgeBase::from_markdown("")
            .retrieve("anything")
            .is_empty());
        let kb = KnowledgeBase::from_markdown("# A\nLeeds.\n# B\nLeeds.");
        assert!(kb.retrieve("").is_empty());
        assert!(kb.retrieve("the and of to it is").is_empty());
        assert!(kb.retrieve("日本語 العربية 😀").is_empty());
        assert_eq!(kb.retrieve(&"leeds ".repeat(5000)).len(), 1);
    }
}
