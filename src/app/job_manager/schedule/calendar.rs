use std::collections::BTreeSet;
use std::fmt::Write;

use chrono::Datelike;
use serde_json::Value;

use super::{LocalTime, MONTH_MAX_DAYS, MonthDays, Months, Trigger, Weekdays, schedule_date};

pub(super) fn parse(value: &Value) -> Option<Trigger> {
    parse_with_monthly_selection(value, None)
}

pub(super) fn parse_once(value: &Value, timestamp: &str) -> Option<Trigger> {
    let (date, time) = timestamp.split_once('T')?;
    let date = schedule_date(date)?;
    let time = LocalTime::parse(time)?;
    let trigger = Trigger::once(date, time)?;
    if trigger.once_timestamp()?.as_str() != timestamp {
        return None;
    }
    let (
        calendar_time,
        Date::Monthly {
            day,
            month: Some(month),
        },
    ) = interval(value)?
    else {
        return None;
    };
    (calendar_time == time && date.day() == u32::from(day) && date.month() == u32::from(month))
        .then_some(trigger)
}

pub(super) fn parse_with_monthly_selection(
    value: &Value,
    selection: Option<(MonthDays, Months)>,
) -> Option<Trigger> {
    let values = value
        .as_array()
        .map_or_else(|| std::slice::from_ref(value), Vec::as_slice);
    let (time, date) = interval(values.first()?)?;
    match date {
        Date::Daily if values.len() == 1 && selection.is_none() => Some(Trigger::Daily(time)),
        Date::Weekly(_) if selection.is_none() => {
            let mut mask = 0;
            for value in values {
                let (other, Date::Weekly(day)) = interval(value)? else {
                    return None;
                };
                let bit = weekday_bit(day)?;
                if other != time || mask & bit != 0 {
                    return None;
                }
                mask |= bit;
            }
            Some(Trigger::Weekly {
                time,
                days: Weekdays::from_mask(mask)?,
            })
        }
        Date::Monthly { .. } => {
            let mut dates = BTreeSet::new();
            let mut all_months = false;
            for value in values {
                let (other, Date::Monthly { day, month }) = interval(value)? else {
                    return None;
                };
                if other != time {
                    return None;
                }
                if let Some(month) = month {
                    if day > max_day(month) || !dates.insert((month, day)) {
                        return None;
                    }
                } else {
                    all_months = true;
                    for month in (1..=12).filter(|&month| day <= max_day(month)) {
                        if !dates.insert((month, day)) {
                            return None;
                        }
                    }
                }
            }
            let (day_mask, mut month_mask) = monthly_masks(dates.iter().copied());
            if all_months {
                month_mask = Months::ALL;
            }
            let (days, months) = match selection {
                Some(selection) => selection,
                None => (
                    MonthDays::from_mask(day_mask)?,
                    Months::from_mask(month_mask)?,
                ),
            };
            if !monthly_pairs(days, months).eq(dates) {
                return None;
            }
            Trigger::monthly(time, days, months)
        }
        _ => None,
    }
}

enum Date {
    Daily,
    Weekly(u64),
    Monthly { day: u8, month: Option<u8> },
}

fn interval(value: &Value) -> Option<(LocalTime, Date)> {
    let object = value.as_object()?;
    let time = LocalTime::new(
        object.get("Hour")?.as_u64()?.try_into().ok()?,
        object.get("Minute")?.as_u64()?.try_into().ok()?,
    )?;
    let date = match (
        object.get("Weekday"),
        object.get("Day"),
        object.get("Month"),
    ) {
        (None, None, None) if object.len() == 2 => Date::Daily,
        (Some(weekday), None, None) if object.len() == 3 => Date::Weekly(weekday.as_u64()?),
        (None, Some(day), month) if object.len() == 3 + usize::from(month.is_some()) => {
            let day = day.as_u64()?.try_into().ok()?;
            if !(1..=31).contains(&day) {
                return None;
            }
            let month = if let Some(month) = month {
                let month = month.as_u64()?.try_into().ok()?;
                if !(1..=12).contains(&month) {
                    return None;
                }
                Some(month)
            } else {
                None
            };
            Date::Monthly { day, month }
        }
        _ => return None,
    };
    Some((time, date))
}

fn weekday_bit(day: u64) -> Option<u8> {
    match day {
        0..=6 => Some(1 << day),
        7 => Some(1), // launchd also recognizes 7 as Sunday.
        _ => None,
    }
}

fn max_day(month: u8) -> u8 {
    MONTH_MAX_DAYS[usize::from(month - 1)]
}

fn monthly_pairs(days: MonthDays, months: Months) -> impl Iterator<Item = (u8, u8)> {
    (1..=12)
        .filter(move |&month| months.mask() & (1 << (month - 1)) != 0)
        .flat_map(move |month| {
            (1..=max_day(month))
                .filter(move |&day| days.mask() & (1 << (day - 1)) != 0)
                .map(move |day| (month, day))
        })
}

fn monthly_masks(dates: impl IntoIterator<Item = (u8, u8)>) -> (u32, u16) {
    dates
        .into_iter()
        .fold((0, 0), |(days, months), (month, day)| {
            (days | (1 << (day - 1)), months | (1 << (month - 1)))
        })
}

pub(super) fn monthly_selection_hint(trigger: &Trigger) -> Option<(MonthDays, Months)> {
    let &Trigger::Monthly { days, months, .. } = trigger else {
        return None;
    };
    // Short months can make a selected day or month disappear from the effective dates.
    (monthly_masks(monthly_pairs(days, months)) != (days.mask(), months.mask()))
        .then_some((days, months))
}

pub(super) fn plist_calendar(trigger: &Trigger) -> String {
    let LocalTime { hour, minute } = trigger.time().expect("calendar trigger has a time");
    let clock = format!(
        "<key>Hour</key><integer>{hour}</integer><key>Minute</key><integer>{minute}</integer>"
    );
    match *trigger {
        Trigger::Once { date, .. } => format!(
            "<dict><key>Month</key><integer>{}</integer><key>Day</key><integer>{}</integer>{clock}</dict>",
            date.month(),
            date.day(),
        ),
        Trigger::Daily(_) => format!("<dict>{clock}</dict>"),
        Trigger::Weekly { days, .. } => {
            let mut plist = String::from("<array>");
            for day in (0..7).filter(|&day| days.mask() & (1 << day) != 0) {
                // String writes cannot fail.
                write!(
                    plist,
                    "<dict><key>Weekday</key><integer>{day}</integer>{clock}</dict>"
                )
                .unwrap();
            }
            plist.push_str("</array>");
            plist
        }
        Trigger::Monthly { days, months, .. } => {
            let mut plist = String::from("<array>");
            if months.mask() == Months::ALL && days.mask() < (1 << 28) {
                for day in (1..=28).filter(|&day| days.mask() & (1 << (day - 1)) != 0) {
                    write!(
                        plist,
                        "<dict><key>Day</key><integer>{day}</integer>{clock}</dict>"
                    )
                    .unwrap();
                }
            } else {
                for (month, day) in monthly_pairs(days, months) {
                    write!(
                        plist,
                        "<dict><key>Month</key><integer>{month}</integer><key>Day</key><integer>{day}</integer>{clock}</dict>"
                    )
                    .unwrap();
                }
            }
            plist.push_str("</array>");
            plist
        }
        #[cfg(windows)]
        Trigger::Logon { .. } => unreachable!("logon triggers are Windows-only"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn round_trips_one_time_calendar_with_the_runner_timestamp() {
        let date = schedule_date("2030-07-19").unwrap();
        let time = LocalTime::new(7, 5).unwrap();
        let once = Trigger::once(date, time).unwrap();
        let calendar = json!({ "Month": 7, "Day": 19, "Hour": 7, "Minute": 5 });

        assert_eq!(once.once_timestamp().as_deref(), Some("2030-07-19T07:05"));
        assert_eq!(
            parse_once(&calendar, "2030-07-19T07:05").as_ref(),
            Some(&once)
        );
        assert!(matches!(parse(&calendar), Some(Trigger::Monthly { .. })));
        assert_eq!(
            plist_calendar(&once),
            "<dict><key>Month</key><integer>7</integer><key>Day</key><integer>19</integer><key>Hour</key><integer>7</integer><key>Minute</key><integer>5</integer></dict>"
        );
    }

    #[test]
    fn reads_and_writes_daily_and_weekly_calendar_intervals() {
        let time = LocalTime::new(7, 5).unwrap();
        let daily = Trigger::Daily(time);
        assert_eq!(
            parse(&json!({"Hour": 7, "Minute": 5})).as_ref(),
            Some(&daily)
        );

        let clock = "<key>Hour</key><integer>7</integer><key>Minute</key><integer>5</integer>";
        assert_eq!(plist_calendar(&daily), format!("<dict>{clock}</dict>"));

        let weekly = Trigger::Weekly {
            time,
            days: Weekdays::from_mask((1 << 1) | 1).unwrap(),
        };
        let calendar = json!([
            {"Hour": 7, "Minute": 5, "Weekday": 1},
            {"Hour": 7, "Minute": 5, "Weekday": 7}
        ]);
        assert_eq!(parse(&calendar).as_ref(), Some(&weekly));
        let day =
            |weekday| format!("<dict><key>Weekday</key><integer>{weekday}</integer>{clock}</dict>");
        assert_eq!(
            plist_calendar(&weekly),
            format!("<array>{}{}</array>", day(0), day(1))
        );
    }

    #[test]
    fn round_trips_monthly_calendar_dates_and_keeps_selected_short_months() {
        let time = LocalTime::new(7, 5).unwrap();
        let days = MonthDays::from_mask(1 << 30).unwrap();
        let months = Months::from_mask(0b111).unwrap();
        let monthly = Trigger::monthly(time, days, months).unwrap();
        let calendar = json!([
            { "Month": 1, "Day": 31, "Hour": 7, "Minute": 5 },
            { "Month": 3, "Day": 31, "Hour": 7, "Minute": 5 }
        ]);
        let hint = monthly_selection_hint(&monthly);
        assert_eq!(hint, Some((days, months)));
        assert_eq!(
            parse_with_monthly_selection(&calendar, hint).as_ref(),
            Some(&monthly)
        );
        let clock = "<key>Hour</key><integer>7</integer><key>Minute</key><integer>5</integer>";
        let date = |month| {
            format!(
                "<dict><key>Month</key><integer>{month}</integer><key>Day</key><integer>31</integer>{clock}</dict>"
            )
        };
        assert_eq!(
            plist_calendar(&monthly),
            format!("<array>{}{}</array>", date(1), date(3))
        );

        let months = Months::from_mask(Months::ALL).unwrap();
        let days = MonthDays::from_mask((1 << 1) | (1 << 27)).unwrap();
        let monthly = Trigger::monthly(time, days, months).unwrap();
        assert_eq!(monthly_selection_hint(&monthly), None);
        assert_eq!(
            parse(&json!([
                { "Day": 2, "Hour": 7, "Minute": 5 },
                { "Day": 28, "Hour": 7, "Minute": 5 }
            ]))
            .as_ref(),
            Some(&monthly)
        );
        let date = |day| format!("<dict><key>Day</key><integer>{day}</integer>{clock}</dict>");
        assert_eq!(
            plist_calendar(&monthly),
            format!("<array>{}{}</array>", date(2), date(28))
        );
    }
}
