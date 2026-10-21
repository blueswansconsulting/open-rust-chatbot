//! UK postcodes in a visitor's message, and where each GB outcode (postcode district) is.
//!
//! The centroid table (src/data/outcode-centroids.csv, built from OS Code-Point Open by
//! src/data/build-outcode-centroids.py; see src/data/NOTICE) has one row per geographic GB
//! outcode: `OUTCODE,lat,lon`. Northern Ireland (BT) is not in Code-Point Open, so a BT postcode
//! is recognised but has no position.
//!
//! An outcode on its own ("BL6", "E5") is everywhere in ordinary text ("the M1", "cell E5",
//! "vitamin B12"), so it only counts with postcode context: "in/near/around BL6", "my postcode
//! is BL6", a service or trade word in the message, or the outcode as the whole message. A full
//! postcode ("BL6 4AB", "bl64ab") needs no context. Only the outcode is ever kept or shown.

use regex::Regex;
use std::sync::LazyLock;

static CENTROIDS: &str = include_str!("data/outcode-centroids.csv");

/// (outcode padded with zero bytes, lat, lon), sorted by outcode. About 2,750 rows, 33 KB.
static TABLE: LazyLock<Vec<([u8; 4], f32, f32)>> = LazyLock::new(|| {
    let mut rows: Vec<([u8; 4], f32, f32)> = CENTROIDS
        .lines()
        .filter_map(|line| {
            let mut parts = line.split(',');
            let key = key(parts.next()?)?;
            let lat = parts.next()?.parse().ok()?;
            let lon = parts.next()?.parse().ok()?;
            Some((key, lat, lon))
        })
        .collect();
    rows.sort_unstable_by_key(|r| r.0);
    rows.dedup_by_key(|r| r.0);
    rows.shrink_to_fit();
    rows
});

/// A full postcode: outward code, optional space, inward code (digit and two letters, never
/// C, I, K, M, O or V).
static FULL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b([a-z]{1,2}[0-9][a-z0-9]?)[ \t]*([0-9][abd-hjlnp-uw-z]{2})\b").unwrap()
});
/// Something shaped like an outward code on its own.
static OUTWARD: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\b([a-z]{1,2}[0-9][a-z0-9]?)\b").unwrap());
static WORD: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[\p{L}\p{N}']+").unwrap());
/// A motorway junction ("J4") after an outcode-shaped road name.
static JUNCTION: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^j[0-9]{1,2}[a-z]?$").unwrap());
/// An ordinal after an outcode-shaped flat or floor ("B1 2nd floor").
static ORDINAL: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[0-9]+(?:st|nd|rd|th)$").unwrap());

/// Words right before an outcode that make it a postcode ("near BL6", "postcode BL6").
const BEFORE: [&str; 15] = [
    "in",
    "near",
    "nr",
    "around",
    "postcode",
    "postcodes",
    "outcode",
    "district",
    "area",
    "from",
    "at",
    "cover",
    "covering",
    "serving",
    "serve",
];
/// Words right before an outcode that make it something else ("the M1", "cell E5", "fit GU10").
const NOT_BEFORE: [&str; 22] = [
    "the", "junction", "jct", "cell", "cells", "column", "row", "range", "vitamin", "a", "an", "j",
    "fit", "fitted", "fitting", "change", "replace", "replaced", "type", "size", "model", "rated",
];
/// Words right after an outcode that make it something else ("M1 motorway", "E5 cell", "GU10
/// downlights", "B32 breakers").
const NOT_AFTER: [&str; 44] = [
    "motorway",
    "mway",
    "junction",
    "jct",
    "toll",
    "northbound",
    "southbound",
    "eastbound",
    "westbound",
    "services",
    "ring",
    "road",
    "exit",
    "slip",
    "cell",
    "cells",
    "column",
    "row",
    "formula",
    "spreadsheet",
    "breaker",
    "breakers",
    "mcb",
    "mcbs",
    "rcbo",
    "rcbos",
    "bulb",
    "bulbs",
    "lamp",
    "lamps",
    "downlight",
    "downlights",
    "spotlight",
    "spotlights",
    "fitting",
    "fittings",
    "holder",
    "petrol",
    "fuel",
    "cable",
    "deficiency",
    "levels",
    "supplement",
    "supplements",
];
/// Words anywhere in the message that make a lone outcode a spreadsheet cell, a road or a
/// vitamin.
/// Only without strong context ("near BL6", "my postcode is BL6"); "junction box" and "solar
/// cells" are everyday electrician words, so they are not here.
const NOT_IN_MESSAGE: [&str; 8] = [
    "spreadsheet",
    "excel",
    "formula",
    "motorway",
    "worksheet",
    "vlookup",
    "vitamin",
    "vitamins",
];
/// Words after an ordinal that make "B1 2nd" a floor or exit, not a postcode.
const ORDINAL_NOUNS: [&str; 12] = [
    "floor", "floors", "flat", "exit", "junction", "time", "times", "year", "years", "storey",
    "door", "turning",
];
/// Words two back that make "in B12" a quantity, not a place ("rich in B12").
const NOT_TWO_BEFORE: [&str; 5] = ["rich", "high", "low", "deficient", "lacking"];

/// The outcode as a lookup key: upper case, 2 to 4 letters and digits, zero padded.
fn key(outcode: &str) -> Option<[u8; 4]> {
    let outcode = outcode.trim();
    if !(2..=4).contains(&outcode.len()) || !outcode.bytes().all(|b| b.is_ascii_alphanumeric()) {
        return None;
    }
    let mut k = [0u8; 4];
    for (slot, b) in k.iter_mut().zip(outcode.bytes()) {
        *slot = b.to_ascii_uppercase();
    }
    Some(k)
}

/// Where a GB outcode's postcodes are on average (WGS84 degrees), if it is a geographic one.
/// London districts split into sub-districts ("W1" is W1A…W1W, "SW1" is SW1A…SW1Y) get the
/// mean of their sub-districts.
pub fn centroid(outcode: &str) -> Option<(f64, f64)> {
    let k = key(outcode)?;
    let t = &*TABLE;
    if let Ok(i) = t.binary_search_by_key(&k, |r| r.0) {
        return Some((t[i].1 as f64, t[i].2 as f64));
    }
    let len = k.iter().position(|&b| b == 0).unwrap_or(4);
    if len >= 4 || !k[len - 1].is_ascii_digit() {
        return None;
    }
    // Sub-districts: the same characters then one letter ("W1" -> "W1A").
    let from = t.partition_point(|r| r.0 < k);
    let subs: Vec<&([u8; 4], f32, f32)> = t[from..]
        .iter()
        .take_while(|r| r.0[..len] == k[..len])
        .filter(|r| r.0[len].is_ascii_alphabetic() && (len + 1 == 4 || r.0[len + 1] == 0))
        .collect();
    if subs.is_empty() {
        return None;
    }
    let n = subs.len() as f64;
    let lat = subs.iter().map(|r| r.1 as f64).sum::<f64>() / n;
    let lon = subs.iter().map(|r| r.2 as f64).sum::<f64>() / n;
    Some((lat, lon))
}

/// How many outcodes the table has.
pub fn len() -> usize {
    TABLE.len()
}

/// Heap bytes the parsed table takes.
pub fn approx_bytes() -> usize {
    TABLE.capacity() * std::mem::size_of::<([u8; 4], f32, f32)>()
}

/// Great-circle distance in km.
pub fn distance_km(a: (f64, f64), b: (f64, f64)) -> f64 {
    let (lat1, lat2) = (a.0.to_radians(), b.0.to_radians());
    let dlat = lat2 - lat1;
    let dlon = (b.1 - a.1).to_radians();
    let h = (dlat / 2.0).sin().powi(2) + lat1.cos() * lat2.cos() * (dlon / 2.0).sin().powi(2);
    2.0 * 6371.0088 * h.sqrt().min(1.0).asin()
}

/// A postcode recognised in a message: its outcode in upper case, where it is (None for
/// Northern Ireland, which the table does not cover) and where it was in the message.
#[derive(Debug, Clone, PartialEq)]
pub struct Postcode {
    pub outcode: Box<str>,
    pub at: Option<(f64, f64)>,
    pub span: std::ops::Range<usize>,
}

fn known(outcode: &str, span: std::ops::Range<usize>) -> Option<Postcode> {
    let outcode = outcode.to_ascii_uppercase();
    let at = centroid(&outcode);
    let northern_ireland = outcode.len() >= 3
        && outcode.starts_with("BT")
        && outcode[2..].bytes().all(|b| b.is_ascii_digit());
    (at.is_some() || northern_ireland).then(|| Postcode {
        outcode: outcode.into(),
        at,
        span,
    })
}

/// A known outcode as a [`Postcode`] (for one carried over from an earlier turn).
pub fn outcode(outcode: &str) -> Option<Postcode> {
    key(outcode)?;
    known(outcode, 0..0)
}

/// True if `inward` ("2nd") is an ordinal: written in lower case, or followed by "floor",
/// "exit"…. "BL1 2ND" on its own is a postcode.
fn ordinal_at(inward: &str, rest: &str) -> bool {
    let suffix = inward.get(1..).unwrap_or_default();
    if !["st", "nd", "rd", "th"].contains(&suffix.to_ascii_lowercase().as_str()) {
        return false;
    }
    let next = WORD
        .find(rest)
        .map(|w| w.as_str().to_lowercase())
        .unwrap_or_default();
    ORDINAL_NOUNS.contains(&next.as_str()) || suffix.bytes().all(|b| b.is_ascii_lowercase())
}

/// The first postcode in `text`: a full one anywhere, else an outward code with postcode
/// context. `service_context` is true when the message names a service or trade.
pub fn find(text: &str, service_context: bool) -> Option<Postcode> {
    // Long messages are cut so a pasted essay stays cheap.
    let text = match text.char_indices().nth(2_000) {
        Some((at, _)) => &text[..at],
        None => text,
    };
    let full = FULL.captures_iter(text).find_map(|c| {
        let whole = c.get(0)?;
        // "B1 2nd floor", "M1 1st exit" are ordinals; "BL1 2ND" is a postcode.
        if ordinal_at(&c[2], &text[whole.end()..]) {
            return None;
        }
        known(&c[1], whole.range())
    });
    if full.is_some() {
        return full;
    }
    // The message's words, once, with where each is.
    let words: Vec<(std::ops::Range<usize>, String)> = WORD
        .find_iter(text)
        .map(|m| (m.range(), m.as_str().to_lowercase()))
        .collect();
    let vetoed = words
        .iter()
        .any(|(_, w)| NOT_IN_MESSAGE.contains(&w.as_str()));
    for m in OUTWARD.find_iter(text) {
        let Some(p) = known(m.as_str(), m.range()) else {
            continue;
        };
        let at = words.partition_point(|(r, _)| r.start < m.start());
        let before: Vec<&str> = words[..at].iter().map(|(_, w)| w.as_str()).collect();
        let after = words.get(at + 1).map(|(_, w)| w.as_str());
        let prev = before.last().copied();
        let prev2 = before.len().checked_sub(2).map(|i| before[i]);
        // "the E5 area", "a BL6 based electrician" are postcodes after all.
        let area_after =
            after.is_some_and(|w| ["area", "based", "postcode", "district"].contains(&w));
        if prev.is_some_and(|w| NOT_BEFORE.contains(&w)) && !area_after
            || prev2.is_some_and(|w| NOT_TWO_BEFORE.contains(&w))
            || after.is_some_and(|w| NOT_AFTER.contains(&w) || JUNCTION.is_match(w))
            || words.get(at + 1).is_some_and(|(r, w)| {
                ORDINAL.is_match(w) && ordinal_at(&text[r.clone()], &text[r.end..])
            })
            || text[m.end()..].starts_with(':')
        {
            continue;
        }
        let whole_message = words.len() == 1
            || (words.len() <= 3
                && before
                    .iter()
                    .all(|w| ["it's", "its", "it", "is", "thats", "that's"].contains(w))
                && after.is_none());
        // "my postcode is BL6", "postcode's BL6", "post code: BL6".
        let postcode_is = prev2.is_some_and(|w| ["postcode", "code", "outcode"].contains(&w))
            && prev.is_some_and(|w| ["is", "was", "s"].contains(&w))
            || prev.is_some_and(|w| ["postcode's", "postcode\u{2019}s"].contains(&w));
        let strong =
            whole_message || postcode_is || area_after || prev.is_some_and(|w| BEFORE.contains(&w));
        if strong || service_context && !vetoed {
            return Some(p);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_table_covers_gb_outcodes_only() {
        assert!((2_700..3_100).contains(&len()), "{}", len());
        assert!(approx_bytes() < 64 * 1024, "{}", approx_bytes());
        for line in CENTROIDS.lines() {
            let mut parts = line.split(',');
            let outcode = parts.next().unwrap();
            let lat: f64 = parts.next().unwrap().parse().unwrap();
            let lon: f64 = parts.next().unwrap().parse().unwrap();
            assert!(key(outcode).is_some(), "{line}");
            assert!(!outcode.starts_with("BT"), "no Northern Ireland: {line}");
            // Great Britain, Scilly to Shetland.
            assert!(
                (49.8..61.0).contains(&lat) && (-8.7..2.0).contains(&lon),
                "{line}"
            );
        }
        // Non-geographic districts (PO boxes, large users) are left out.
        for gone in ["BS98", "E98", "NE98", "SW95", "N81", "BX1", "GIR"] {
            assert_eq!(centroid(gone), None, "{gone}");
        }
        for (outcode, lat, lon) in [
            ("BL6", 53.59, -2.54),
            ("E5", 51.56, -0.05),
            ("SW1A", 51.50, -0.13),
            ("WF1", 53.69, -1.49),
            ("IV1", 57.48, -4.24),
        ] {
            let (a, b) = centroid(outcode).expect(outcode);
            assert!(
                (a - lat).abs() < 0.02 && (b - lon).abs() < 0.02,
                "{outcode}"
            );
        }
        assert_eq!(centroid("bl6"), centroid("BL6"));
    }

    #[test]
    fn distances_are_great_circle_km() {
        let bolton = centroid("BL1").unwrap();
        let horwich = centroid("BL6").unwrap();
        let d = distance_km(bolton, horwich);
        assert!((5.0..8.0).contains(&d), "{d}");
        let london = centroid("E5").unwrap();
        let d = distance_km(bolton, london);
        assert!((260.0..300.0).contains(&d), "{d}");
        assert_eq!(distance_km(london, london), 0.0);
    }

    fn outcode(text: &str, service: bool) -> Option<String> {
        find(text, service).map(|p| p.outcode.to_string())
    }

    #[test]
    fn full_postcodes_are_found_anywhere_and_keep_only_the_outcode() {
        for (text, want) in [
            ("BL6 4AB", "BL6"),
            ("my postcode is bl64ab", "BL6"),
            ("I'm at WF1 2AB, any good sparks?", "WF1"),
            ("SW1A 1AA", "SW1A"),
            ("ec1v 9ab please", "EC1V"),
        ] {
            assert_eq!(outcode(text, false).as_deref(), Some(want), "{text}");
        }
        // Shaped like a postcode but no such district.
        assert_eq!(outcode("ZZ9 9ZZ", false), None);
        assert_eq!(outcode("QQ1 2AB", false), None);
    }

    #[test]
    fn a_lone_outcode_needs_postcode_context() {
        for (text, service, want) in [
            ("EICR near BL6", true, Some("BL6")),
            ("electrician in E5", true, Some("E5")),
            ("anyone around wf1?", false, Some("WF1")),
            ("my postcode is BL6", false, Some("BL6")),
            ("BL6", false, Some("BL6")),
            ("it's BL6", false, Some("BL6")),
            ("rewire, BL6", true, Some("BL6")),
            ("M1 motorway", false, None),
            ("electrician near the M1", true, None),
            ("traffic on the M1 northbound", false, None),
            ("the total is in cell E5 of my spreadsheet", false, None),
            ("E5:F9", false, None),
            ("put it in E5", false, Some("E5")),
            ("vitamin B12", false, None),
            ("I need an EICR, B12 on the form", false, None),
            ("BL6 4", false, None),
        ] {
            assert_eq!(outcode(text, service).as_deref(), want, "{text}");
        }
    }

    #[test]
    fn northern_ireland_is_recognised_without_a_position() {
        let p = find("electrician in BT1 1AA", true).unwrap();
        assert_eq!(&*p.outcode, "BT1");
        assert_eq!(p.at, None);
    }

    #[test]
    fn a_long_message_stays_cheap() {
        let text = "near BL6 ".repeat(5_000);
        let started = std::time::Instant::now();
        assert!(find(&text, true).is_some());
        let text = "lorem ipsum dolor ".repeat(5_000);
        assert!(find(&text, true).is_none());
        assert!(started.elapsed() < std::time::Duration::from_millis(200));
    }
}
