//! MPD request parsing: command lines and filter expressions.

use crate::library::query::{Filter, Op, SortKey, TagSel};
use crate::library::tags;

/// Splits a request line into the command name and its arguments.
/// Arguments are whitespace-separated, or double-quoted with `\` escapes.
pub fn tokenize(line: &str) -> Result<(String, Vec<String>), String> {
    let mut chars = line.trim_start().chars().peekable();
    let mut name = String::new();
    while let Some(&c) = chars.peek() {
        if c.is_whitespace() {
            break;
        }
        name.push(c);
        chars.next();
    }
    let mut args = Vec::new();
    loop {
        while chars.next_if(|c| c.is_whitespace()).is_some() {}
        let Some(&first) = chars.peek() else { break };
        let mut arg = String::new();
        if first == '"' {
            chars.next();
            loop {
                match chars.next() {
                    Some('"') => break,
                    Some('\\') => match chars.next() {
                        Some(c) => arg.push(c),
                        None => return Err("unterminated quoted argument".into()),
                    },
                    Some(c) => arg.push(c),
                    None => return Err("unterminated quoted argument".into()),
                }
            }
            if chars.peek().is_some_and(|c| !c.is_whitespace()) {
                return Err("space expected after closing '\"'".into());
            }
        } else {
            while let Some(c) = chars.next_if(|c| !c.is_whitespace()) {
                arg.push(c);
            }
        }
        args.push(arg);
    }
    Ok((name, args))
}

/// Parses a filter expression such as `((Artist == 'x') AND (!(Genre contains "y")))`.
pub fn filter_expression(input: &str) -> Result<Filter, String> {
    let mut p = ExprParser { s: input.as_bytes(), i: 0, input };
    let filter = p.expr()?;
    p.ws();
    if p.i != p.s.len() {
        return Err(format!("unparsed garbage after expression: {:?}", &input[p.i..]));
    }
    Ok(filter)
}

struct ExprParser<'a> {
    s: &'a [u8],
    i: usize,
    input: &'a str,
}

impl ExprParser<'_> {
    fn ws(&mut self) {
        while self.s.get(self.i).is_some_and(u8::is_ascii_whitespace) {
            self.i += 1;
        }
    }

    fn eat(&mut self, token: &str) -> bool {
        self.ws();
        if self.s[self.i..].starts_with(token.as_bytes()) {
            self.i += token.len();
            true
        } else {
            false
        }
    }

    fn expect(&mut self, token: &str) -> Result<(), String> {
        if self.eat(token) { Ok(()) } else { Err(format!("'{token}' expected")) }
    }

    /// A bare word: tag names and operators.
    fn word(&mut self) -> String {
        self.ws();
        let start = self.i;
        while self.s.get(self.i).is_some_and(|&c| c.is_ascii_alphanumeric() || b"_-=!~<>".contains(&c)) {
            self.i += 1;
        }
        self.input[start..self.i].to_owned()
    }

    fn quoted(&mut self) -> Result<String, String> {
        self.ws();
        let quote = match self.s.get(self.i) {
            Some(&q @ (b'"' | b'\'')) => q as char,
            _ => return Err("quoted string expected".into()),
        };
        let mut chars = self.input[self.i + 1..].char_indices();
        let mut out = String::new();
        while let Some((off, c)) = chars.next() {
            match c {
                '\\' => match chars.next() {
                    Some((_, c)) => out.push(c),
                    None => break,
                },
                c if c == quote => {
                    self.i += 1 + off + 1;
                    return Ok(out);
                }
                c => out.push(c),
            }
        }
        Err("closing quote not found".into())
    }

    fn expr(&mut self) -> Result<Filter, String> {
        self.expect("(")?;
        self.ws();
        if self.eat("!") {
            let inner = self.expr()?;
            self.expect(")")?;
            return Ok(Filter::Not(Box::new(inner)));
        }
        if self.s.get(self.i) == Some(&b'(') {
            let mut parts = vec![self.expr()?];
            while self.eat("AND") {
                parts.push(self.expr()?);
            }
            self.expect(")")?;
            return Ok(if parts.len() == 1 { parts.pop().unwrap() } else { Filter::And(parts) });
        }

        let name = self.word();
        let filter = match name.to_ascii_lowercase().as_str() {
            "base" => Filter::Base(normalize_dir(&self.quoted()?)),
            "modified-since" | "added-since" => Filter::AddedSince(timestamp(&self.quoted()?)?),
            "file" => {
                let op = self.word();
                let value = self.quoted()?;
                match op.as_str() {
                    "==" => Filter::File(value),
                    "!=" => Filter::Not(Box::new(Filter::File(value))),
                    _ => return Err(format!("unsupported operator for file: {op}")),
                }
            }
            "audioformat" | "prio" => return Err(format!("{name} filters are not supported")),
            _ => {
                let tag = tag_sel(&name)?;
                let op = self.word();
                let op = match op.as_str() {
                    "==" | "eq_cs" | "eq_ci" => Op::Eq,
                    "!=" => Op::Ne,
                    "contains" | "contains_cs" | "contains_ci" => Op::Contains,
                    "starts_with" | "starts_with_cs" | "starts_with_ci" => Op::StartsWith,
                    "=~" | "!~" => return Err("regular expressions are not supported".into()),
                    _ => return Err(format!("unknown filter operator: {op:?}")),
                };
                Filter::Tag { tag, op, value: self.quoted()? }
            }
        };
        self.expect(")")?;
        Ok(filter)
    }
}

pub fn tag_sel(name: &str) -> Result<TagSel, String> {
    if name.eq_ignore_ascii_case("any") {
        return Ok(TagSel::Any);
    }
    match tags::canonical(name) {
        Some(tag) => Ok(TagSel::Tag(tag)),
        None if tags::is_unsupported(name) => Ok(TagSel::Absent),
        None => Err(format!("unknown tag type: {name}")),
    }
}

/// Strips slashes MPD clients sometimes add around directory names.
pub fn normalize_dir(dir: &str) -> String {
    dir.trim_matches('/').to_owned()
}

/// MPD accepts a Unix timestamp or ISO 8601. Song `added` values are ISO
/// 8601 UTC, so both are normalised to that for string comparison.
fn timestamp(value: &str) -> Result<String, String> {
    if let Ok(secs) = value.parse::<i64>() {
        return Ok(iso8601(secs));
    }
    if value.len() >= 10 && value.as_bytes()[4] == b'-' {
        return Ok(value.to_owned());
    }
    Err(format!("invalid timestamp: {value}"))
}

/// Formats a Unix timestamp as `YYYY-MM-DDTHH:MM:SSZ`.
pub fn iso8601(secs: i64) -> String {
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    // Civil-from-days (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

/// The arguments of `find`, `search`, `count`, `list` and friends: a filter
/// (expression or legacy `TAG VALUE` pairs) followed by optional keywords.
#[derive(Debug, Default)]
pub struct FilterArgs {
    pub filter: Option<Filter>,
    pub sort: Option<(SortKey, bool)>,
    pub window: Option<(usize, usize)>,
    pub groups: Vec<&'static str>,
    pub position: Option<String>,
}

/// `legacy_op` is the operator for `TAG VALUE` pairs: `Eq` for `find`,
/// `Contains` for `search`.
pub fn filter_args(args: &[String], legacy_op: Op) -> Result<FilterArgs, String> {
    let mut out = FilterArgs::default();
    let mut rest = args;
    let mut parts = Vec::new();

    if let Some(first) = rest.first()
        && first.starts_with('(')
    {
        parts.push(filter_expression(first)?);
        rest = &rest[1..];
    } else {
        while let [tag, value, tail @ ..] = rest {
            if is_keyword(tag) {
                break;
            }
            parts.push(legacy_pair(tag, value, legacy_op)?);
            rest = tail;
        }
    }

    while let [keyword, value, tail @ ..] = rest {
        match keyword.to_ascii_lowercase().as_str() {
            "sort" => {
                let (desc, name) = match value.strip_prefix('-') {
                    Some(name) => (true, name),
                    None => (false, value.as_str()),
                };
                let key = if name.eq_ignore_ascii_case("last-modified") || name.eq_ignore_ascii_case("added") {
                    SortKey::Added
                } else {
                    SortKey::Tag(tags::canonical(name).ok_or_else(|| format!("unknown sort tag: {name}"))?)
                };
                out.sort = Some((key, desc));
            }
            "window" => out.window = Some(range(value)?),
            "group" => out.groups.push(
                tags::canonical(value).ok_or_else(|| format!("unknown group tag: {value}"))?,
            ),
            "position" => out.position = Some(value.clone()),
            _ => return Err(format!("unexpected argument: {keyword}")),
        }
        rest = tail;
    }
    if !rest.is_empty() {
        return Err("incorrect number of arguments".into());
    }

    out.filter = match parts.len() {
        0 => None,
        1 => parts.pop(),
        _ => Some(Filter::And(parts)),
    };
    Ok(out)
}

fn is_keyword(arg: &str) -> bool {
    ["sort", "window", "group", "position"].iter().any(|k| arg.eq_ignore_ascii_case(k))
}

fn legacy_pair(tag: &str, value: &str, op: Op) -> Result<Filter, String> {
    Ok(match tag.to_ascii_lowercase().as_str() {
        "file" => Filter::File(value.to_owned()),
        "base" => Filter::Base(normalize_dir(value)),
        "modified-since" | "added-since" => Filter::AddedSince(timestamp(value)?),
        _ => Filter::Tag { tag: tag_sel(tag)?, op, value: value.to_owned() },
    })
}

/// `START:END`, `START:` (to the end), or `N` (just N).
pub fn range(arg: &str) -> Result<(usize, usize), String> {
    let bad = || format!("invalid range: {arg}");
    match arg.split_once(':') {
        Some((start, "")) => Ok((start.parse().map_err(|_| bad())?, usize::MAX)),
        Some((start, end)) => {
            let (start, end) = (start.parse().map_err(|_| bad())?, end.parse().map_err(|_| bad())?);
            if end < start { Err(bad()) } else { Ok((start, end)) }
        }
        None => {
            let n: usize = arg.parse().map_err(|_| bad())?;
            Ok((n, n + 1))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn tokenizing() {
        assert_eq!(tokenize("status").unwrap(), ("status".into(), vec![]));
        assert_eq!(
            tokenize(r#"find artist "Guns N' Roses" album "Say \"Hi\" \\ x""#).unwrap(),
            ("find".into(), args(&["artist", "Guns N' Roses", "album", r#"Say "Hi" \ x"#]))
        );
        assert_eq!(tokenize(r#"lsinfo """#).unwrap().1, [""]);
        assert!(tokenize(r#"find "unterminated"#).is_err());
        assert!(tokenize(r#"find "a"b"#).is_err());
    }

    #[test]
    fn expressions() {
        let tag = |t, op, v: &str| Filter::Tag { tag: TagSel::Tag(t), op, value: v.into() };
        assert_eq!(filter_expression("(Artist == 'A\\'s')").unwrap(), tag(tags::ARTIST, Op::Eq, "A's"));
        assert_eq!(
            filter_expression(r#"((albumartist == "X") AND (!(genre contains 'Pop')))"#).unwrap(),
            Filter::And(vec![
                tag(tags::ALBUM_ARTIST, Op::Eq, "X"),
                Filter::Not(Box::new(tag(tags::GENRE, Op::Contains, "Pop"))),
            ])
        );
        assert_eq!(filter_expression("(base '/A/B/')").unwrap(), Filter::Base("A/B".into()));
        assert_eq!(
            filter_expression("(any starts_with 'x')").unwrap(),
            Filter::Tag { tag: TagSel::Any, op: Op::StartsWith, value: "x".into() }
        );
        assert_eq!(
            filter_expression("(modified-since '0')").unwrap(),
            Filter::AddedSince("1970-01-01T00:00:00Z".into())
        );
        assert!(filter_expression("(Artist =~ 'x')").is_err());
        assert!(filter_expression("(Nope == 'x')").is_err());
        assert!(filter_expression("(Artist == 'x') junk").is_err());
    }

    #[test]
    fn filter_arguments() {
        let parsed = filter_args(&args(&["artist", "A", "album", "B", "sort", "-Track", "window", "0:5"]), Op::Eq).unwrap();
        assert_eq!(
            parsed.filter,
            Some(Filter::And(vec![
                Filter::Tag { tag: TagSel::Tag(tags::ARTIST), op: Op::Eq, value: "A".into() },
                Filter::Tag { tag: TagSel::Tag(tags::ALBUM), op: Op::Eq, value: "B".into() },
            ]))
        );
        assert_eq!(parsed.sort, Some((SortKey::Tag(tags::TRACK), true)));
        assert_eq!(parsed.window, Some((0, 5)));

        let grouped = filter_args(&args(&["(Genre == 'Rock')", "group", "albumartist"]), Op::Eq).unwrap();
        assert_eq!(grouped.groups, [tags::ALBUM_ARTIST]);
        assert!(filter_args(&args(&["group", "album"]), Op::Eq).unwrap().filter.is_none());
        assert!(filter_args(&args(&["artist"]), Op::Eq).is_err());
    }

    #[test]
    fn ranges_and_time() {
        assert_eq!(range("3").unwrap(), (3, 4));
        assert_eq!(range("1:4").unwrap(), (1, 4));
        assert_eq!(range("2:").unwrap(), (2, usize::MAX));
        assert!(range("4:1").is_err());
        assert_eq!(iso8601(1_700_000_000), "2023-11-14T22:13:20Z");
        assert_eq!(iso8601(951_782_400), "2000-02-29T00:00:00Z");
    }
}
