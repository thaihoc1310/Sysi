//! TASKS: what a quick-add line means, how the list is grouped and labelled,
//! when reminders are due, and the desktop notifications that carry them.
//!
//! Everything but the notifications works on the local wall clock as a day
//! number and a minute of that day, so it can be tested without a clock.

use std::cell::RefCell;
use std::collections::HashMap;

use gtk::gio;
use gtk::glib;
use gtk::glib::ToVariant;

use crate::state::{Task, TaskStatus};
use crate::usage::{civil_from_days, days_from_civil, weekday_from_days};

/// The local wall clock: days since 1970-01-01 and minutes since midnight.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Now {
    pub day: i64,
    pub minute: u16,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Due {
    pub day: i64,
    pub minute: Option<u16>,
}

/// What a quick-add line asks for.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Draft {
    pub title: String,
    pub list: Option<String>,
    pub priority: u8,
    pub due: Option<Due>,
    /// Asked for as "in so long": when to ring, in seconds from now, to the
    /// second rather than the minute the due date is kept in.
    pub ring_in: Option<i64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Group {
    Overdue,
    Today,
    Upcoming,
    NoDate,
    Done,
}

impl Group {
    pub fn label(self) -> &'static str {
        match self {
            Group::Overdue => "QUÁ HẠN",
            Group::Today => "HÔM NAY",
            Group::Upcoming => "SẮP TỚI",
            Group::NoDate => "KHÔNG HẠN",
            Group::Done => "ĐÃ XONG",
        }
    }
}

pub fn now_local() -> Now {
    let Ok(now) = glib::DateTime::now_local() else {
        return Now { day: 0, minute: 0 };
    };
    let (year, month, day) = now.ymd();
    Now {
        day: days_from_civil(i64::from(year), i64::from(month), i64::from(day)).unwrap_or(0),
        minute: (now.hour() * 60 + now.minute()) as u16,
    }
}

/// Milliseconds since the epoch of a local day and minute.
pub fn local_ms(day: i64, minute: u16) -> Option<i64> {
    let (year, month, date) = civil_from_days(day);
    let time = glib::DateTime::from_local(
        year as i32,
        month as i32,
        date as i32,
        i32::from(minute / 60),
        i32::from(minute % 60),
        0.0,
    )
    .ok()?;
    Some(time.to_unix() * 1000)
}

/// When a task with this due date reminds: at its time, if it has one.
pub fn remind_at(due: Option<Due>) -> Option<i64> {
    let due = due?;
    local_ms(due.day, due.minute?)
}

// ---------------------------------------------------------------------------
// Quick add

#[derive(Clone, Copy, PartialEq, Eq)]
enum Daypart {
    Morning,
    Noon,
    Evening,
}

#[derive(Clone, Copy)]
enum DateSpec {
    Day(i64),
    /// Monday is 0.
    Weekday {
        weekday: i64,
        next_week: bool,
    },
}

struct Token {
    text: String,
    /// Lowercased, trailing punctuation off: what is matched.
    key: String,
    /// Inside quotes, or a link: never read as a date or a list.
    protected: bool,
    used: bool,
}

fn tokenize(text: &str) -> Vec<Token> {
    let text = glib::normalize(text, glib::NormalizeMode::DefaultCompose);
    let mut tokens = Vec::new();
    let mut quoted = false;
    let mut word = String::new();
    let mut push = |word: &mut String, quoted: bool| {
        if word.is_empty() {
            return;
        }
        let key = word
            .to_lowercase()
            .trim_end_matches([',', '.', ';'])
            .to_owned();
        tokens.push(Token {
            protected: quoted || word.contains("://"),
            text: std::mem::take(word),
            key,
            used: false,
        });
    };
    for ch in text.chars() {
        if matches!(ch, '"' | '\u{201c}' | '\u{201d}') {
            push(&mut word, quoted);
            quoted = !quoted;
        } else if ch.is_whitespace() {
            push(&mut word, quoted);
        } else {
            word.push(ch);
        }
    }
    push(&mut word, quoted);
    tokens
}

struct Reader<'a> {
    tokens: &'a [Token],
}

impl Reader<'_> {
    /// The key at `at`, if that token is still free to be read.
    fn key(&self, at: usize) -> Option<&str> {
        self.tokens
            .get(at)
            .filter(|token| !token.protected && !token.used)
            .map(|token| token.key.as_str())
    }

    fn is(&self, at: usize, words: &[&str]) -> bool {
        self.key(at).is_some_and(|key| words.contains(&key))
    }
}

const DAYPART_MORNING: &[&str] = &["sáng", "sang"];
const DAYPART_NOON: &[&str] = &["trưa", "trua"];
const DAYPART_EVENING: &[&str] = &["chiều", "chieu", "tối", "toi"];

fn daypart_at(reader: &Reader, at: usize) -> Option<Daypart> {
    if reader.is(at, DAYPART_MORNING) {
        Some(Daypart::Morning)
    } else if reader.is(at, DAYPART_NOON) {
        Some(Daypart::Noon)
    } else if reader.is(at, DAYPART_EVENING) {
        Some(Daypart::Evening)
    } else {
        None
    }
}

/// A date at `at`, how many tokens it takes, and the part of day it names.
fn date_at(reader: &Reader, at: usize, now: Now) -> Option<(DateSpec, usize, Option<Daypart>)> {
    let key = reader.key(at)?;
    let today = DateSpec::Day(now.day);
    let tomorrow = DateSpec::Day(now.day + 1);
    // "hôm nay"; "sáng / chiều / tối nay"; and the same with "mai".
    if reader.is(at, &["hôm", "hom"]) && reader.is(at + 1, &["nay"]) {
        return Some((today, 2, None));
    }
    if let Some(part) = daypart_at(reader, at) {
        if reader.is(at + 1, &["nay"]) {
            return Some((today, 2, Some(part)));
        }
        if reader.is(at + 1, &["mai"]) {
            return Some((tomorrow, 2, Some(part)));
        }
    }
    match key {
        "today" => return Some((today, 1, None)),
        "tonight" => return Some((today, 1, Some(Daypart::Evening))),
        "tomorrow" => return Some((tomorrow, 1, None)),
        "mốt" => return Some((DateSpec::Day(now.day + 2), 1, None)),
        _ => {}
    }
    if reader.is(at, &["ngày", "ngay"]) {
        if reader.is(at + 1, &["mai"]) {
            return Some((tomorrow, 2, None));
        }
        if reader.is(at + 1, &["kia"]) {
            return Some((DateSpec::Day(now.day + 2), 2, None));
        }
    }
    // "mai" alone, unless a capital follows: "gặp Mai Anh".
    if key == "mai" {
        let name_follows = reader.tokens.get(at + 1).is_some_and(|next| {
            !next.protected && next.text.chars().next().is_some_and(char::is_uppercase)
        });
        if !name_follows {
            return Some((tomorrow, 1, None));
        }
    }
    if let Some((day, taken)) = period_at(reader, at, now) {
        return Some((DateSpec::Day(day), taken, None));
    }
    if let Some((weekday, taken)) = weekday_at(reader, at) {
        let after = at + taken;
        let next_week = (reader.is(after, &["tuần", "tuan"]) && reader.is(after + 1, &["sau"]))
            || (reader.is(after, &["next"]) && reader.is(after + 1, &["week"]));
        let spec = DateSpec::Weekday { weekday, next_week };
        return Some((spec, taken + if next_week { 2 } else { 0 }, None));
    }
    if (reader.is(at, &["tuần", "tuan"]) && reader.is(at + 1, &["sau"]))
        || (reader.is(at, &["next"]) && reader.is(at + 1, &["week"]))
    {
        let spec = DateSpec::Weekday {
            weekday: 0,
            next_week: true,
        };
        return Some((spec, 2, None));
    }
    calendar_date(key, now).map(|day| (DateSpec::Day(day), 1, None))
}

const WEEK: &[&str] = &["tuần", "tuan"];
const MONTH: &[&str] = &["tháng", "thang"];
const THIS: &[&str] = &["này", "nay"];
const NEXT: &[&str] = &["sau"];
const END: &[&str] = &["cuối", "cuoi"];

/// A stretch of time named as a deadline: "tuần này" is due by Friday,
/// "cuối tuần" on Saturday, "tháng này" on the month's last day, "tháng
/// sau" on the first of the next. Returns the day and the tokens taken.
fn period_at(reader: &Reader, at: usize, now: Now) -> Option<(i64, usize)> {
    let weekday = weekday_from_days(now.day);
    let monday = now.day - weekday;
    // The working week ends on Friday; once that is gone, on Sunday.
    let friday = if weekday <= 4 { monday + 4 } else { monday + 6 };
    let saturday = if weekday <= 5 { monday + 5 } else { now.day };
    let (year, month, _) = civil_from_days(now.day);
    let first_of = |months_on: i64| {
        let index = year * 12 + (month - 1) + months_on;
        days_from_civil(index.div_euclid(12), index.rem_euclid(12) + 1, 1)
    };
    let is = |offset: usize, words: &[&str]| reader.is(at + offset, words);
    // "<head> tuần/tháng [này|sau]": how far the trailing word takes it.
    let unit = |offset: usize| -> (bool, usize) {
        if is(offset, NEXT) {
            (true, 1)
        } else if is(offset, THIS) {
            (false, 1)
        } else {
            (false, 0)
        }
    };
    if is(0, WEEK) && is(1, THIS) {
        return Some((friday, 2));
    }
    if is(0, MONTH) && is(1, THIS) {
        return Some((first_of(1)? - 1, 2));
    }
    if is(0, MONTH) && is(1, NEXT) {
        return Some((first_of(1)?, 2));
    }
    if is(0, &["trong"]) || is(0, END) {
        let end = is(0, END);
        if is(1, WEEK) {
            let (next, extra) = unit(2);
            let day = match (end, next) {
                (false, false) => friday,
                (false, true) => monday + 7 + 4,
                (true, false) => saturday,
                (true, true) => monday + 7 + 5,
            };
            return Some((day, 2 + extra));
        }
        if is(1, MONTH) {
            let (next, extra) = unit(2);
            return Some((first_of(if next { 2 } else { 1 })? - 1, 2 + extra));
        }
        if end && is(1, &["năm", "nam"]) {
            let extra = usize::from(is(2, THIS));
            return Some((days_from_civil(year, 12, 31)?, 2 + extra));
        }
    }
    if is(0, &["đầu", "dau"]) && is(1, MONTH) && is(2, NEXT) {
        return Some((first_of(1)?, 3));
    }
    let english = |first: &str, second: &str| {
        reader.key(at) == Some(first) && reader.key(at + 1) == Some(second)
    };
    if english("this", "week") {
        return Some((friday, 2));
    }
    if english("this", "weekend") {
        return Some((saturday, 2));
    }
    if reader.key(at) == Some("weekend") {
        return Some((saturday, 1));
    }
    if english("this", "month") {
        return Some((first_of(1)? - 1, 2));
    }
    if english("next", "month") {
        return Some((first_of(1)?, 2));
    }
    if reader.key(at) == Some("end")
        && reader.key(at + 1) == Some("of")
        && reader.key(at + 2) == Some("month")
    {
        return Some((first_of(1)? - 1, 3));
    }
    None
}

/// A weekday at `at` (Monday is 0) and how many tokens it takes.
fn weekday_at(reader: &Reader, at: usize) -> Option<(i64, usize)> {
    let key = reader.key(at)?;
    // "thứ" only with its accent: "thu 2 triệu" is money coming in.
    if key == "thứ" {
        let number = reader.key(at + 1)?;
        let weekday = match number {
            "2" | "hai" => 0,
            "3" | "ba" => 1,
            "4" | "tư" => 2,
            "5" | "năm" => 3,
            "6" | "sáu" => 4,
            "7" | "bảy" => 5,
            _ => return None,
        };
        return Some((weekday, 2));
    }
    if reader.is(at, &["chủ", "chu"]) && reader.is(at + 1, &["nhật", "nhat"]) {
        return Some((6, 2));
    }
    let weekday = match key {
        "t2" => 0,
        "t3" => 1,
        "t4" => 2,
        "t5" => 3,
        "t6" => 4,
        "t7" => 5,
        "monday" => 0,
        "tuesday" => 1,
        "wednesday" => 2,
        "thursday" => 3,
        "friday" => 4,
        "saturday" => 5,
        "sunday" => 6,
        _ => return None,
    };
    Some((weekday, 1))
}

/// "15/11" or "15/11/2026". Without a year, a date already gone this year is
/// next year's.
fn calendar_date(key: &str, now: Now) -> Option<i64> {
    let mut parts = key.split('/');
    let date: i64 = parts.next()?.parse().ok()?;
    let month: i64 = parts.next()?.parse().ok()?;
    let year = parts.next();
    if parts.next().is_some() {
        return None;
    }
    let valid = |year: i64| {
        let day = days_from_civil(year, month, date)?;
        // days_from_civil takes 31/2 and rolls it into March.
        (civil_from_days(day) == (year, month, date)).then_some(day)
    };
    match year {
        Some(year) => {
            let year: i64 = year.parse().ok().filter(|year| *year >= 1970)?;
            valid(year)
        }
        None => {
            let (this_year, _, _) = civil_from_days(now.day);
            let day = valid(this_year)?;
            if day < now.day {
                valid(this_year + 1)
            } else {
                Some(day)
            }
        }
    }
}

/// A clock time at `at`: hour, minute, tokens taken, and the part of day
/// that came with it.
fn time_at(reader: &Reader, at: usize) -> Option<(u16, u16, usize, Option<Daypart>)> {
    let key = reader.key(at)?;
    let (hour, minute, pm) = clock(key)?;
    if let Some(pm) = pm {
        let hour = match (hour, pm) {
            (12, false) => 0,
            (hour, true) if hour < 12 => hour + 12,
            (hour, _) => hour,
        };
        return (hour < 24).then_some((hour, minute, 1, None));
    }
    let part = daypart_at(reader, at + 1);
    Some((hour, minute, 1 + usize::from(part.is_some()), part))
}

/// "15h", "15h30", "15:30", "3pm", "3:30pm": hour, minute, and am/pm.
fn clock(key: &str) -> Option<(u16, u16, Option<bool>)> {
    let (body, pm) = if let Some(body) = key.strip_suffix("am") {
        (body, Some(false))
    } else if let Some(body) = key.strip_suffix("pm") {
        (body, Some(true))
    } else {
        (key, None)
    };
    let (hour, minute) = match body.split_once(['h', ':']) {
        Some((hour, minute)) => {
            let separator_is_h = body.as_bytes().get(hour.len()) == Some(&b'h');
            // "3:" means nothing; "3h" is three o'clock.
            if minute.is_empty() && !separator_is_h {
                return None;
            }
            (hour, minute)
        }
        // A bare number is a time only with am/pm: "3pm", not "3".
        None if pm.is_some() => (body, ""),
        None => return None,
    };
    if hour.is_empty() || hour.len() > 2 || !hour.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    if !minute.is_empty() && (minute.len() != 2 || !minute.bytes().all(|b| b.is_ascii_digit())) {
        return None;
    }
    let hour: u16 = hour.parse().ok()?;
    let minute: u16 = if minute.is_empty() {
        0
    } else {
        minute.parse().ok()?
    };
    let max_hour = if pm.is_some() { 12 } else { 23 };
    (hour <= max_hour && minute < 60 && !(pm.is_some() && hour == 0)).then_some((hour, minute, pm))
}

/// Put a bare hour in the part of the day it means. Without one, 1 to 6
/// is the afternoon: nobody books a call for three in the morning.
fn place_hour(hour: u16, part: Option<Daypart>) -> u16 {
    match part {
        Some(Daypart::Morning) => {
            if hour == 12 {
                0
            } else {
                hour
            }
        }
        Some(Daypart::Noon) => {
            if hour <= 2 {
                hour + 12
            } else {
                hour
            }
        }
        Some(Daypart::Evening) => {
            if hour < 12 {
                hour + 12
            } else {
                hour
            }
        }
        None => {
            if (1..=6).contains(&hour) {
                hour + 12
            } else {
                hour
            }
        }
    }
}

enum Span {
    Seconds(i64),
    Days(i64),
}

/// "30p nữa", "2 tiếng nữa", "in 30m", "in 3 days".
fn span_at(reader: &Reader, at: usize) -> Option<(Span, usize)> {
    fn unit(word: &str) -> Option<i64> {
        // Seconds per unit; whole days are negative so they stay dates.
        Some(match word {
            "s" | "giây" | "giay" | "sec" | "secs" | "second" | "seconds" => 1,
            "p" | "ph" | "phút" | "phut" | "m" | "min" | "mins" | "minute" | "minutes" => 60,
            "h" | "giờ" | "gio" | "tiếng" | "tieng" | "hr" | "hrs" | "hour" | "hours" => 3600,
            "ngày" | "ngay" | "d" | "day" | "days" => -1,
            "tuần" | "tuan" | "w" | "week" | "weeks" => -7,
            _ => return None,
        })
    }
    fn amount(word: &str) -> Option<(i64, Option<i64>)> {
        let digits = word.bytes().take_while(u8::is_ascii_digit).count();
        if digits == 0 || digits > 4 {
            return None;
        }
        let number: i64 = word[..digits].parse().ok()?;
        let rest = &word[digits..];
        if rest.is_empty() {
            Some((number, None))
        } else {
            Some((number, Some(unit(rest)?)))
        }
    }
    let span = |number: i64, per: i64| {
        if per < 0 {
            Span::Days(number * -per)
        } else {
            Span::Seconds(number * per)
        }
    };
    const LATER: &[&str] = &["nữa", "nua"];
    let english = reader.is(at, &["in"]);
    let start = at + usize::from(english);
    let (number, per) = amount(reader.key(start)?)?;
    let (per, taken) = match per {
        Some(per) => (per, 1),
        None => (unit(reader.key(start + 1)?)?, 2),
    };
    if english {
        return Some((span(number, per), 1 + taken));
    }
    reader
        .is(start + taken, LATER)
        .then(|| (span(number, per), taken + 1))
}

/// Read a quick-add line: the title, and whatever list, priority and due
/// date it names. Only the first date and the first time count; anything
/// after them stays in the title, as does anything in quotes.
pub fn parse(text: &str, now: Now) -> Draft {
    let mut tokens = tokenize(text);
    let mut draft = Draft::default();
    let mut date: Option<DateSpec> = None;
    let mut time: Option<(u16, u16)> = None;
    let mut pending_hour: Option<(u16, u16, Option<Daypart>)> = None;
    let mut hint: Option<Daypart> = None;
    let mut at = 0;
    while at < tokens.len() {
        let reader = Reader { tokens: &tokens };
        let Some(key) = reader.key(at) else {
            at += 1;
            continue;
        };
        let mut taken = 0;
        if draft.list.is_none() && is_list_tag(key) {
            draft.list = Some(
                tokens[at].text[1..]
                    .trim_end_matches([',', '.', ';'])
                    .to_owned(),
            );
            taken = 1;
        } else if draft.priority == 0 && matches!(key, "!1" | "!2" | "!3") {
            draft.priority = key[1..].parse().unwrap_or(0);
            taken = 1;
        } else if let Some((span, n)) = (date.is_none() && pending_hour.is_none())
            .then(|| span_at(&reader, at))
            .flatten()
        {
            match span {
                Span::Seconds(seconds) => {
                    draft.ring_in = Some(seconds);
                    let total = i64::from(now.minute) + seconds / 60;
                    date = Some(DateSpec::Day(now.day + total.div_euclid(1440)));
                    time = Some((
                        (total.rem_euclid(1440) / 60) as u16,
                        (total.rem_euclid(60)) as u16,
                    ));
                    // A time is fixed: nothing later may move it.
                    pending_hour = Some((0, 0, None));
                }
                Span::Days(days) => date = Some(DateSpec::Day(now.day + days)),
            }
            taken = n;
        } else if let Some((spec, n, part)) =
            date.is_none().then(|| date_at(&reader, at, now)).flatten()
        {
            date = Some(spec);
            hint = hint.or(part);
            taken = n;
        } else if let Some((hour, minute, n, part)) = pending_hour
            .is_none()
            .then(|| time_at(&reader, at))
            .flatten()
        {
            pending_hour = Some((hour, minute, part));
            taken = n;
        }
        if taken == 0 {
            at += 1;
            continue;
        }
        for token in &mut tokens[at..at + taken] {
            token.used = true;
        }
        at += taken;
    }
    if time.is_none() {
        if let Some((hour, minute, part)) = pending_hour {
            time = Some((place_hour(hour, part.or(hint)), minute));
        }
    }
    let minute = time.map(|(hour, minute)| hour * 60 + minute);
    let day = match date {
        Some(DateSpec::Day(day)) => Some(day),
        Some(DateSpec::Weekday { weekday, next_week }) => {
            let today = weekday_from_days(now.day);
            Some(if next_week {
                now.day + (7 - today) + weekday
            } else {
                let ahead = (weekday - today).rem_euclid(7);
                let gone = ahead == 0 && minute.is_some_and(|minute| minute <= now.minute);
                now.day + if gone { 7 } else { ahead }
            })
        }
        None => minute.map(|minute| {
            if minute > now.minute {
                now.day
            } else {
                now.day + 1
            }
        }),
    };
    draft.due = day.map(|day| Due { day, minute });
    // "nhắc tôi …" / "remind me to …" asks for the reminder; the task is
    // what follows, or a plain reminder when nothing does.
    let free: Vec<usize> = (0..tokens.len()).filter(|at| !tokens[*at].used).collect();
    let asked = free.first().is_some_and(|at| {
        matches!(tokens[*at].key.as_str(), "nhắc" | "nhac" | "remind")
            && free.get(1).is_some_and(|next| {
                matches!(
                    tokens[*next].key.as_str(),
                    "tôi" | "toi" | "mình" | "minh" | "me" | "em" | "anh"
                )
            })
    });
    if asked {
        for at in &free[..2] {
            tokens[*at].used = true;
        }
        if let Some(to) = free.get(2) {
            if matches!(tokens[*to].key.as_str(), "to" | "là" | "la" | "về" | "ve") {
                tokens[*to].used = true;
            }
        }
    }
    draft.title = tokens
        .iter()
        .filter(|token| !token.used)
        .map(|token| token.text.as_str())
        .collect::<Vec<_>>()
        .join(" ");
    if asked && draft.title.is_empty() && draft.due.is_some() {
        draft.title = "Nhắc nhở".to_owned();
    }
    draft
}

fn is_list_tag(key: &str) -> bool {
    let Some(name) = key.strip_prefix('#') else {
        return false;
    };
    let name = name.trim_end_matches([',', '.', ';']);
    name.chars().next().is_some_and(char::is_alphabetic)
        && name
            .chars()
            .all(|ch| ch.is_alphanumeric() || ch == '-' || ch == '_')
}

/// The list a typed name means: an existing one whatever its case, or the
/// name as typed.
pub fn resolve_list(name: &str, existing: &[String]) -> String {
    existing
        .iter()
        .find(|list| list.to_lowercase() == name.to_lowercase())
        .cloned()
        .unwrap_or_else(|| name.to_owned())
}

/// The lists in use, in the order they first appear.
pub fn lists(tasks: &[Task]) -> Vec<String> {
    let mut lists: Vec<String> = Vec::new();
    for task in tasks {
        if !task.list.is_empty() && !lists.contains(&task.list) {
            lists.push(task.list.clone());
        }
    }
    lists
}

// ---------------------------------------------------------------------------
// Labels

const WEEKDAYS: [&str; 7] = ["T2", "T3", "T4", "T5", "T6", "T7", "CN"];

fn clock_label(minute: u16) -> String {
    format!("{:02}:{:02}", minute / 60, minute % 60)
}

/// How a due date reads beside its task, from where `now` stands.
pub fn due_label(due: Due, now: Now) -> String {
    let time = due.minute.map(clock_label);
    let with_time = |day: String| match &time {
        Some(time) => format!("{day} {time}"),
        None => day,
    };
    let offset = due.day - now.day;
    match offset {
        0 => time.unwrap_or_else(|| "hôm nay".to_owned()),
        -1 => with_time("hôm qua".to_owned()),
        1 => with_time("mai".to_owned()),
        2..=6 => with_time(WEEKDAYS[weekday_from_days(due.day) as usize].to_owned()),
        _ => {
            let (year, month, date) = civil_from_days(due.day);
            let (this_year, _, _) = civil_from_days(now.day);
            with_time(if year == this_year {
                format!("{date}/{month}")
            } else {
                format!("{date}/{month}/{year}")
            })
        }
    }
}

/// The line under the quick-add field: what the text will become.
pub fn preview(draft: &Draft, now: Now) -> String {
    let mut parts = Vec::new();
    if let Some(due) = draft.due {
        let when = due_label(due, now);
        parts.push(match due.day - now.day {
            0 if due.minute.is_some() => format!("hôm nay {when}"),
            _ => when,
        });
    }
    if let Some(list) = &draft.list {
        parts.push(format!("#{list}"));
    }
    if draft.priority > 0 {
        parts.push(format!("!{}", draft.priority));
    }
    if draft.title.trim().is_empty() {
        if parts.is_empty() {
            return String::new();
        }
        parts.push("cần tên việc".to_owned());
    }
    if parts.is_empty() {
        return String::new();
    }
    format!("\u{2192} {}", parts.join(" \u{b7} "))
}

// ---------------------------------------------------------------------------
// Grouping

pub fn due_of(task: &Task) -> Option<Due> {
    task.due_day.map(|day| Due {
        day,
        minute: task.due_minute,
    })
}

pub fn is_overdue(task: &Task, now: Now) -> bool {
    let Some(due) = due_of(task) else {
        return false;
    };
    task.status != TaskStatus::Done
        && (due.day < now.day
            || (due.day == now.day && due.minute.is_some_and(|minute| minute < now.minute)))
}

pub fn group_of(task: &Task, now: Now) -> Group {
    if task.status == TaskStatus::Done {
        return Group::Done;
    }
    if is_overdue(task, now) {
        return Group::Overdue;
    }
    match due_of(task) {
        Some(due) if due.day == now.day => Group::Today,
        Some(_) => Group::Upcoming,
        None => Group::NoDate,
    }
}

/// The tasks of `list` (all of them for `None`), grouped as the card shows
/// them, empty groups left out.
pub fn group<'a>(tasks: &'a [Task], list: Option<&str>, now: Now) -> Vec<(Group, Vec<&'a Task>)> {
    let order = [
        Group::Overdue,
        Group::Today,
        Group::Upcoming,
        Group::NoDate,
        Group::Done,
    ];
    let mut groups: Vec<(Group, Vec<&Task>)> = order.iter().map(|g| (*g, Vec::new())).collect();
    for task in tasks {
        if list.is_some_and(|list| task.list != list) {
            continue;
        }
        let group = group_of(task, now);
        if let Some((_, members)) = groups.iter_mut().find(|(g, _)| *g == group) {
            members.push(task);
        }
    }
    for (group, members) in &mut groups {
        // Priority 1 is the most urgent; none goes last.
        let rank = |task: &Task| if task.priority == 0 { 9 } else { task.priority };
        let doing_first = |task: &Task| task.status != TaskStatus::Doing;
        let when = |task: &Task| (task.due_day, task.due_minute);
        match group {
            Group::Upcoming => members.sort_by_key(|t| (when(t), doing_first(t), rank(t), t.id)),
            Group::Done => members.sort_by_key(|t| std::cmp::Reverse((t.completed_at, t.id))),
            _ => members.sort_by_key(|t| (doing_first(t), rank(t), when(t), t.id)),
        }
    }
    groups.retain(|(_, members)| !members.is_empty());
    groups
}

// ---------------------------------------------------------------------------
// Reminders

/// The soonest reminder still to ring.
pub fn next_reminder(tasks: &[Task]) -> Option<i64> {
    tasks
        .iter()
        .filter(|task| task.status != TaskStatus::Done && !task.reminded)
        .filter_map(|task| task.remind_at)
        .min()
}

/// The two daily round-ups: 9:00 and 14:00.
pub const DIGEST_MINUTES: [u16; 2] = [9 * 60, 14 * 60];

/// Which round-up is due now, if one is: the latest slot already reached
/// today and not yet sent. Starting after both only sends the later one.
pub fn digest_due(now: Now, sent: Option<(i64, u8)>) -> Option<u8> {
    let slot = DIGEST_MINUTES
        .iter()
        .rposition(|minute| now.minute >= *minute)? as u8;
    match sent {
        Some((day, done)) if day == now.day && done >= slot => None,
        _ => Some(slot),
    }
}

// ---------------------------------------------------------------------------
// Notifications, through the desktop's notification server

const BUS_NAME: &str = "org.freedesktop.Notifications";
const OBJECT_PATH: &str = "/org/freedesktop/Notifications";

thread_local! {
    static BUS: RefCell<Option<gio::DBusConnection>> = const { RefCell::new(None) };
}

fn bus() -> Option<gio::DBusConnection> {
    BUS.with(|bus| {
        let mut bus = bus.borrow_mut();
        if bus.is_none() {
            *bus = gio::bus_get_sync(gio::BusType::Session, None::<&gio::Cancellable>).ok();
        }
        bus.clone()
    })
}

pub struct Notice<'a> {
    pub summary: &'a str,
    pub body: &'a str,
    /// Action keys and their button labels.
    pub actions: &'a [(&'a str, &'a str)],
    /// An alarm: rings, stays until answered, comes through Do Not Disturb.
    pub alarm: bool,
}

/// Show a notification; `sent` gets its id once the server has one.
pub fn notify(notice: Notice, sent: impl FnOnce(Option<u32>) + 'static) {
    let Some(bus) = bus() else {
        sent(None);
        return;
    };
    let actions: Vec<String> = notice
        .actions
        .iter()
        .flat_map(|(key, label)| [(*key).to_owned(), (*label).to_owned()])
        .collect();
    let mut hints: HashMap<String, glib::Variant> = HashMap::new();
    hints.insert("desktop-entry".into(), "io.sysi.Overlay".to_variant());
    // The spec's urgency is a byte; any other type is ignored.
    hints.insert(
        "urgency".into(),
        (if notice.alarm { 2u8 } else { 1u8 }).to_variant(),
    );
    if notice.alarm {
        hints.insert("sound-name".into(), "alarm-clock-elapsed".to_variant());
    }
    let parameters = (
        "Sysi",
        0u32,
        "alarm-symbolic",
        notice.summary,
        notice.body,
        actions,
        hints,
        -1i32,
    )
        .to_variant();
    bus.call(
        Some(BUS_NAME),
        OBJECT_PATH,
        BUS_NAME,
        "Notify",
        Some(&parameters),
        Some(glib::VariantTy::new("(u)").expect("valid reply type")),
        gio::DBusCallFlags::NONE,
        5000,
        None::<&gio::Cancellable>,
        move |reply| {
            sent(
                reply
                    .ok()
                    .and_then(|reply| reply.get::<(u32,)>())
                    .map(|(id,)| id),
            );
        },
    );
}

pub fn close_notification(id: u32) {
    let Some(bus) = bus() else {
        return;
    };
    bus.call(
        Some(BUS_NAME),
        OBJECT_PATH,
        BUS_NAME,
        "CloseNotification",
        Some(&(id,).to_variant()),
        None,
        gio::DBusCallFlags::NONE,
        5000,
        None::<&gio::Cancellable>,
        |_| {},
    );
}

/// Hear the buttons pressed on Sysi's notifications: `on_action` gets the
/// notification id and the action key ("default" for a click on the body).
pub fn listen(on_action: impl Fn(u32, &str) + 'static) {
    let Some(bus) = bus() else {
        return;
    };
    bus.signal_subscribe(
        Some(BUS_NAME),
        Some(BUS_NAME),
        Some("ActionInvoked"),
        Some(OBJECT_PATH),
        None,
        gio::DBusSignalFlags::NONE,
        move |_, _, _, _, _, parameters| {
            if let Some((id, action)) = parameters.get::<(u32, String)>() {
                on_action(id, &action);
            }
        },
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Wednesday 2026-10-28, 10:00.
    fn now() -> Now {
        Now {
            day: days_from_civil(2026, 10, 28).unwrap(),
            minute: 10 * 60,
        }
    }

    fn day(year: i64, month: i64, date: i64) -> i64 {
        days_from_civil(year, month, date).unwrap()
    }

    fn due(text: &str) -> Option<(i64, Option<u16>)> {
        parse(text, now()).due.map(|due| (due.day, due.minute))
    }

    fn title(text: &str) -> String {
        parse(text, now()).title
    }

    fn at(hour: u16, minute: u16) -> Option<u16> {
        Some(hour * 60 + minute)
    }

    #[test]
    fn the_example_line_reads_whole() {
        let draft = parse("gọi khách mai 3h chiều #work !1", now());
        assert_eq!(draft.title, "gọi khách");
        assert_eq!(draft.list.as_deref(), Some("work"));
        assert_eq!(draft.priority, 1);
        assert_eq!(
            draft.due,
            Some(Due {
                day: now().day + 1,
                minute: at(15, 0)
            })
        );
    }

    #[test]
    fn unaccented_typing_reads_the_same() {
        assert_eq!(
            due("goi khach mai 3h chieu"),
            Some((now().day + 1, at(15, 0)))
        );
        assert_eq!(due("hop hom nay 9h sang"), Some((now().day, at(9, 0))));
        assert_eq!(due("nop bao cao ngay mai"), Some((now().day + 1, None)));
    }

    #[test]
    fn days_by_name() {
        let today = now().day;
        assert_eq!(due("việc hôm nay"), Some((today, None)));
        assert_eq!(due("việc ngày mai"), Some((today + 1, None)));
        assert_eq!(due("việc mốt"), Some((today + 2, None)));
        assert_eq!(due("việc ngày kia"), Some((today + 2, None)));
        assert_eq!(due("task today"), Some((today, None)));
        assert_eq!(due("task tomorrow"), Some((today + 1, None)));
    }

    #[test]
    fn weekdays_count_forward_from_wednesday() {
        let today = now().day;
        assert_eq!(due("nộp báo cáo thứ 6"), Some((today + 2, None)));
        assert_eq!(due("nộp báo cáo thứ sáu"), Some((today + 2, None)));
        assert_eq!(due("nộp t2"), Some((today + 5, None)));
        assert_eq!(due("đi chơi chủ nhật"), Some((today + 4, None)));
        assert_eq!(due("đi chơi chu nhat"), Some((today + 4, None)));
        assert_eq!(due("demo friday"), Some((today + 2, None)));
        // Today's own weekday is today while its time is still ahead.
        assert_eq!(due("họp thứ 4"), Some((today, None)));
        assert_eq!(due("họp thứ 4 15h"), Some((today, at(15, 0))));
        assert_eq!(due("họp thứ 4 9h sáng"), Some((today + 7, at(9, 0))));
    }

    #[test]
    fn next_week() {
        let today = now().day;
        // Next Monday, then that week's Friday.
        assert_eq!(due("lên kế hoạch tuần sau"), Some((today + 5, None)));
        assert_eq!(due("demo thứ 6 tuần sau"), Some((today + 9, None)));
        assert_eq!(due("plan next week"), Some((today + 5, None)));
        assert_eq!(title("demo thứ 6 tuần sau"), "demo");
    }

    #[test]
    fn stretches_of_time_are_deadlines() {
        let today = now().day; // Wednesday 28/10/2026
        assert_eq!(due("xong báo cáo tuần này"), Some((today + 2, None)));
        assert_eq!(due("xong báo cáo trong tuần"), Some((today + 2, None)));
        assert_eq!(due("xong bao cao trong tuan"), Some((today + 2, None)));
        assert_eq!(due("xong trong tuần sau"), Some((today + 9, None)));
        assert_eq!(due("dọn nhà cuối tuần"), Some((today + 3, None)));
        assert_eq!(due("dọn nhà cuối tuần sau"), Some((today + 10, None)));
        assert_eq!(due("đóng tiền tháng này"), Some((day(2026, 10, 31), None)));
        assert_eq!(
            due("đóng tiền trong tháng"),
            Some((day(2026, 10, 31), None))
        );
        assert_eq!(due("đóng tiền cuối tháng"), Some((day(2026, 10, 31), None)));
        assert_eq!(
            due("đóng tiền cuối tháng sau"),
            Some((day(2026, 11, 30), None))
        );
        assert_eq!(due("gia hạn tháng sau"), Some((day(2026, 11, 1), None)));
        assert_eq!(due("gia hạn đầu tháng sau"), Some((day(2026, 11, 1), None)));
        assert_eq!(due("tổng kết cuối năm"), Some((day(2026, 12, 31), None)));
        assert_eq!(due("ship this week"), Some((today + 2, None)));
        assert_eq!(due("hike this weekend"), Some((today + 3, None)));
        assert_eq!(due("pay end of month"), Some((day(2026, 10, 31), None)));
        assert_eq!(due("renew next month"), Some((day(2026, 11, 1), None)));
        assert_eq!(title("xong báo cáo trong tuần"), "xong báo cáo");
        // With a time too.
        assert_eq!(due("họp cuối tuần 9h sáng"), Some((today + 3, at(9, 0))));
        // A plain "trong" stays a word.
        assert_eq!(due("ngồi trong phòng"), None);
        // Past Friday, "this week" is Sunday.
        let saturday = Now {
            day: today + 3,
            minute: 600,
        };
        assert_eq!(
            parse("xong tuần này", saturday).due.map(|due| due.day),
            Some(today + 4)
        );
        // December rolls into January.
        let december = Now {
            day: day(2026, 12, 10),
            minute: 600,
        };
        assert_eq!(
            parse("gia hạn tháng sau", december).due.map(|due| due.day),
            Some(day(2027, 1, 1))
        );
    }

    #[test]
    fn calendar_dates() {
        assert_eq!(due("nộp thuế 15/11"), Some((day(2026, 11, 15), None)));
        assert_eq!(due("nộp thuế 15/11/2027"), Some((day(2027, 11, 15), None)));
        // Already gone this year: next year's.
        assert_eq!(due("sinh nhật 3/2"), Some((day(2027, 2, 3), None)));
        assert_eq!(due("sai ngày 31/2"), None);
        assert_eq!(title("sai ngày 31/2"), "sai ngày 31/2");
    }

    #[test]
    fn clock_times() {
        let today = now().day;
        assert_eq!(due("gọi 15h"), Some((today, at(15, 0))));
        assert_eq!(due("gọi 15h30"), Some((today, at(15, 30))));
        assert_eq!(due("gọi 15:30"), Some((today, at(15, 30))));
        assert_eq!(due("gọi 3pm"), Some((today, at(15, 0))));
        assert_eq!(due("gọi 3:30pm"), Some((today, at(15, 30))));
        assert_eq!(due("gọi 11am"), Some((today, at(11, 0))));
        assert_eq!(due("ăn 12h trưa"), Some((today, at(12, 0))));
        assert_eq!(due("ăn 1h trưa"), Some((today, at(13, 0))));
        assert_eq!(due("xem phim 8h tối"), Some((today, at(20, 0))));
        // A bare small hour is the afternoon.
        assert_eq!(due("gọi 3h"), Some((today, at(15, 0))));
        // Gone today: tomorrow.
        assert_eq!(due("chạy bộ 7h"), Some((today + 1, at(7, 0))));
        assert_eq!(due("chạy bộ 7h sáng"), Some((today + 1, at(7, 0))));
    }

    #[test]
    fn a_part_of_day_before_the_date_places_the_hour() {
        let today = now().day;
        assert_eq!(due("xem phim tối nay 8h"), Some((today, at(20, 0))));
        assert_eq!(due("họp sáng mai 9h"), Some((today + 1, at(9, 0))));
        assert_eq!(due("watch tonight 8h"), Some((today, at(20, 0))));
    }

    #[test]
    fn spans_from_now() {
        let today = now().day;
        assert_eq!(due("uống nước 30p nữa"), Some((today, at(10, 30))));
        assert_eq!(due("uống nước 30 phút nữa"), Some((today, at(10, 30))));
        assert_eq!(due("gọi lại 2 tiếng nữa"), Some((today, at(12, 0))));
        assert_eq!(due("gọi lại 2h nữa"), Some((today, at(12, 0))));
        assert_eq!(due("gia hạn 3 ngày nữa"), Some((today + 3, None)));
        assert_eq!(due("ping in 30m"), Some((today, at(10, 30))));
        assert_eq!(due("ping in 2 hours"), Some((today, at(12, 0))));
        assert_eq!(due("renew in 3 days"), Some((today + 3, None)));
        assert_eq!(due("ngủ 15 tiếng nữa"), Some((today + 1, at(1, 0))));
        assert_eq!(title("uống nước 30p nữa"), "uống nước");
    }

    #[test]
    fn remind_me_in_seconds() {
        let draft = parse("nhắc tôi 30s nữa", now());
        assert_eq!(draft.title, "Nhắc nhở");
        assert_eq!(draft.ring_in, Some(30));
        assert_eq!(
            draft.due,
            Some(Due {
                day: now().day,
                minute: at(10, 0)
            })
        );
        let draft = parse("nhac toi goi me 90 giây nữa", now());
        assert_eq!(draft.title, "goi me");
        assert_eq!(draft.ring_in, Some(90));
        assert_eq!(
            draft.due,
            Some(Due {
                day: now().day,
                minute: at(10, 1)
            })
        );
        let draft = parse("remind me to stretch in 45s", now());
        assert_eq!(draft.title, "stretch");
        assert_eq!(draft.ring_in, Some(45));
        assert_eq!(parse("uống nước 30p nữa", now()).ring_in, Some(1800));
        // A date or clock time rings on its minute.
        assert_eq!(parse("gọi 15h", now()).ring_in, None);
        // "nhắc" alone is just a word.
        assert_eq!(parse("nhắc việc", now()).title, "nhắc việc");
    }

    #[test]
    fn ordinary_words_stay_in_the_title() {
        // Lone "nay", "cn", "thu 2", "mot", 3-letter weekdays, dashes.
        assert_eq!(due("việc nay khó"), None);
        assert_eq!(due("báo cáo cn"), None);
        assert_eq!(due("thu 2 triệu tiền nhà"), None);
        assert_eq!(due("mot cai ban"), None);
        assert_eq!(due("sun cream"), None);
        assert_eq!(due("đặt bàn 3-4 người"), None);
        // A part of day with no hour: "ăn trưa".
        assert_eq!(due("đi ăn trưa"), None);
        assert_eq!(title("đi ăn trưa"), "đi ăn trưa");
        // Accented words that fold to a date word are not it.
        assert_eq!(due("sửa mái nhà"), None);
        assert_eq!(due("một việc"), None);
        // "app1", "P1", "#123" are not a priority or a list.
        let draft = parse("fix app1 phòng P1 #123", now());
        assert_eq!(draft.priority, 0);
        assert_eq!(draft.list, None);
        assert_eq!(draft.title, "fix app1 phòng P1 #123");
        // A plain number is not a time.
        assert_eq!(due("mua 3 quả cam"), None);
    }

    #[test]
    fn a_name_is_not_tomorrow() {
        assert_eq!(due("gặp Mai Anh"), None);
        assert_eq!(title("gặp Mai Anh"), "gặp Mai Anh");
        assert_eq!(due("gặp Mai Anh mai"), Some((now().day + 1, None)));
        assert_eq!(due("gặp \"mai\" 15h"), Some((now().day, at(15, 0))));
        assert_eq!(title("gặp \"mai\" 15h"), "gặp mai");
    }

    #[test]
    fn links_are_left_alone() {
        let draft = parse("đọc https://x.io/15/11 mai", now());
        assert_eq!(draft.title, "đọc https://x.io/15/11");
        assert_eq!(
            draft.due,
            Some(Due {
                day: now().day + 1,
                minute: None
            })
        );
    }

    #[test]
    fn only_the_first_date_and_time_count() {
        let draft = parse("dời họp mai sang thứ 6 15h 16h", now());
        assert_eq!(
            draft.due,
            Some(Due {
                day: now().day + 1,
                minute: at(15, 0)
            })
        );
        assert_eq!(draft.title, "dời họp sang thứ 6 16h");
    }

    #[test]
    fn lists_and_priority_anywhere() {
        let draft = parse("#ca-nhan !2 mua sữa", now());
        assert_eq!(draft.list.as_deref(), Some("ca-nhan"));
        assert_eq!(draft.priority, 2);
        assert_eq!(draft.title, "mua sữa");
        let draft = parse("mua sữa #Cá_nhân,", now());
        assert_eq!(draft.list.as_deref(), Some("Cá_nhân"));
        assert_eq!(
            resolve_list("work", &["Work".to_owned(), "Home".to_owned()]),
            "Work"
        );
        assert_eq!(resolve_list("new", &["Work".to_owned()]), "new");
    }

    #[test]
    fn labels() {
        let now = now();
        let today = now.day;
        let label = |day: i64, minute: Option<u16>| due_label(Due { day, minute }, now);
        assert_eq!(label(today, at(14, 0)), "14:00");
        assert_eq!(label(today, None), "hôm nay");
        assert_eq!(label(today - 1, None), "hôm qua");
        assert_eq!(label(today + 1, at(15, 0)), "mai 15:00");
        assert_eq!(label(today + 3, at(15, 0)), "T7 15:00");
        assert_eq!(label(day(2026, 11, 15), None), "15/11");
        assert_eq!(label(day(2027, 1, 2), None), "2/1/2027");
        let draft = parse("gọi khách mai 3h chiều #work !1", now);
        assert_eq!(
            preview(&draft, now),
            "\u{2192} mai 15:00 \u{b7} #work \u{b7} !1"
        );
        assert_eq!(
            preview(&parse("mai", now), now),
            "\u{2192} mai \u{b7} cần tên việc"
        );
        assert_eq!(preview(&parse("mua sữa", now), now), "");
    }

    fn task(id: u64, due: Option<(i64, Option<u16>)>) -> Task {
        Task {
            id,
            title: format!("task {id}"),
            due_day: due.map(|(day, _)| day),
            due_minute: due.and_then(|(_, minute)| minute),
            ..Task::default()
        }
    }

    #[test]
    fn grouping_and_order() {
        let now = now();
        let today = now.day;
        let mut tasks = vec![
            task(1, Some((today - 1, None))),
            task(2, Some((today, at(9, 0)))),
            task(3, Some((today, at(14, 0)))),
            task(4, Some((today + 2, None))),
            task(5, None),
            task(6, Some((today, None))),
            task(7, Some((today + 1, None))),
        ];
        tasks[5].priority = 1;
        tasks.push(Task {
            status: TaskStatus::Done,
            completed_at: Some(1),
            ..task(8, None)
        });
        let groups = group(&tasks, None, now);
        let ids: Vec<(Group, Vec<u64>)> = groups
            .iter()
            .map(|(group, members)| (*group, members.iter().map(|t| t.id).collect()))
            .collect();
        assert_eq!(
            ids,
            vec![
                // 9:00 today has passed at 10:00.
                (Group::Overdue, vec![1, 2]),
                // Priority first, then the time.
                (Group::Today, vec![6, 3]),
                (Group::Upcoming, vec![7, 4]),
                (Group::NoDate, vec![5]),
                (Group::Done, vec![8]),
            ]
        );
        tasks[4].list = "Work".into();
        let work = group(&tasks, Some("Work"), now);
        assert_eq!(work.len(), 1);
        assert_eq!(work[0].1[0].id, 5);
        assert_eq!(lists(&tasks), vec!["Work".to_owned()]);
    }

    #[test]
    fn reminders_and_round_ups() {
        let mut tasks = vec![task(1, None), task(2, None), task(3, None)];
        tasks[0].remind_at = Some(300);
        tasks[1].remind_at = Some(200);
        tasks[1].reminded = true;
        tasks[2].remind_at = Some(100);
        tasks[2].status = TaskStatus::Done;
        assert_eq!(next_reminder(&tasks), Some(300));

        let at_minute = |minute: u16| Now { day: 10, minute };
        assert_eq!(digest_due(at_minute(8 * 60), None), None);
        assert_eq!(digest_due(at_minute(9 * 60), None), Some(0));
        assert_eq!(digest_due(at_minute(10 * 60), Some((10, 0))), None);
        assert_eq!(digest_due(at_minute(14 * 60), Some((10, 0))), Some(1));
        assert_eq!(digest_due(at_minute(15 * 60), Some((10, 1))), None);
        // Started late: only the later round-up.
        assert_eq!(digest_due(at_minute(15 * 60), Some((9, 1))), Some(1));
    }
}
