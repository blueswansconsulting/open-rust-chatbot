use std::path::Path;

#[derive(Debug, Clone, PartialEq)]
pub struct KnowledgeChunk {
    pub id: String,
    pub header_path: Vec<String>,
    pub text: String,
    // Pre-tokenised header + text so search does no allocation per chunk.
    tokens: Vec<String>,
}

pub struct KnowledgeBase {
    chunks: Vec<KnowledgeChunk>,
}

const DEFAULT_CHUNK_WORDS: usize = 512;
const DEFAULT_OVERLAP_WORDS: usize = 75;

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
        .filter(|t| t.len() > 2)
        .map(str::to_owned)
        .collect()
}

fn chunk(id: String, header_path: Vec<String>, text: String) -> KnowledgeChunk {
    let tokens = tokens(&format!("{} {}", header_path.join(" "), text));
    KnowledgeChunk {
        id,
        header_path,
        text,
        tokens,
    }
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
        Self { chunks }
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

    pub fn search(&self, query: &str, limit: usize) -> Vec<&KnowledgeChunk> {
        let query_tokens: std::collections::HashSet<String> = tokens(query).into_iter().collect();
        let mut scored: Vec<(usize, usize, &KnowledgeChunk)> = self
            .chunks
            .iter()
            .enumerate()
            .map(|(i, c)| {
                (
                    c.tokens
                        .iter()
                        .filter(|t| query_tokens.contains(*t))
                        .count(),
                    i,
                    c,
                )
            })
            .filter(|(score, _, _)| *score > 0)
            .collect();
        // Stable by original order on ties, matching Array.prototype.sort.
        scored.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
        scored.into_iter().take(limit).map(|(_, _, c)| c).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
