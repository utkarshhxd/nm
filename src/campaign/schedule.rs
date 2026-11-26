// SPDX-License-Identifier: AGPL-3.0-only

//! Wall-clock time in a named zone, turned into an instant (spec M2c §4.3).
//!
//! RFC 5545 §3.3.5 decides the two awkward cases, and decides them the same way in both schedule
//! modes: a local time the clocks skip is read with the offset in force BEFORE the gap, so it lands
//! one gap later (02:30 on a spring-forward day is 03:30); a local time that happens twice is its
//! FIRST occurrence. The API echoes the result, so the operator sees the instant before sending.

use chrono::{DateTime, Duration, LocalResult, NaiveDateTime, Offset, TimeZone, Utc};
use chrono_tz::{TZ_VARIANTS, Tz};

/// `local` in `zone`, as an instant.
pub fn resolve(local: NaiveDateTime, zone: Tz) -> DateTime<Utc> {
    match zone.from_local_datetime(&local) {
        LocalResult::Single(t) => t.with_timezone(&Utc),
        // `Ambiguous(first, second)`: the earlier instant is the first occurrence.
        LocalResult::Ambiguous(first, _) => first.with_timezone(&Utc),
        LocalResult::None => {
            // In a gap. The offset before it is the one in force a day or two earlier: a gap is at
            // most a day long, and a zone does not change its rules twice inside two days.
            let before = zone
                .offset_from_utc_datetime(&(local - Duration::hours(48)))
                .fix()
                .local_minus_utc();
            Utc.from_utc_datetime(&(local - Duration::seconds(i64::from(before))))
        }
    }
}

/// The earliest instant `local` is reached anywhere: the easternmost zone's. A recipient-local send
/// starts here, and every other zone's turn comes later.
pub fn earliest(local: NaiveDateTime) -> DateTime<Utc> {
    zone_instants(local)
        .1
        .into_iter()
        .min()
        .expect("chrono-tz has zones")
}

/// `local` in every zone a contact may hold, as parallel arrays: the names and the instants.
/// Computed here and passed to the resolution statement because contact zones are validated by
/// `chrono-tz`, so Postgres's own tz database must not decide them (spec §4.3 ruling 3).
pub fn zone_instants(local: NaiveDateTime) -> (Vec<String>, Vec<DateTime<Utc>>) {
    TZ_VARIANTS
        .iter()
        .map(|z| (z.name().to_owned(), resolve(local, *z)))
        .unzip()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(s: &str) -> NaiveDateTime {
        NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M").expect("a local time")
    }

    fn utc(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s)
            .expect("an instant")
            .to_utc()
    }

    #[test]
    fn a_skipped_local_time_moves_forward_by_the_gap() {
        // 02:30 does not exist on 2027-03-14 in New York: 02:00 EST jumps to 03:00 EDT.
        assert_eq!(
            resolve(at("2027-03-14T02:30"), Tz::America__New_York),
            utc("2027-03-14T07:30:00Z")
        );
        // The same wall-clock minute, so the result reads 03:30 EDT.
        let local = resolve(at("2027-03-14T02:30"), Tz::America__New_York)
            .with_timezone(&Tz::America__New_York);
        assert_eq!(local.format("%H:%M").to_string(), "03:30");
        // A time either side of the gap is untouched.
        assert_eq!(
            resolve(at("2027-03-14T01:59"), Tz::America__New_York),
            utc("2027-03-14T06:59:00Z")
        );
        assert_eq!(
            resolve(at("2027-03-14T03:00"), Tz::America__New_York),
            utc("2027-03-14T07:00:00Z")
        );
    }

    #[test]
    fn an_ambiguous_local_time_takes_the_first_occurrence() {
        // 01:30 happens at 05:30Z (EDT) and again at 06:30Z (EST); the first is the answer.
        assert_eq!(
            resolve(at("2027-11-07T01:30"), Tz::America__New_York),
            utc("2027-11-07T05:30:00Z")
        );
    }

    #[test]
    fn a_gap_that_is_not_an_hour_still_moves_forward_by_its_own_length() {
        // Lord Howe Island springs forward by 30 minutes (02:00 -> 02:30) on 2027-10-03.
        let gap = resolve(at("2027-10-03T02:15"), Tz::Australia__Lord_Howe);
        let local = gap.with_timezone(&Tz::Australia__Lord_Howe);
        assert_eq!(local.format("%H:%M").to_string(), "02:45");
    }

    #[test]
    fn the_earliest_instant_is_the_easternmost_zone() {
        // UTC+14 (Pacific/Kiritimati) reaches 09:00 on the 10th at 19:00Z on the 9th.
        assert_eq!(
            earliest(at("2027-01-10T09:00")),
            utc("2027-01-09T19:00:00Z")
        );
    }

    #[test]
    fn every_zone_a_contact_may_hold_has_an_instant() {
        let (names, instants) = zone_instants(at("2027-03-14T02:30"));
        assert_eq!(names.len(), TZ_VARIANTS.len());
        assert_eq!(instants.len(), TZ_VARIANTS.len());
        // Every name round-trips, so a contact's stored zone finds its own entry.
        assert!(names.iter().all(|n| n.parse::<Tz>().is_ok()));
    }
}
