//! Multi-site mode: bot settings loaded from a sites API and cached in memory.
//!
//! `GET {SITES_API_URL}/{id}` with `Authorization: Bearer {INTERNAL_API_TOKEN}` returns the bot
//! (see README "Multi-site mode"). `404` (or `status` other than `active`) means the bot does not
//! exist. Each bot is cached for `SITES_CACHE_SECONDS`; while the API is unreachable the last good
//! copy is served for up to `SITES_STALE_SECONDS`, after which requests get `503`.
//!
//! A bot record may name a listings dataset (`datasets.listings.sha256`). It is fetched from
//! `GET {SITES_API_URL}/{id}/datasets/listings?sha=…`, checked against that sha256, parsed once
//! and kept per bot until the sha changes, so settings-only edits never refetch it. If a new
//! dataset can't be fetched, the previous one keeps being served.

use crate::config::{render_system_prompt, valid_origin, Config, SitesConfig, SAFETY_RULES};
use crate::kb::KnowledgeBase;
use crate::listings::Listings;
use crate::reply::LinkPolicy;
use crate::tools::{self, ToolDefinition};
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Unknown ids are remembered this long, so guessing ids can't flood the sites API.
const NEGATIVE_TTL: Duration = Duration::from_secs(10);
/// After a failed fetch, the stale copy is used for this long before the API is tried again.
const ERROR_RETRY: Duration = Duration::from_secs(5);
const FETCH_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_RESPONSE_BYTES: usize = 1024 * 1024;
/// Cap on cached ids (found and missing), whatever their size.
const MAX_CACHED_IDS: usize = 10_000;
/// Outbound fetches in flight at once; more callers wait their turn.
const MAX_CONCURRENT_FETCHES: usize = 16;
/// How long a request for a bot with no usable copy waits for a fetch slot before a 503.
const SLOT_WAIT: Duration = Duration::from_secs(2);
/// Largest listings dataset body accepted.
pub const MAX_DATASET_BYTES: usize = 3 * 1024 * 1024;
const DATASET_TIMEOUT: Duration = Duration::from_secs(10);
/// After a dataset fetch fails, the same sha is not retried for this long (unless invalidated).
const DATASET_RETRY: Duration = Duration::from_secs(30);
/// The sites API's answer when the requested sha is no longer the bot's current dataset.
const NOT_CURRENT: &str = "sha not current (HTTP 404)";

/// Tool-calling directory bots are told to use the listings tool (before the safety rules).
const DIRECTORY_TOOL_HINT: &str = "\n\nThis site is a directory of businesses. To say which businesses are listed (by service, town or business name), call search_directory_listings and name only businesses it returns, with the counts it gives; links in its results may be shared. A call-back request goes to the directory team, who follow up themselves, not to a listed business.";

/// Everything one bot needs to chat. In single-site mode there is one, built from the env.
pub struct Site {
    /// The bot id in multi-site mode; empty for the single-site bot.
    pub id: String,
    pub bot_name: String,
    pub system_prompt: String,
    pub kb: Arc<KnowledgeBase>,
    /// The directory listings dataset, for bots that have one.
    pub listings: Option<Arc<Listings>>,
    pub lead_capture: bool,
    pub tool_definitions: Vec<ToolDefinition>,
    /// Where confirmed leads go (already carrying `site=<id>` in multi-site mode).
    pub lead_webhook_url: Option<String>,
    pub allowed_origins: Vec<String>,
    /// Public widget settings served at `/b/<id>/config`.
    pub public_config: Value,
    /// The partner that owns this bot, if any (an opaque id, used for partner-wide caps).
    pub partner_id: Option<String>,
    pub caps: SpendCaps,
    /// The link hosts replies may carry (see [`link_policy`]).
    pub links: LinkPolicy,
    /// The knowledge base is parsed once per version: `version`, or `updatedAt` for older APIs.
    version_key: String,
    /// Rough memory cost, for the cache budget.
    size: usize,
}

/// Reply limits for one bot (and its partner). `None` means no limit.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SpendCaps {
    #[serde(default)]
    pub replies_per_hour: Option<u32>,
    #[serde(default)]
    pub replies_per_day: Option<u32>,
    #[serde(default)]
    pub partner_replies_per_day: Option<u32>,
}

/// Links a bot's replies may carry: its own origins and pages (privacy page), and the hosts
/// of the links and emails in its knowledge base and of its listings' pages.
pub fn link_policy(
    kb: &KnowledgeBase,
    listings: Option<&Listings>,
    own_urls: &[&str],
) -> LinkPolicy {
    LinkPolicy::new(
        own_urls
            .iter()
            .map(|u| u.to_string())
            .chain(kb.link_hosts().iter().cloned())
            .chain(
                listings
                    .into_iter()
                    .flat_map(|l| l.link_hosts().iter().cloned()),
            ),
    )
}

fn tool_definitions(lead_capture: bool, listings: bool) -> Vec<ToolDefinition> {
    let mut defs: Vec<ToolDefinition> = tools::definitions()
        .into_iter()
        .filter(|t| lead_capture || t.name != "propose_lead")
        .collect();
    if listings {
        defs.push(tools::listings_definition());
    }
    defs
}

impl Site {
    /// The single-site bot, from the environment configuration.
    pub fn from_config(config: &Config, kb: KnowledgeBase) -> Self {
        let origins: Vec<&str> = config.allowed_origins.iter().map(String::as_str).collect();
        let links = link_policy(&kb, None, &origins);
        Site {
            id: String::new(),
            bot_name: config.bot_name.clone(),
            system_prompt: config.system_prompt.clone(),
            kb: Arc::new(kb),
            listings: None,
            lead_capture: config.lead_capture,
            tool_definitions: tool_definitions(config.lead_capture, false),
            lead_webhook_url: config.lead_webhook_url.clone(),
            allowed_origins: config.allowed_origins.clone(),
            public_config: json!({
                "title": null, "subtitle": null, "greeting": null, "placeholder": null, "privacyUrl": null,
                "avatar": null, "avatarMotion": null,
                "attention": null, "teaser": null, "teaserFallback": null, "label": null,
                "tabAlert": false, "replySound": false,
            }),
            partner_id: None,
            caps: SpendCaps::default(),
            links,
            version_key: String::new(),
            size: 0,
        }
    }

    #[cfg(test)]
    pub(crate) fn for_tests(id: &str) -> Self {
        Site {
            id: id.into(),
            bot_name: "b".into(),
            system_prompt: String::new(),
            kb: Arc::new(KnowledgeBase::from_markdown("")),
            listings: None,
            lead_capture: false,
            tool_definitions: Vec::new(),
            lead_webhook_url: None,
            allowed_origins: Vec::new(),
            public_config: Value::Null,
            partner_id: None,
            caps: SpendCaps::default(),
            links: LinkPolicy::default(),
            version_key: "v1".into(),
            size: 0,
        }
    }

    pub fn allows_origin(&self, origin: &str) -> bool {
        self.allowed_origins.iter().any(|a| a == origin)
    }
}

/// How the widget asks for attention; anything unknown means it doesn't.
fn attention_mode(value: Option<&str>) -> Option<&'static str> {
    ["teaser", "label", "both"]
        .into_iter()
        .find(|m| Some(*m) == value.map(str::trim))
}

/// One line of prompt text: trimmed, no control or invisible characters, at most `max` characters.
fn short_text(value: Option<String>, max: usize) -> Option<String> {
    let text: String = value?
        .chars()
        .map(|c| if c.is_whitespace() { ' ' } else { c })
        .filter(|c| !c.is_control() && !matches!(*c, '\u{200b}'..='\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2060}'..='\u{206f}' | '\u{feff}'))
        .collect();
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    (!text.is_empty() && text.chars().count() <= max).then_some(text)
}

/// Bot ids are `[a-z0-9-]{2,40}`; anything else is rejected before any fetch.
pub fn valid_id(id: &str) -> bool {
    (2..=40).contains(&id.len())
        && id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

/// `base` with its `site` query parameter replaced by `id`.
pub fn lead_url_for_site(base: &str, id: &str) -> Option<String> {
    let mut url = reqwest::Url::parse(base).ok()?;
    let kept: Vec<(String, String)> = url
        .query_pairs()
        .filter(|(k, _)| k != "site")
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect();
    url.query_pairs_mut()
        .clear()
        .extend_pairs(kept)
        .append_pair("site", id);
    Some(url.into())
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct BotRecord {
    #[serde(default)]
    status: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    bot_name: Option<String>,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    subtitle: Option<String>,
    #[serde(default)]
    greeting: Option<String>,
    #[serde(default)]
    placeholder: Option<String>,
    #[serde(default)]
    privacy_url: Option<String>,
    #[serde(default)]
    avatar: Option<String>,
    #[serde(default)]
    avatar_motion: Option<String>,
    #[serde(default)]
    attention: Option<String>,
    #[serde(default)]
    teaser: Option<String>,
    #[serde(default)]
    teaser_fallback: Option<String>,
    #[serde(default)]
    label: Option<String>,
    #[serde(default)]
    tab_alert: bool,
    #[serde(default)]
    reply_sound: bool,
    #[serde(default)]
    allowed_origins: Vec<String>,
    #[serde(default)]
    instructions: Option<String>,
    #[serde(default)]
    knowledge_base: String,
    #[serde(default)]
    lead_capture: bool,
    #[serde(default)]
    updated_at: String,
    #[serde(default)]
    version: Option<u64>,
    #[serde(default)]
    partner_id: Option<String>,
    #[serde(default)]
    caps: SpendCaps,
    /// `{"listings": {"version", "sha256", "bytes"}}` or null. Read leniently: an unexpected
    /// shape means "no dataset", never a broken bot.
    #[serde(default)]
    datasets: Option<Value>,
}

/// The listings dataset a bot record points at.
#[derive(Debug, Clone, PartialEq, Eq)]
struct DatasetRef {
    sha256: String,
    bytes: Option<u64>,
}

impl BotRecord {
    fn version_key(&self) -> String {
        match self.version {
            Some(v) => format!("v{v}"),
            None => self.updated_at.clone(),
        }
    }

    fn listings_ref(&self) -> Option<DatasetRef> {
        let r = self.datasets.as_ref()?.get("listings")?;
        let sha = r.get("sha256")?.as_str()?.trim().to_ascii_lowercase();
        (sha.len() == 64 && sha.bytes().all(|b| b.is_ascii_hexdigit())).then(|| DatasetRef {
            sha256: sha,
            bytes: r.get("bytes").and_then(Value::as_u64),
        })
    }
}

/// True if `body` hashes to `sha`, either as sent or as canonical JSON (keys sorted, compact).
fn sha_matches(body: &[u8], sha: &str) -> bool {
    use sha2::{Digest, Sha256};
    if hex::encode(Sha256::digest(body)) == sha {
        return true;
    }
    // serde_json's map is ordered by key (no preserve_order feature), so this is canonical.
    serde_json::from_slice::<Value>(body)
        .ok()
        .and_then(|v| serde_json::to_vec(&v).ok())
        .is_some_and(|canonical| hex::encode(Sha256::digest(&canonical)) == sha)
}

/// Reads at most `max` bytes of a response body, whatever it claims.
async fn read_capped(mut response: reqwest::Response, max: usize) -> Result<Vec<u8>, String> {
    if response.content_length().is_some_and(|n| n > max as u64) {
        return Err("response too large".into());
    }
    let mut body = Vec::new();
    loop {
        match response.chunk().await {
            Ok(Some(chunk)) if body.len() + chunk.len() <= max => body.extend_from_slice(&chunk),
            Ok(None) => return Ok(body),
            Ok(Some(_)) => return Err("response too large".into()),
            Err(_) => return Err("response unreadable".into()),
        }
    }
}

fn non_empty(v: Option<String>) -> Option<String> {
    v.map(|s| s.trim().to_owned()).filter(|s| !s.is_empty())
}

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum SiteError {
    /// Unknown, paused or badly formed id: 404.
    NotFound,
    /// The sites API is unreachable and there is no recent copy: 503.
    Unavailable,
}

enum Entry {
    Found {
        site: Arc<Site>,
        /// Last successful fetch.
        fetched: Instant,
        /// Last failed fetch, if the API has failed since.
        failed: Option<Instant>,
        /// Last time a request used it, for least-recently-used eviction.
        used: Instant,
    },
    Missing {
        at: Instant,
    },
}

impl Entry {
    fn size(&self) -> usize {
        match self {
            Entry::Found { site, .. } => site.size,
            Entry::Missing { .. } => 64,
        }
    }
}

/// The bot cache: a map that keeps a running total of its approximate size.
#[derive(Default)]
struct Cache {
    map: HashMap<String, Entry>,
    bytes: usize,
}

impl Cache {
    fn get(&self, id: &str) -> Option<&Entry> {
        self.map.get(id)
    }
    fn get_mut(&mut self, id: &str) -> Option<&mut Entry> {
        self.map.get_mut(id)
    }
    fn insert(&mut self, id: String, entry: Entry) {
        self.bytes += entry.size();
        if let Some(old) = self.map.insert(id, entry) {
            self.bytes -= old.size();
        }
    }
    fn remove(&mut self, id: &str) {
        if let Some(old) = self.map.remove(id) {
            self.bytes -= old.size();
        }
    }
    #[cfg(test)]
    fn len(&self) -> usize {
        self.map.len()
    }
    fn retain(&mut self, mut keep: impl FnMut(&Entry) -> bool) {
        let mut freed = 0;
        self.map.retain(|_, e| {
            let k = keep(e);
            if !k {
                freed += e.size();
            }
            k
        });
        self.bytes -= freed;
    }
    fn over(&self, max_ids: usize, max_bytes: usize) -> bool {
        self.map.len() > max_ids || self.bytes > max_bytes
    }
    /// Drops expired entries, then unknown ids, then the least recently used bots until the
    /// cache fits. Never empties the whole cache at once, so one busy partner can't flush
    /// everyone else's bots.
    fn evict(&mut self, max_ids: usize, max_bytes: usize, expired: impl Fn(&Entry) -> bool) {
        self.retain(|e| !expired(e));
        if self.over(max_ids, max_bytes) {
            self.retain(|e| matches!(e, Entry::Found { .. }));
        }
        if !self.over(max_ids, max_bytes) {
            return;
        }
        let mut by_use: Vec<(Instant, String)> = self
            .map
            .iter()
            .filter_map(|(id, e)| match e {
                Entry::Found { used, .. } => Some((*used, id.clone())),
                Entry::Missing { .. } => None,
            })
            .collect();
        by_use.sort();
        // Evict down to 90% so a full cache doesn't evict on every insert.
        let (ids, bytes) = (max_ids * 9 / 10, max_bytes / 10 * 9);
        for (_, id) in by_use {
            if self.map.len() <= ids && self.bytes <= bytes {
                break;
            }
            self.remove(&id);
        }
    }
}

enum Lookup {
    Fresh(Result<Arc<Site>, SiteError>),
    Stale { site: Arc<Site>, refresh: bool },
    Miss,
}

enum Fetched {
    Found(Box<BotRecord>),
    Missing,
    Failed,
}

pub struct SiteStore {
    settings: SitesConfig,
    production: bool,
    lead_webhook_url: Option<String>,
    http: reqwest::Client,
    entries: Mutex<Cache>,
    fetch_slots: tokio::sync::Semaphore,
    /// Bumped on every invalidation, so a fetch that was already in flight (and may have read
    /// the old settings) is not cached over it.
    invalidations: AtomicU64,
    /// Bots with a background refresh in flight.
    refreshing: Mutex<std::collections::HashSet<String>>,
    /// True for text-only providers (no tool calling).
    text_only: bool,
    /// The parsed listings dataset per bot. Kept apart from the bot cache so an invalidation
    /// (a settings edit) doesn't throw it away: it is replaced only when its sha changes.
    datasets: Mutex<HashMap<String, Arc<Listings>>>,
    /// Last failed dataset fetch per bot: (sha, when), so a broken dataset isn't refetched on
    /// every request.
    dataset_failures: Mutex<HashMap<String, (String, Instant)>>,
    /// Held while a dataset is fetched and parsed, so at most one body is in memory at a time
    /// and concurrent misses for the same sha collapse into one fetch (re-checked after it).
    dataset_lock: tokio::sync::Mutex<()>,
    /// Bots whose new dataset is being fetched in the background.
    dataset_refreshing: Mutex<std::collections::HashSet<String>>,
}

impl SiteStore {
    pub fn new(config: &Config, settings: SitesConfig, http: reqwest::Client) -> Self {
        Self {
            settings,
            production: config.production,
            // Lead capture also needs the secret; without one it is off (or log-only in dev).
            lead_webhook_url: config
                .lead_webhook_url
                .clone()
                .filter(|_| config.lead_webhook_secret.is_some()),
            http,
            entries: Mutex::default(),
            fetch_slots: tokio::sync::Semaphore::new(MAX_CONCURRENT_FETCHES),
            invalidations: AtomicU64::new(0),
            refreshing: Mutex::default(),
            text_only: !config.llm.supports_tools(),
            datasets: Mutex::default(),
            dataset_failures: Mutex::default(),
            dataset_lock: tokio::sync::Mutex::new(()),
            dataset_refreshing: Mutex::default(),
        }
    }

    pub fn invalidate(&self, id: &str) {
        let mut entries = self.entries.lock().unwrap();
        entries.remove(id);
        self.invalidations.fetch_add(1, Ordering::SeqCst);
        // Something changed: a dataset that failed before may be fetchable now.
        self.dataset_failures.lock().unwrap().remove(id);
    }

    /// Drops a bot's dataset (it was removed, or the bot is gone).
    fn forget_dataset(&self, id: &str) {
        self.datasets.lock().unwrap().remove(id);
        self.dataset_failures.lock().unwrap().remove(id);
    }

    /// The dataset `wanted` names. The cached one is used if its sha matches. If a different
    /// (older) one is cached, it keeps being served while the new one is fetched in the
    /// background, so no request waits on a dataset push. Only a bot with no dataset yet waits
    /// for the fetch. On any failure the previous dataset (if any) is kept. The flag is true when
    /// the sites API says the sha is no longer current (404), so the caller should re-read the bot.
    async fn listings_for(
        self: &Arc<Self>,
        id: &str,
        wanted: Option<DatasetRef>,
    ) -> (Option<Arc<Listings>>, bool) {
        let Some(wanted) = wanted else {
            self.forget_dataset(id);
            return (None, false);
        };
        let current = self.current_dataset(id);
        if current
            .as_ref()
            .is_some_and(|l| l.sha256() == wanted.sha256)
            || self.failed_recently(id, &wanted.sha256)
        {
            return (current, false);
        }
        if current.is_some() {
            self.refresh_dataset_in_background(id, wanted);
            return (current, false);
        }
        let Ok(_guard) =
            tokio::time::timeout(DATASET_TIMEOUT + SLOT_WAIT, self.dataset_lock.lock()).await
        else {
            // Served without listings this once. Don't let this load be cached (it would hide the
            // dataset for the cache TTL); the next request retries.
            self.invalidations.fetch_add(1, Ordering::SeqCst);
            return (None, false);
        };
        // Another request may have fetched it while this one waited.
        let current = self.current_dataset(id);
        if current
            .as_ref()
            .is_some_and(|l| l.sha256() == wanted.sha256)
            || self.failed_recently(id, &wanted.sha256)
        {
            return (current, false);
        }
        match self.fetch_and_store(id, &wanted).await {
            Ok(listings) => (Some(listings), false),
            Err(rotated) => (current, rotated),
        }
    }

    fn current_dataset(&self, id: &str) -> Option<Arc<Listings>> {
        self.datasets.lock().unwrap().get(id).cloned()
    }

    fn failed_recently(&self, id: &str, sha: &str) -> bool {
        self.dataset_failures
            .lock()
            .unwrap()
            .get(id)
            .is_some_and(|(failed, at)| failed == sha && at.elapsed() < DATASET_RETRY)
    }

    /// Fetches the new dataset while the old one keeps being served. When it arrives (or the
    /// sha turns out to be stale), the cached bot is dropped so the next request rebuilds it.
    fn refresh_dataset_in_background(self: &Arc<Self>, id: &str, wanted: DatasetRef) {
        if !self
            .dataset_refreshing
            .lock()
            .unwrap()
            .insert(id.to_owned())
        {
            return; // one dataset fetch per bot at a time
        }
        let store = self.clone();
        let id = id.to_owned();
        tokio::spawn(async move {
            {
                let _guard = store.dataset_lock.lock().await;
                let done = store
                    .current_dataset(&id)
                    .is_some_and(|l| l.sha256() == wanted.sha256)
                    || store.failed_recently(&id, &wanted.sha256);
                let rebuild = done || {
                    match store.fetch_and_store(&id, &wanted).await {
                        Ok(_) => true,
                        Err(rotated) => rotated,
                    }
                };
                if rebuild {
                    store.entries.lock().unwrap().remove(&id);
                    // A load already in flight may hold the old dataset: don't let it cache.
                    store.invalidations.fetch_add(1, Ordering::SeqCst);
                }
            }
            store.dataset_refreshing.lock().unwrap().remove(&id);
        });
    }

    /// Fetches, verifies and stores a dataset. Every failure is logged and backed off for
    /// [`DATASET_RETRY`] (a 404 too, so a sha the API keeps refusing isn't refetched on every
    /// request); the error is true for a 404 (the sha is no longer current).
    async fn fetch_and_store(&self, id: &str, wanted: &DatasetRef) -> Result<Arc<Listings>, bool> {
        let kept_previous = self.current_dataset(id).is_some();
        let result = self.fetch_listings(id, wanted).await;
        match result {
            Ok(listings) => {
                crate::log::log_event(
                    "listings_loaded",
                    json!({ "bot": id, "sha256": wanted.sha256, "listings": listings.len(), "bytes": listings.approx_bytes() }),
                );
                let listings = Arc::new(listings);
                self.datasets
                    .lock()
                    .unwrap()
                    .insert(id.to_owned(), listings.clone());
                self.dataset_failures.lock().unwrap().remove(id);
                Ok(listings)
            }
            Err(detail) => {
                let rotated = detail == NOT_CURRENT;
                crate::log::log_event(
                    if rotated {
                        "listings_sha_rotated"
                    } else {
                        "listings_fetch_failed"
                    },
                    json!({ "bot": id, "sha256": wanted.sha256, "detail": detail, "kept_previous": kept_previous }),
                );
                self.dataset_failures
                    .lock()
                    .unwrap()
                    .insert(id.to_owned(), (wanted.sha256.clone(), Instant::now()));
                Err(rotated)
            }
        }
    }

    async fn fetch_listings(&self, id: &str, wanted: &DatasetRef) -> Result<Listings, String> {
        if wanted.bytes.is_some_and(|b| b > MAX_DATASET_BYTES as u64) {
            return Err("dataset too large".into());
        }
        let url = format!("{}/{id}/datasets/listings", self.settings.api_url);
        let response = self
            .http
            .get(url)
            .query(&[("sha", wanted.sha256.as_str())])
            .timeout(DATASET_TIMEOUT)
            .bearer_auth(&self.settings.token)
            .send()
            .await
            .map_err(|e| e.without_url().to_string())?;
        let status = response.status();
        if status == reqwest::StatusCode::NOT_FOUND {
            return Err(NOT_CURRENT.into());
        }
        if !status.is_success() {
            return Err(format!("HTTP {}", status.as_u16()));
        }
        let body = read_capped(response, MAX_DATASET_BYTES).await?;
        let sha = wanted.sha256.clone();
        // Hashing and parsing up to 3 MB is CPU work: keep it off the (single) async thread.
        tokio::task::spawn_blocking(move || {
            if !sha_matches(&body, &sha) {
                return Err("sha256 mismatch".to_owned());
            }
            Listings::parse(&body, &sha)
        })
        .await
        .map_err(|_| "dataset parse panicked".to_owned())?
    }

    /// Loads a bot. A fresh cached copy is returned at once; an expired one (still within
    /// the stale window) is also returned at once while one background fetch refreshes it, so
    /// known bots never wait on the sites API. Only ids with no usable copy wait, briefly, for
    /// a fetch slot.
    pub async fn get(self: &Arc<Self>, id: &str) -> Result<Arc<Site>, SiteError> {
        if !valid_id(id) {
            return Err(SiteError::NotFound);
        }
        match self.lookup(id) {
            Lookup::Fresh(result) => return result,
            Lookup::Stale { site, refresh } => {
                if refresh {
                    self.refresh_in_background(id);
                }
                return Ok(site);
            }
            Lookup::Miss => {}
        }
        let _slot = tokio::time::timeout(SLOT_WAIT, self.fetch_slots.acquire())
            .await
            .map_err(|_| SiteError::Unavailable)?
            .map_err(|_| SiteError::Unavailable)?;
        // Another caller may have fetched it while this one waited for a slot.
        match self.lookup(id) {
            Lookup::Fresh(result) => return result,
            Lookup::Stale { site, .. } => return Ok(site),
            Lookup::Miss => {}
        }
        self.load(id).await
    }

    fn refresh_in_background(self: &Arc<Self>, id: &str) {
        if !self.refreshing.lock().unwrap().insert(id.to_owned()) {
            return; // one refresh per bot at a time
        }
        let store = self.clone();
        let id = id.to_owned();
        tokio::spawn(async move {
            if let Ok(_slot) = store.fetch_slots.acquire().await {
                let _ = store.load(&id).await;
            }
            store.refreshing.lock().unwrap().remove(&id);
        });
    }

    /// Fetches a bot and records the outcome in the cache.
    async fn load(self: &Arc<Self>, id: &str) -> Result<Arc<Site>, SiteError> {
        let generation = self.invalidations.load(Ordering::SeqCst);
        let fetched = self.fetch(id).await;
        // The knowledge base is parsed once per version, and outside the cache lock so a burst
        // of changed bots doesn't hold up every other lookup.
        let (built, fetched) = match fetched {
            Fetched::Found(mut record) if record.status == "active" => {
                let (mut listings, rotated) = self.listings_for(id, record.listings_ref()).await;
                if rotated {
                    // The dataset changed between reading the bot and fetching it: read the bot
                    // again for the new sha (once), instead of treating it as an error.
                    if let Fetched::Found(newer) = self.fetch(id).await {
                        if newer.status == "active" && newer.listings_ref() != record.listings_ref()
                        {
                            record = newer;
                            listings = self.listings_for(id, record.listings_ref()).await.0;
                        }
                    }
                }
                let key = record.version_key();
                let previous = match self.entries.lock().unwrap().get(id) {
                    Some(Entry::Found { site, .. }) => Some(site.clone()),
                    _ => None,
                };
                // Only the parsed knowledge base is reused; everything else (caps, origins, the
                // partner) is rebuilt from the record, so it can't go stale under one version.
                let kb = previous
                    .filter(|site| !site.version_key.is_empty() && site.version_key == key)
                    .map(|site| site.kb.clone());
                let site = Arc::new(self.build(id, *record, kb, listings));
                (Some(site), Fetched::Missing)
            }
            other => (None, other),
        };
        let mut entries = self.entries.lock().unwrap();
        // An invalidation while this fetch was in flight means it may hold old settings.
        let cacheable = generation == self.invalidations.load(Ordering::SeqCst);
        let now = Instant::now();
        let result = match (built, fetched) {
            (Some(site), _) => {
                if cacheable {
                    entries.insert(
                        id.to_owned(),
                        Entry::Found {
                            site: site.clone(),
                            fetched: now,
                            failed: None,
                            used: now,
                        },
                    );
                }
                Ok(site)
            }
            (None, Fetched::Found(_) | Fetched::Missing) => {
                if cacheable {
                    entries.insert(id.to_owned(), Entry::Missing { at: now });
                }
                self.forget_dataset(id);
                Err(SiteError::NotFound)
            }
            (None, Fetched::Failed) => match entries.get_mut(id) {
                Some(Entry::Found {
                    site,
                    fetched,
                    failed,
                    ..
                }) if fetched.elapsed() < self.stale_ttl(site) => {
                    *failed = Some(now);
                    Ok(site.clone())
                }
                _ => {
                    // Too old to serve (or never loaded): drop it so nothing stale lingers.
                    if matches!(entries.get(id), Some(Entry::Found { .. })) {
                        entries.remove(id);
                        self.forget_dataset(id);
                    }
                    Err(SiteError::Unavailable)
                }
            },
        };
        if entries.over(MAX_CACHED_IDS, self.settings.max_cache_bytes) {
            entries.evict(MAX_CACHED_IDS, self.settings.max_cache_bytes, |e| match e {
                Entry::Missing { at } => at.elapsed() >= NEGATIVE_TTL,
                Entry::Found { site, fetched, .. } => fetched.elapsed() >= self.stale_ttl(site),
            });
            // Datasets of evicted bots go too, so they stay within the cache budget.
            self.datasets
                .lock()
                .unwrap()
                .retain(|id, _| matches!(entries.get(id), Some(Entry::Found { .. })));
        }
        result
    }

    /// Partner bots are served stale for less time, so disabling a partner takes effect fast.
    fn stale_ttl(&self, site: &Site) -> Duration {
        if site.partner_id.is_some() {
            self.settings.stale_ttl.min(self.settings.partner_stale_ttl)
        } else {
            self.settings.stale_ttl
        }
    }

    fn lookup(&self, id: &str) -> Lookup {
        let mut entries = self.entries.lock().unwrap();
        match entries.get_mut(id) {
            Some(Entry::Missing { at }) if at.elapsed() < NEGATIVE_TTL => {
                Lookup::Fresh(Err(SiteError::NotFound))
            }
            Some(Entry::Found {
                site,
                fetched,
                failed,
                used,
            }) => {
                if fetched.elapsed() < self.settings.cache_ttl {
                    *used = Instant::now();
                    Lookup::Fresh(Ok(site.clone()))
                } else if fetched.elapsed() < self.stale_ttl(site) {
                    *used = Instant::now();
                    Lookup::Stale {
                        site: site.clone(),
                        // After a failure, wait a little before asking the sites API again.
                        refresh: failed.is_none_or(|f| f.elapsed() >= ERROR_RETRY),
                    }
                } else {
                    Lookup::Miss
                }
            }
            _ => Lookup::Miss,
        }
    }

    async fn fetch(&self, id: &str) -> Fetched {
        let url = format!("{}/{id}", self.settings.api_url);
        let response = self
            .http
            .get(url)
            .timeout(FETCH_TIMEOUT)
            .bearer_auth(&self.settings.token)
            .send()
            .await;
        let response = match response {
            Ok(r) => r,
            Err(e) => {
                crate::log::log_event(
                    "sites_api_failed",
                    json!({ "bot": id, "detail": e.without_url().to_string() }),
                );
                return Fetched::Failed;
            }
        };
        let status = response.status();
        if status == reqwest::StatusCode::NOT_FOUND {
            return Fetched::Missing;
        }
        if !status.is_success()
            || response
                .content_length()
                .is_some_and(|n| n > MAX_RESPONSE_BYTES as u64)
        {
            crate::log::log_event(
                "sites_api_failed",
                json!({ "bot": id, "detail": format!("HTTP {}", status.as_u16()) }),
            );
            return Fetched::Failed;
        }
        // Read at most MAX_RESPONSE_BYTES, whatever the response claims.
        let Ok(body) = read_capped(response, MAX_RESPONSE_BYTES).await else {
            crate::log::log_event(
                "sites_api_failed",
                json!({ "bot": id, "detail": "response too large or unreadable" }),
            );
            return Fetched::Failed;
        };
        match serde_json::from_slice::<BotRecord>(&body) {
            Ok(record) => Fetched::Found(Box::new(record)),
            Err(_) => {
                crate::log::log_event(
                    "sites_api_failed",
                    json!({ "bot": id, "detail": "invalid bot JSON" }),
                );
                Fetched::Failed
            }
        }
    }

    fn build(
        &self,
        id: &str,
        record: BotRecord,
        kb: Option<Arc<KnowledgeBase>>,
        listings: Option<Arc<Listings>>,
    ) -> Site {
        let version_key = record.version_key();
        let size = record.knowledge_base.len() * 3
            + record.instructions.as_deref().map_or(0, str::len)
            + listings.as_ref().map_or(0, |l| l.approx_bytes())
            + 4096;
        let bot_name = non_empty(record.bot_name)
            .or_else(|| non_empty(record.name.clone()))
            .unwrap_or_else(|| "Assistant".into());
        let lead_webhook_url = self
            .lead_webhook_url
            .as_deref()
            .and_then(|base| lead_url_for_site(base, id));
        // Confirmed leads need somewhere to go (outside dev mode, where they are only logged).
        let lead_capture = record.lead_capture && (lead_webhook_url.is_some() || !self.production);
        let template = non_empty(record.instructions)
            .unwrap_or_else(|| crate::config::DEFAULT_SYSTEM_PROMPT.into());
        let allowed_origins: Vec<String> = record
            .allowed_origins
            .iter()
            .map(|o| o.trim().trim_end_matches('/').to_ascii_lowercase())
            .filter(|o| valid_origin(o, self.production))
            .take(10)
            .collect();
        let partner_id = non_empty(record.partner_id);
        let privacy_url = non_empty(record.privacy_url)
            .filter(|u| u.starts_with("https://") || u.starts_with("http://"));
        let mut system_prompt =
            render_system_prompt(&template, &bot_name, lead_capture, self.text_only);
        if listings.is_some() && !self.text_only {
            // Text-only bots get the lookup itself with every turn instead.
            system_prompt.insert_str(
                system_prompt.len() - SAFETY_RULES.len(),
                DIRECTORY_TOOL_HINT,
            );
        }
        let avatar = crate::avatars::preset(record.avatar.as_deref());
        let avatar_motion = avatar.map(|_| crate::avatars::motion(record.avatar_motion.as_deref()));
        let attention = attention_mode(record.attention.as_deref());
        let prompt_text = |text: Option<String>, max: usize| attention.and(short_text(text, max));
        let kb =
            kb.unwrap_or_else(|| Arc::new(KnowledgeBase::from_markdown(&record.knowledge_base)));
        let own_urls: Vec<&str> = allowed_origins
            .iter()
            .map(String::as_str)
            .chain(privacy_url.as_deref())
            .collect();
        let links = link_policy(&kb, listings.as_deref(), &own_urls);
        Site {
            id: id.to_owned(),
            system_prompt,
            kb,
            tool_definitions: tool_definitions(lead_capture, listings.is_some() && !self.text_only),
            listings,
            lead_capture,
            lead_webhook_url,
            allowed_origins,
            public_config: json!({
                "title": non_empty(record.title).or_else(|| non_empty(record.name)),
                "subtitle": non_empty(record.subtitle),
                "greeting": non_empty(record.greeting),
                "placeholder": non_empty(record.placeholder),
                "privacyUrl": privacy_url,
                "avatar": avatar,
                "avatarMotion": avatar_motion,
                "attention": attention,
                "teaser": prompt_text(record.teaser, 120),
                "teaserFallback": prompt_text(record.teaser_fallback, 120),
                "label": prompt_text(record.label, 30),
                "tabAlert": record.tab_alert,
                "replySound": record.reply_sound,
            }),
            bot_name,
            partner_id,
            caps: record.caps,
            links,
            version_key,
            size,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_short_lowercase_slugs() {
        assert!(
            valid_id("er")
                && valid_id("abc")
                && valid_id("plumbing-co-7f3a")
                && valid_id(&"a".repeat(40))
        );
        assert!(!valid_id("a") && !valid_id(&"a".repeat(41)));
        assert!(!valid_id("Abc") && !valid_id("a_b") && !valid_id("a.b") && !valid_id("a/b"));
        assert!(!valid_id("../x") && !valid_id("abc%2f"));
    }

    #[test]
    fn lead_url_gets_the_site_parameter_replaced() {
        assert_eq!(
            lead_url_for_site("https://d.example/api/leads?site=old&k=v", "er").unwrap(),
            "https://d.example/api/leads?k=v&site=er"
        );
        assert_eq!(
            lead_url_for_site("https://d.example/api/leads", "tradesites").unwrap(),
            "https://d.example/api/leads?site=tradesites"
        );
        assert!(lead_url_for_site("not a url", "x").is_none());
    }

    #[test]
    fn bot_origins_must_be_exact() {
        assert!(valid_origin("https://a.example", true));
        assert!(valid_origin("http://localhost:3000", true));
        assert!(!valid_origin("http://a.example", true));
        assert!(!valid_origin("https://*.example", true));
        assert!(!valid_origin("https://a.example/path", true));
        assert!(!valid_origin("*", true));
        assert!(!valid_origin("https://", true));
    }

    fn site(size: usize) -> Arc<Site> {
        let mut s = Site::for_tests("x");
        s.size = size;
        Arc::new(s)
    }

    fn found(size: usize, used: Instant) -> Entry {
        Entry::Found {
            site: site(size),
            fetched: Instant::now(),
            failed: None,
            used,
        }
    }

    #[test]
    fn cache_tracks_its_size_through_replace_and_remove() {
        let mut c = Cache::default();
        c.insert("a".into(), found(100, Instant::now()));
        c.insert("b".into(), found(50, Instant::now()));
        c.insert("a".into(), found(10, Instant::now()));
        assert_eq!(c.bytes, 60);
        c.remove("b");
        c.remove("missing");
        assert_eq!(c.bytes, 10);
        c.retain(|_| false);
        assert_eq!((c.bytes, c.len()), (0, 0));
    }

    #[test]
    fn a_full_cache_drops_least_recently_used_bots_not_everything() {
        let start = Instant::now();
        let mut c = Cache::default();
        for (i, id) in ["old", "mid", "hot"].iter().enumerate() {
            c.insert(
                (*id).into(),
                found(100, start + Duration::from_secs(i as u64)),
            );
        }
        c.insert("nope".into(), Entry::Missing { at: start });
        // Budget for two bots: the unknown id goes first, then the least recently used bot.
        c.evict(10, 250, |_| false);
        assert!(c.get("hot").is_some() && c.get("mid").is_some());
        assert!(c.get("old").is_none() && c.get("nope").is_none());
        assert!(c.bytes <= 250);
    }

    #[test]
    fn the_id_cap_also_evicts_by_least_recent_use() {
        let start = Instant::now();
        let mut c = Cache::default();
        for i in 0..20u64 {
            c.insert(format!("b{i}"), found(1, start + Duration::from_secs(i)));
        }
        c.evict(10, usize::MAX, |_| false);
        assert!(c.len() <= 9, "evicts to 90%: {}", c.len());
        assert!(c.get("b19").is_some() && c.get("b0").is_none());
    }

    #[test]
    fn version_wins_over_updated_at_for_reparsing() {
        let mut r: BotRecord = serde_json::from_value(json!({
            "status": "active", "updatedAt": "2026-10-19T17:00:00.000Z", "version": 7
        }))
        .unwrap();
        assert_eq!(r.version_key(), "v7");
        r.version = None;
        assert_eq!(r.version_key(), "2026-10-19T17:00:00.000Z");
    }

    #[test]
    fn dataset_refs_are_read_leniently() {
        let sha = "a".repeat(64);
        let r = |datasets: Value| -> Option<DatasetRef> {
            serde_json::from_value::<BotRecord>(json!({"status": "active", "datasets": datasets}))
                .unwrap()
                .listings_ref()
        };
        assert_eq!(
            r(json!({"listings": {"version": 3, "sha256": sha.to_uppercase(), "bytes": 10}})),
            Some(DatasetRef {
                sha256: sha.clone(),
                bytes: Some(10)
            })
        );
        assert_eq!(r(json!(null)), None);
        assert_eq!(r(json!({"listings": null})), None);
        assert_eq!(
            r(json!({"listings": {"sha256": "abc"}})),
            None,
            "not a sha256"
        );
        assert_eq!(r(json!("garbage")), None);
        // The currently deployed dashboard sends no `datasets` key at all.
        let old: BotRecord = serde_json::from_value(json!({"status": "active"})).unwrap();
        assert_eq!(old.listings_ref(), None);
    }

    #[test]
    fn sha_is_checked_over_the_body_or_its_canonical_json() {
        use sha2::{Digest, Sha256};
        let body = br#"{ "b": 1, "a": [1, 2.5, "x"] }"#;
        let raw = hex::encode(Sha256::digest(body));
        let canonical = hex::encode(Sha256::digest(br#"{"a":[1,2.5,"x"],"b":1}"#));
        assert!(sha_matches(body, &raw));
        assert!(sha_matches(body, &canonical));
        assert!(!sha_matches(body, &"0".repeat(64)));
        assert!(!sha_matches(b"not json", &canonical));
    }

    #[test]
    fn caps_and_partner_are_read_but_never_published() {
        let r: BotRecord = serde_json::from_value(json!({
            "status": "active", "partnerId": "tradesites",
            "caps": {"repliesPerHour": 60, "repliesPerDay": 300, "partnerRepliesPerDay": 20000}
        }))
        .unwrap();
        assert_eq!(
            r.caps,
            SpendCaps {
                replies_per_hour: Some(60),
                replies_per_day: Some(300),
                partner_replies_per_day: Some(20000)
            }
        );
        assert_eq!(r.partner_id.as_deref(), Some("tradesites"));
    }
}
