use chrono::{DateTime, Datelike, Duration as ChronoDuration, Local, TimeZone, Timelike, Utc};

const MAX_LOOKAHEAD_MINUTES: i64 = 60 * 24 * 366 * 5;

#[derive(Clone, Debug)]
pub(super) struct CronSchedule {
    minutes: CronField,
    hours: CronField,
    days: CronField,
    months: CronField,
    weekdays: CronField,
}

impl CronSchedule {
    pub(super) fn parse(expr: &str) -> Result<Self, String> {
        let parts = expr.split_whitespace().collect::<Vec<_>>();
        if parts.len() != 5 {
            return Err("cron expression must have 5 fields".to_string());
        }

        Ok(Self {
            minutes: CronField::parse(parts[0], 0, 59, &[])?,
            hours: CronField::parse(parts[1], 0, 23, &[])?,
            days: CronField::parse(parts[2], 1, 31, &[])?,
            months: CronField::parse(parts[3], 1, 12, MONTH_NAMES)?,
            weekdays: CronField::parse(parts[4], 0, 7, WEEKDAY_NAMES)?,
        })
    }

    pub(super) fn next_after(&self, after: DateTime<Utc>) -> Result<DateTime<Utc>, String> {
        let mut candidate = after
            .with_timezone(&Local)
            .with_second(0)
            .and_then(|value| value.with_nanosecond(0))
            .ok_or_else(|| "failed to truncate timestamp".to_string())?
            + ChronoDuration::minutes(1);

        for _ in 0..MAX_LOOKAHEAD_MINUTES {
            if self.matches(&candidate) {
                return Ok(candidate.with_timezone(&Utc));
            }
            candidate += ChronoDuration::minutes(1);
        }

        Err("cron expression has no matching time in the next 5 years".to_string())
    }

    fn matches<Tz: TimeZone>(&self, dt: &DateTime<Tz>) -> bool {
        if !self.minutes.matches(dt.minute())
            || !self.hours.matches(dt.hour())
            || !self.months.matches(dt.month())
        {
            return false;
        }

        let day_matches = self.days.matches(dt.day());
        let weekday_matches = self.weekdays.matches(dt.weekday().num_days_from_sunday());

        if self.days.restricted && self.weekdays.restricted {
            day_matches || weekday_matches
        } else {
            day_matches && weekday_matches
        }
    }
}

#[derive(Clone, Debug)]
struct CronField {
    allowed: Vec<bool>,
    restricted: bool,
}

impl CronField {
    fn parse(
        input: &str,
        min: u32,
        max: u32,
        names: &[(&'static str, u32)],
    ) -> Result<Self, String> {
        let mut allowed = vec![false; (max + 1) as usize];
        let mut restricted = false;

        for raw_part in input.split(',') {
            let part = raw_part.trim();
            if part.is_empty() {
                return Err(format!("empty cron field part in '{input}'"));
            }

            let (base, step) = match part.split_once('/') {
                Some((base, step)) => {
                    let step = step
                        .parse::<u32>()
                        .map_err(|_| format!("invalid step '{step}'"))?;
                    if step == 0 {
                        return Err("cron step must be greater than 0".to_string());
                    }
                    (base, step)
                }
                None => (part, 1),
            };

            let is_wildcard = base == "*" || base == "?";
            let (start, end) = if is_wildcard {
                (min, max)
            } else if let Some((start, end)) = base.split_once('-') {
                (
                    parse_cron_value(start, min, max, names)?,
                    parse_cron_value(end, min, max, names)?,
                )
            } else {
                let value = parse_cron_value(base, min, max, names)?;
                (value, value)
            };

            if start > end {
                return Err(format!("cron range start {start} is greater than {end}"));
            }

            restricted |= !is_wildcard || step > 1;

            let mut value = start;
            while value <= end {
                set_allowed(&mut allowed, min, max, value)?;
                match value.checked_add(step) {
                    Some(next) => value = next,
                    None => break,
                }
            }
        }

        if !allowed.iter().any(|allowed| *allowed) {
            return Err(format!("cron field '{input}' allows no values"));
        }

        Ok(Self {
            allowed,
            restricted,
        })
    }

    fn matches(&self, value: u32) -> bool {
        self.allowed.get(value as usize).copied().unwrap_or(false)
    }
}

pub fn normalize_cron_expression(input: &str) -> Result<String, String> {
    let parts = input.split_whitespace().collect::<Vec<_>>();
    match parts.len() {
        5 => {
            let normalized = parts.join(" ");
            CronSchedule::parse(&normalized)?;
            Ok(normalized)
        }
        6 if parts[0] == "0" => {
            let normalized = parts[1..].join(" ");
            CronSchedule::parse(&normalized)?;
            Ok(normalized)
        }
        6 => Err("seconds field is only supported when it is exactly 0".to_string()),
        _ => {
            Err("cron expression must be 5 fields, or 6 fields with leading 0 seconds".to_string())
        }
    }
}

fn set_allowed(allowed: &mut [bool], min: u32, max: u32, value: u32) -> Result<(), String> {
    if value < min || value > max {
        return Err(format!(
            "cron value {value} is outside allowed range {min}..={max}"
        ));
    }

    if max == 7 && value == 7 {
        allowed[0] = true;
    }

    allowed[value as usize] = true;
    Ok(())
}

fn parse_cron_value(
    raw: &str,
    min: u32,
    max: u32,
    names: &[(&'static str, u32)],
) -> Result<u32, String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Err("empty cron value".to_string());
    }

    let value = parse_named_value(raw, names).or_else(|| raw.parse::<u32>().ok());
    let Some(value) = value else {
        return Err(format!("invalid cron value '{raw}'"));
    };

    if value < min || value > max {
        return Err(format!(
            "cron value {value} is outside allowed range {min}..={max}"
        ));
    }

    Ok(value)
}

fn parse_named_value(raw: &str, names: &[(&'static str, u32)]) -> Option<u32> {
    names
        .iter()
        .find_map(|(name, value)| name.eq_ignore_ascii_case(raw).then_some(*value))
}

const MONTH_NAMES: &[(&str, u32)] = &[
    ("JAN", 1),
    ("FEB", 2),
    ("MAR", 3),
    ("APR", 4),
    ("MAY", 5),
    ("JUN", 6),
    ("JUL", 7),
    ("AUG", 8),
    ("SEP", 9),
    ("OCT", 10),
    ("NOV", 11),
    ("DEC", 12),
];

const WEEKDAY_NAMES: &[(&str, u32)] = &[
    ("SUN", 0),
    ("MON", 1),
    ("TUE", 2),
    ("WED", 3),
    ("THU", 4),
    ("FRI", 5),
    ("SAT", 6),
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_accepts_five_fields_and_leading_zero_seconds() {
        assert_eq!(
            normalize_cron_expression("*/30 * * * *").unwrap(),
            "*/30 * * * *"
        );
        assert_eq!(
            normalize_cron_expression("0 */10 * * * *").unwrap(),
            "*/10 * * * *"
        );
    }

    #[test]
    fn normalize_rejects_nonzero_seconds() {
        assert!(normalize_cron_expression("*/30 * * * * *").is_err());
    }

    #[test]
    fn field_parser_supports_steps_ranges_and_names() {
        let schedule = CronSchedule::parse("*/15 9-17 * JAN MON-FRI").unwrap();

        assert!(schedule.minutes.matches(0));
        assert!(schedule.minutes.matches(45));
        assert!(!schedule.minutes.matches(46));
        assert!(schedule.hours.matches(9));
        assert!(schedule.hours.matches(17));
        assert!(!schedule.hours.matches(18));
        assert!(schedule.months.matches(1));
        assert!(!schedule.months.matches(2));
        assert!(schedule.weekdays.matches(1));
        assert!(schedule.weekdays.matches(5));
        assert!(!schedule.weekdays.matches(6));
    }

    #[test]
    fn weekday_seven_matches_sunday_zero() {
        let schedule = CronSchedule::parse("0 0 * * 7").unwrap();
        assert!(schedule.weekdays.matches(0));
    }
}
