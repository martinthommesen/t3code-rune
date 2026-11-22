use time::{Date, Duration, OffsetDateTime, PrimitiveDateTime, Time, UtcOffset, Weekday};

use super::{Day, Preset};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Unit {
    Minutes,
    Hours,
    Days,
}

impl Unit {
    pub(crate) const ALL: [Self; 3] = [Self::Minutes, Self::Hours, Self::Days];

    pub(crate) fn minutes(self) -> u16 {
        match self {
            Self::Minutes => 1,
            Self::Hours => 60,
            Self::Days => 24 * 60,
        }
    }

    pub(crate) fn for_duration(minutes: u16) -> Self {
        Self::ALL
            .into_iter()
            .rev()
            .find(|unit| minutes.is_multiple_of(unit.minutes()))
            .expect("minutes divides every duration")
    }

    pub(crate) fn label(self, amount: u16) -> &'static str {
        match (self, amount == 1) {
            (Self::Minutes, true) => "minute",
            (Self::Minutes, false) => "minutes",
            (Self::Hours, true) => "hour",
            (Self::Hours, false) => "hours",
            (Self::Days, true) => "day",
            (Self::Days, false) => "days",
        }
    }
}

impl Day {
    pub(crate) const ALL: [Self; 9] = [
        Self::Today,
        Self::Tomorrow,
        Self::Monday,
        Self::Tuesday,
        Self::Wednesday,
        Self::Thursday,
        Self::Friday,
        Self::Saturday,
        Self::Sunday,
    ];

    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Today => "Today",
            Self::Tomorrow => "Tomorrow",
            Self::Monday => "Monday",
            Self::Tuesday => "Tuesday",
            Self::Wednesday => "Wednesday",
            Self::Thursday => "Thursday",
            Self::Friday => "Friday",
            Self::Saturday => "Saturday",
            Self::Sunday => "Sunday",
        }
    }

    fn days_ahead(self, weekday: Weekday) -> i64 {
        let target = match self {
            Self::Today => return 0,
            Self::Tomorrow => return 1,
            Self::Monday => Weekday::Monday,
            Self::Tuesday => Weekday::Tuesday,
            Self::Wednesday => Weekday::Wednesday,
            Self::Thursday => Weekday::Thursday,
            Self::Friday => Weekday::Friday,
            Self::Saturday => Weekday::Saturday,
            Self::Sunday => Weekday::Sunday,
        };
        let ahead = (target.number_days_from_monday() + 7 - weekday.number_days_from_monday()) % 7;
        i64::from(if ahead == 0 { 7 } else { ahead })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Wake {
    pub(crate) instant: OffsetDateTime,
    pub(crate) label: String,
    pub(crate) when: String,
}

pub(crate) fn resolve(
    preset: &Preset,
    now: OffsetDateTime,
    twenty_four_hour: bool,
) -> Option<Wake> {
    let today = local(now).date();
    let (instant, label, label_names_day) = match preset {
        Preset::After { minutes } => {
            let unit = Unit::for_duration(*minutes);
            let amount = *minutes / unit.minutes();
            let instant = local(now.checked_add(Duration::minutes(i64::from(*minutes)))?);
            (
                instant,
                format!("In {amount} {}", unit.label(amount)),
                false,
            )
        }
        Preset::At { day, time } => {
            let days = day.days_ahead(today.weekday());
            let date = today.checked_add(Duration::days(days))?;
            let (hour, minute) = time.split_once(':')?;
            let hour = hour.parse::<u8>().ok()?;
            let minute = minute.parse::<u8>().ok()?;
            let instant = at_local(date, Time::from_hms(hour, minute, 0).ok()?)?;
            if instant <= now {
                return None;
            }
            let label = match (days, day) {
                (0, _) if hour < 12 => "This morning".to_owned(),
                (0, _) if hour < 17 => "This afternoon".to_owned(),
                (0, _) => "This evening".to_owned(),
                (1, _) => "Tomorrow".to_owned(),
                (_, Day::Monday) => "Next week".to_owned(),
                (7, _) => format!("Next {}", day.label()),
                _ => day.label().to_owned(),
            };
            (instant, label, !matches!((days, day), (2.., Day::Monday)))
        }
    };
    let wall_time = format_time(instant.time(), twenty_four_hour);
    let when = if label_names_day || instant.date() == today {
        wall_time
    } else {
        let day = instant.weekday().to_string();
        format!("{} {wall_time}", &day[..3])
    };
    Some(Wake {
        instant,
        label,
        when,
    })
}

fn local(instant: OffsetDateTime) -> OffsetDateTime {
    instant.to_offset(UtcOffset::local_offset_at(instant).unwrap_or(UtcOffset::UTC))
}

fn at_local(date: Date, time: Time) -> Option<OffsetDateTime> {
    let wall = PrimitiveDateTime::new(date, time);
    let utc = wall.assume_utc();
    // Try the offsets on both sides of a clock change. Repeated times use the earlier
    // instant. Skipped times move forward by the gap, preserving the minute in the hour.
    [-2, 0, 2]
        .into_iter()
        .filter_map(|days| utc.checked_add(Duration::days(days)))
        .map(local)
        .map(|hour| local(wall.assume_offset(hour.offset())))
        .map(|instant| {
            (
                PrimitiveDateTime::new(instant.date(), instant.time()),
                instant,
            )
        })
        .filter(|(candidate, _)| *candidate >= wall)
        .min()
        .map(|(_, instant)| instant)
}

fn format_time(time: Time, twenty_four_hour: bool) -> String {
    let hour = time.hour();
    let minute = time.minute();
    if twenty_four_hour {
        return format!("{hour:02}:{minute:02}");
    }
    let period = if hour < 12 { "AM" } else { "PM" };
    let hour = if hour.is_multiple_of(12) {
        12
    } else {
        hour % 12
    };
    format!("{hour}:{minute:02} {period}")
}
