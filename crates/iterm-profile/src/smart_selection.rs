use plist::Value;
use regex_lite::Regex;
use serde::{Deserialize, Serialize};

/// Precision priority for iTerm2 Smart Selection Rules (higher wins ties).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum SmartSelectionPrecision {
    VeryLow = 1,
    Low = 2,
    Normal = 3,
    High = 4,
    VeryHigh = 5,
}

impl SmartSelectionPrecision {
    pub fn from_iterm_str(s: &str) -> Self {
        match s {
            "very_low" => Self::VeryLow,
            "low" => Self::Low,
            "normal" => Self::Normal,
            "high" => Self::High,
            "very_high" => Self::VeryHigh,
            _ => Self::Normal,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SmartSelectionRule {
    pub notes: String,
    pub precision: SmartSelectionPrecision,
    pub regex: String,
}

/// Convert POSIX character classes used in iTerm2's ICU regexes (`[[:letter:][:number:]]`)
/// into standard Rust regex equivalents so imported rules compile cleanly.
fn normalize_iterm_regex(raw: &str) -> String {
    raw.replace("[:letter:]", "a-zA-Z")
        .replace("[:number:]", "0-9")
        .replace("[:alnum:]", "a-zA-Z0-9")
}

#[derive(Debug, Clone)]
pub struct SmartSelectionMatch {
    pub start_byte: usize,
    pub end_byte: usize,
    pub precision: SmartSelectionPrecision,
    pub rule_notes: String,
}

/// Compiled smart selection rule matcher.
pub struct SmartSelector {
    compiled: Vec<(SmartSelectionRule, Regex)>,
}

impl SmartSelector {
    pub fn new(rules: &[SmartSelectionRule]) -> Self {
        let mut compiled = Vec::with_capacity(rules.len());
        for rule in rules {
            let norm = normalize_iterm_regex(&rule.regex);
            if let Ok(re) = Regex::new(&norm) {
                compiled.push((rule.clone(), re));
            }
        }
        Self { compiled }
    }

    /// Find the highest-precision smart selection match covering `byte_offset` in `line`.
    pub fn match_at(&self, line: &str, byte_offset: usize) -> Option<SmartSelectionMatch> {
        let mut best: Option<SmartSelectionMatch> = None;
        for (rule, re) in &self.compiled {
            for m in re.find_iter(line) {
                if byte_offset >= m.start() && byte_offset < m.end() {
                    let cand = SmartSelectionMatch {
                        start_byte: m.start(),
                        end_byte: m.end(),
                        precision: rule.precision,
                        rule_notes: rule.notes.clone(),
                    };
                    let take = match &best {
                        None => true,
                        Some(prev) => {
                            cand.precision > prev.precision
                                || (cand.precision == prev.precision
                                    && (cand.end_byte - cand.start_byte)
                                        > (prev.end_byte - prev.start_byte))
                        }
                    };
                    if take {
                        best = Some(cand);
                    }
                }
            }
        }
        best
    }
}

pub fn default_smart_selection_rules() -> Vec<SmartSelectionRule> {
    vec![
        SmartSelectionRule {
            notes: "Word bounded by whitespace".into(),
            precision: SmartSelectionPrecision::Low,
            regex: r"\S+".into(),
        },
        SmartSelectionRule {
            notes: "Paths".into(),
            precision: SmartSelectionPrecision::Normal,
            regex: r"~?/?([a-zA-Z0-9._-]+/+)+[a-zA-Z0-9._-]+/?".into(),
        },
        SmartSelectionRule {
            notes: "Quoted string".into(),
            precision: SmartSelectionPrecision::Normal,
            regex: r#"@?"(?:[^"\\]|\\.)*""#.into(),
        },
        SmartSelectionRule {
            notes: "email address".into(),
            precision: SmartSelectionPrecision::High,
            regex: r"\b[A-Za-z0-9._%+-]+@[A-Za-z0-9.-]+\.[A-Za-z]{2,}\b".into(),
        },
        SmartSelectionRule {
            notes: "HTTP URL".into(),
            precision: SmartSelectionPrecision::VeryHigh,
            regex: r"https?://([a-z0-9A-Z]+(:[a-zA-Z0-9]+)?@)?[a-z0-9A-Z]+(\.[a-z0-9A-Z]+)*((:[0-9]+)?)(/[a-zA-Z0-9;/\.\-_+%~?&@=#\(\)]*)?".into(),
        },
        SmartSelectionRule {
            notes: "SSH URL".into(),
            precision: SmartSelectionPrecision::VeryHigh,
            regex: r"\bssh://?([a-z0-9A-Z_]+@)?([a-zA-Z0-9\-]+\.)*[a-zA-Z0-9\-]+\b".into(),
        },
    ]
}

pub fn parse_iterm_plist_rules(arr: &[Value]) -> Vec<SmartSelectionRule> {
    let mut out = Vec::new();
    for item in arr {
        let Some(d) = item.as_dictionary() else {
            continue;
        };
        let Some(regex) = d.get("regex").and_then(|v| v.as_string()) else {
            continue;
        };
        let notes = d
            .get("notes")
            .and_then(|v| v.as_string())
            .unwrap_or("")
            .to_string();
        let precision = d
            .get("precision")
            .and_then(|v| v.as_string())
            .map(SmartSelectionPrecision::from_iterm_str)
            .unwrap_or(SmartSelectionPrecision::Normal);
        out.push(SmartSelectionRule {
            notes,
            precision,
            regex: regex.to_string(),
        });
    }
    out
}

/// Check if `ch` is considered part of a word given iTerm2's `WordCharacters` preference.
#[inline]
pub fn is_word_char(ch: char, word_characters: &str) -> bool {
    ch.is_alphanumeric() || word_characters.contains(ch)
}
