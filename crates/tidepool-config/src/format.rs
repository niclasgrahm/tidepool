//! Number formats: how a metric's value reads on screen. A metric's
//! `format` in its Tidepool extension says it for every visual; a visual's
//! `format` says it again for that visual only. The value itself never
//! changes: a format is display only.
//!
//! A format is words separated by spaces, in any order:
//!
//! - at most one style: `int`, `decimal`, `percent` (the value is a
//!   fraction: 0.25 reads 25.0%) or `currency:XXX` (an ISO code, `currency:EUR`);
//! - `compact`: thousands and up read short, with a suffix (167.8M);
//! - `decimals:N`: digits after the point, 0 to 6.

use std::fmt;
use std::str::FromStr;

/// What a format's words say.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct NumberFormat {
    pub style: Style,
    pub compact: bool,
    pub decimals: Option<u8>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum Style {
    /// No style word: as the value comes.
    #[default]
    Plain,
    Int,
    Decimal,
    Percent,
    /// An ISO 4217 code.
    Currency(String),
}

/// The most digits `decimals:N` allows.
pub const MAX_DECIMALS: u8 = 6;

/// What a format may say, for a diagnostic's help.
pub const HELP: &str = "a format is a style (int, decimal, percent or currency:XXX with an ISO \
                        code), `compact` and `decimals:N`, in any order: `currency:EUR compact`";

impl FromStr for NumberFormat {
    type Err = String;

    /// Reads a format; the error says which word is wrong.
    fn from_str(text: &str) -> Result<Self, String> {
        let mut format = Self::default();
        let mut styled = false;
        if text.trim().is_empty() {
            return Err("a format can't be empty".into());
        }
        for word in text.split_whitespace() {
            let style = match word {
                "int" => Some(Style::Int),
                "decimal" => Some(Style::Decimal),
                "percent" => Some(Style::Percent),
                _ => match word.strip_prefix("currency:") {
                    Some(code)
                        if code.len() == 3 && code.chars().all(|c| c.is_ascii_uppercase()) =>
                    {
                        Some(Style::Currency(code.into()))
                    }
                    Some(code) => {
                        return Err(format!(
                            "`{code}` is not an ISO currency code (three capital letters)"
                        ));
                    }
                    None => None,
                },
            };
            if let Some(style) = style {
                if styled {
                    return Err(format!("`{word}` is a second style"));
                }
                styled = true;
                format.style = style;
            } else if word == "compact" {
                if format.compact {
                    return Err("`compact` is there twice".into());
                }
                format.compact = true;
            } else if let Some(digits) = word.strip_prefix("decimals:") {
                if format.decimals.is_some() {
                    return Err("`decimals` is there twice".into());
                }
                match digits.parse::<u8>() {
                    Ok(n) if n <= MAX_DECIMALS => format.decimals = Some(n),
                    _ => return Err(format!("`decimals:` takes 0 to {MAX_DECIMALS}")),
                }
            } else {
                return Err(format!("unknown word `{word}`"));
            }
        }
        Ok(format)
    }
}

impl fmt::Display for NumberFormat {
    /// The format's words, in the order the docs give them.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut words: Vec<String> = Vec::new();
        match &self.style {
            Style::Plain => {}
            Style::Int => words.push("int".into()),
            Style::Decimal => words.push("decimal".into()),
            Style::Percent => words.push("percent".into()),
            Style::Currency(code) => words.push(format!("currency:{code}")),
        }
        if self.compact {
            words.push("compact".into());
        }
        if let Some(n) = self.decimals {
            words.push(format!("decimals:{n}"));
        }
        f.write_str(&words.join(" "))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> NumberFormat {
        match text.parse() {
            Ok(format) => format,
            Err(e) => panic!("expected `{text}` to be a format: {e}"),
        }
    }

    fn problem(text: &str) -> String {
        match text.parse::<NumberFormat>() {
            Ok(format) => panic!("expected `{text}` to be refused, got {format:?}"),
            Err(e) => e,
        }
    }

    #[test]
    fn words_come_in_any_order() {
        assert_eq!(
            parse("compact currency:EUR"),
            NumberFormat {
                style: Style::Currency("EUR".into()),
                compact: true,
                decimals: None,
            }
        );
        assert_eq!(parse("decimals:0 compact"), parse("compact  decimals:0"));
        assert_eq!(parse("percent").style, Style::Percent);
        assert_eq!(parse("compact").style, Style::Plain);
        // the old single words still mean what they did
        for old in ["int", "decimal", "percent", "currency:SEK"] {
            assert_eq!(parse(old).to_string(), old);
        }
        assert_eq!(
            parse("decimals:1 compact currency:USD").to_string(),
            "currency:USD compact decimals:1"
        );
    }

    #[test]
    fn a_wrong_word_is_named() {
        assert_eq!(problem("compakt"), "unknown word `compakt`");
        assert_eq!(problem("int percent"), "`percent` is a second style");
        assert!(problem("currency:eur").contains("`eur`"));
        assert!(problem("decimals:9").contains("0 to 6"));
        assert!(problem("decimals:x").contains("0 to 6"));
        assert_eq!(problem("compact compact"), "`compact` is there twice");
        assert_eq!(problem("  "), "a format can't be empty");
    }
}
