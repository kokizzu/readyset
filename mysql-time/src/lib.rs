use std::cmp::Ordering;
use std::convert::{TryFrom, TryInto};
use std::fmt;
use std::fmt::Formatter;
use std::hash::{Hash, Hasher};
use std::ops::{Add, Sub};
use std::str::FromStr;

use chrono::{Duration, NaiveDateTime, NaiveTime, Timelike};
use mysql_common::value::convert::{FromValue, FromValueError};
use mysql_common::value::Value;
use proptest::arbitrary::Arbitrary;
use proptest::strategy::Strategy;
use readyset_util::arbitrary::arbitrary_duration;
use serde::{Deserialize, Serialize};
use thiserror::Error;

const MICROSECS_IN_SECOND: i64 = 1_000_000;

const MAX_MYSQL_TIME_SECONDS: i64 = 3020399; // 3020399 secs = 838:59:59

/// Errors that can occur when converting various types into a [`MySqlTime`]
#[derive(Debug, Error, PartialEq, Eq)]
pub enum ConvertError {
    /// An error occurred when parsing a string into a [`MySqlTime`].
    ///
    /// In MySQL, these result in an all-zero time
    #[error("Error parsing string as time")]
    ParseError,

    /// A [`MySqlTime`] was parsed successfully, but one of the fields was out of bounds.
    ///
    /// In MySQL, these result in a NULL value
    #[error("{0}")]
    OutOfBounds(String),
}

/// MySQL's TIME type implementation.
/// Internally, this uses an `i64` to store the nano value of the time, which maps
/// 1:1 to a [`chrono::Duration`] which allows for negative durations. All operations
/// internally are performed on a [`chrono::Duration`], with conversion to and from
/// that type as needed.
/// This struct ensures that the inner `i64` is at all times within
/// the MySQL's TIME range, which is `-838:59:59` to `838:59:59`.
/// Following the MySQL's TIME behavior, this struct also allows to be constructed with
/// an invalid [`chrono::Duration`] (for example, one that surpasses or falls below the
/// allowed range), in which case it is "truncated" to the closest range limit.
#[derive(Clone, Copy, Serialize, Deserialize)]
pub struct MySqlTime {
    nanos: i64,
}

impl MySqlTime {
    /// The maximum value that a [`MySqlTime`] can represent: `838:59:59`.
    pub const MAX: MySqlTime = MySqlTime {
        nanos: MAX_MYSQL_TIME_SECONDS * 10i64.pow(9),
    };

    /// The minimum value that a [`MySqlTime`] can represent: `-838:59:59`.
    pub const MIN: MySqlTime = MySqlTime {
        nanos: -MAX_MYSQL_TIME_SECONDS * 10i64.pow(9),
    };

    /// Creates a new [`MySqlTime`] with the given [`chrono::Duration`].
    /// Note that if the [`chrono::Duration`] surpasses the MySQL's TIME max value, then
    /// the [`MySqlTime::MAX`] is used (resp. [`MySqlTime::MIN`] if the
    /// [`chrono::Duration`] falls below the MySQL's TIME min value).
    ///
    /// # Example
    ///
    /// ```rust
    /// use chrono::Duration;
    /// use mysql_time::MySqlTime;
    ///
    /// let duration: Duration = Duration::hours(838); // Within range
    /// let mysql_time: MySqlTime = MySqlTime::new(duration); // 838:00:00
    /// assert!(mysql_time.is_positive());
    /// assert_eq!(838, mysql_time.hour());
    /// assert_eq!(0, mysql_time.minutes());
    /// assert_eq!(0, mysql_time.seconds());
    /// assert_eq!(0, mysql_time.microseconds());
    ///
    /// let exceeded_duration: Duration = Duration::hours(839); // Out of range
    /// let truncated_mysql_time: MySqlTime = MySqlTime::new(exceeded_duration); // 838:59:59
    ///
    /// assert!(truncated_mysql_time.is_positive());
    /// assert_eq!(838, truncated_mysql_time.hour());
    /// assert_eq!(59, truncated_mysql_time.minutes());
    /// assert_eq!(59, truncated_mysql_time.seconds());
    /// assert_eq!(0, truncated_mysql_time.microseconds());
    /// ```
    pub fn new(duration: Duration) -> MySqlTime {
        let secs = duration.num_seconds();
        if secs > MAX_MYSQL_TIME_SECONDS {
            return MySqlTime::MAX;
        }
        if secs < (-MAX_MYSQL_TIME_SECONDS) {
            return MySqlTime::MIN;
        }
        MySqlTime {
            nanos: duration.num_nanoseconds().expect("Limit checked above"),
        }
    }

    /// Creates a new [`MySqlTime`] from the given `hour`, `minutes`, `seconds`
    /// and `microseconds`.
    /// The sign of the [`MySqlTime`] is given by the `hour` parameter.
    /// Truncation of the [`MySqlTime`] applies if the time exceeds/falls below
    /// the allowed range.
    ///
    /// # Example
    ///
    /// ```rust
    /// use mysql_time::MySqlTime;
    ///
    /// let mysql_time_from_hmsus: MySqlTime = MySqlTime::from_hmsus(false, 3, 5, 37, 300000); // -03:05:37.300000
    /// let mysql_time_from_hmsus_invalid_range: MySqlTime = MySqlTime::from_hmsus(false, 900, 5, 37, 300000); // -838:59:59
    /// ```
    pub fn from_hmsus(
        positive: bool,
        hour: u16,
        minutes: u8,
        seconds: u8,
        microseconds: u64,
    ) -> MySqlTime {
        let sum = (hour as i64 * 3600 * MICROSECS_IN_SECOND)
            + (minutes.min(59) as i64 * 60 * MICROSECS_IN_SECOND)
            + (seconds.min(59) as i64 * MICROSECS_IN_SECOND)
            + (microseconds.min(999_999) as i64);
        MySqlTime::new(Duration::microseconds(sum * if positive { 1 } else { -1 }))
    }

    /// Creates a new [`MySqlTime`] from the given `microseconds`.
    /// Truncation of the [`MySqlTime`] applies if the time exceeds/falls below
    /// the allowed range.
    ///
    /// # Example
    ///
    /// ```rust
    /// use mysql_time::MySqlTime;
    ///
    /// let mysql_time_from_ms: MySqlTime = MySqlTime::from_microseconds(3020399000000); // 838:59:59
    /// let mysql_time_from_ms_invalid_range: MySqlTime = MySqlTime::from_microseconds(3020399000001); // 838:59:59
    /// ```
    pub fn from_microseconds(microseconds: i64) -> MySqlTime {
        MySqlTime::new(Duration::microseconds(microseconds))
    }

    /// Attempts to parse a byte array into a new [`MySqlTime`].
    ///
    /// # Example
    ///
    /// ```rust
    /// use mysql_time::{MySqlTime, ConvertError};
    ///
    /// macro_rules! assert_time {
    ///     ($mysql_time:expr, $positive:literal , $h:literal, $m:literal, $s:literal, $us: literal) => {
    ///         assert_eq!($mysql_time.is_positive(), $positive);
    ///         assert_eq!($mysql_time.hour(), $h);
    ///         assert_eq!($mysql_time.minutes(), $m);
    ///         assert_eq!($mysql_time.seconds(), $s);
    ///         assert_eq!($mysql_time.microseconds(), $us);
    ///     };
    /// }
    ///
    /// let result = MySqlTime::from_bytes("not-timestamp".as_bytes());
    /// assert_eq!(result, Err(ConvertError::ParseError));
    ///
    /// let mysql_time: MySqlTime = MySqlTime::from_bytes("1112".as_bytes()).unwrap(); // 00:11:12
    /// assert_time!(mysql_time, true, 0, 11, 12, 0);
    ///
    /// let mysql_time: MySqlTime = MySqlTime::from_bytes("11:12".as_bytes()).unwrap(); // 00:11:12
    /// assert_time!(mysql_time, true, 11, 12, 0, 0);
    ///
    /// assert!(MySqlTime::from_bytes("60".as_bytes()).is_err());
    /// ```
    pub fn from_bytes(bytes: &[u8]) -> Result<MySqlTime, ConvertError> {
        let (positive, hour, minutes, seconds, microseconds) = parse::h_m_s_us(bytes)
            .map(|res| res.1)
            .map_err(|_| ConvertError::ParseError)?;
        if minutes > 59 {
            return Err(ConvertError::OutOfBounds(
                "Minutes can't be greater than 59".to_owned(),
            ));
        }
        if seconds > 59 {
            return Err(ConvertError::OutOfBounds(
                "Seconds can't be greater than 59".to_owned(),
            ));
        }
        if microseconds > 999_999 {
            return Err(ConvertError::OutOfBounds(
                "Microseconds can't be greater than 999999".to_owned(),
            ));
        }
        Ok(MySqlTime::from_hmsus(
            positive,
            hour,
            minutes,
            seconds,
            microseconds as u64,
        ))
    }

    /// Returns the sign of the [`MySqlTime`] as 1 if it's positive, or -1 if it's negative.
    ///
    /// # Example
    ///
    /// ```rust
    /// use mysql_time::MySqlTime;
    ///
    /// let neg_mysql_time = MySqlTime::from_hmsus(false, 2, 23, 58, 829313); // -02:23:58.829313
    /// assert_eq!(neg_mysql_time.is_positive(), false);
    ///
    /// let pos_mysql_time = MySqlTime::from_hmsus(true, 2, 23, 58, 829313); // 02:23:58.829313
    /// assert_eq!(pos_mysql_time.is_positive(), true);
    /// ```
    pub fn is_positive(&self) -> bool {
        self.nanos.is_positive()
    }

    /// Returns the `hour` from this [`MySqlTime`]
    ///
    /// # Example
    ///
    /// ```rust
    /// use mysql_time::MySqlTime;
    ///
    /// let mysql_time = MySqlTime::from_hmsus(false, 2, 23, 58, 829313); // -02:23:58.829313
    /// assert_eq!(mysql_time.hour(), 2);
    /// ```
    pub fn hour(&self) -> u16 {
        self.duration()
            .num_hours()
            .abs()
            .try_into()
            .unwrap_or(u16::MAX)
    }

    /// Returns the `minutes` from this [`MySqlTime`].
    ///
    /// # Example
    ///
    /// ```rust
    /// use mysql_time::MySqlTime;
    ///
    /// let mysql_time = MySqlTime::from_hmsus(false, 2, 23, 58, 829313); // -02:23:58.829313
    /// assert_eq!(mysql_time.minutes(), 23);
    /// ```
    pub fn minutes(&self) -> u8 {
        (self.duration().num_minutes().abs() % 60)
            .try_into()
            .unwrap_or(59)
            .min(59)
    }

    /// Returns the `seconds` from this [`MySqlTime`].
    ///
    /// # Example
    ///
    /// ```rust
    /// use mysql_time::MySqlTime;
    ///
    /// let mysql_time = MySqlTime::from_hmsus(false, 2, 23, 58, 829313); // -02:23:58.829313
    /// assert_eq!(mysql_time.seconds(), 58);
    /// ```
    pub fn seconds(&self) -> u8 {
        (self.duration().num_seconds().abs() % 60)
            .try_into()
            .unwrap_or(59)
            .min(59)
    }

    /// Returns the `microseconds` from this [`MySqlTime`].
    ///
    /// # Example
    ///
    /// ```rust
    /// use mysql_time::MySqlTime;
    ///
    /// let mysql_time = MySqlTime::from_hmsus(false, 2, 23, 58, 829313); // -02:23:58.829313
    /// assert_eq!(mysql_time.microseconds(), 829313);
    /// ```
    pub fn microseconds(&self) -> u32 {
        self.duration()
            .num_microseconds()
            .map(|us| (us.abs() % MICROSECS_IN_SECOND) as u32)
            .unwrap_or(0)
    }

    fn duration(&self) -> Duration {
        Duration::nanoseconds(self.nanos)
    }
}

impl Default for MySqlTime {
    fn default() -> Self {
        MySqlTime::new(Duration::microseconds(0))
    }
}

impl fmt::Display for MySqlTime {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let sign = if self.is_positive() || self.nanos == 0 {
            ""
        } else {
            "-"
        };
        let h = self.hour();
        let m = self.minutes();
        let s = self.seconds();
        let us = self.microseconds();
        if us != 0 {
            write!(f, "{sign}{h:02}:{m:02}:{s:02}.{us:06}")
        } else {
            write!(f, "{sign}{h:02}:{m:02}:{s:02}")
        }
    }
}

impl fmt::Debug for MySqlTime {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(f, "{self}")
    }
}

impl PartialEq for MySqlTime {
    fn eq(&self, other: &Self) -> bool {
        self.is_positive() == other.is_positive()
            && self.hour() == other.hour()
            && self.minutes() == other.minutes()
            && self.seconds() == other.seconds()
            && self.microseconds() == other.microseconds()
    }
}

impl Eq for MySqlTime {}

impl PartialOrd for MySqlTime {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for MySqlTime {
    fn cmp(&self, other: &Self) -> Ordering {
        self.is_positive()
            .cmp(&other.is_positive())
            .then(self.hour().cmp(&other.hour()))
            .then(self.minutes().cmp(&other.minutes()))
            .then(self.seconds().cmp(&other.seconds()))
            .then(self.microseconds().cmp(&other.microseconds()))
    }
}

impl Hash for MySqlTime {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.is_positive().hash(state);
        self.hour().hash(state);
        self.minutes().hash(state);
        self.seconds().hash(state);
        self.microseconds().hash(state);
    }
}

impl From<MySqlTime> for Duration {
    fn from(t: MySqlTime) -> Self {
        Duration::nanoseconds(t.nanos)
    }
}

mod parse {
    use nom::branch::alt;
    use nom::bytes::complete::take_while_m_n;
    use nom::character::complete::{char, digit1};
    use nom::character::{self, is_digit};
    use nom::combinator::{complete, eof, map, map_parser, opt};
    use nom::multi::{fold_many0, many0};
    use nom::sequence::{preceded, terminated, tuple};
    use nom::IResult;

    use super::*;

    fn microseconds_padding(i: &[u8]) -> IResult<&[u8], u32> {
        let num_digits = i.len();
        map(character::complete::u32, move |number| {
            number * (10u32.pow(6 - num_digits as u32))
        })(i)
    }

    fn microseconds(i: &[u8]) -> IResult<&[u8], u32> {
        preceded(
            complete(char('.')),
            map_parser(take_while_m_n(1, 6, is_digit), microseconds_padding),
        )(i)
    }

    fn seconds(i: &[u8]) -> IResult<&[u8], u8> {
        preceded(
            complete(char(':')),
            map_parser(take_while_m_n(1, 2, is_digit), character::complete::u8),
        )(i)
    }

    /// Creates a number from an array of digits.
    /// Each position of the array must be a number from 0-9.
    fn to_number(digits: &[u8]) -> u64 {
        // These u8 are actual numbers, NOT a byte representing a char. Thus, it is
        // safe to perform arithmetic operations on them to yield a number.
        let mut res = 0u64;
        for &n in digits {
            res = res * 10 + n as u64;
        }
        res
    }

    fn one_digit(i: &[u8]) -> IResult<&[u8], u8> {
        map_parser(take_while_m_n(1, 1, is_digit), character::complete::u8)(i)
    }

    fn h_m_s_us_no_colons(i: &[u8]) -> IResult<&[u8], (bool, u16, u8, u8, u32)> {
        map(
            terminated(
                tuple((
                    opt(char('-')),
                    fold_many0(one_digit, Vec::new, |mut acc: Vec<u8>, num: u8| {
                        acc.push(num);
                        acc
                    }),
                    opt(microseconds),
                )),
                eof,
            ),
            |(sign, numbers, microseconds)| {
                let digits = numbers.len();
                let (hour, minutes, seconds) = if digits > 4 {
                    (
                        to_number(&numbers[0..digits - 4]),
                        to_number(&numbers[digits - 4..digits - 2]),
                        to_number(&numbers[digits - 2..digits]),
                    )
                } else if digits > 2 {
                    (
                        0,
                        to_number(&numbers[0..digits - 2]),
                        to_number(&numbers[digits - 2..digits]),
                    )
                } else {
                    (0, 0, to_number(&numbers[0..digits]))
                };
                (
                    sign.is_none(),
                    hour.try_into().unwrap_or(u16::MAX),
                    minutes.try_into().unwrap_or(u8::MAX),
                    seconds.try_into().unwrap_or(u8::MAX),
                    microseconds.unwrap_or(0),
                )
            },
        )(i)
    }

    fn h_m_s_us_colons(i: &[u8]) -> IResult<&[u8], (bool, u16, u8, u8, u32)> {
        map(
            terminated(
                tuple((
                    opt(char('-')),
                    terminated(map_parser(digit1, character::complete::u32), char(':')),
                    map_parser(take_while_m_n(1, 2, is_digit), character::complete::u8),
                    opt(seconds),
                    opt(microseconds),
                )),
                eof,
            ),
            |(sign, hour, minutes, seconds, microseconds)| {
                (
                    sign.is_none(),
                    hour.try_into().unwrap_or(u16::MAX),
                    minutes,
                    seconds.unwrap_or(0),
                    microseconds.unwrap_or(0),
                )
            },
        )(i)
    }

    pub fn h_m_s_us(i: &[u8]) -> IResult<&[u8], (bool, u16, u8, u8, u32)> {
        preceded(
            many0(char(' ')),
            alt((complete(h_m_s_us_colons), complete(h_m_s_us_no_colons))),
        )(i)
    }
}

impl FromStr for MySqlTime {
    type Err = ConvertError;

    /// Attempts to parse a [`&str`] into a [`MySqlTime`], according to the parsing rules
    /// defined by [MySQL's TIME string](https://dev.mysql.com/doc/refman/8.0/en/time.html)
    /// interpretation.
    ///
    /// # Example
    ///
    /// ```rust
    /// use mysql_time::{MySqlTime, ConvertError};
    ///
    /// macro_rules! assert_time {
    ///     ($mysql_time:expr, $positive:literal , $h:literal, $m:literal, $s:literal, $us: literal) => {
    ///         assert_eq!($mysql_time.is_positive(), $positive);
    ///         assert_eq!($mysql_time.hour(), $h);
    ///         assert_eq!($mysql_time.minutes(), $m);
    ///         assert_eq!($mysql_time.seconds(), $s);
    ///         assert_eq!($mysql_time.microseconds(), $us);
    ///     };
    /// }
    ///
    /// let result: Result<MySqlTime, _> = "not-timestamp".parse();
    /// assert_eq!(result, Err(ConvertError::ParseError));
    ///
    /// let mysql_time: MySqlTime = "1112".parse().unwrap(); // 00:11:12
    /// assert_time!(mysql_time, true, 0, 11, 12, 0);
    ///
    /// let mysql_time: MySqlTime = "11:12".parse().unwrap(); // 00:11:12
    /// assert_time!(mysql_time, true, 11, 12, 0, 0);
    ///
    /// assert!("60".parse::<MySqlTime>().is_err());
    /// ```
    fn from_str(string: &str) -> Result<Self, Self::Err> {
        MySqlTime::from_bytes(string.as_bytes())
    }
}

impl From<NaiveTime> for MySqlTime {
    fn from(nt: NaiveTime) -> Self {
        let h = nt.hour() as i64;
        let m = nt.minute() as i64;
        let s = nt.second() as i64;
        let us = (nt.nanosecond() / 1_000) as i64;
        let sum = (h * 60 * 60 * MICROSECS_IN_SECOND)
            + (m * 60 * MICROSECS_IN_SECOND)
            + (s * MICROSECS_IN_SECOND)
            + us;
        MySqlTime::new(Duration::microseconds(sum))
    }
}

impl From<MySqlTime> for NaiveTime {
    fn from(t: MySqlTime) -> Self {
        NaiveTime::from_hms_micro_opt(
            t.hour().into(),
            t.minutes().into(),
            t.seconds().into(),
            t.microseconds(),
        )
        .unwrap()
    }
}

impl From<MySqlTime> for Value {
    fn from(mysql_time: MySqlTime) -> Self {
        let total_hours = mysql_time.hour();
        let days = (total_hours / 24) as u32;
        let hours = (total_hours % 24) as u8;
        Value::Time(
            !mysql_time.is_positive(),
            days,
            hours,
            mysql_time.minutes(),
            mysql_time.seconds(),
            mysql_time.microseconds(),
        )
    }
}

#[derive(Debug)]
pub struct ParseIr<T> {
    output: T,
}

impl From<ParseIr<MySqlTime>> for MySqlTime {
    fn from(value: ParseIr<MySqlTime>) -> Self {
        value.output
    }
}

impl TryFrom<Value> for ParseIr<MySqlTime> {
    type Error = FromValueError;
    fn try_from(v: Value) -> Result<ParseIr<MySqlTime>, FromValueError> {
        match v {
            Value::Time(is_neg, days, hours, minutes, seconds, microseconds) => {
                let hours = (days * 24) as u16 + hours as u16;
                Ok(ParseIr {
                    output: MySqlTime::from_hmsus(
                        !is_neg,
                        hours,
                        minutes,
                        seconds,
                        microseconds as u64,
                    ),
                })
            }
            Value::Bytes(val_bytes) => match MySqlTime::from_bytes(&val_bytes) {
                Ok(time) => Ok(ParseIr { output: time }),
                Err(_) => Err(FromValueError(Value::Bytes(val_bytes))),
            },
            v => Err(FromValueError(v)),
        }
    }
}

impl FromValue for MySqlTime {
    type Intermediate = ParseIr<MySqlTime>;
}

macro_rules! impl_try_from_num {
    ( $x:ty ) => {
        impl TryFrom<$x> for MySqlTime {
            type Error = ConvertError;

            fn try_from(value: $x) -> Result<Self, Self::Error> {
                MySqlTime::from_str(format!("{:.6}", value).as_str())
            }
        }
    };
}

impl_try_from_num!(u8);
impl_try_from_num!(u16);
impl_try_from_num!(u32);
impl_try_from_num!(u64);
impl_try_from_num!(i8);
impl_try_from_num!(i16);
impl_try_from_num!(i32);
impl_try_from_num!(i64);
impl_try_from_num!(f32);
impl_try_from_num!(f64);

impl Sub for MySqlTime {
    type Output = MySqlTime;

    fn sub(self, rhs: Self) -> Self::Output {
        MySqlTime::new(self.duration().sub(rhs.duration()))
    }
}

impl Add for MySqlTime {
    type Output = MySqlTime;

    fn add(self, rhs: Self) -> Self::Output {
        MySqlTime::new(self.duration().add(rhs.duration()))
    }
}

impl Add<NaiveDateTime> for MySqlTime {
    type Output = NaiveDateTime;

    fn add(self, rhs: NaiveDateTime) -> Self::Output {
        rhs.add(self.duration())
    }
}

impl Arbitrary for MySqlTime {
    type Parameters = ();
    type Strategy = proptest::strategy::BoxedStrategy<MySqlTime>;

    fn arbitrary_with((): Self::Parameters) -> Self::Strategy {
        arbitrary_duration().prop_map(Self::new).boxed()
    }
}

#[cfg(test)]
mod tests {
    use std::collections::hash_map::DefaultHasher;

    use readyset_util::arbitrary::{
        arbitrary_duration, arbitrary_naive_date_time, arbitrary_naive_time,
    };
    use test_strategy::proptest;
    use test_utils::tags;

    use super::*;

    macro_rules! assert_valid {
        ($mysql_time:expr,$duration:expr) => {
            if $duration > MAX_MYSQL_TIME_SECONDS {
                assert_eq!($mysql_time.duration().num_seconds(), MAX_MYSQL_TIME_SECONDS);
            } else if $duration < -MAX_MYSQL_TIME_SECONDS {
                assert_eq!(
                    $mysql_time.duration().num_seconds(),
                    -MAX_MYSQL_TIME_SECONDS
                );
            } else {
                assert_eq!($mysql_time.duration().num_seconds(), $duration);
            }
        };
    }

    macro_rules! assert_time {
        ($mysql_time:expr, $positive:literal , $h:literal, $m:literal, $s:literal, $us: literal) => {
            assert_eq!($mysql_time.is_positive(), $positive);
            assert_eq!($mysql_time.hour(), $h);
            assert_eq!($mysql_time.minutes(), $m);
            assert_eq!($mysql_time.seconds(), $s);
            assert_eq!($mysql_time.microseconds(), $us);
        };
    }

    #[tags(no_retry)]
    #[proptest]
    fn new(#[strategy(arbitrary_duration())] duration: Duration) {
        let mysql_time = MySqlTime::new(duration);
        let total_secs = duration.num_seconds();
        assert_valid!(mysql_time, total_secs);
    }

    #[test]
    fn new_exceeded_range() {
        let duration = Duration::seconds(MAX_MYSQL_TIME_SECONDS + 1);
        let mysql_time = MySqlTime::new(duration);
        assert_valid!(mysql_time, MAX_MYSQL_TIME_SECONDS);
    }

    #[test]
    fn new_below_range() {
        let duration = Duration::seconds(-MAX_MYSQL_TIME_SECONDS - 1);
        let mysql_time = MySqlTime::new(duration);
        assert_valid!(mysql_time, -MAX_MYSQL_TIME_SECONDS);
    }

    #[tags(no_retry)]
    #[proptest]
    fn from_microseconds(#[strategy(arbitrary_duration())] duration: Duration) {
        let mysql_time =
            MySqlTime::from_microseconds(duration.num_microseconds().unwrap_or(i64::MAX));
        let total_secs = duration.num_seconds();
        assert_valid!(mysql_time, total_secs);
    }

    #[test]
    fn eq() {
        let duration1 = Duration::nanoseconds(1222333999); // 00:00:01.222333
        let duration2 = Duration::nanoseconds(1222333555); // 00:00:01.222333
        let duration3 = Duration::nanoseconds(1222333000); // 00:00:01.222333
        let mysql_time1 = MySqlTime::new(duration1);
        let mysql_time2 = MySqlTime::new(duration2);
        let mysql_time3 = MySqlTime::new(duration3);
        // Reflexiveness
        assert!(mysql_time1.eq(&mysql_time1)); // Used like this to avoid Clippy from complaining

        // Symmetry
        assert_eq!(mysql_time1, mysql_time2);
        assert_eq!(mysql_time2, mysql_time1);

        // Transitiveness
        assert_eq!(mysql_time2, mysql_time3);
        assert_eq!(mysql_time1, mysql_time3);
    }

    #[test]
    fn hash() {
        let duration1 = Duration::nanoseconds(1222333999); // 00:00:01.222333
        let duration2 = Duration::nanoseconds(1222333555); // 00:00:01.222333
        let mysql_time1 = MySqlTime::new(duration1);
        let mysql_time2 = MySqlTime::new(duration2);

        let mut hasher1 = DefaultHasher::new();
        mysql_time1.hash(&mut hasher1);

        let mut hasher2 = DefaultHasher::new();
        mysql_time2.hash(&mut hasher2);

        assert_eq!(hasher1.finish(), hasher2.finish());
    }

    #[test]
    fn ord() {
        let duration1 = Duration::nanoseconds(1222333000); // 00:00:01.222334
        let duration2 = Duration::nanoseconds(1222334000); // 00:00:01.222335
        let duration3 = Duration::nanoseconds(1222335000); // 00:00:01.222336
        let mysql_time1 = MySqlTime::new(duration1);
        let mysql_time2 = MySqlTime::new(duration2);
        let mysql_time3 = MySqlTime::new(duration3);

        assert!(mysql_time1 < mysql_time2);
        assert!(mysql_time1 < mysql_time2);

        assert!(mysql_time2 < mysql_time3);
        assert!(mysql_time1 < mysql_time3)
    }

    #[tags(no_retry)]
    #[proptest]
    fn sub(
        #[strategy(arbitrary_duration())] duration1: Duration,
        #[strategy(arbitrary_duration())] duration2: Duration,
    ) {
        let mysql_time1 = MySqlTime::new(duration1);
        let mysql_time2 = MySqlTime::new(duration2);
        let total_secs = (duration1 - duration2).num_seconds();
        assert_valid!(mysql_time1 - mysql_time2, total_secs);
    }

    #[tags(no_retry)]
    #[proptest]
    fn add(
        #[strategy(arbitrary_duration())] duration1: Duration,
        #[strategy(arbitrary_duration())] duration2: Duration,
    ) {
        let mysql_time1 = MySqlTime::new(duration1);
        let mysql_time2 = MySqlTime::new(duration2);
        let total_secs = (duration1 + duration2).num_seconds();
        assert_valid!(mysql_time1 + mysql_time2, total_secs);
    }

    #[tags(no_retry)]
    #[proptest]
    fn add_naive_date_time(
        #[strategy(arbitrary_duration())] duration: Duration,
        #[strategy(arbitrary_naive_date_time())] ndt: NaiveDateTime,
    ) {
        let mysql_time = MySqlTime::new(duration);
        let new_datetime = ndt.add(duration);
        assert_eq!(mysql_time + ndt, new_datetime);
    }

    mod from_str {
        use super::*;

        #[tags(no_retry)]
        #[proptest]
        fn from_str(#[strategy(arbitrary_duration())] duration: Duration) {
            let duration_str = duration_to_str(duration);
            let mysql_time = MySqlTime::from_str(duration_str.as_str()).unwrap();
            let total_secs = duration.num_seconds();
            assert_valid!(mysql_time, total_secs);
        }

        #[tags(no_retry)]
        #[proptest]
        fn from_str_display(#[strategy(arbitrary_duration())] duration: Duration) {
            let mysql_time = MySqlTime::new(duration);
            let parsed_time = MySqlTime::from_str(mysql_time.to_string().as_str()).unwrap();
            assert_eq!(mysql_time, parsed_time);
        }

        #[test]
        fn from_str_without_colons() {
            let mysql_time = MySqlTime::from_str("1234559").unwrap();
            assert_time!(mysql_time, true, 123, 45, 59, 0);

            let mysql_time = MySqlTime::from_str("0000001234559").unwrap();
            assert_time!(mysql_time, true, 123, 45, 59, 0);

            let mysql_time = MySqlTime::from_str("1234559.6").unwrap();
            assert_time!(mysql_time, true, 123, 45, 59, 600_000);

            let mysql_time = MySqlTime::from_str("0000001234559.6").unwrap();
            assert_time!(mysql_time, true, 123, 45, 59, 600_000);

            let mysql_time = MySqlTime::from_str("-1234559").unwrap();
            assert_time!(mysql_time, false, 123, 45, 59, 0);

            let mysql_time = MySqlTime::from_str("-0000001234559").unwrap();
            assert_time!(mysql_time, false, 123, 45, 59, 0);

            let mysql_time = MySqlTime::from_str("-1234559.6").unwrap();
            assert_time!(mysql_time, false, 123, 45, 59, 600_000);

            let mysql_time = MySqlTime::from_str("-0000001234559.6").unwrap();
            assert_time!(mysql_time, false, 123, 45, 59, 600_000);

            let mysql_time = MySqlTime::from_str("234559").unwrap();
            assert_time!(mysql_time, true, 23, 45, 59, 0);

            let mysql_time = MySqlTime::from_str("000000234559").unwrap();
            assert_time!(mysql_time, true, 23, 45, 59, 0);

            let mysql_time = MySqlTime::from_str("234559.65").unwrap();
            assert_time!(mysql_time, true, 23, 45, 59, 650_000);

            let mysql_time = MySqlTime::from_str("000000234559.65").unwrap();
            assert_time!(mysql_time, true, 23, 45, 59, 650_000);

            let mysql_time = MySqlTime::from_str("-234559").unwrap();
            assert_time!(mysql_time, false, 23, 45, 59, 0);

            let mysql_time = MySqlTime::from_str("-000000234559").unwrap();
            assert_time!(mysql_time, false, 23, 45, 59, 0);

            let mysql_time = MySqlTime::from_str("-234559.65").unwrap();
            assert_time!(mysql_time, false, 23, 45, 59, 650_000);

            let mysql_time = MySqlTime::from_str("34559").unwrap();
            assert_time!(mysql_time, true, 3, 45, 59, 0);

            let mysql_time = MySqlTime::from_str("00000034559").unwrap();
            assert_time!(mysql_time, true, 3, 45, 59, 0);

            let mysql_time = MySqlTime::from_str("34559.654").unwrap();
            assert_time!(mysql_time, true, 3, 45, 59, 654_000);

            let mysql_time = MySqlTime::from_str("00000034559.654").unwrap();
            assert_time!(mysql_time, true, 3, 45, 59, 654_000);

            let mysql_time = MySqlTime::from_str("-34559").unwrap();
            assert_time!(mysql_time, false, 3, 45, 59, 0);

            let mysql_time = MySqlTime::from_str("-00000034559").unwrap();
            assert_time!(mysql_time, false, 3, 45, 59, 0);

            let mysql_time = MySqlTime::from_str("-34559.654").unwrap();
            assert_time!(mysql_time, false, 3, 45, 59, 654_000);

            let mysql_time = MySqlTime::from_str("-00000034559.654").unwrap();
            assert_time!(mysql_time, false, 3, 45, 59, 654_000);

            let mysql_time = MySqlTime::from_str("4559").unwrap();
            assert_time!(mysql_time, true, 0, 45, 59, 0);

            let mysql_time = MySqlTime::from_str("0000004559").unwrap();
            assert_time!(mysql_time, true, 0, 45, 59, 0);

            let mysql_time = MySqlTime::from_str("4559.6543").unwrap();
            assert_time!(mysql_time, true, 0, 45, 59, 654_300);

            let mysql_time = MySqlTime::from_str("0000004559.6543").unwrap();
            assert_time!(mysql_time, true, 0, 45, 59, 654_300);

            let mysql_time = MySqlTime::from_str("-4559").unwrap();
            assert_time!(mysql_time, false, 0, 45, 59, 0);

            let mysql_time = MySqlTime::from_str("-0000004559").unwrap();
            assert_time!(mysql_time, false, 0, 45, 59, 0);

            let mysql_time = MySqlTime::from_str("-4559.6543").unwrap();
            assert_time!(mysql_time, false, 0, 45, 59, 654_300);

            let mysql_time = MySqlTime::from_str("-0000004559.6543").unwrap();
            assert_time!(mysql_time, false, 0, 45, 59, 654_300);

            let mysql_time = MySqlTime::from_str("559").unwrap();
            assert_time!(mysql_time, true, 0, 5, 59, 0);

            let mysql_time = MySqlTime::from_str("000000559").unwrap();
            assert_time!(mysql_time, true, 0, 5, 59, 0);

            let mysql_time = MySqlTime::from_str("559.65432").unwrap();
            assert_time!(mysql_time, true, 0, 5, 59, 654_320);

            let mysql_time = MySqlTime::from_str("000000559.65432").unwrap();
            assert_time!(mysql_time, true, 0, 5, 59, 654_320);

            let mysql_time = MySqlTime::from_str("-559").unwrap();
            assert_time!(mysql_time, false, 0, 5, 59, 0);

            let mysql_time = MySqlTime::from_str("-000000559").unwrap();
            assert_time!(mysql_time, false, 0, 5, 59, 0);

            let mysql_time = MySqlTime::from_str("-559.65432").unwrap();
            assert_time!(mysql_time, false, 0, 5, 59, 654_320);

            let mysql_time = MySqlTime::from_str("-000000559.65432").unwrap();
            assert_time!(mysql_time, false, 0, 5, 59, 654_320);

            let mysql_time = MySqlTime::from_str("9").unwrap();
            assert_time!(mysql_time, true, 0, 0, 9, 0);

            let mysql_time = MySqlTime::from_str("0000009").unwrap();
            assert_time!(mysql_time, true, 0, 0, 9, 0);

            let mysql_time = MySqlTime::from_str("9.654321").unwrap();
            assert_time!(mysql_time, true, 0, 0, 9, 654_321);

            let mysql_time = MySqlTime::from_str("0000009.654321").unwrap();
            assert_time!(mysql_time, true, 0, 0, 9, 654_321);

            let mysql_time = MySqlTime::from_str("-9").unwrap();
            assert_time!(mysql_time, false, 0, 0, 9, 0);

            let mysql_time = MySqlTime::from_str("-0000009").unwrap();
            assert_time!(mysql_time, false, 0, 0, 9, 0);

            let mysql_time = MySqlTime::from_str("-9.654321").unwrap();
            assert_time!(mysql_time, false, 0, 0, 9, 654_321);

            let mysql_time = MySqlTime::from_str("-0000009.654321").unwrap();
            assert_time!(mysql_time, false, 0, 0, 9, 654_321);

            let mysql_time = MySqlTime::from_str("67");
            mysql_time.unwrap_err();

            let mysql_time = MySqlTime::from_str("00000067");
            mysql_time.unwrap_err();

            let mysql_time = MySqlTime::from_str("67.654321");
            mysql_time.unwrap_err();

            let mysql_time = MySqlTime::from_str("00000067.654321");
            mysql_time.unwrap_err();

            let mysql_time = MySqlTime::from_str("-67");
            mysql_time.unwrap_err();

            let mysql_time = MySqlTime::from_str("-00000067");
            mysql_time.unwrap_err();

            let mysql_time = MySqlTime::from_str("-67.654321");
            mysql_time.unwrap_err();

            let mysql_time = MySqlTime::from_str("-00000067.654321");
            mysql_time.unwrap_err();
        }

        #[test]
        fn from_str_with_colons() {
            let mysql_time = MySqlTime::from_str("123:45:59").unwrap();
            assert_time!(mysql_time, true, 123, 45, 59, 0);

            let mysql_time = MySqlTime::from_str("000000123:45:59").unwrap();
            assert_time!(mysql_time, true, 123, 45, 59, 0);

            let mysql_time = MySqlTime::from_str("123:45:59.6").unwrap();
            assert_time!(mysql_time, true, 123, 45, 59, 600_000);

            let mysql_time = MySqlTime::from_str("000000123:45:59.6").unwrap();
            assert_time!(mysql_time, true, 123, 45, 59, 600_000);

            let mysql_time = MySqlTime::from_str("-123:45:59").unwrap();
            assert_time!(mysql_time, false, 123, 45, 59, 0);

            let mysql_time = MySqlTime::from_str("-000000123:45:59").unwrap();
            assert_time!(mysql_time, false, 123, 45, 59, 0);

            let mysql_time = MySqlTime::from_str("-123:45:59.6").unwrap();
            assert_time!(mysql_time, false, 123, 45, 59, 600_000);

            let mysql_time = MySqlTime::from_str("-000000123:45:59.6").unwrap();
            assert_time!(mysql_time, false, 123, 45, 59, 600_000);

            let mysql_time = MySqlTime::from_str("23:45:59").unwrap();
            assert_time!(mysql_time, true, 23, 45, 59, 0);

            let mysql_time = MySqlTime::from_str("00000023:45:59").unwrap();
            assert_time!(mysql_time, true, 23, 45, 59, 0);

            let mysql_time = MySqlTime::from_str("23:45:59.65").unwrap();
            assert_time!(mysql_time, true, 23, 45, 59, 650_000);

            let mysql_time = MySqlTime::from_str("00000023:45:59.65").unwrap();
            assert_time!(mysql_time, true, 23, 45, 59, 650_000);

            let mysql_time = MySqlTime::from_str("-23:45:59").unwrap();
            assert_time!(mysql_time, false, 23, 45, 59, 0);

            let mysql_time = MySqlTime::from_str("-000000023:45:59").unwrap();
            assert_time!(mysql_time, false, 23, 45, 59, 0);

            let mysql_time = MySqlTime::from_str("-23:45:59.65").unwrap();
            assert_time!(mysql_time, false, 23, 45, 59, 650_000);

            let mysql_time = MySqlTime::from_str("-00000023:45:59.65").unwrap();
            assert_time!(mysql_time, false, 23, 45, 59, 650_000);

            let mysql_time = MySqlTime::from_str("3:45:59").unwrap();
            assert_time!(mysql_time, true, 3, 45, 59, 0);

            let mysql_time = MySqlTime::from_str("0000003:45:59").unwrap();
            assert_time!(mysql_time, true, 3, 45, 59, 0);

            let mysql_time = MySqlTime::from_str("3:45:59.654").unwrap();
            assert_time!(mysql_time, true, 3, 45, 59, 654_000);

            let mysql_time = MySqlTime::from_str("0000003:45:59.654").unwrap();
            assert_time!(mysql_time, true, 3, 45, 59, 654_000);

            let mysql_time = MySqlTime::from_str("-3:45:59").unwrap();
            assert_time!(mysql_time, false, 3, 45, 59, 0);

            let mysql_time = MySqlTime::from_str("-0000003:45:59").unwrap();
            assert_time!(mysql_time, false, 3, 45, 59, 0);

            let mysql_time = MySqlTime::from_str("-3:45:59.654").unwrap();
            assert_time!(mysql_time, false, 3, 45, 59, 654_000);

            let mysql_time = MySqlTime::from_str("-0000003:45:59.654").unwrap();
            assert_time!(mysql_time, false, 3, 45, 59, 654_000);

            let mysql_time = MySqlTime::from_str("45:59").unwrap();
            assert_time!(mysql_time, true, 45, 59, 0, 0);

            let mysql_time = MySqlTime::from_str("00000045:59").unwrap();
            assert_time!(mysql_time, true, 45, 59, 0, 0);

            let mysql_time = MySqlTime::from_str("45:59.6543").unwrap();
            assert_time!(mysql_time, true, 45, 59, 0, 654_300);

            let mysql_time = MySqlTime::from_str("00000045:59.6543").unwrap();
            assert_time!(mysql_time, true, 45, 59, 0, 654_300);

            let mysql_time = MySqlTime::from_str("-45:59").unwrap();
            assert_time!(mysql_time, false, 45, 59, 0, 0);

            let mysql_time = MySqlTime::from_str("-00000045:59").unwrap();
            assert_time!(mysql_time, false, 45, 59, 0, 0);

            let mysql_time = MySqlTime::from_str("-45:59.6543").unwrap();
            assert_time!(mysql_time, false, 45, 59, 0, 654_300);

            let mysql_time = MySqlTime::from_str("-00000045:59.6543").unwrap();
            assert_time!(mysql_time, false, 45, 59, 0, 654_300);

            let mysql_time = MySqlTime::from_str("5:59").unwrap();
            assert_time!(mysql_time, true, 5, 59, 0, 0);

            let mysql_time = MySqlTime::from_str("0000005:59").unwrap();
            assert_time!(mysql_time, true, 5, 59, 0, 0);

            let mysql_time = MySqlTime::from_str("5:59.65432").unwrap();
            assert_time!(mysql_time, true, 5, 59, 0, 654_320);

            let mysql_time = MySqlTime::from_str("0000005:59.65432").unwrap();
            assert_time!(mysql_time, true, 5, 59, 0, 654_320);

            let mysql_time = MySqlTime::from_str("-5:59").unwrap();
            assert_time!(mysql_time, false, 5, 59, 0, 0);

            let mysql_time = MySqlTime::from_str("-0000005:59").unwrap();
            assert_time!(mysql_time, false, 5, 59, 0, 0);

            let mysql_time = MySqlTime::from_str("-5:59.65432").unwrap();
            assert_time!(mysql_time, false, 5, 59, 0, 654_320);

            let mysql_time = MySqlTime::from_str("-0000005:59.65432").unwrap();
            assert_time!(mysql_time, false, 5, 59, 0, 654_320);

            let mysql_time = MySqlTime::from_str("5:9").unwrap();
            assert_time!(mysql_time, true, 5, 9, 0, 0);

            let mysql_time = MySqlTime::from_str("0000005:9").unwrap();
            assert_time!(mysql_time, true, 5, 9, 0, 0);

            let mysql_time = MySqlTime::from_str("5:9.654321").unwrap();
            assert_time!(mysql_time, true, 5, 9, 0, 654_321);

            let mysql_time = MySqlTime::from_str("5:9.654321").unwrap();
            assert_time!(mysql_time, true, 5, 9, 0, 654_321);

            let mysql_time = MySqlTime::from_str("0000005:9.654321").unwrap();
            assert_time!(mysql_time, true, 5, 9, 0, 654_321);

            let mysql_time = MySqlTime::from_str("-5:9").unwrap();
            assert_time!(mysql_time, false, 5, 9, 0, 0);

            let mysql_time = MySqlTime::from_str("-0000005:9").unwrap();
            assert_time!(mysql_time, false, 5, 9, 0, 0);

            let mysql_time = MySqlTime::from_str("-5:9.654321").unwrap();
            assert_time!(mysql_time, false, 5, 9, 0, 654_321);

            let mysql_time = MySqlTime::from_str("-0000005:9.654321").unwrap();
            assert_time!(mysql_time, false, 5, 9, 0, 654_321);
        }

        #[test]
        fn from_str_non_timestamp() {
            let result = MySqlTime::from_str("banana");
            assert_eq!(result, Err(ConvertError::ParseError));
        }
    }

    mod try_from {
        use super::*;

        #[test]
        fn try_from_u8() {
            let mysql_time = MySqlTime::try_from(59u8).unwrap();
            assert_time!(mysql_time, true, 0, 0, 59, 0);
        }

        #[test]
        fn try_from_u16() {
            let mysql_time = MySqlTime::try_from(4559u16).unwrap();
            assert_time!(mysql_time, true, 0, 45, 59, 0);
        }

        #[test]
        fn try_from_u32() {
            let mysql_time = MySqlTime::try_from(1234559u32).unwrap();
            assert_time!(mysql_time, true, 123, 45, 59, 0);
        }

        #[test]
        fn try_from_u64() {
            let mysql_time = MySqlTime::try_from(1234559u64).unwrap();
            assert_time!(mysql_time, true, 123, 45, 59, 0);
        }

        #[test]
        fn try_from_i8() {
            let mysql_time = MySqlTime::try_from(59i8).unwrap();
            assert_time!(mysql_time, true, 0, 0, 59, 0);

            let mysql_time = MySqlTime::try_from(-59i8).unwrap();
            assert_time!(mysql_time, false, 0, 0, 59, 0);
        }

        #[test]
        fn try_from_i16() {
            let mysql_time = MySqlTime::try_from(4559i16).unwrap();
            assert_time!(mysql_time, true, 0, 45, 59, 0);

            let mysql_time = MySqlTime::try_from(-4559i16).unwrap();
            assert_time!(mysql_time, false, 0, 45, 59, 0);
        }

        #[test]
        fn try_from_i32() {
            let mysql_time = MySqlTime::try_from(1234559i32).unwrap();
            assert_time!(mysql_time, true, 123, 45, 59, 0);

            let mysql_time = MySqlTime::try_from(-1234559i32).unwrap();
            assert_time!(mysql_time, false, 123, 45, 59, 0);
        }

        #[test]
        fn try_from_i64() {
            let mysql_time = MySqlTime::try_from(1234559i64).unwrap();
            assert_time!(mysql_time, true, 123, 45, 59, 0);

            let mysql_time = MySqlTime::try_from(-1234559i64).unwrap();
            assert_time!(mysql_time, false, 123, 45, 59, 0);
        }

        #[test]
        fn try_from_f32() {
            let mysql_time = MySqlTime::try_from(1234559.5f32).unwrap();
            assert_time!(mysql_time, true, 123, 45, 59, 500_000);

            let mysql_time = MySqlTime::try_from(-1234559.5f32).unwrap();
            assert_time!(mysql_time, false, 123, 45, 59, 500_000);
        }

        #[test]
        fn try_from_f64() {
            let mysql_time = MySqlTime::try_from(1234559.654321f64).unwrap();
            assert_time!(mysql_time, true, 123, 45, 59, 654_321);

            let mysql_time = MySqlTime::try_from(-1234559.654321f64).unwrap();
            assert_time!(mysql_time, false, 123, 45, 59, 654_321);
        }
    }

    fn duration_to_str(duration: Duration) -> String {
        let total_secs = duration.num_seconds();
        let h = total_secs.abs() / 3600;
        let m = total_secs.abs() % 3600 / 60;
        let s = total_secs.abs() % 60;
        let us = duration
            .num_microseconds()
            .map(|us| (us.abs() % MICROSECS_IN_SECOND) as u32)
            .unwrap_or(0);
        let sign = if total_secs.is_negative() { "-" } else { "" };
        if us != 0 {
            format!("{sign}{h:02}:{m:02}:{s:02}.{us:06}")
        } else {
            format!("{sign}{h:02}:{m:02}:{s:02}")
        }
    }

    #[tags(no_retry)]
    #[proptest]
    fn naive_time_from_into_round_trip(#[strategy(arbitrary_naive_time())] naive_time: NaiveTime) {
        let mt = MySqlTime::from(naive_time);
        let round_trip = NaiveTime::from(mt);
        assert_eq!(naive_time, round_trip);
    }
}
