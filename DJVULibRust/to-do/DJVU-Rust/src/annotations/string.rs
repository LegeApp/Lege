use std::borrow::Cow;
use std::ffi::{OsStr, OsString};
use std::num::ParseIntError;

pub trait DjvuStrExt {
    fn to_escaped(&self, to_seven_bit: bool) -> String;
    fn from_escaped(&self) -> String;
    fn to_int(&self) -> Result<i64, ParseIntError>;
    fn to_float(&self) -> Result<f64, std::num::ParseFloatError>;
    fn substr(&self, from: i32, len: Option<u32>) -> Cow<'_, str>;
}

impl DjvuStrExt for str {
    fn to_escaped(&self, to_seven_bit: bool) -> String {
        let mut escaped = String::with_capacity(self.len());
        for c in self.chars() {
            match c {
                '<' => escaped.push_str("<"),
                '>' => escaped.push_str(">"),
                '&' => escaped.push_str("&"),
                '\'' => escaped.push_str("'"),
                '"' => escaped.push_str("\""),
                c if c.is_control() => escaped.push_str(&format!("&#{};", c as u32)),
                c if to_seven_bit && !c.is_ascii() => escaped.push_str(&format!("&#{};", c as u32)),
                _ => escaped.push(c),
            }
        }
        escaped
    }

    fn from_escaped(&self) -> String {
        let mut unescaped = String::with_capacity(self.len());
        let mut last_end = 0;
        let mut it = self.match_indices('&');

        while let Some((start, _)) = it.next() {
            unescaped.push_str(&self[last_end..start]);
            if let Some(end) = self[start..].find(';') {
                let entity = &self[start + 1..start + end];
                last_end = start + end + 1;
                let ch = match entity {
                    "lt" => Some('<' as u32),
                    "gt" => Some('>' as u32),
                    "amp" => Some('&' as u32),
                    "apos" => Some('\'' as u32),
                    "quot" => Some('"' as u32),
                    _ => None,
                };
                let char_code = if entity.starts_with("#x") || entity.starts_with("#X") {
                    u32::from_str_radix(&entity[2..], 16).ok()
                } else if entity.starts_with('#') {
                    u32::from_str_radix(&entity[1..], 10).ok()
                } else {
                    ch
                };
                if let Some(code) = char_code.and_then(char::from_u32) {
                    unescaped.push(code);
                } else {
                    unescaped.push_str(&self[start..last_end]);
                }
            } else {
                last_end = start;
                break;
            }
        }
        unescaped.push_str(&self[last_end..]);
        unescaped
    }

    fn to_int(&self) -> Result<i64, ParseIntError> {
        self.trim().parse::<i64>()
    }

    fn to_float(&self) -> Result<f64, std::num::ParseFloatError> {
        self.trim().parse::<f64>()
    }

    fn substr(&self, from: i32, len: Option<u32>) -> Cow<'_, str> {
        let char_count = self.chars().count();
        if char_count == 0 {
            return Cow::Borrowed("");
        }
        let start_char = if from >= 0 {
            from as usize
        } else {
            char_count.saturating_sub((-from) as usize)
        };
        if start_char >= char_count {
            return Cow::Borrowed("");
        }
        let end_char = match len {
            Some(l) => (start_char + l as usize).min(char_count),
            None => char_count,
        };
        let start_byte = self.char_indices().nth(start_char).map_or(0, |(i, _)| i);
        let end_byte = self
            .char_indices()
            .nth(end_char)
            .map_or(self.len(), |(i, _)| i);
        Cow::Borrowed(&self[start_byte..end_byte])
    }
}

pub fn utf8_to_native(s: &str) -> OsString {
    OsString::from(s)
}

pub fn native_to_utf8(os_str: &OsStr) -> Result<String, OsString> {
    os_str.to_os_string().into_string()
}
