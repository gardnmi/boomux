use std::error::Error;
use std::fmt;

use chrono::{Datelike, LocalResult, NaiveDate, TimeZone, Utc};

pub const MAX_PROMPT_BYTES: usize = 65_536;
pub const MAX_CRON_BYTES: usize = 256;
pub const MAX_TIMEZONE_BYTES: usize = 256;
pub const MAX_INTEGRATION_KEY_BYTES: usize = 256;
pub const MAX_EXTERNAL_SESSION_ID_BYTES: usize = 256;
pub const MAX_SCHEDULES_PER_WORKSPACE: usize = 64;
const MAX_OCCURRENCE_SEARCH_DAYS: u32 = 146_097;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SchedulingError {
    EmptyValue {
        field: &'static str,
    },
    ValueTooLong {
        field: &'static str,
        max_bytes: usize,
    },
    InvalidValue {
        field: &'static str,
        reason: &'static str,
    },
    InvalidCron {
        field: Option<usize>,
        reason: &'static str,
    },
    InvalidConvenience {
        reason: &'static str,
    },
    InvalidTimezone,
    SystemTimezoneUnavailable,
    TimeOutOfRange,
    OccurrenceSearchExhausted,
}

impl fmt::Display for SchedulingError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyValue { field } => write!(formatter, "{field} must not be empty"),
            Self::ValueTooLong { field, max_bytes } => {
                write!(formatter, "{field} must be at most {max_bytes} bytes")
            }
            Self::InvalidValue { field, reason } => {
                write!(formatter, "{field} is invalid: {reason}")
            }
            Self::InvalidCron {
                field: Some(field),
                reason,
            } => write!(formatter, "cron field {field} is invalid: {reason}"),
            Self::InvalidCron {
                field: None,
                reason,
            } => write!(formatter, "cron expression is invalid: {reason}"),
            Self::InvalidConvenience { reason } => {
                write!(formatter, "schedule convenience is invalid: {reason}")
            }
            Self::InvalidTimezone => formatter.write_str("timezone is not a valid IANA timezone"),
            Self::SystemTimezoneUnavailable => {
                formatter.write_str("system IANA timezone could not be resolved")
            }
            Self::TimeOutOfRange => {
                formatter.write_str("schedule time is outside the supported range")
            }
            Self::OccurrenceSearchExhausted => {
                formatter.write_str("schedule has no occurrence within the bounded search range")
            }
        }
    }
}

impl Error for SchedulingError {}

/// Validates and whitespace-normalizes the supported five-field cron subset.
pub fn canonicalize_cron(expression: &str) -> Result<String, SchedulingError> {
    validate_byte_bound("cron expression", expression, MAX_CRON_BYTES)?;
    let fields = expression.split_whitespace().collect::<Vec<_>>();
    if fields.len() != 5 {
        return Err(SchedulingError::InvalidCron {
            field: None,
            reason: "expected exactly five fields",
        });
    }

    const BOUNDS: [(u32, u32); 5] = [(0, 59), (0, 23), (1, 31), (1, 12), (0, 6)];
    for (index, (field, (minimum, maximum))) in fields.iter().zip(BOUNDS).enumerate() {
        validate_cron_field(field, minimum, maximum).map_err(|reason| {
            SchedulingError::InvalidCron {
                field: Some(index + 1),
                reason,
            }
        })?;
    }

    Ok(fields.join(" "))
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CronSchedule {
    minutes: Vec<u32>,
    hours: Vec<u32>,
    days_of_month: Vec<u32>,
    months: Vec<u32>,
    days_of_week: Vec<u32>,
    any_day_of_month: bool,
    any_day_of_week: bool,
    timezone: chrono_tz::Tz,
}

impl CronSchedule {
    pub fn compile(expression: &str, timezone: &str) -> Result<Self, SchedulingError> {
        let expression = canonicalize_cron(expression)?;
        let fields = expression.split_whitespace().collect::<Vec<_>>();
        let timezone = canonicalize_timezone(timezone)?
            .parse()
            .map_err(|_| SchedulingError::InvalidTimezone)?;
        Ok(Self {
            minutes: compile_field(fields[0], 0, 59),
            hours: compile_field(fields[1], 0, 23),
            days_of_month: compile_field(fields[2], 1, 31),
            months: compile_field(fields[3], 1, 12),
            days_of_week: compile_field(fields[4], 0, 6),
            any_day_of_month: wildcard_origin(fields[2]),
            any_day_of_week: wildcard_origin(fields[4]),
            timezone,
        })
    }

    pub fn ensure_possible(&self) -> Result<(), SchedulingError> {
        let mut date =
            NaiveDate::from_ymd_opt(2000, 1, 1).ok_or(SchedulingError::TimeOutOfRange)?;
        for _ in 0..MAX_OCCURRENCE_SEARCH_DAYS {
            if self.date_matches(date)
                && self.hours.iter().any(|hour| {
                    self.minutes.iter().any(|minute| {
                        date.and_hms_opt(*hour, *minute, 0)
                            .and_then(|local| first_local_instant(self.timezone, local))
                            .is_some()
                    })
                })
            {
                return Ok(());
            }
            date = date.succ_opt().ok_or(SchedulingError::TimeOutOfRange)?;
        }
        Err(SchedulingError::OccurrenceSearchExhausted)
    }

    pub fn next_after_ms(&self, frontier_ms: u64) -> Result<u64, SchedulingError> {
        let frontier = utc_from_millis(frontier_ms)?;
        let mut date = frontier.with_timezone(&self.timezone).date_naive();
        for _ in 0..MAX_OCCURRENCE_SEARCH_DAYS {
            if self.date_matches(date) {
                for &hour in &self.hours {
                    for &minute in &self.minutes {
                        let local = date
                            .and_hms_opt(hour, minute, 0)
                            .ok_or(SchedulingError::TimeOutOfRange)?;
                        if let Some(candidate) = first_local_instant(self.timezone, local) {
                            let candidate_ms = millis_from_utc(candidate)?;
                            if candidate_ms > frontier_ms {
                                return Ok(candidate_ms);
                            }
                        }
                    }
                }
            }
            date = date.succ_opt().ok_or(SchedulingError::TimeOutOfRange)?;
        }
        Err(SchedulingError::OccurrenceSearchExhausted)
    }

    pub fn latest_at_or_before_ms(&self, ceiling_ms: u64) -> Result<u64, SchedulingError> {
        let ceiling = utc_from_millis(ceiling_ms)?;
        let mut date = ceiling.with_timezone(&self.timezone).date_naive();
        for _ in 0..MAX_OCCURRENCE_SEARCH_DAYS {
            if self.date_matches(date) {
                for &hour in self.hours.iter().rev() {
                    for &minute in self.minutes.iter().rev() {
                        let local = date
                            .and_hms_opt(hour, minute, 0)
                            .ok_or(SchedulingError::TimeOutOfRange)?;
                        if let Some(candidate) = first_local_instant(self.timezone, local) {
                            let candidate_ms = millis_from_utc(candidate)?;
                            if candidate_ms <= ceiling_ms {
                                return Ok(candidate_ms);
                            }
                        }
                    }
                }
            }
            date = date.pred_opt().ok_or(SchedulingError::TimeOutOfRange)?;
        }
        Err(SchedulingError::OccurrenceSearchExhausted)
    }

    fn date_matches(&self, date: NaiveDate) -> bool {
        if self.months.binary_search(&date.month()).is_err() {
            return false;
        }
        let dom = self.days_of_month.binary_search(&date.day()).is_ok();
        let dow = self
            .days_of_week
            .binary_search(&date.weekday().num_days_from_sunday())
            .is_ok();
        match (self.any_day_of_month, self.any_day_of_week) {
            (true, true) => true,
            (true, false) => dow,
            (false, true) => dom,
            (false, false) => dom || dow,
        }
    }
}

fn wildcard_origin(field: &str) -> bool {
    field.split(',').any(|component| {
        component
            .split_once('/')
            .map_or(component, |(base, _)| base)
            == "*"
    })
}

fn compile_field(field: &str, minimum: u32, maximum: u32) -> Vec<u32> {
    let mut selected = vec![false; (maximum + 1) as usize];
    for component in field.split(',') {
        let (base, step) = component
            .split_once('/')
            .map_or((component, 1), |(base, step)| {
                (base, step.parse::<u32>().expect("validated cron step"))
            });
        let (start, end) = if base == "*" {
            (minimum, maximum)
        } else if let Some((start, end)) = base.split_once('-') {
            (
                start.parse::<u32>().expect("validated cron range"),
                end.parse::<u32>().expect("validated cron range"),
            )
        } else {
            let value = base.parse::<u32>().expect("validated cron value");
            (value, value)
        };
        for value in (start..=end).step_by(step as usize) {
            selected[value as usize] = true;
        }
    }
    (minimum..=maximum)
        .filter(|value| selected[*value as usize])
        .collect()
}

fn utc_from_millis(value: u64) -> Result<chrono::DateTime<Utc>, SchedulingError> {
    let value = i64::try_from(value).map_err(|_| SchedulingError::TimeOutOfRange)?;
    Utc.timestamp_millis_opt(value)
        .single()
        .ok_or(SchedulingError::TimeOutOfRange)
}

pub(crate) fn validate_timestamp_ms(value: u64) -> Result<(), SchedulingError> {
    utc_from_millis(value).map(|_| ())
}

fn millis_from_utc(value: chrono::DateTime<chrono_tz::Tz>) -> Result<u64, SchedulingError> {
    u64::try_from(value.with_timezone(&Utc).timestamp_millis())
        .map_err(|_| SchedulingError::TimeOutOfRange)
}

fn first_local_instant(
    timezone: chrono_tz::Tz,
    local: chrono::NaiveDateTime,
) -> Option<chrono::DateTime<chrono_tz::Tz>> {
    match timezone.from_local_datetime(&local) {
        LocalResult::None => None,
        LocalResult::Single(value) => Some(value),
        LocalResult::Ambiguous(first, second) => Some(first.min(second)),
    }
}

pub fn every_minutes_cron(minutes: u8) -> Result<String, SchedulingError> {
    if minutes == 0 || 60 % minutes != 0 {
        return Err(SchedulingError::InvalidConvenience {
            reason: "minute interval must be a positive divisor of 60",
        });
    }
    Ok(match minutes {
        1 => "* * * * *".to_owned(),
        60 => "0 * * * *".to_owned(),
        _ => format!("*/{minutes} * * * *"),
    })
}

pub fn every_hours_cron(hours: u8) -> Result<String, SchedulingError> {
    if hours == 0 || 24 % hours != 0 {
        return Err(SchedulingError::InvalidConvenience {
            reason: "hour interval must be a positive divisor of 24",
        });
    }
    Ok(match hours {
        1 => "0 * * * *".to_owned(),
        24 => "0 0 * * *".to_owned(),
        _ => format!("0 */{hours} * * *"),
    })
}

pub fn daily_cron(hour: u8, minute: u8) -> Result<String, SchedulingError> {
    validate_time(hour, minute)?;
    Ok(format!("{minute} {hour} * * *"))
}

pub fn weekdays_cron(hour: u8, minute: u8) -> Result<String, SchedulingError> {
    validate_time(hour, minute)?;
    Ok(format!("{minute} {hour} * * 1-5"))
}

pub fn weekly_cron(day: &str, hour: u8, minute: u8) -> Result<String, SchedulingError> {
    validate_time(hour, minute)?;
    let day = match day.to_ascii_lowercase().as_str() {
        "sun" | "sunday" => 0,
        "mon" | "monday" => 1,
        "tue" | "tues" | "tuesday" => 2,
        "wed" | "wednesday" => 3,
        "thu" | "thur" | "thurs" | "thursday" => 4,
        "fri" | "friday" => 5,
        "sat" | "saturday" => 6,
        _ => {
            return Err(SchedulingError::InvalidConvenience {
                reason: "weekly day must be a common English day name",
            });
        }
    };
    Ok(format!("{minute} {hour} * * {day}"))
}

pub fn canonicalize_timezone(timezone: &str) -> Result<String, SchedulingError> {
    validate_byte_bound("timezone", timezone, MAX_TIMEZONE_BYTES)?;
    if timezone.trim().is_empty() {
        return Err(SchedulingError::EmptyValue { field: "timezone" });
    }
    timezone
        .trim()
        .parse::<chrono_tz::Tz>()
        .map(|timezone| timezone.to_string())
        .map_err(|_| SchedulingError::InvalidTimezone)
}

pub fn resolve_system_timezone() -> Result<String, SchedulingError> {
    let timezone =
        iana_time_zone::get_timezone().map_err(|_| SchedulingError::SystemTimezoneUnavailable)?;
    canonicalize_timezone(&timezone).map_err(|_| SchedulingError::SystemTimezoneUnavailable)
}

pub fn validate_prompt(prompt: &str) -> Result<(), SchedulingError> {
    validate_required_bounded("prompt", prompt, MAX_PROMPT_BYTES)
}

pub fn validate_integration_key(integration: &str) -> Result<(), SchedulingError> {
    validate_identity("integration key", integration, MAX_INTEGRATION_KEY_BYTES)
}

pub fn validate_external_session_id(session_id: &str) -> Result<(), SchedulingError> {
    validate_identity(
        "external session ID",
        session_id,
        MAX_EXTERNAL_SESSION_ID_BYTES,
    )
}

fn validate_identity(
    field: &'static str,
    value: &str,
    max_bytes: usize,
) -> Result<(), SchedulingError> {
    validate_required_bounded(field, value, max_bytes)?;
    if value.chars().any(char::is_control) {
        return Err(SchedulingError::InvalidValue {
            field,
            reason: "control characters are not allowed",
        });
    }
    Ok(())
}

fn validate_cron_field(field: &str, minimum: u32, maximum: u32) -> Result<(), &'static str> {
    for component in field.split(',') {
        if component.is_empty() {
            return Err("list contains an empty item");
        }
        let (base, step) = match component.split_once('/') {
            Some((base, step)) if !base.contains('/') && !step.contains('/') => {
                if base != "*" && !base.contains('-') {
                    return Err("steps are allowed only on a wildcard or range");
                }
                let step = parse_number(step).ok_or("step must be numeric")?;
                if step == 0 || step > maximum {
                    return Err("step is outside the field range");
                }
                (base, Some(step))
            }
            Some(_) => return Err("step syntax is malformed"),
            None => (component, None),
        };

        if base == "*" {
            continue;
        }
        if let Some((start, end)) = base.split_once('-') {
            if start.contains('-') || end.contains('-') {
                return Err("range syntax is malformed");
            }
            let start = parse_number(start).ok_or("range values must be numeric")?;
            let end = parse_number(end).ok_or("range values must be numeric")?;
            if start < minimum || end > maximum {
                return Err("range value is outside the field bounds");
            }
            if start > end {
                return Err("range must be ascending");
            }
            continue;
        }
        if step.is_some() {
            return Err("step base is malformed");
        }
        let value = parse_number(base).ok_or("value must be numeric")?;
        if value < minimum || value > maximum {
            return Err("value is outside the field bounds");
        }
    }
    Ok(())
}

fn parse_number(value: &str) -> Option<u32> {
    (!value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit()))
        .then(|| value.parse().ok())
        .flatten()
}

fn validate_time(hour: u8, minute: u8) -> Result<(), SchedulingError> {
    if hour > 23 || minute > 59 {
        return Err(SchedulingError::InvalidConvenience {
            reason: "time must use a 00:00 through 23:59 value",
        });
    }
    Ok(())
}

fn validate_required_bounded(
    field: &'static str,
    value: &str,
    max_bytes: usize,
) -> Result<(), SchedulingError> {
    validate_byte_bound(field, value, max_bytes)?;
    if value.trim().is_empty() {
        return Err(SchedulingError::EmptyValue { field });
    }
    Ok(())
}

fn validate_byte_bound(
    field: &'static str,
    value: &str,
    max_bytes: usize,
) -> Result<(), SchedulingError> {
    if value.len() > max_bytes {
        return Err(SchedulingError::ValueTooLong { field, max_bytes });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn utc_ms(year: i32, month: u32, day: u32, hour: u32, minute: u32) -> u64 {
        u64::try_from(
            Utc.with_ymd_and_hms(year, month, day, hour, minute, 0)
                .single()
                .unwrap()
                .timestamp_millis(),
        )
        .unwrap()
    }

    #[test]
    fn cron_accepts_supported_grammar_and_normalizes_only_whitespace() {
        for expression in [
            "* * * * *",
            "0,15,30,45 9-17 * 1,6,12 1-5",
            "*/15 9-17/2 1-31/3 * 0,6",
            "01 02 03 04 05",
        ] {
            assert_eq!(canonicalize_cron(expression).unwrap(), expression);
        }
        assert_eq!(
            canonicalize_cron("  */15\t 9-17/2\n* * 1-5  ").unwrap(),
            "*/15 9-17/2 * * 1-5"
        );
    }

    #[test]
    fn cron_rejects_unsupported_and_malformed_forms() {
        for expression in [
            "0 0 * *",
            "0 0 * * * *",
            "0 0 * JAN *",
            "@daily",
            "0 0 ? * *",
            "0 0 L * *",
            "0 0 1W * *",
            "0 0 * * 1#2",
            "60 0 * * *",
            "0 24 * * *",
            "0 0 0 * *",
            "0 0 * 13 *",
            "0 0 * * 7",
            "0 0 * * 5-1",
            "*/0 * * * *",
            "*/61 * * * *",
            "*/60 * * * *",
            "0 */24 * * *",
            "1/2 * * * *",
            "1--2 * * * *",
            "1,,2 * * * *",
            "*/x * * * *",
        ] {
            assert!(
                canonicalize_cron(expression).is_err(),
                "accepted {expression}"
            );
        }
    }

    #[test]
    fn cron_enforces_source_bound_without_echoing_input() {
        let private = "s".repeat(MAX_CRON_BYTES + 1);
        let error = canonicalize_cron(&private).unwrap_err();
        assert!(!error.to_string().contains(&private));
        assert_eq!(
            error,
            SchedulingError::ValueTooLong {
                field: "cron expression",
                max_bytes: MAX_CRON_BYTES,
            }
        );
    }

    #[test]
    fn conveniences_compile_to_canonical_numeric_cron() {
        assert_eq!(every_minutes_cron(1).unwrap(), "* * * * *");
        assert_eq!(every_minutes_cron(15).unwrap(), "*/15 * * * *");
        assert_eq!(every_minutes_cron(60).unwrap(), "0 * * * *");
        assert_eq!(every_hours_cron(6).unwrap(), "0 */6 * * *");
        assert_eq!(every_hours_cron(24).unwrap(), "0 0 * * *");
        assert_eq!(daily_cron(9, 5).unwrap(), "5 9 * * *");
        assert_eq!(weekdays_cron(17, 30).unwrap(), "30 17 * * 1-5");
        assert_eq!(weekly_cron("MONDAY", 8, 0).unwrap(), "0 8 * * 1");
        assert_eq!(weekly_cron("tues", 8, 0).unwrap(), "0 8 * * 2");

        for expression in [
            every_minutes_cron(20).unwrap(),
            every_hours_cron(8).unwrap(),
            daily_cron(23, 59).unwrap(),
            weekdays_cron(0, 0).unwrap(),
            weekly_cron("Sunday", 12, 30).unwrap(),
        ] {
            assert_eq!(canonicalize_cron(&expression).unwrap(), expression);
        }
    }

    #[test]
    fn conveniences_reject_invalid_intervals_times_and_days() {
        assert!(every_minutes_cron(0).is_err());
        assert!(every_minutes_cron(7).is_err());
        assert!(every_hours_cron(5).is_err());
        assert!(daily_cron(24, 0).is_err());
        assert!(weekdays_cron(0, 60).is_err());
        assert!(weekly_cron("funday", 0, 0).is_err());
    }

    #[test]
    fn timezone_uses_iana_parser_and_stable_display_name() {
        assert_eq!(
            canonicalize_timezone("America/New_York").unwrap(),
            "America/New_York"
        );
        assert_eq!(canonicalize_timezone("UTC").unwrap(), "UTC");
        assert_eq!(canonicalize_timezone(" \tUTC\n").unwrap(), "UTC");
        assert!(canonicalize_timezone("Not/A_Private_Zone").is_err());
        assert!(canonicalize_timezone("").is_err());

        let private = "Private/".to_owned() + &"x".repeat(MAX_TIMEZONE_BYTES);
        let message = canonicalize_timezone(&private).unwrap_err().to_string();
        assert!(!message.contains(&private));
    }

    #[test]
    fn required_values_enforce_whitespace_and_byte_bounds() {
        for prompt in ["", " \t\n"] {
            assert!(matches!(
                validate_prompt(prompt),
                Err(SchedulingError::EmptyValue { field: "prompt" })
            ));
        }
        assert!(validate_prompt("run tests").is_ok());
        assert!(validate_prompt(&"x".repeat(MAX_PROMPT_BYTES)).is_ok());
        assert!(validate_prompt(&"x".repeat(MAX_PROMPT_BYTES + 1)).is_err());

        assert!(validate_integration_key("opencode").is_ok());
        assert!(validate_integration_key(" ").is_err());
        assert!(validate_integration_key("open\ncode").is_err());
        assert!(validate_integration_key(&"x".repeat(MAX_INTEGRATION_KEY_BYTES + 1)).is_err());
        assert!(validate_external_session_id("session-1").is_ok());
        assert!(validate_external_session_id("").is_err());
        assert!(validate_external_session_id("session\u{1b}").is_err());
        assert!(
            validate_external_session_id(&"x".repeat(MAX_EXTERNAL_SESSION_ID_BYTES + 1)).is_err()
        );
    }

    #[test]
    fn compiled_cron_handles_fields_dom_dow_or_and_strict_frontiers() {
        let cron = CronSchedule::compile("0,0,30 9-11/2 * * *", "UTC").unwrap();
        assert_eq!(
            cron.next_after_ms(utc_ms(2026, 1, 1, 9, 0)),
            Ok(utc_ms(2026, 1, 1, 9, 30))
        );
        assert_eq!(
            cron.next_after_ms(utc_ms(2026, 1, 1, 9, 30)),
            Ok(utc_ms(2026, 1, 1, 11, 0))
        );

        let or = CronSchedule::compile("0 0 31 * 1", "UTC").unwrap();
        assert_eq!(
            or.next_after_ms(utc_ms(2026, 1, 4, 23, 59)),
            Ok(utc_ms(2026, 1, 5, 0, 0))
        );
        assert_eq!(
            or.latest_at_or_before_ms(utc_ms(2026, 1, 6, 0, 0)),
            Ok(utc_ms(2026, 1, 5, 0, 0))
        );

        let candidate = utc_ms(2026, 1, 7, 9, 0);
        assert_eq!(cron.next_after_ms(candidate - 1), Ok(candidate));
        assert_eq!(
            cron.latest_at_or_before_ms(candidate - 1),
            Ok(utc_ms(2026, 1, 6, 11, 30))
        );
        assert_eq!(cron.latest_at_or_before_ms(candidate), Ok(candidate));
        assert_eq!(cron.latest_at_or_before_ms(candidate + 1), Ok(candidate));
    }

    #[test]
    fn wildcard_origin_controls_standard_dom_dow_semantics() {
        let stepped_dom = CronSchedule::compile("0 0 */2 * 1", "UTC").unwrap();
        assert_eq!(
            stepped_dom.next_after_ms(utc_ms(2024, 1, 7, 0, 0)),
            Ok(utc_ms(2024, 1, 8, 0, 0))
        );
        let full_dom_range = CronSchedule::compile("0 0 1-31 * 1", "UTC").unwrap();
        assert_eq!(
            full_dom_range.next_after_ms(utc_ms(2024, 1, 2, 0, 0) - 1),
            Ok(utc_ms(2024, 1, 2, 0, 0))
        );

        let stepped_dow = CronSchedule::compile("0 0 15 */2 */2", "UTC").unwrap();
        assert_eq!(
            stepped_dow.next_after_ms(utc_ms(2024, 1, 14, 0, 0)),
            Ok(utc_ms(2024, 1, 15, 0, 0))
        );
        let full_dow_range = CronSchedule::compile("0 0 31 */2 0-6", "UTC").unwrap();
        assert_eq!(
            full_dow_range.next_after_ms(utc_ms(2024, 1, 2, 0, 0) - 1),
            Ok(utc_ms(2024, 1, 2, 0, 0))
        );
    }

    #[test]
    fn compiled_cron_skips_new_york_gap_and_uses_first_repeated_minute_only() {
        let gap = CronSchedule::compile("30 2 * * *", "America/New_York").unwrap();
        assert_eq!(
            gap.next_after_ms(utc_ms(2024, 3, 9, 8, 0)),
            Ok(utc_ms(2024, 3, 11, 6, 30))
        );

        let repeated = CronSchedule::compile("30 1 * * *", "America/New_York").unwrap();
        let first = utc_ms(2024, 11, 3, 5, 30);
        assert_eq!(repeated.next_after_ms(utc_ms(2024, 11, 3, 4, 0)), Ok(first));
        assert_eq!(
            repeated.next_after_ms(first),
            Ok(utc_ms(2024, 11, 4, 6, 30))
        );
    }

    #[test]
    fn compiled_cron_handles_lord_howe_non_hour_gap_and_impossible_dates() {
        let gap = CronSchedule::compile("15 2 * * *", "Australia/Lord_Howe").unwrap();
        assert_eq!(
            gap.next_after_ms(utc_ms(2024, 10, 5, 16, 0)),
            Ok(utc_ms(2024, 10, 6, 15, 15))
        );

        let impossible = CronSchedule::compile("0 0 30 2 *", "UTC").unwrap();
        assert_eq!(
            impossible.next_after_ms(utc_ms(2024, 1, 1, 0, 0)),
            Err(SchedulingError::OccurrenceSearchExhausted)
        );
        assert_eq!(
            impossible.ensure_possible(),
            Err(SchedulingError::OccurrenceSearchExhausted)
        );
        for _ in 0..MAX_SCHEDULES_PER_WORKSPACE {
            assert_eq!(
                impossible.ensure_possible(),
                Err(SchedulingError::OccurrenceSearchExhausted)
            );
        }

        let fold = CronSchedule::compile("45 1 * * *", "Australia/Lord_Howe").unwrap();
        let first = utc_ms(2024, 4, 6, 14, 45);
        assert_eq!(fold.next_after_ms(first - 1), Ok(first));
        assert_eq!(
            fold.latest_at_or_before_ms(utc_ms(2024, 4, 6, 15, 20)),
            Ok(first)
        );
    }
}
