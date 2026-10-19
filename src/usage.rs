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
use crate::sites::Site;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::HashMap;
use std::sync::Mutex;

const HOUR: u64 = 3600;
const DAY: u64 = 86_400;
/// One visitor may use at most this share (1/n) of a bot's daily cap.
const PER_VISITOR_SHARE: u32 = 10;
/// Visitors tracked per bot per day; beyond this new visitors share one bucket.
const MAX_VISITORS_TRACKED: usize = 4096;

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
    state: Mutex<State>,
}

pub fn hour_iso(hour_start: u64) -> String {
    chrono::DateTime::from_timestamp(hour_start as i64, 0)
        .unwrap_or_default()
        .format("%Y-%m-%dT%H:00:00Z")
        .to_string()
}

pub fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

impl BotWindow {
    fn roll(&mut self, now: u64) {
        if self.hour != now / HOUR {
            self.hour = now / HOUR;
            self.hour_count = 0;
        }
        if self.day != now / DAY {
            self.day = now / DAY;
            self.day_count = 0;
            self.visitors.clear();
        }
    }
}

impl Default for Meter {
    fn default() -> Self {
        Self::new()
    }
}

impl Meter {
    pub fn new() -> Self {
        Self {
            // Fresh on every boot, so batch ids from before a restart are never reused.
            instance_id: crate::random_token()[..16].to_owned(),
            state: Mutex::default(),
        }
    }

    /// True if this visitor may start another turn with this bot. A refusal marks the hour
    /// as capped in the usage report.
    pub fn admit(&self, site: &Site, visitor: &str, now: u64) -> bool {
        if site.id.is_empty() {
            return true;
        }
        let mut st = self.state.lock().unwrap();
        let caps = site.caps;
        let window = st.bots.entry(site.id.clone()).or_default();
        window.roll(now);
        let visitor_cap = caps.replies_per_day.map(|d| (d / PER_VISITOR_SHARE).max(1));
        let visitor_used = window.visitors.get(visitor).copied().unwrap_or(0);
        let mut ok = caps.replies_per_hour.is_none_or(|c| window.hour_count < c)
            && caps.replies_per_day.is_none_or(|c| window.day_count < c)
            && visitor_cap.is_none_or(|c| visitor_used < c);
        if let (Some(partner), Some(cap)) = (&site.partner_id, caps.partner_replies_per_day) {
            let (day, used) = st.partners.get(partner).copied().unwrap_or((now / DAY, 0));
            ok &= day != now / DAY || used < cap;
        }
        if !ok {
            st.pending
                .entry((site.id.clone(), now / HOUR * HOUR))
                .or_default()
                .capped = true;
        }
        ok
    }

    /// True if the bot (or its partner) is out of replies for now; per-visitor limits don't count.
    pub fn bot_capped(&self, site: &Site, now: u64) -> bool {
        if site.id.is_empty() {
            return false;
        }
        let mut st = self.state.lock().unwrap();
        let caps = site.caps;
        let capped = {
            let window = st.bots.entry(site.id.clone()).or_default();
            window.roll(now);
            caps.replies_per_hour
                .is_some_and(|c| window.hour_count >= c)
                || caps.replies_per_day.is_some_and(|c| window.day_count >= c)
        };
        let partner_capped = match (&site.partner_id, caps.partner_replies_per_day) {
            (Some(p), Some(cap)) => st
                .partners
                .get(p)
                .is_some_and(|&(day, used)| day == now / DAY && used >= cap),
            _ => false,
        };
        capped || partner_capped
    }

    /// Counts one turn that reached the model. `completed` turns are billable replies; failed
    /// ones still cost a model call, so they count towards caps and are reported.
    pub fn record(
        &self,
        site: &Site,
        visitor: &str,
        completed: bool,
        chars_in: usize,
        chars_out: usize,
        now: u64,
    ) {
        if site.id.is_empty() {
            return;
        }
        let mut st = self.state.lock().unwrap();
        let window = st.bots.entry(site.id.clone()).or_default();
        window.roll(now);
        window.hour_count = window.hour_count.saturating_add(1);
        window.day_count = window.day_count.saturating_add(1);
        let key = if window.visitors.len() < MAX_VISITORS_TRACKED
            || window.visitors.contains_key(visitor)
        {
            visitor.to_owned()
        } else {
            String::from("~overflow")
        };
        *window.visitors.entry(key).or_default() += 1;
        if let Some(partner) = &site.partner_id {
            let entry = st.partners.entry(partner.clone()).or_insert((now / DAY, 0));
            if entry.0 != now / DAY {
                *entry = (now / DAY, 0);
            }
            entry.1 = entry.1.saturating_add(1);
        }
        let counts = st
            .pending
            .entry((site.id.clone(), now / HOUR * HOUR))
            .or_default();
        counts.model_calls += 1;
        counts.replies += u64::from(completed);
        counts.chars_in += chars_in as u64;
        counts.chars_out += chars_out as u64;
    }

    /// Restores today's totals after a restart. Only raises counters, never lowers them.
    pub fn seed(&self, seed: &Seed, now: u64) {
        let mut st = self.state.lock().unwrap();
        for (id, counts) in &seed.bots {
            let window = st.bots.entry(id.clone()).or_default();
            window.roll(now);
            window.hour_count = window.hour_count.max(counts.hour);
            window.day_count = window.day_count.max(counts.day);
        }
        for (id, &used) in &seed.partners {
            let entry = st.partners.entry(id.clone()).or_insert((now / DAY, 0));
            if entry.0 != now / DAY {
                *entry = (now / DAY, 0);
            }
            entry.1 = entry.1.max(used);
        }
    }

    /// The batch to send now: the one still in flight if its last send failed, otherwise a new
    /// one holding everything counted so far.
    pub fn next_batch(&self, now: u64) -> Batch {
        let mut st = self.state.lock().unwrap();
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
        Ok(r) if r.status().is_client_error() && r.status().as_u16() != 429 => Sent::Rejected,
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
    use crate::sites::SpendCaps;

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

    #[test]
    fn counts_are_per_bot_and_hour_and_include_failed_model_calls() {
        let m = Meter::new();
        let a = site("a", None, SpendCaps::default());
        let b = site("b", None, SpendCaps::default());
        m.record(&a, "1", true, 10, 20, T0);
        m.record(&a, "1", false, 5, 0, T0 + 1);
        m.record(&b, "2", true, 1, 2, T0 + HOUR);
        let batch = m.next_batch(T0 + HOUR);
        assert_eq!(batch.entries.len(), 2);
        let ea = batch.entries.iter().find(|e| e.bot_id == "a").unwrap();
        assert_eq!(
            ea.counts,
            Counts {
                replies: 1,
                model_calls: 2,
                chars_in: 15,
                chars_out: 20,
                capped: false
            }
        );
        assert_eq!(ea.hour, hour_iso(T0 / HOUR * HOUR));
        assert_eq!(batch.watermark_hour, hour_iso((T0 + HOUR) / HOUR * HOUR));
    }

    #[test]
    fn the_single_site_bot_is_never_metered_or_capped() {
        let m = Meter::new();
        let s = site("", None, caps(Some(0), Some(0), None));
        assert!(m.admit(&s, "1", T0));
        m.record(&s, "1", true, 1, 1, T0);
        assert!(m.next_batch(T0).entries.is_empty());
    }

    #[test]
    fn a_failed_batch_is_resent_with_the_same_id_and_new_counts_wait_for_the_next() {
        let m = Meter::new();
        let a = site("a", None, SpendCaps::default());
        m.record(&a, "1", true, 1, 1, T0);
        let first = m.next_batch(T0);
        m.finish_batch(&first, Sent::Failed);
        m.record(&a, "1", true, 1, 1, T0);
        let again = m.next_batch(T0);
        assert_eq!(
            again, first,
            "same batch, same id, not merged with new counts"
        );
        m.finish_batch(&again, Sent::Accepted);
        let next = m.next_batch(T0);
        assert_ne!(next.batch_id, first.batch_id);
        assert_eq!(
            next.entries[0].counts.replies, 1,
            "the later reply is not lost"
        );
    }

    #[test]
    fn a_rejected_batch_is_dropped_not_retried_forever() {
        let m = Meter::new();
        let a = site("a", None, SpendCaps::default());
        m.record(&a, "1", true, 1, 1, T0);
        let b = m.next_batch(T0);
        m.finish_batch(&b, Sent::Rejected);
        assert_ne!(m.next_batch(T0).batch_id, b.batch_id);
    }

    #[test]
    fn batch_ids_differ_between_boots() {
        assert_ne!(
            Meter::new().next_batch(T0).batch_id,
            Meter::new().next_batch(T0).batch_id
        );
    }

    #[test]
    fn hourly_and_daily_caps_stop_turns_and_mark_the_hour() {
        let m = Meter::new();
        let s = site("a", None, caps(Some(2), Some(3), None));
        for v in ["v1", "v2"] {
            assert!(m.admit(&s, v, T0));
            m.record(&s, v, true, 1, 1, T0);
        }
        assert!(!m.admit(&s, "v3", T0), "hourly cap");
        assert!(m.bot_capped(&s, T0));
        assert!(m.admit(&s, "v3", T0 + HOUR), "new hour");
        m.record(&s, "v3", true, 1, 1, T0 + HOUR);
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
    fn one_visitor_cannot_use_up_the_whole_day() {
        let m = Meter::new();
        let s = site("a", None, caps(None, Some(100), None));
        let mut used = 0;
        while m.admit(&s, "attacker", T0) {
            m.record(&s, "attacker", true, 1, 1, T0);
            used += 1;
            assert!(used <= 100);
        }
        assert_eq!(used, 10, "a tenth of the daily cap");
        assert!(m.admit(&s, "real visitor", T0));
        assert!(!m.bot_capped(&s, T0), "the bot itself is still available");
    }

    #[test]
    fn the_partner_ceiling_spans_all_its_bots_and_not_other_partners() {
        let m = Meter::new();
        let a = site("ts-a", Some("ts"), caps(None, None, Some(2)));
        let b = site("ts-b", Some("ts"), caps(None, None, Some(2)));
        let other = site("zz-a", Some("zz"), caps(None, None, Some(2)));
        m.record(&a, "1", true, 1, 1, T0);
        m.record(&b, "2", true, 1, 1, T0);
        assert!(!m.admit(&a, "3", T0) && !m.admit(&b, "3", T0));
        assert!(m.bot_capped(&b, T0));
        assert!(m.admit(&other, "3", T0));
        assert!(m.admit(&a, "3", T0 + DAY));
    }

    #[test]
    fn caps_survive_a_restart_through_the_seed() {
        let m = Meter::new();
        let s = site("ts-a", Some("ts"), caps(Some(5), Some(10), Some(50)));
        let seed: Seed = serde_json::from_value(json!({
            "bots": {"ts-a": {"hour": 5, "day": 7}}, "partners": {"ts": 3}
        }))
        .unwrap();
        m.seed(&seed, T0);
        assert!(
            !m.admit(&s, "1", T0),
            "hour already used up before the restart"
        );
        assert!(m.admit(&s, "1", T0 + HOUR));
    }
}
