//! Multi-site mode: reply counting for billing, and per-bot spend caps.
//!
//! Every chat turn that reaches the model is counted per bot and per UTC hour. Counts are sent
//! to the dashboard in batches (`POST {USAGE_API_URL}` with `USAGE_TOKEN`); a batch that fails
//! is retried with the same `batchId` so it is never counted twice. Counts live here, not in
//! the bot cache, so evicting a bot never resets its caps, and today's totals are read back
//! from the dashboard on startup so a restart doesn't either.
//!
//! Caps come from the bot record (`caps`): replies per hour and per day for the bot, a daily
//! ceiling for its partner, and a per-visitor share of the bot's daily cap so one source can't
//! use it all up.

use crate::log::log_event;
use crate::sites::{Site, SpendCaps};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::HashMap;
use std::sync::Mutex;

const HOUR: u64 = 3600;
const DAY: u64 = 86_400;
/// One visitor may use at most this share (1/n) of a bot's daily cap...
const PER_VISITOR_SHARE: u32 = 10;
/// ...but never fewer than this many turns a day.
const MIN_PER_VISITOR: u32 = 10;
/// Visitors tracked per bot per day; beyond this new visitors share one bucket.
const MAX_VISITORS_TRACKED: usize = 4096;
/// One visitor using this share (percent) of a bot's hourly cap is logged as heavy use.
const HEAVY_SHARE_PERCENT: u32 = 20;

/// Whether a visitor may start another turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Admission {
    Admitted,
    /// This visitor has used their own share for now; the bot still answers others.
    VisitorLimit,
    /// The bot (or its partner) is out of replies for everyone.
    BotBusy,
}

#[derive(Default, Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Counts {
    pub replies: u64,
    pub model_calls: u64,
    pub chars_in: u64,
    pub chars_out: u64,
    pub capped: bool,
}

#[derive(Serialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Entry {
    pub bot_id: String,
    pub hour: String,
    #[serde(flatten)]
    pub counts: Counts,
}

#[derive(Serialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Batch {
    pub batch_id: String,
    pub instance_id: String,
    /// Every count for hours before this one has been sent (in this or an earlier batch).
    pub watermark_hour: String,
    pub entries: Vec<Entry>,
}

/// What the dashboard said about a batch.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum Sent {
    /// Stored (or a duplicate of one already stored).
    Accepted,
    /// Refused as invalid: retrying the same batch can never succeed, so drop it.
    Rejected,
    /// Network error or 5xx/429: retry the same batch later.
    Failed,
}

#[derive(Default)]
struct BotWindow {
    hour: u64,
    hour_count: u32,
    day: u64,
    day_count: u32,
    visitors: HashMap<String, u32>,
    /// Turns per visitor this hour.
    visitors_hour: HashMap<String, u32>,
    /// The hour a cap refusal was last logged, so a capped bot logs once an hour.
    busy_logged: Option<u64>,
    /// Visitors already logged as heavy users this hour.
    heavy_logged: std::collections::HashSet<String>,
}

#[derive(Default)]
struct State {
    seq: u64,
    pending: HashMap<(String, u64), Counts>,
    inflight: Option<Batch>,
    bots: HashMap<String, BotWindow>,
    /// Partner id -> (day, replies that day).
    partners: HashMap<String, (u64, u32)>,
}

/// Today's totals as the dashboard has them, used to restore caps after a restart.
#[derive(Deserialize, Default, Debug)]
#[serde(rename_all = "camelCase")]
pub struct Seed {
    #[serde(default)]
    pub bots: HashMap<String, SeedBot>,
    #[serde(default)]
    pub partners: HashMap<String, u32>,
}

#[derive(Deserialize, Default, Debug, Clone, Copy)]
#[serde(rename_all = "camelCase")]
pub struct SeedBot {
    #[serde(default)]
    pub hour: u32,
    #[serde(default)]
    pub day: u32,
}

pub struct Meter {
    instance_id: String,
    reporting: bool,
    /// Turns one visitor may start per bot per hour (`VISITOR_REPLIES_PER_HOUR`).
    visitor_hourly: Option<u32>,
    state: Mutex<State>,
}

pub fn hour_iso(hour_start: u64) -> String {
    chrono::DateTime::from_timestamp(hour_start as i64, 0)
        .unwrap_or_default()
        .format("%Y-%m-%dT%H:00:00Z")
        .to_string()
}

/// A visitor key (an IP address or /64) for logs: a short one-way hash, never the address.
fn visitor_hash(visitor: &str) -> String {
    use sha2::Digest;
    hex::encode(&sha2::Sha256::digest(visitor.as_bytes())[..6])
}

pub fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

impl BotWindow {
    /// Moves to a new hour or day. Only ever forwards, so a late call with an older time
    /// can't reset the current counts.
    fn roll(&mut self, now: u64) {
        if now / HOUR > self.hour {
            self.hour = now / HOUR;
            self.hour_count = 0;
            self.visitors_hour.clear();
            self.heavy_logged.clear();
        }
        if now / DAY > self.day {
            self.day = now / DAY;
            self.day_count = 0;
            self.visitors.clear();
        }
    }
}

/// Defaults applied to partner bots whose record doesn't carry caps, so a missing field never
/// means "unlimited" for a bot someone else is billed for.
const PARTNER_DEFAULT_CAPS: SpendCaps = SpendCaps {
    replies_per_hour: Some(60),
    replies_per_day: Some(300),
    partner_replies_per_day: Some(20_000),
};

fn effective_caps(site: &Site) -> SpendCaps {
    let caps = site.caps;
    if site.partner_id.is_none() {
        return caps;
    }
    SpendCaps {
        replies_per_hour: caps
            .replies_per_hour
            .or(PARTNER_DEFAULT_CAPS.replies_per_hour),
        replies_per_day: caps
            .replies_per_day
            .or(PARTNER_DEFAULT_CAPS.replies_per_day),
        partner_replies_per_day: caps
            .partner_replies_per_day
            .or(PARTNER_DEFAULT_CAPS.partner_replies_per_day),
    }
}

fn partner_used(partners: &HashMap<String, (u64, u32)>, partner: &str, now: u64) -> u32 {
    match partners.get(partner) {
        Some(&(day, used)) if day == now / DAY => used,
        _ => 0,
    }
}

impl Meter {
    /// `reporting` is false when usage isn't sent anywhere; counts for caps are still kept.
    pub fn new(reporting: bool) -> Self {
        Self {
            // Fresh on every boot, so batch ids from before a restart are never reused.
            instance_id: crate::random_token()[..16].to_owned(),
            reporting,
            visitor_hourly: None,
            state: Mutex::default(),
        }
    }

    fn pending<'a>(&self, st: &'a mut State, bot: &str, now: u64) -> Option<&'a mut Counts> {
        self.reporting.then(|| {
            st.pending
                .entry((bot.to_owned(), now / HOUR * HOUR))
                .or_default()
        })
    }

    /// Limits each visitor to `cap` turns per bot per hour (0: no hourly limit), well below a
    /// bot's hourly cap, so one visitor can't make the chat busy for everyone.
    pub fn with_visitor_hourly_cap(mut self, cap: u32) -> Self {
        self.visitor_hourly = (cap > 0).then_some(cap);
        self
    }

    /// Like [`Meter::admit_turn`], as a yes or no.
    pub fn admit(&self, site: &Site, visitor: &str, now: u64) -> bool {
        self.admit_turn(site, visitor, now) == Admission::Admitted
    }

    /// Checks the caps and, if this visitor may start another turn, counts it straight away
    /// (as one model call), before the model is asked. Counting up front means a visitor who
    /// disconnects mid-reply is still counted, and concurrent turns can't overshoot the caps.
    /// A visitor over their own daily or hourly share gets `VisitorLimit` (even if the bot is
    /// also capped, as their own limit is what they should be told about).
    pub fn admit_turn(&self, site: &Site, visitor: &str, now: u64) -> Admission {
        if site.id.is_empty() {
            return Admission::Admitted;
        }
        let caps = effective_caps(site);
        let mut logs = Vec::new();
        let admission = {
            let mut st = self.state.lock().unwrap();
            let partner_ok = match (&site.partner_id, caps.partner_replies_per_day) {
                (Some(p), Some(cap)) => partner_used(&st.partners, p, now) < cap,
                _ => true,
            };
            let window = st.bots.entry(site.id.clone()).or_default();
            window.roll(now);
            let hour_ok = caps.replies_per_hour.is_none_or(|c| window.hour_count < c);
            let day_ok = caps.replies_per_day.is_none_or(|c| window.day_count < c);
            let visitor_key = if window.visitors.len() < MAX_VISITORS_TRACKED
                || window.visitors.contains_key(visitor)
            {
                visitor.to_owned()
            } else {
                String::from("~overflow")
            };
            let visitor_cap = caps
                .replies_per_day
                .map(|d| (d / PER_VISITOR_SHARE).max(MIN_PER_VISITOR));
            let visitor_used = window.visitors.get(&visitor_key).copied().unwrap_or(0);
            let visitor_hour_used = window.visitors_hour.get(&visitor_key).copied().unwrap_or(0);
            // The shared overflow bucket isn't limited, so it can't be used to lock out new
            // visitors.
            let visitor_ok = visitor_key == "~overflow"
                || (visitor_cap.is_none_or(|c| visitor_used < c)
                    && self.visitor_hourly.is_none_or(|c| visitor_hour_used < c));
            if !visitor_ok {
                Admission::VisitorLimit
            } else if !(hour_ok && day_ok && partner_ok) {
                if window.busy_logged != Some(now / HOUR) {
                    window.busy_logged = Some(now / HOUR);
                    let top = window.visitors_hour.values().copied().max().unwrap_or(0);
                    let limit = if !partner_ok {
                        "partner_day"
                    } else if !day_ok {
                        "day"
                    } else {
                        "hour"
                    };
                    logs.push((
                        "bot_cap_reached",
                        json!({
                            "bot": site.id,
                            "limit": limit,
                            "hourCount": window.hour_count,
                            "dayCount": window.day_count,
                            "topVisitorHourCount": top,
                        }),
                    ));
                }
                if let Some(counts) = self.pending(&mut st, &site.id, now) {
                    counts.capped = true;
                }
                Admission::BotBusy
            } else {
                window.hour_count = window.hour_count.saturating_add(1);
                window.day_count = window.day_count.saturating_add(1);
                *window.visitors.entry(visitor_key.clone()).or_default() += 1;
                let used = window.visitors_hour.entry(visitor_key.clone()).or_default();
                *used += 1;
                let used = *used;
                if let Some(cap) = caps.replies_per_hour.filter(|&c| c > 0) {
                    if visitor_key != "~overflow"
                        && used * 100 >= cap * HEAVY_SHARE_PERCENT
                        && window.heavy_logged.insert(visitor_key.clone())
                    {
                        logs.push((
                            "visitor_heavy_use",
                            json!({
                                "bot": site.id,
                                "visitor": visitor_hash(&visitor_key),
                                "hourCount": used,
                                "botHourlyCap": cap,
                            }),
                        ));
                    }
                }
                if let Some(partner) = &site.partner_id {
                    let entry = st.partners.entry(partner.clone()).or_insert((now / DAY, 0));
                    if now / DAY > entry.0 {
                        *entry = (now / DAY, 0);
                    }
                    entry.1 = entry.1.saturating_add(1);
                }
                if let Some(counts) = self.pending(&mut st, &site.id, now) {
                    counts.model_calls += 1;
                }
                Admission::Admitted
            }
        };
        for (event, fields) in logs {
            log_event(event, fields);
        }
        admission
    }

    /// True if the bot (or its partner) is out of replies for now; per-visitor limits don't count.
    pub fn bot_capped(&self, site: &Site, now: u64) -> bool {
        if site.id.is_empty() {
            return false;
        }
        let caps = effective_caps(site);
        let mut st = self.state.lock().unwrap();
        let partner_capped = match (&site.partner_id, caps.partner_replies_per_day) {
            (Some(p), Some(cap)) => partner_used(&st.partners, p, now) >= cap,
            _ => false,
        };
        let window = st.bots.entry(site.id.clone()).or_default();
        window.roll(now);
        partner_capped
            || caps
                .replies_per_hour
                .is_some_and(|c| window.hour_count >= c)
            || caps.replies_per_day.is_some_and(|c| window.day_count >= c)
    }

    /// Records how an admitted turn ended: a completed reply is billable; either way the
    /// characters in and out are kept for reconciliation.
    pub fn record(&self, site: &Site, completed: bool, chars_in: usize, chars_out: usize) {
        if site.id.is_empty() {
            return;
        }
        let mut st = self.state.lock().unwrap();
        if let Some(counts) = self.pending(&mut st, &site.id, unix_now()) {
            counts.replies += u64::from(completed);
            counts.chars_in += chars_in as u64;
            counts.chars_out += chars_out as u64;
        }
    }

    /// Adds today's totals from the dashboard after a restart (before anything counted since
    /// boot has been reported, so nothing is counted twice).
    pub fn seed(&self, seed: &Seed, now: u64) {
        let mut st = self.state.lock().unwrap();
        for (id, counts) in &seed.bots {
            let window = st.bots.entry(id.clone()).or_default();
            window.roll(now);
            window.hour_count = window.hour_count.saturating_add(counts.hour);
            window.day_count = window.day_count.saturating_add(counts.day);
        }
        for (id, &used) in &seed.partners {
            let entry = st.partners.entry(id.clone()).or_insert((now / DAY, 0));
            if now / DAY > entry.0 {
                *entry = (now / DAY, 0);
            }
            entry.1 = entry.1.saturating_add(used);
        }
    }

    /// The batch to send now: the one still in flight if its last send failed, otherwise a new
    /// one holding everything counted so far. Also forgets counters from earlier days.
    pub fn next_batch(&self, now: u64) -> Batch {
        let mut st = self.state.lock().unwrap();
        st.bots.retain(|_, w| w.day >= now / DAY);
        st.partners.retain(|_, (day, _)| *day >= now / DAY);
        if let Some(batch) = &st.inflight {
            return batch.clone();
        }
        st.seq += 1;
        let mut entries: Vec<Entry> = st
            .pending
            .drain()
            .map(|((bot_id, hour), counts)| Entry {
                bot_id,
                hour: hour_iso(hour),
                counts,
            })
            .collect();
        entries.sort_by(|a, b| (&a.hour, &a.bot_id).cmp(&(&b.hour, &b.bot_id)));
        let batch = Batch {
            batch_id: format!("{}:{}", self.instance_id, st.seq),
            instance_id: self.instance_id.clone(),
            watermark_hour: hour_iso(now / HOUR * HOUR),
            entries,
        };
        st.inflight = Some(batch.clone());
        batch
    }

    /// True if there is anything to send (counts, or a batch waiting to be resent).
    pub fn has_unsent(&self) -> bool {
        let st = self.state.lock().unwrap();
        st.inflight.is_some() || !st.pending.is_empty()
    }

    pub fn finish_batch(&self, batch: &Batch, sent: Sent) {
        let mut st = self.state.lock().unwrap();
        if st.inflight.as_ref().map(|b| &b.batch_id) != Some(&batch.batch_id) {
            return;
        }
        match sent {
            Sent::Accepted => st.inflight = None,
            Sent::Rejected => {
                log_event(
                    "usage_batch_rejected",
                    json!({ "batch": batch.batch_id, "entries": batch.entries.len() }),
                );
                st.inflight = None;
            }
            Sent::Failed => {}
        }
    }
}

/// Sends one batch to the dashboard.
pub async fn send(http: &reqwest::Client, url: &str, token: &str, batch: &Batch) -> Sent {
    let res = http
        .post(url)
        .timeout(std::time::Duration::from_secs(5))
        .bearer_auth(token)
        .json(batch)
        .send()
        .await;
    match res {
        Ok(r) if r.status().is_success() => Sent::Accepted,
        // Only "this batch is invalid" is final; auth or routing errors (401, 403, 404) are
        // configuration problems that a fix will clear, so the batch is kept and retried.
        Ok(r) if matches!(r.status().as_u16(), 400 | 409 | 422) => Sent::Rejected,
        Ok(r) => {
            log_event(
                "usage_flush_failed",
                json!({ "detail": format!("HTTP {}", r.status().as_u16()) }),
            );
            Sent::Failed
        }
        Err(e) => {
            log_event(
                "usage_flush_failed",
                json!({ "detail": e.without_url().to_string() }),
            );
            Sent::Failed
        }
    }
}

/// Reads today's totals from the dashboard (`GET {url}/today`).
pub async fn fetch_seed(http: &reqwest::Client, url: &str, token: &str) -> Option<Seed> {
    let res = http
        .get(format!("{url}/today"))
        .timeout(std::time::Duration::from_secs(5))
        .bearer_auth(token)
        .send()
        .await
        .ok()?;
    if !res.status().is_success() {
        return None;
    }
    res.json().await.ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    const T0: u64 = 1_760_000_400; // some time on a UTC day, not on an hour boundary

    fn site(id: &str, partner: Option<&str>, caps: SpendCaps) -> Site {
        let mut s = Site::for_tests(id);
        s.partner_id = partner.map(str::to_owned);
        s.caps = caps;
        s
    }

    fn caps(hour: Option<u32>, day: Option<u32>, partner: Option<u32>) -> SpendCaps {
        SpendCaps {
            replies_per_hour: hour,
            replies_per_day: day,
            partner_replies_per_day: partner,
        }
    }

    fn turn(m: &Meter, s: &Site, visitor: &str, now: u64) -> bool {
        let ok = m.admit(s, visitor, now);
        if ok {
            m.record(s, true, 1, 1);
        }
        ok
    }

    #[test]
    fn counts_are_per_bot_and_hour_and_include_failed_model_calls() {
        let m = Meter::new(true);
        let a = site("a", None, SpendCaps::default());
        assert!(m.admit(&a, "1", T0));
        m.record(&a, true, 10, 20);
        assert!(m.admit(&a, "1", T0));
        m.record(&a, false, 5, 0);
        assert!(
            m.admit(&a, "1", T0),
            "admitted but never finished (visitor left)"
        );
        let batch = m.next_batch(unix_now());
        let ea = batch
            .entries
            .iter()
            .find(|e| e.bot_id == "a" && e.hour == hour_iso(T0 / HOUR * HOUR))
            .unwrap();
        assert_eq!(ea.counts.model_calls, 3);
        let total: u64 = batch.entries.iter().map(|e| e.counts.replies).sum();
        assert_eq!(total, 1);
    }

    #[test]
    fn the_single_site_bot_is_never_metered_or_capped() {
        let m = Meter::new(true);
        let s = site("", None, caps(Some(0), Some(0), None));
        assert!(turn(&m, &s, "1", T0));
        assert!(m.next_batch(T0).entries.is_empty());
    }

    #[test]
    fn without_reporting_nothing_piles_up_but_caps_still_work() {
        let m = Meter::new(false);
        let s = site("a", None, caps(Some(1), None, None));
        assert!(turn(&m, &s, "1", T0));
        assert!(!m.admit(&s, "2", T0));
        assert!(!m.has_unsent());
    }

    #[test]
    fn a_failed_batch_is_resent_with_the_same_id_and_new_counts_wait_for_the_next() {
        let m = Meter::new(true);
        let a = site("a", None, SpendCaps::default());
        turn(&m, &a, "1", T0);
        let first = m.next_batch(T0);
        m.finish_batch(&first, Sent::Failed);
        turn(&m, &a, "1", T0);
        let again = m.next_batch(T0);
        assert_eq!(
            again, first,
            "same batch, same id, not merged with new counts"
        );
        m.finish_batch(&again, Sent::Accepted);
        let next = m.next_batch(T0);
        assert_ne!(next.batch_id, first.batch_id);
        assert_eq!(
            next.entries
                .iter()
                .map(|e| e.counts.model_calls)
                .sum::<u64>(),
            1,
            "the later turn is not lost"
        );
    }

    #[test]
    fn a_rejected_batch_is_dropped_not_retried_forever() {
        let m = Meter::new(true);
        let a = site("a", None, SpendCaps::default());
        turn(&m, &a, "1", T0);
        let b = m.next_batch(T0);
        m.finish_batch(&b, Sent::Rejected);
        assert_ne!(m.next_batch(T0).batch_id, b.batch_id);
    }

    #[test]
    fn batch_ids_differ_between_boots() {
        assert_ne!(
            Meter::new(true).next_batch(T0).batch_id,
            Meter::new(true).next_batch(T0).batch_id
        );
    }

    #[test]
    fn hourly_and_daily_caps_stop_turns_and_mark_the_hour() {
        let m = Meter::new(true);
        let s = site("a", None, caps(Some(2), Some(3), None));
        assert!(turn(&m, &s, "v1", T0) && turn(&m, &s, "v2", T0));
        assert!(!m.admit(&s, "v3", T0), "hourly cap");
        assert!(m.bot_capped(&s, T0));
        assert!(turn(&m, &s, "v3", T0 + HOUR), "new hour");
        assert!(!m.admit(&s, "v4", T0 + HOUR), "daily cap");
        let capped: Vec<bool> = m
            .next_batch(T0 + HOUR)
            .entries
            .iter()
            .map(|e| e.counts.capped)
            .collect();
        assert!(capped.contains(&true));
        assert!(m.admit(&s, "v4", T0 + DAY), "new day");
    }

    #[test]
    fn admitting_reserves_so_unfinished_turns_still_count_towards_caps() {
        let m = Meter::new(true);
        let s = site("a", None, caps(Some(5), None, None));
        let admitted = (0..50)
            .filter(|i| m.admit(&s, &format!("v{i}"), T0))
            .count();
        assert_eq!(
            admitted, 5,
            "never more than the cap, even with no turn finished"
        );
    }

    #[test]
    fn a_late_call_with_an_older_time_never_resets_the_current_hour() {
        let m = Meter::new(true);
        let s = site("a", None, caps(Some(5), None, None));
        for i in 0..5 {
            assert!(m.admit(&s, &format!("v{i}"), T0 + HOUR));
        }
        assert!(
            !m.admit(&s, "late", T0),
            "an older time doesn't roll the window back"
        );
        assert!(
            !m.admit(&s, "next", T0 + HOUR),
            "still capped in the current hour"
        );
    }

    #[test]
    fn one_visitor_cannot_use_up_the_whole_day() {
        let m = Meter::new(true);
        let s = site("a", None, caps(None, Some(1000), None));
        let mut used = 0;
        while turn(&m, &s, "attacker", T0) {
            used += 1;
            assert!(used <= 1000);
        }
        assert_eq!(used, 100, "a tenth of the daily cap");
        assert!(m.admit(&s, "real visitor", T0));
        assert!(!m.bot_capped(&s, T0), "the bot itself is still available");
        let capped = m.next_batch(T0).entries.iter().any(|e| e.counts.capped);
        assert!(
            !capped,
            "one visitor's limit doesn't mark the bot's hour as capped"
        );
    }

    #[test]
    fn small_daily_caps_still_allow_a_real_conversation() {
        let m = Meter::new(true);
        let s = site("a", None, caps(None, Some(20), None));
        assert_eq!((0..20).filter(|_| turn(&m, &s, "v", T0)).count(), 10);
    }

    #[test]
    fn the_partner_ceiling_spans_all_its_bots_and_not_other_partners() {
        let m = Meter::new(true);
        let a = site("ts-a", Some("ts"), caps(None, None, Some(2)));
        let b = site("ts-b", Some("ts"), caps(None, None, Some(2)));
        let other = site("zz-a", Some("zz"), caps(None, None, Some(2)));
        assert!(turn(&m, &a, "1", T0) && turn(&m, &b, "2", T0));
        assert!(!m.admit(&a, "3", T0) && !m.admit(&b, "3", T0));
        assert!(m.bot_capped(&b, T0));
        assert!(m.admit(&other, "3", T0));
        assert!(m.admit(&a, "3", T0 + DAY));
    }

    #[test]
    fn partner_bots_without_caps_get_the_default_caps() {
        let m = Meter::new(true);
        let s = site("ts-a", Some("ts"), SpendCaps::default());
        let admitted = (0..100)
            .filter(|i| m.admit(&s, &format!("v{i}"), T0))
            .count();
        assert_eq!(admitted, 60);
        let own = site("own", None, SpendCaps::default());
        assert_eq!(
            (0..100)
                .filter(|i| m.admit(&own, &format!("v{i}"), T0))
                .count(),
            100
        );
    }

    #[test]
    fn caps_survive_a_restart_through_the_seed() {
        let m = Meter::new(true);
        let s = site("ts-a", Some("ts"), caps(Some(5), Some(10), Some(50)));
        let seed: Seed = serde_json::from_value(json!({
            "bots": {"ts-a": {"hour": 4, "day": 7}}, "partners": {"ts": 3}
        }))
        .unwrap();
        assert!(m.admit(&s, "before seed", T0));
        m.seed(&seed, T0);
        assert!(
            !m.admit(&s, "1", T0),
            "4 before the restart + 1 since = the hourly cap"
        );
        assert!(m.admit(&s, "1", T0 + HOUR));
    }

    #[test]
    fn counters_from_earlier_days_are_forgotten() {
        let m = Meter::new(true);
        let s = site("a", Some("p"), caps(Some(100), Some(100), Some(100)));
        assert!(m.admit(&s, "v", T0));
        m.next_batch(T0 + DAY);
        assert!(m.state.lock().unwrap().bots.is_empty());
        assert!(m.state.lock().unwrap().partners.is_empty());
    }

    #[test]
    fn redteam_28_one_visitor_is_limited_per_hour_and_others_still_get_answers() {
        let m = Meter::new(true).with_visitor_hourly_cap(15);
        // The red-team bot: 60 replies an hour, 300 a day.
        let s = site("ts-a", Some("ts"), SpendCaps::default());
        let used = (0..60).filter(|_| turn(&m, &s, "attacker", T0)).count();
        assert_eq!(used, 15);
        assert_eq!(m.admit_turn(&s, "attacker", T0), Admission::VisitorLimit);
        assert_eq!(m.admit_turn(&s, "real visitor", T0), Admission::Admitted);
        assert!(!m.bot_capped(&s, T0));
        // A new hour gives the visitor a new share (within the daily one).
        assert_eq!(m.admit_turn(&s, "attacker", T0 + HOUR), Admission::Admitted);
    }

    #[test]
    fn redteam_28_a_busy_bot_and_a_limited_visitor_are_told_apart() {
        let m = Meter::new(true).with_visitor_hourly_cap(2);
        let s = site("a", None, caps(Some(3), None, None));
        assert!(turn(&m, &s, "v1", T0) && turn(&m, &s, "v1", T0));
        assert_eq!(m.admit_turn(&s, "v1", T0), Admission::VisitorLimit);
        assert!(turn(&m, &s, "v2", T0));
        assert_eq!(m.admit_turn(&s, "v3", T0), Admission::BotBusy);
        assert_eq!(
            m.admit_turn(&s, "v1", T0),
            Admission::VisitorLimit,
            "a visitor over their own share is told so, even when the bot is busy"
        );
    }

    #[test]
    fn the_visitor_hourly_cap_is_off_at_zero_and_never_applies_to_the_single_site_bot() {
        let m = Meter::new(true).with_visitor_hourly_cap(0);
        let s = site("a", None, caps(None, Some(1000), None));
        assert_eq!((0..50).filter(|_| turn(&m, &s, "v", T0)).count(), 50);
        let m = Meter::new(true).with_visitor_hourly_cap(1);
        let single = site("", None, SpendCaps::default());
        assert!((0..5).all(|_| m.admit(&single, "v", T0)));
    }

    #[test]
    fn visitors_are_logged_by_hash_never_by_address() {
        let h = visitor_hash("2001:db8:1:2::/64");
        assert_eq!(h.len(), 12);
        assert!(!h.contains(':'));
        assert_eq!(h, visitor_hash("2001:db8:1:2::/64"));
    }
}
