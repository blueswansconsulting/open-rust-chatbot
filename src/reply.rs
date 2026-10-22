//! Making text safe to show or pass on.
//!
//! - Replies are plain text: Markdown links become `text: url`, and bold, headings, bullets
//!   and code marks are dropped (the widget shows text as typed).
//! - Replies only carry links the bot knows: its own origins, and the hosts of links in its
//!   knowledge base and listings (on shared hosts such as bit.ly or docs.google.com, only those
//!   pages). Any other link, a `javascript:`/`data:` URL, a link with
//!   userinfo or an IP host becomes "[link removed]".
//! - Lead fields carry no links, markup, injected `LEAD:` lines or JSON at all.

use regex::{Captures, Regex};
use std::collections::HashSet;
use std::sync::{Arc, LazyLock};

pub const LINK_REMOVED: &str = "[link removed]";

/// Top-level domains a bare host ("evil-pay.co.uk", "www.example.com") is recognised by. A
/// short list, so prose like "e.g." or "approx." is never taken for a link.
const TLDS: &str = "com|net|org|info|biz|io|co|uk|me|app|dev|xyz|site|online|shop|store|top|\
click|link|live|life|tech|ai|gov|edu|eu|us|ca|au|nz|ie|de|fr|es|nl|ru|cn|tk|ml|ga|cf|gq|pw|cc|\
tv|ly|gg|je|im|win|bid|loan|work|website|space|pro|club|vip|icu|cyou|sbs|page|cloud|\
services|support|help|email|today|news|ltd|plc|scot|wales|london";

/// Shared suffixes that are never a whole allowed host (so "co.uk" in the knowledge base can't
/// allow every .co.uk site).
const PUBLIC_SUFFIXES: [&str; 14] = [
    "co.uk", "org.uk", "me.uk", "ltd.uk", "plc.uk", "net.uk", "gov.uk", "ac.uk", "sch.uk",
    "nhs.uk", "com.au", "co.nz", "co.za", "com.br",
];

/// Hosts where anyone can have a page (link shorteners, forms, payment links, social sites,
/// webmail, free site hosts). A link to one in the knowledge base or listings allows only that
/// page (and pages under it), never the whole host.
const SHARED_HOSTS: &[&str] = &[
    "bit.ly",
    "tinyurl.com",
    "t.co",
    "goo.gl",
    "ow.ly",
    "buff.ly",
    "rebrand.ly",
    "cutt.ly",
    "is.gd",
    "shorturl.at",
    "tiny.cc",
    "linktr.ee",
    "google.com",
    "google.co.uk",
    "forms.gle",
    "g.page",
    "g.co",
    "maps.app.goo.gl",
    "paypal.me",
    "paypal.com",
    "stripe.com",
    "square.site",
    "sumup.com",
    "monzo.me",
    "revolut.me",
    "gofundme.com",
    "wa.me",
    "whatsapp.com",
    "t.me",
    "calendly.com",
    "facebook.com",
    "fb.com",
    "fb.me",
    "m.me",
    "instagram.com",
    "linkedin.com",
    "twitter.com",
    "x.com",
    "youtube.com",
    "youtu.be",
    "tiktok.com",
    "pinterest.com",
    "reddit.com",
    "github.com",
    "github.io",
    "gitlab.io",
    "netlify.app",
    "vercel.app",
    "pages.dev",
    "workers.dev",
    "herokuapp.com",
    "web.app",
    "firebaseapp.com",
    "glitch.me",
    "replit.app",
    "azurewebsites.net",
    "amazonaws.com",
    "cloudfront.net",
    "wixsite.com",
    "squarespace.com",
    "weebly.com",
    "wordpress.com",
    "blogspot.com",
    "carrd.co",
    "notion.site",
    "notion.so",
    "typeform.com",
    "jotform.com",
    "surveymonkey.com",
    "eventbrite.com",
    "dropbox.com",
    "1drv.ms",
    "live.com",
    "office.com",
    "sharepoint.com",
    "box.com",
    "wetransfer.com",
    "trustpilot.com",
    "checkatrade.com",
    "mybuilder.com",
    "ratedpeople.com",
    "yell.com",
    "bark.com",
    "nextdoor.com",
    "gmail.com",
    "googlemail.com",
    "outlook.com",
    "hotmail.com",
    "hotmail.co.uk",
    "live.co.uk",
    "yahoo.com",
    "yahoo.co.uk",
    "icloud.com",
    "me.com",
    "aol.com",
    "btinternet.com",
    "sky.com",
    "proton.me",
    "protonmail.com",
];

/// Redirect pages on shared hosts ("google.com/url?q=…"): a link to one allows nothing, as
/// it could lead anywhere.
const REDIRECT_PATHS: [&str; 9] = [
    "/url",
    "/l.php",
    "/l",
    "/redirect",
    "/redir",
    "/out",
    "/away",
    "/link",
    "/r",
];

/// How [`LinkPolicy::allowed_in`] marks an exact email address on a shared host.
const EMAIL_ENTRY: &str = "email:";

fn shared_host(host: &str) -> bool {
    SHARED_HOSTS
        .iter()
        .any(|s| host == *s || host.strip_suffix(s).is_some_and(|p| p.ends_with('.')))
}

/// Anything that can be a link: a URL with a scheme (any scheme, so `xhttps://` and
/// `HTTPS://` count), a script-like scheme without slashes, an email address, or a bare host
/// with a known top-level domain (with or without `www.` and a path).
static LINK: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(&format!(
        r#"(?i)(?P<url>[a-z][a-z0-9+.\-]*://[^\s<>"']*)|(?P<scheme>\b(?:javascript|vbscript|data|file|blob|about|mailto|tel)\s*:[^\s<>"']+)|(?P<email>[a-z0-9._%+\-]+@(?P<edomain>(?:[a-z0-9](?:[a-z0-9\-]{{0,61}}[a-z0-9])?\.)+[a-z]{{2,24}}))|(?P<host>\b(?:[a-z0-9](?:[a-z0-9\-]{{0,61}}[a-z0-9])?\.)+(?:{TLDS})\b(?:[/:?#][^\s<>"']*)?)"#
    ))
    .unwrap()
});

/// `[text](url)` and `![alt](url)`, with an optional `<…>` around the URL and a title.
static MD_LINK: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"!?\[([^\[\]\n]{0,300})\]\(\s*<?([^()\s<>]*(?:\([^()\s<>]*\)[^()\s<>]*)*)>?(?:\s+"[^"\n]*")?\s*\)"#)
        .unwrap()
});
/// `<https://…>` autolinks.
static AUTOLINK: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"<([a-zA-Z][a-zA-Z0-9+.\-]*:[^<>\s]+)>").unwrap());
static HEADING: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[ \t]*#{1,6}[ \t]+").unwrap());
static BULLET: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^([ \t]*)[*+][ \t]+").unwrap());
/// The hidden lead marker in any form: `LEAD:`, `lead :`, `**LEAD:**`.
static LEAD_MARKER: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\*{0,2}\bLEAD\*{0,2}\s*:").unwrap());

/// Characters that hide or reorder text: zero-width characters, bidi controls and the like.
pub fn is_invisible(c: char) -> bool {
    matches!(
        c,
        '\u{00AD}'
            | '\u{034F}'
            | '\u{061C}'
            | '\u{115F}'
            | '\u{1160}'
            | '\u{17B4}'
            | '\u{17B5}'
            | '\u{180B}'..='\u{180F}'
            | '\u{200B}'..='\u{200F}'
            | '\u{202A}'..='\u{202E}'
            | '\u{2060}'..='\u{206F}'
            | '\u{3164}'
            | '\u{FE00}'..='\u{FE0F}'
            | '\u{FEFF}'
            | '\u{FFA0}'
            | '\u{FFF9}'..='\u{FFFB}'
            | '\u{E0000}'..='\u{E007F}'
    )
}

/// Which links a reply may carry.
#[derive(Clone, Debug, Default)]
pub struct LinkPolicy {
    /// Hosts allowed with all their pages and subdomains.
    hosts: Arc<HashSet<String>>,
    /// Pages on shared hosts, as `host/path`: allowed with the pages under them.
    pages: Arc<Vec<String>>,
    /// Exact email addresses a reply may carry: the visitor's own, and the business's own
    /// webmail addresses from the knowledge base.
    emails: HashSet<String>,
}

/// A host in one form: lower case, no trailing dot, no `www.`.
fn normal_host(host: &str) -> String {
    let host = host.trim().trim_end_matches('.').to_ascii_lowercase();
    host.strip_prefix("www.").map(str::to_owned).unwrap_or(host)
}

/// Where a matched link goes, if it can be allowed at all: (host, path without a trailing
/// `/`), for http(s) URLs without userinfo or an IP address, emails (no path) and bare hosts.
/// None means the link is never allowed.
fn link_target(caps: &Captures) -> Option<(String, String)> {
    if let Some(url) = caps.name("url") {
        let raw = trim_trailing_punctuation(url.as_str());
        let parsed = reqwest::Url::parse(raw).ok()?;
        if !matches!(parsed.scheme(), "http" | "https")
            || !parsed.username().is_empty()
            || parsed.password().is_some()
        {
            return None;
        }
        // `domain()` is None for IP addresses.
        let host = normal_host(parsed.domain()?);
        return Some((host, parsed.path().trim_end_matches('/').to_owned()));
    }
    if let Some(domain) = caps.name("edomain") {
        return Some((normal_host(domain.as_str()), String::new()));
    }
    if let Some(host) = caps.name("host") {
        let text = trim_trailing_punctuation(host.as_str());
        // A URL inside a bare host's path ("good.example/x?https://evil.example") is a link of
        // its own to the widget: never allowed.
        if text.to_ascii_lowercase().contains("://") {
            return None;
        }
        let end = text.find(['/', ':', '?', '#']).unwrap_or(text.len());
        let path_end = text.find(['?', '#']).unwrap_or(text.len()).max(end);
        let path = if text[end..].starts_with('/') {
            text[end..path_end].trim_end_matches('/')
        } else {
            ""
        };
        return Some((normal_host(&text[..end]), path.to_owned()));
    }
    None
}

/// What the widget leaves off the end of a link ("see https://x.com." links `https://x.com`).
fn trim_trailing_punctuation(s: &str) -> &str {
    s.trim_end_matches(['.', ',', ';', ':', '!', '?', ')', ']'])
}

impl LinkPolicy {
    /// Allows each of `allowed`: an origin or URL (`https://example.com`) or a host
    /// (`example.com`) with all its pages and subdomains, or a `host/path` page (from
    /// [`LinkPolicy::allowed_in`]) with the pages under it.
    pub fn new<I, S>(allowed: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let mut hosts = HashSet::new();
        let mut pages = Vec::new();
        let mut emails = HashSet::new();
        for entry in allowed {
            let entry = entry.as_ref().trim();
            if let Some(email) = entry.strip_prefix(EMAIL_ENTRY) {
                emails.insert(email.to_ascii_lowercase());
            } else if entry.contains("://") {
                if let Some(d) = reqwest::Url::parse(entry)
                    .ok()
                    .and_then(|u| u.domain().map(normal_host))
                {
                    hosts.insert(d);
                }
            } else if let Some((host, path)) = entry.split_once('/') {
                let path = path.trim_end_matches('/');
                if !path.is_empty() {
                    pages.push(format!("{}/{path}", normal_host(host)));
                }
            } else {
                hosts.insert(normal_host(entry));
            }
        }
        hosts.retain(|h| h.contains('.') && !PUBLIC_SUFFIXES.contains(&h.as_str()));
        pages.sort();
        pages.dedup();
        Self {
            hosts: Arc::new(hosts),
            pages: Arc::new(pages),
            emails,
        }
    }

    /// What the links in `texts` allow (for [`LinkPolicy::new`]): the host of each http(s) URL,
    /// email and `www.` host, or on a shared host ([`SHARED_HOSTS`]) only that page. Bare hosts
    /// without `www.` ("prices.zip") are prose more often than links, so they allow nothing.
    pub fn allowed_in<'a>(texts: impl IntoIterator<Item = &'a str>) -> Vec<String> {
        let mut out = Vec::new();
        for text in texts {
            for caps in LINK.captures_iter(text) {
                if caps
                    .name("host")
                    .is_some_and(|h| !h.as_str().to_ascii_lowercase().starts_with("www."))
                {
                    continue;
                }
                let Some((host, path)) = link_target(&caps) else {
                    continue;
                };
                if !shared_host(&host) {
                    out.push(host);
                } else if let Some(email) = caps.name("email") {
                    // The business's own webmail address, exactly.
                    out.push(format!(
                        "{EMAIL_ENTRY}{}",
                        email.as_str().to_ascii_lowercase()
                    ));
                } else if !path.is_empty() && !REDIRECT_PATHS.contains(&path.as_str()) {
                    out.push(format!("{host}{path}"));
                }
            }
        }
        out.sort();
        out.dedup();
        out
    }

    /// The same policy, also allowing these email addresses (the visitor's own) to be repeated.
    pub fn with_emails<'a>(&self, emails: impl IntoIterator<Item = &'a str>) -> Self {
        let mut policy = self.clone();
        policy
            .emails
            .extend(emails.into_iter().map(|e| e.trim().to_ascii_lowercase()));
        policy
    }

    pub fn allows_host(&self, host: &str) -> bool {
        let host = normal_host(host);
        self.hosts.iter().any(|h| {
            host == *h
                || host
                    .strip_suffix(h.as_str())
                    .is_some_and(|p| p.ends_with('.'))
        })
    }

    fn allows(&self, caps: &Captures) -> bool {
        if caps
            .name("email")
            .is_some_and(|e| self.emails.contains(&e.as_str().to_ascii_lowercase()))
        {
            return true;
        }
        let Some((host, path)) = link_target(caps) else {
            return false;
        };
        if self.allows_host(&host) {
            return true;
        }
        let page = format!("{host}{path}");
        self.pages.iter().any(|p| {
            page == *p
                || page
                    .strip_prefix(p.as_str())
                    .is_some_and(|rest| rest.starts_with('/'))
        })
    }
}

/// `text` with every link `policy` doesn't allow replaced by "[link removed]" (the widget's
/// trailing punctuation is kept). Repeated until nothing changes, so a replacement can never
/// join up with the text around it into a new link.
pub fn filter_links(text: &str, policy: &LinkPolicy) -> String {
    let mut out = text.to_owned();
    for _ in 0..4 {
        let next = LINK
            .replace_all(&out, |caps: &Captures| {
                let whole = &caps[0];
                if policy.allows(caps) {
                    return whole.to_owned();
                }
                let kept = if caps.name("url").is_some() || caps.name("host").is_some() {
                    trim_trailing_punctuation(whole)
                } else {
                    whole
                };
                format!("{LINK_REMOVED}{}", &whole[kept.len()..])
            })
            .into_owned();
        if next == out {
            break;
        }
        out = next;
    }
    out
}

/// True if `c` stays in a reply: not invisible, not a code mark, and no control character but
/// a line break or tab.
fn kept_in_reply(c: char) -> bool {
    !is_invisible(c) && c != '`' && (!c.is_control() || c == '\n' || c == '\t')
}

/// A reply as plain text: Markdown links become `text: url`, and headings, bold, bullets and
/// code marks are dropped. `at_line_start` says whether `text` starts a line (it may be a
/// piece of a streamed reply).
pub fn plain_text(text: &str, at_line_start: bool) -> String {
    let text: String = text.chars().filter(|&c| kept_in_reply(c)).collect();
    let text = MD_LINK.replace_all(&text, |c: &Captures| {
        let label = c[1].trim().trim_matches('*').trim();
        let url = c[2].trim();
        if label.is_empty() || label == url {
            url.to_owned()
        } else if url.is_empty() {
            label.to_owned()
        } else {
            format!("{label}: {url}")
        }
    });
    let text = AUTOLINK.replace_all(&text, "$1");
    let mut out = String::with_capacity(text.len());
    for (i, line) in text.split_inclusive('\n').enumerate() {
        if i == 0 && !at_line_start {
            out.push_str(line);
            continue;
        }
        let line = HEADING.replace(line, "");
        out.push_str(&BULLET.replace(&line, "$1- "));
    }
    // Bold and italics: every "**", and a "*" touching a word (not "5 * 3").
    let out = out.replace("**", "");
    let chars: Vec<char> = out.chars().collect();
    chars
        .iter()
        .enumerate()
        .filter(|&(i, &c)| {
            if c != '*' {
                return true;
            }
            let before = i.checked_sub(1).map(|j| chars[j]);
            let after = chars.get(i + 1).copied();
            // "5*3" and "5 * 3" keep theirs.
            let digit_or_space =
                |c: Option<char>| c.is_none_or(|c| c.is_whitespace() || c.is_ascii_digit());
            (before.is_none_or(char::is_whitespace) && after.is_none_or(char::is_whitespace))
                || (before.is_some_and(|c| c.is_ascii_digit()) && digit_or_space(after))
                || (after.is_some_and(|c| c.is_ascii_digit()) && digit_or_space(before))
        })
        .map(|(_, c)| *c)
        .collect()
}

/// A reply ready to show: plain text, with only allowed links.
pub fn clean_reply(text: &str, policy: &LinkPolicy, at_line_start: bool) -> String {
    filter_links(&plain_text(text, at_line_start), policy)
}

/// Cleans a reply as it streams. Text is passed on up to the last whitespace, so a link is
/// always checked whole and pieces only ever join at whitespace; an unfinished Markdown link
/// is held back until it closes.
pub struct ReplyFilter<'a> {
    policy: &'a LinkPolicy,
    pending: String,
    at_line_start: bool,
}

impl<'a> ReplyFilter<'a> {
    pub fn new(policy: &'a LinkPolicy) -> Self {
        Self {
            policy,
            pending: String::new(),
            at_line_start: true,
        }
    }

    /// Adds streamed text; returns what can be shown now (may be empty).
    pub fn push(&mut self, chunk: &str) -> String {
        // Characters cleaning drops go first, so text is only ever cut at whitespace that is
        // still there when shown (a dropped "\r" would let two pieces join into one link).
        self.pending
            .extend(chunk.chars().filter(|&c| kept_in_reply(c)));
        let cut = self.safe_cut();
        if cut == 0 {
            return String::new();
        }
        let head: String = self.pending.drain(..cut).collect();
        self.emit(&head)
    }

    /// The rest of the reply, at its end.
    pub fn finish(&mut self) -> String {
        let head = std::mem::take(&mut self.pending);
        self.emit(&head)
    }

    fn emit(&mut self, head: &str) -> String {
        let out = clean_reply(head, self.policy, self.at_line_start);
        if let Some(last) = head.chars().last() {
            self.at_line_start = last == '\n';
        }
        out
    }

    /// Where the text that is safe to clean now ends: just after the last whitespace, and
    /// before an unfinished Markdown link (cut at the whitespace before it).
    fn safe_cut(&self) -> usize {
        let p = &self.pending;
        let after_space = |s: &str| {
            s.char_indices()
                .rev()
                .find(|(_, c)| c.is_whitespace())
                .map_or(0, |(i, c)| i + c.len_utf8())
        };
        let cut = after_space(p);
        let Some(open) = p[..cut].rfind('[') else {
            return cut;
        };
        let seg = &p[open..cut];
        let unfinished = match seg.find(']') {
            None => true,
            Some(j) => seg[j + 1..].starts_with('(') && !seg[j..].contains(')'),
        };
        if unfinished {
            after_space(&p[..open])
        } else {
            cut
        }
    }
}

/// Drops JSON objects and arrays (`{…}`, `[{…}]`, `["…"]`) from `text`, up to their closing
/// bracket or the end. "[link removed]" and other bracketed words are left alone.
fn drop_json(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(i) = rest.find(['{', '[']) {
        let after = rest[i + 1..].trim_start();
        let json_like = rest[i..].starts_with('{') || after.starts_with(['{', '[', '"', ']']);
        if !json_like {
            out.push_str(&rest[..i + 1]);
            rest = &rest[i + 1..];
            continue;
        }
        out.push_str(&rest[..i]);
        out.push(' ');
        rest = &rest[i + bracket_extent(&rest[i..])..];
    }
    out.push_str(rest);
    out
}

/// Where the bracketed value at the start of `text` ends: after its matching bracket, or the
/// end of `text`.
fn bracket_extent(text: &str) -> usize {
    let (mut depth, mut in_string, mut escaped) = (0usize, false, false);
    for (i, c) in text.char_indices() {
        if in_string {
            match c {
                _ if escaped => escaped = false,
                '\\' => escaped = true,
                '"' => in_string = false,
                _ => {}
            }
            continue;
        }
        match c {
            '"' => in_string = true,
            '{' | '[' => depth += 1,
            '}' | ']' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return i + 1;
                }
            }
            _ => {}
        }
    }
    text.len()
}

/// A lead field as one line of plain text: cut at any `LEAD:` marker, without JSON, links,
/// emails or hosts ("[link removed]"), invisible or control characters, on one line, at most
/// `max` characters.
pub fn lead_text(text: &str, max: usize) -> String {
    let text = LEAD_MARKER.find(text).map_or(text, |m| &text[..m.start()]);
    let text: String = text
        .chars()
        .filter(|&c| !is_invisible(c))
        .map(|c| if c.is_whitespace() { ' ' } else { c })
        .filter(|c| !c.is_control())
        .collect();
    let text = filter_links(&drop_json(&text), &LinkPolicy::default());
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    // Spaces left before punctuation where something was dropped ("Bolton ." -> "Bolton.").
    let mut tidy = String::with_capacity(text.len());
    for c in text.chars() {
        if ",.;:!?".contains(c) && tidy.ends_with(' ') {
            tidy.pop();
        }
        tidy.push(c);
    }
    tidy.chars().take(max).collect::<String>().trim().to_owned()
}

/// True if a short lead field (a name or call-back time) has markup, JSON, a lead marker or a
/// link in it.
pub fn has_markup_or_link(text: &str) -> bool {
    text.contains(['<', '>', '{', '}', '[', ']'])
        || LEAD_MARKER.is_match(text)
        || LINK.is_match(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn site_policy() -> LinkPolicy {
        LinkPolicy::new([
            "https://electriciansranked.co.uk",
            "chat-demo.tradesites.ai",
            "d.example.com",
        ])
    }

    #[test]
    fn redteam_30_only_known_hosts_survive_in_replies() {
        let p = site_policy();
        for (input, want) in [
            (
                "See https://electriciansranked.co.uk/bolton.",
                "See https://electriciansranked.co.uk/bolton.",
            ),
            (
                "See https://www.electriciansranked.co.uk/bolton",
                "See https://www.electriciansranked.co.uk/bolton",
            ),
            (
                "Profile: https://chat-demo.tradesites.ai/p/1",
                "Profile: https://chat-demo.tradesites.ai/p/1",
            ),
            // Phishing and lookalike hosts.
            (
                "Pay at https://evil.example.com/pay now",
                "Pay at [link removed] now",
            ),
            (
                "Verify: https://electriciansranked-verify.example.com/login.",
                "Verify: [link removed].",
            ),
            (
                "https://electriciansranked.co.uk.evil.xyz/login",
                "[link removed]",
            ),
            // Userinfo, IP hosts and other schemes.
            (
                "https://electriciansranked.co.uk@evil.example.com/x",
                "[link removed]",
            ),
            ("http://192.168.0.1/admin", "[link removed]"),
            ("http://[::1]/admin", "[link removed]"),
            ("Click javascript:alert(1) now", "Click [link removed] now"),
            (
                "Open data:text/html,<b>x</b>",
                "Open [link removed]<b>x</b>",
            ),
            ("HTTPS://EVIL.EXAMPLE.COM", "[link removed]"),
            ("xhttps://evil.example.com", "[link removed]"),
            // Bare hosts and emails.
            (
                "Visit www.evil.example.com today",
                "Visit [link removed] today",
            ),
            (
                "Visit evil-pay.co.uk/pay today",
                "Visit [link removed] today",
            ),
            ("Email pay@evil.com", "Email [link removed]"),
            (
                "Email hello@electriciansranked.co.uk",
                "Email hello@electriciansranked.co.uk",
            ),
            // Prose is not a link.
            (
                "e.g. a rewire, approx. 3 days",
                "e.g. a rewire, approx. 3 days",
            ),
        ] {
            assert_eq!(filter_links(input, &p), want, "{input}");
        }
    }

    #[test]
    fn redteam_30_hosts_come_from_texts_but_never_a_bare_public_suffix() {
        let hosts = LinkPolicy::allowed_in([
            "See [our page](https://www.example-electric.co.uk/eicr) or email jo@sparks.example.org.",
            "Any .co.uk address, or co.uk alone, or http://10.0.0.1/",
        ]);
        let p = LinkPolicy::new(hosts);
        assert!(p.allows_host("example-electric.co.uk"));
        assert!(p.allows_host("sparks.example.org"));
        assert!(!p.allows_host("evil.co.uk"));
        assert!(!p.allows_host("10.0.0.1"));
    }

    #[test]
    fn a_visitors_own_email_may_be_repeated() {
        let p = site_policy().with_emails(["Sam@Gmail.com"]);
        assert_eq!(
            filter_links("We'll email sam@gmail.com.", &p),
            "We'll email sam@gmail.com."
        );
        assert_eq!(
            filter_links("Or visit gmail.com", &p),
            "Or visit [link removed]"
        );
    }

    #[test]
    fn redteam_29_markdown_becomes_plain_text() {
        for (input, want) in [
            (
                "You can [view their profile](https://d.example.com/p/1).",
                "You can view their profile: https://d.example.com/p/1.",
            ),
            (
                "[https://d.example.com/p/1](https://d.example.com/p/1)",
                "https://d.example.com/p/1",
            ),
            (
                "**Hayvern Electrical** is *highly* rated",
                "Hayvern Electrical is highly rated",
            ),
            (
                "## Opening hours\nWe open at 9.",
                "Opening hours\nWe open at 9.",
            ),
            ("Options:\n* EICR\n* Rewire", "Options:\n- EICR\n- Rewire"),
            ("Use `code` here, 5 * 3", "Use code here, 5 * 3"),
            (
                "See <https://d.example.com/x>",
                "See https://d.example.com/x",
            ),
        ] {
            assert_eq!(plain_text(input, true), want, "{input}");
        }
        // Markdown links to other sites still lose the link.
        assert_eq!(
            clean_reply(
                "[Pay here](https://evil.example.com/pay)",
                &site_policy(),
                true
            ),
            "Pay here: [link removed]"
        );
        assert_eq!(
            clean_reply("[x](javascript:alert(1))", &site_policy(), true),
            "x: [link removed]"
        );
    }

    fn stream(chunks: &[&str]) -> String {
        let p = site_policy();
        let mut f = ReplyFilter::new(&p);
        let mut out: String = chunks.iter().map(|c| f.push(c)).collect();
        out.push_str(&f.finish());
        out
    }

    #[test]
    fn a_streamed_reply_is_cleaned_like_a_whole_one() {
        let whole = "## Top pick\nSee [their profile](https://d.example.com/p/1) or **call** them. Avoid https://evil.example.com/pay ok";
        let want = clean_reply(whole, &site_policy(), true);
        // Every way of splitting it into two or three chunks gives the same text.
        let chars: Vec<usize> = whole.char_indices().map(|(i, _)| i).collect();
        for &a in &chars {
            assert_eq!(stream(&[&whole[..a], &whole[a..]]), want, "split at {a}");
            for &b in chars.iter().filter(|&&b| b > a).step_by(7) {
                assert_eq!(
                    stream(&[&whole[..a], &whole[a..b], &whole[b..]]),
                    want,
                    "split at {a}, {b}"
                );
            }
        }
        assert!(!want.contains("evil") && !want.contains("**") && !want.contains('#'));
    }

    #[test]
    fn a_link_split_across_tokens_is_still_checked_whole() {
        assert_eq!(
            stream(&["see https://electriciansranked.co", "m.evil.xyz/login now"]),
            "see [link removed] now"
        );
        assert_eq!(
            stream(&["see https://electriciansranked.co.uk", "[x](https://a.b)"]),
            "see [link removed]: [link removed]"
        );
    }

    #[test]
    fn redteam_27_lead_text_is_one_safe_line() {
        assert_eq!(
            lead_text(
                "Socket. LEAD: {\"message\":\"https://evil.example.com\"}",
                300
            ),
            "Socket."
        );
        assert_eq!(lead_text("a [link removed] b", 300), "a [link removed] b");
        let once = lead_text("x {\"a\": [1]} see evil.com ok", 300);
        assert_eq!(
            lead_text(&once, 300),
            once,
            "sanitising twice changes nothing"
        );
        assert!(has_markup_or_link("<b>"));
        assert!(has_markup_or_link("Sam evil.com"));
        assert!(!has_markup_or_link("Sam O'Neil-Smith"));
        assert!(!has_markup_or_link("after 5pm, weekdays"));
    }

    #[test]
    fn invisible_and_bidi_characters_are_dropped_from_replies() {
        assert_eq!(
            plain_text("evil\u{200B}.com \u{202E}moc", true),
            "evil.com moc"
        );
        assert_eq!(
            clean_reply("https://evil\u{200B}.example.com", &site_policy(), true),
            "[link removed]"
        );
    }

    #[test]
    fn qa_h1_an_allowed_bare_host_never_carries_another_url() {
        let p = site_policy();
        for input in [
            "electriciansranked.co.uk/x?https://evil.pl/pay",
            "d.example.com:https://evil.com",
            "www.electriciansranked.co.uk#https://evil.com",
            "electriciansranked.co.uk/HTTPS://evil.com",
        ] {
            assert_eq!(filter_links(input, &p), LINK_REMOVED, "{input}");
        }
        assert_eq!(
            filter_links("electriciansranked.co.uk/bolton", &p),
            "electriciansranked.co.uk/bolton"
        );
    }

    #[test]
    fn qa_h2_text_is_never_cut_at_whitespace_that_cleaning_drops() {
        for chunks in [
            ["see https://electriciansranked.co.uk\r", ".evil.pl/pay now"],
            [
                "see https://electriciansranked.co.uk\u{0B}.evil.pl/pay",
                " now",
            ],
            [
                "see https://electriciansranked.co.uk\u{85}",
                ".evil.pl/pay now",
            ],
            [
                "see https://electriciansranked.co.uk\u{0C}",
                ".evil.pl/pay now",
            ],
        ] {
            let out = stream(&chunks);
            assert!(!out.contains("evil"), "{chunks:?} -> {out}");
        }
    }

    #[test]
    fn qa_m1_a_page_on_a_shared_host_allows_only_that_page() {
        let allowed = LinkPolicy::allowed_in([
            "Pay at https://paypal.me/biz, book at https://calendly.com/sparks/visit, \
             short link https://bit.ly/abc, form https://docs.google.com/forms/d/X/viewform, \
             or email sparks@gmail.com. Find us on https://facebook.com",
        ]);
        let p = LinkPolicy::new(allowed);
        for ok in [
            "https://paypal.me/biz",
            "https://paypal.me/biz/10",
            "https://calendly.com/sparks/visit",
            "https://bit.ly/abc",
            "https://docs.google.com/forms/d/X/viewform",
        ] {
            assert_eq!(filter_links(ok, &p), ok);
        }
        for bad in [
            "https://paypal.me/attacker",
            "https://paypal.me/bizattacker",
            "https://bit.ly/evil",
            "https://docs.google.com/forms/d/EVIL/viewform",
            "https://facebook.com/evil",
            "https://gmail.com",
            "https://calendly.com/attacker",
        ] {
            assert_eq!(filter_links(bad, &p), LINK_REMOVED, "{bad}");
        }
    }

    #[test]
    fn qa_l1_l2_prose_is_left_alone_and_never_allows_hosts() {
        let p = site_policy();
        for prose in ["5*3 = 15", "Done.It works", "the file.zip", "e.g. 2.5mm"] {
            assert_eq!(clean_reply(prose, &p, true), prose);
        }
        assert_eq!(
            lead_text("rewire [2 bed] house", 300),
            "rewire [2 bed] house"
        );
        assert!(LinkPolicy::allowed_in(["prices.zip and example.org"]).is_empty());
        assert_eq!(
            LinkPolicy::allowed_in(["see www.example.org/x"]),
            vec!["example.org".to_owned()]
        );
    }

    #[test]
    fn qa_r2_the_businesss_own_webmail_survives_and_redirectors_allow_nothing() {
        let p = LinkPolicy::new(LinkPolicy::allowed_in([
            "Email us at jo.sparks@gmail.com or jo@btinternet.com. \
             Old link https://www.google.com/url?q=https://example.org",
        ]));
        assert_eq!(
            filter_links(
                "You can email jo.sparks@gmail.com or JO@btinternet.com.",
                &p
            ),
            "You can email jo.sparks@gmail.com or JO@btinternet.com."
        );
        assert_eq!(
            filter_links("Or other@gmail.com, or https://gmail.com", &p),
            "Or [link removed], or [link removed]"
        );
        assert_eq!(
            filter_links("https://www.google.com/url?q=https://evil.pl/pay", &p),
            LINK_REMOVED
        );
    }
}
