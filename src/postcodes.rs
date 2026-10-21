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

/// (outcode padded with zero bytes, lat, lon), sorted by outcode. About 2,900 rows, 35 KB.
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

/// Words right before an outcode that make it a postcode ("near BL6", "postcode BL6").
const BEFORE: [&str; 10] = [
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
];
/// Words right before an outcode that make it something else ("the M1", "cell E5").
const NOT_BEFORE: [&str; 12] = [
    "the", "junction", "jct", "cell", "cells", "column", "row", "range", "vitamin", "a", "an", "j",
];
/// Words right after an outcode that make it something else ("M1 motorway", "E5 cell").
const NOT_AFTER: [&str; 16] = [
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
    "cell",
    "cells",
    "column",
    "row",
    "formula",
    "spreadsheet",
];
/// Words anywhere in the message that make a lone outcode a spreadsheet cell or a road.
const NOT_IN_MESSAGE: [&str; 8] = [
    "spreadsheet",
    "excel",
    "formula",
    "cell",
    "cells",
    "motorway",
    "worksheet",
    "vlookup",
];

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
pub fn centroid(outcode: &str) -> Option<(f64, f64)> {
    let k = key(outcode)?;
    let t = &*TABLE;
    t.binary_search_by_key(&k, |r| r.0)
        .ok()
        .map(|i| (t[i].1 as f64, t[i].2 as f64))
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

/// A postcode recognised in a message: its outcode in upper case, and where it is (None for
/// Northern Ireland, which the table does not cover).
#[derive(Debug, Clone, PartialEq)]
pub struct Postcode {
    pub outcode: Box<str>,
    pub at: Option<(f64, f64)>,
}

fn known(outcode: &str) -> Option<Postcode> {
    let outcode = outcode.to_ascii_uppercase();
    let at = centroid(&outcode);
    let northern_ireland = outcode.len() >= 3
        && outcode.starts_with("BT")
        && outcode[2..].bytes().all(|b| b.is_ascii_digit());
    (at.is_some() || northern_ireland).then(|| Postcode {
        outcode: outcode.into(),
        at,
    })
}

/// The first postcode in `text`: a full one anywhere, else an outward code with postcode
/// context. `service_context` is true when the message names a service or trade.
pub fn find(text: &str, service_context: bool) -> Option<Postcode> {
    // Long messages are cut so a pasted essay stays cheap.
    let text = match text.char_indices().nth(2_000) {
        Some((at, _)) => &text[..at],
        None => text,
    };
    if let Some(p) = FULL.captures_iter(text).find_map(|c| known(&c[1])) {
        return Some(p);
    }
    let lower = text.to_lowercase();
    let words: Vec<&str> = WORD.find_iter(&lower).map(|m| m.as_str()).collect();
    if words.iter().any(|w| NOT_IN_MESSAGE.contains(w)) {
        return None;
    }
    for m in OUTWARD.find_iter(text) {
        let Some(p) = known(m.as_str()) else {
            continue;
        };
        let before: Vec<String> = WORD
            .find_iter(&text[..m.start()])
            .map(|w| w.as_str().to_lowercase())
            .collect();
        let after = WORD
            .find(&text[m.end()..])
            .map(|w| w.as_str().to_lowercase());
        let prev = before.last().map(String::as_str);
        if prev.is_some_and(|w| NOT_BEFORE.contains(&w))
            || after.as_deref().is_some_and(|w| NOT_AFTER.contains(&w))
            || text[m.end()..].starts_with(':')
        {
            continue;
        }
        let whole_message = words.len() == 1
            || (words.len() <= 3
                && before
                    .iter()
                    .all(|w| ["it's", "its", "it", "is", "thats", "that's"].contains(&w.as_str()))
                && after.is_none());
        // "my postcode is BL6", "postcode's BL6", "post code: BL6".
        let postcode_is = before.len() >= 2
            && ["is", "was", "s"].contains(&before[before.len() - 1].as_str())
            && ["postcode", "code", "outcode"].contains(&before[before.len() - 2].as_str())
            || prev.is_some_and(|w| ["postcode's", "postcode\u{2019}s"].contains(&w));
        if whole_message
            || postcode_is
            || prev.is_some_and(|w| BEFORE.contains(&w))
            || service_context
        {
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
