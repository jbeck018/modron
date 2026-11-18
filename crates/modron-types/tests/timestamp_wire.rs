//! DEP-1 characterization: the timestamp shapes that persisted and wire
//! formats carry. The serde shape is RFC 3339 with `Z` for UTC and the
//! fraction at the shortest of 0, 3, 6 or 9 digits that is exact (the shape
//! chrono's `DateTime<Utc>` wrote before the jiff swap). `to_rfc3339()` writes
//! the same instant with `+00:00` instead of `Z`. These strings are the
//! contract: they were recorded from chrono 0.4.45 and must not change.

use modron_types::{ForkOrigin, SessionEvent, SessionMeta, Timestamp};
use serde::{Deserialize, Serialize};
use serde_json::json;

#[derive(Serialize, Deserialize, Debug, PartialEq)]
struct Stamp {
    at: Timestamp,
}

fn make(y: i16, mo: i8, d: i8, h: i8, mi: i8, s: i8, nanos: i32) -> Timestamp {
    jiff::civil::DateTime::new(y, mo, d, h, mi, s, nanos)
        .unwrap()
        .to_zoned(jiff::tz::TimeZone::UTC)
        .unwrap()
        .timestamp()
        .into()
}

/// (instant, serde shape, to_rfc3339 shape)
type Civil = (i16, i8, i8, i8, i8, i8, i32);
type Case = (Civil, &'static str, &'static str);

fn cases() -> Vec<Case> {
    vec![
        (
            (2026, 11, 18, 4, 0, 5, 0),
            "2026-11-18T04:00:05Z",
            "2026-11-18T04:00:05+00:00",
        ),
        (
            (2026, 11, 18, 4, 0, 5, 123_000_000),
            "2026-11-18T04:00:05.123Z",
            "2026-11-18T04:00:05.123+00:00",
        ),
        (
            (2026, 11, 18, 4, 0, 5, 123_456_000),
            "2026-11-18T04:00:05.123456Z",
            "2026-11-18T04:00:05.123456+00:00",
        ),
        (
            (2026, 11, 18, 4, 0, 5, 123_456_789),
            "2026-11-18T04:00:05.123456789Z",
            "2026-11-18T04:00:05.123456789+00:00",
        ),
        (
            (2026, 11, 18, 4, 0, 5, 1_000),
            "2026-11-18T04:00:05.000001Z",
            "2026-11-18T04:00:05.000001+00:00",
        ),
        (
            (1969, 12, 31, 23, 59, 59, 500_000_000),
            "1969-12-31T23:59:59.500Z",
            "1969-12-31T23:59:59.500+00:00",
        ),
        (
            (1970, 1, 1, 0, 0, 0, 0),
            "1970-01-01T00:00:00Z",
            "1970-01-01T00:00:00+00:00",
        ),
    ]
}

#[test]
fn serde_shape_and_rfc3339_shape_are_pinned() {
    for ((y, mo, d, h, mi, s, ns), serde_shape, rfc) in cases() {
        let at = make(y, mo, d, h, mi, s, ns);
        let v = serde_json::to_value(Stamp { at }).unwrap();
        assert_eq!(
            v,
            json!({ "at": serde_shape }),
            "serde shape for {serde_shape}"
        );
        assert_eq!(at.to_rfc3339(), rfc, "to_rfc3339 shape for {rfc}");
        // Round trip through the serde shape is exact.
        let back: Stamp = serde_json::from_value(v).unwrap();
        assert_eq!(back.at, at, "round trip for {serde_shape}");
    }
}

#[test]
fn parse_accepts_every_shape_written_and_legacy_offsets() {
    let base = make(2026, 11, 18, 4, 0, 5, 0);
    let cases: &[(&str, Timestamp)] = &[
        ("2026-11-18T04:00:05Z", base),
        ("2026-11-18T04:00:05+00:00", base),
        ("2026-11-18T06:00:05+02:00", base),
        (
            "2026-11-18T04:00:05.1Z",
            make(2026, 11, 18, 4, 0, 5, 100_000_000),
        ),
        (
            "2026-11-18T04:00:05.123456789Z",
            make(2026, 11, 18, 4, 0, 5, 123_456_789),
        ),
        (
            "2026-11-18T04:00:05.123456789+00:00",
            make(2026, 11, 18, 4, 0, 5, 123_456_789),
        ),
    ];
    for (text, want) in cases {
        let got: Stamp = serde_json::from_value(json!({ "at": text }))
            .unwrap_or_else(|e| panic!("{text} must parse: {e}"));
        assert_eq!(got.at, *want, "instant for {text}");
    }
}

#[test]
fn parse_rfc3339_accepts_to_rfc3339_output() {
    // memory.rs `last_used`, upgrade.rs `checked_at`, auth.rs `saved_at` are
    // written with to_rfc3339() and read back with parse_from_rfc3339.
    let at = make(2026, 11, 18, 4, 0, 5, 123_456_789);
    let text = at.to_rfc3339();
    assert_eq!(Timestamp::parse_rfc3339(&text), Some(at));
    assert_eq!(Timestamp::parse_rfc3339("not a time"), None);
}

#[test]
fn session_meta_and_handed_off_carry_the_serde_shape() {
    let created = make(2026, 11, 18, 4, 0, 5, 0);
    let updated = make(2026, 11, 18, 4, 0, 5, 123_000_000);
    let meta = SessionMeta {
        id: uuid::Uuid::nil(),
        title: "t".into(),
        created_at: created,
        updated_at: updated,
        message_count: 2,
        forked_from: Some(ForkOrigin {
            parent: uuid::Uuid::nil(),
            turn: 1,
        }),
    };
    let v = serde_json::to_value(&meta).unwrap();
    assert_eq!(v["created_at"], json!("2026-11-18T04:00:05Z"));
    assert_eq!(v["updated_at"], json!("2026-11-18T04:00:05.123Z"));
    let back: SessionMeta = serde_json::from_value(v).unwrap();
    assert_eq!(back.created_at, created);
    assert_eq!(back.updated_at, updated);

    let ev = SessionEvent::HandedOff {
        to: Some("laptop".into()),
        at: created,
        carrier_ref: "refs/modron/x".into(),
    };
    let v = serde_json::to_value(&ev).unwrap();
    assert_eq!(v["type"], json!("handed_off"));
    assert_eq!(v["at"], json!("2026-11-18T04:00:05Z"));
    let line = json!({
        "type": "handed_off",
        "at": "2026-11-18T04:00:05.000Z",
        "carrier_ref": "refs/modron/x"
    });
    let old: SessionEvent = serde_json::from_value(line).unwrap();
    match old {
        SessionEvent::HandedOff { to, at, .. } => {
            assert!(to.is_none());
            assert_eq!(at, created);
        }
        other => panic!("expected HandedOff, got {other:?}"),
    }
}

#[test]
fn strftime_patterns_in_use_are_pinned() {
    // memory.rs entry dates and id stamps (chrono `format` on a UTC instant).
    let at = make(2026, 11, 18, 4, 0, 5, 123_456_789);
    assert_eq!(at.format("%Y-%m-%d"), "2026-11-18");
    assert_eq!(at.format("%Y%m%dT%H%M%S%3f"), "20261118T040005123");
    let ms5 = make(2026, 11, 18, 4, 0, 5, 5_000_000);
    assert_eq!(ms5.format("%Y%m%dT%H%M%S%3f"), "20261118T040005005");
    let epoch_pre = make(1969, 12, 31, 23, 59, 59, 500_000_000);
    assert_eq!(epoch_pre.format("%Y%m%dT%H%M%S%3f"), "19691231T235959500");
}

#[test]
fn serde_path_accepts_the_lenient_inputs_chrono_accepted() {
    // chrono's serde Deserialize was lenient: a space or lowercase `t`/`z`,
    // a `+0000` offset, extra fraction digits (truncated to nanoseconds).
    let base = make(2026, 11, 18, 4, 0, 5, 0);
    for text in [
        "2026-11-18 04:00:05Z",
        "2026-11-18t04:00:05z",
        "2026-11-18T04:00:05+0000",
    ] {
        let got: Stamp = serde_json::from_value(json!({ "at": text }))
            .unwrap_or_else(|e| panic!("{text} must parse: {e}"));
        assert_eq!(got.at, base, "instant for {text}");
    }
    let got: Stamp =
        serde_json::from_value(json!({ "at": "2026-11-18T04:00:05.123456789123Z" })).unwrap();
    assert_eq!(got.at, make(2026, 11, 18, 4, 0, 5, 123_456_789));
}

#[test]
fn leap_second_reads_as_the_same_unix_instant() {
    // Known difference (DEP-1): chrono kept the leap-second marker and wrote
    // `23:59:60+00:00` back; jiff reads the same Unix instant and writes
    // `23:59:59`. No code path writes a leap second, so no file carries one.
    let got: Stamp = serde_json::from_value(json!({ "at": "2016-12-31T23:59:60Z" })).unwrap();
    assert_eq!(got.at.to_rfc3339(), "2016-12-31T23:59:59+00:00");
}
