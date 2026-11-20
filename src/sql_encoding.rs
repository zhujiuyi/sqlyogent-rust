use std::io::{self, Read, Write};

use encoding_rs::{
    BIG5, DecoderResult, EncoderResult, Encoding, GB18030, GBK, ISO_8859_2, UTF_8, WINDOWS_1252,
};
use serde::{Deserialize, Serialize};

pub const DUMP_ENCODING_PREFIX: &str = "-- Harbor SQL dump encoding: ";

const BUFFER_SIZE: usize = 8192;

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum SqlDumpCharset {
    #[default]
    Utf8Mb4,
    #[serde(alias = "utf8")]
    Utf8Mb3,
    Ascii,
    Latin1,
    Latin2,
    Gbk,
    Gb18030,
    Big5,
}

impl SqlDumpCharset {
    pub const ALL: [Self; 8] = [
        Self::Utf8Mb4,
        Self::Utf8Mb3,
        Self::Ascii,
        Self::Latin1,
        Self::Latin2,
        Self::Gbk,
        Self::Gb18030,
        Self::Big5,
    ];

    pub const fn mysql_name(self) -> &'static str {
        match self {
            Self::Utf8Mb4 => "utf8mb4",
            Self::Utf8Mb3 => "utf8mb3",
            Self::Ascii => "ascii",
            Self::Latin1 => "latin1",
            Self::Latin2 => "latin2",
            Self::Gbk => "gbk",
            Self::Gb18030 => "gb18030",
            Self::Big5 => "big5",
        }
    }

    pub const fn label(self) -> &'static str {
        match self {
            Self::Utf8Mb4 => "utf8mb4（完整 UTF-8）",
            Self::Utf8Mb3 => "utf8mb3（三字节 UTF-8）",
            Self::Ascii => "ASCII",
            Self::Latin1 => "latin1（西欧，Windows-1252）",
            Self::Latin2 => "latin2（中欧，ISO-8859-2）",
            Self::Gbk => "GBK（简体中文）",
            Self::Gb18030 => "GB18030（简体中文）",
            Self::Big5 => "Big5（繁体中文）",
        }
    }

    pub fn parse(name: &str) -> Option<Self> {
        let name = name.trim();
        if name.eq_ignore_ascii_case("utf8") {
            return Some(Self::Utf8Mb3);
        }
        Self::ALL
            .into_iter()
            .find(|charset| name.eq_ignore_ascii_case(charset.mysql_name()))
    }

    fn encoding(self) -> &'static Encoding {
        match self {
            Self::Utf8Mb4 | Self::Utf8Mb3 | Self::Ascii => UTF_8,
            Self::Latin1 => WINDOWS_1252,
            Self::Latin2 => ISO_8859_2,
            Self::Gbk => GBK,
            Self::Gb18030 => GB18030,
            Self::Big5 => BIG5,
        }
    }

    fn check_text(self, text: &str) -> io::Result<()> {
        let invalid = match self {
            Self::Utf8Mb3 => text.chars().find(|character| character.len_utf8() == 4),
            Self::Ascii if !text.is_ascii() => text.chars().find(|character| !character.is_ascii()),
            _ => None,
        };
        match invalid {
            Some(character) => Err(self.unmappable(character)),
            None => Ok(()),
        }
    }

    fn unmappable(self, character: char) -> io::Error {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "{} 无法表示字符 U+{:04X}。",
                self.mysql_name(),
                u32::from(character)
            ),
        )
    }

    fn invalid_bytes(self) -> io::Error {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("编码含有无效或不兼容的 {} 字节。", self.mysql_name()),
        )
    }
}

/// Parse a bounded first line, in ASCII. Unmarked SQL has no declared encoding.
pub fn parse_marker(first_line: &[u8]) -> Result<Option<SqlDumpCharset>, String> {
    let Some(name) = first_line.strip_prefix(DUMP_ENCODING_PREFIX.as_bytes()) else {
        return Ok(None);
    };
    let name = name
        .strip_suffix(b"\r\n")
        .or_else(|| name.strip_suffix(b"\n"))
        .unwrap_or(name);
    SqlDumpCharset::ALL
        .into_iter()
        .find(|charset| name == charset.mysql_name().as_bytes())
        .map(Some)
        .ok_or_else(|| "SQL 转储的字符集标记无效或暂不支持。".into())
}

/// Transcode validated UTF-8 SQL without substituting unmappable characters.
pub fn encode_sql(
    charset: SqlDumpCharset,
    input: impl Read,
    mut output: impl Write,
) -> io::Result<()> {
    if charset.encoding() == UTF_8 {
        return decoded_text(SqlDumpCharset::Utf8Mb4, input, |text, _| {
            charset.check_text(text)?;
            output.write_all(text.as_bytes())
        });
    }

    let mut encoder = charset.encoding().new_encoder();
    let mut encoded = [0; BUFFER_SIZE];
    let mut grammar = CjkBytes::new(charset);
    decoded_text(SqlDumpCharset::Utf8Mb4, input, |mut text, last| {
        loop {
            let (result, read, written) =
                encoder.encode_from_utf8_without_replacement(text, &mut encoded, last);
            if let EncoderResult::Unmappable(character) = result {
                return Err(charset.unmappable(character));
            }
            if let Some(grammar) = &mut grammar {
                grammar.check(
                    &encoded[..written],
                    last && result == EncoderResult::InputEmpty,
                )?;
            }
            output.write_all(&encoded[..written])?;
            if result == EncoderResult::InputEmpty {
                return Ok(());
            }
            text = &text[read..];
        }
    })
}

/// Decode declared SQL bytes to UTF-8 without replacement or BOM handling.
pub fn decode_sql(
    charset: SqlDumpCharset,
    input: impl Read,
    mut output: impl Write,
) -> io::Result<()> {
    decoded_text(charset, input, |text, _| {
        charset.check_text(text)?;
        output.write_all(text.as_bytes())
    })
}

fn decoded_text(
    charset: SqlDumpCharset,
    mut input: impl Read,
    mut receive: impl FnMut(&str, bool) -> io::Result<()>,
) -> io::Result<()> {
    let mut decoder = charset.encoding().new_decoder_without_bom_handling();
    let mut bytes = [0; BUFFER_SIZE];
    let mut text = String::with_capacity(BUFFER_SIZE);
    let mut grammar = CjkBytes::new(charset);
    loop {
        let size = match input.read(&mut bytes) {
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            result => result?,
        };
        let eof = size == 0;
        let mut unread = &bytes[..size];
        if let Some(grammar) = &mut grammar {
            grammar.check(unread, eof)?;
        }
        loop {
            text.clear();
            let (result, read) =
                decoder.decode_to_string_without_replacement(unread, &mut text, eof);
            match result {
                DecoderResult::InputEmpty => {
                    receive(&text, eof)?;
                    break;
                }
                DecoderResult::OutputFull => {
                    receive(&text, false)?;
                    unread = &unread[read..];
                }
                DecoderResult::Malformed(_, _) => return Err(charset.invalid_bytes()),
            }
        }
        if eof {
            return Ok(());
        }
    }
}

#[derive(Clone, Copy)]
enum CjkState {
    Start,
    Trail(u8),
    Third,
    Fourth,
}

struct CjkBytes {
    charset: SqlDumpCharset,
    state: CjkState,
}

impl CjkBytes {
    fn new(charset: SqlDumpCharset) -> Option<Self> {
        matches!(
            charset,
            SqlDumpCharset::Gbk | SqlDumpCharset::Gb18030 | SqlDumpCharset::Big5
        )
        .then_some(Self {
            charset,
            state: CjkState::Start,
        })
    }

    fn check(&mut self, bytes: &[u8], last: bool) -> io::Result<()> {
        let big5 = self.charset == SqlDumpCharset::Big5;
        let lead = if big5 { 0xa1..=0xf9 } else { 0x81..=0xfe };
        let trail = if big5 { 0xa1..=0xfe } else { 0x80..=0xfe };
        for &byte in bytes {
            self.state = match self.state {
                CjkState::Start if byte.is_ascii() => CjkState::Start,
                CjkState::Start if lead.contains(&byte) => CjkState::Trail(byte),
                CjkState::Trail(first)
                    if (0x40..=0x7e).contains(&byte) || trail.contains(&byte) =>
                {
                    if !big5 && changed_gb18030_pair(first, byte) {
                        return Err(self.charset.invalid_bytes());
                    }
                    CjkState::Start
                }
                CjkState::Trail(_)
                    if self.charset == SqlDumpCharset::Gb18030 && byte.is_ascii_digit() =>
                {
                    CjkState::Third
                }
                CjkState::Third if lead.contains(&byte) => CjkState::Fourth,
                CjkState::Fourth if byte.is_ascii_digit() => CjkState::Start,
                _ => return Err(self.charset.invalid_bytes()),
            };
        }
        if last && !matches!(self.state, CjkState::Start) {
            return Err(self.charset.invalid_bytes());
        }
        Ok(())
    }
}

fn changed_gb18030_pair(lead: u8, trail: u8) -> bool {
    // MySQL's GB18030-2005 and the WHATWG codec disagree on these two-byte
    // mappings. Reject the ambiguity instead of changing the character.
    matches!(
        (lead, trail),
        (0xa3, 0xa0)
            | (0xa6, 0xd9..=0xdf | 0xec..=0xed | 0xf3)
            | (0xfe, 0x59 | 0x61 | 0x66..=0x67 | 0x6d | 0x7e | 0x90 | 0xa0)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fragments<'a>(&'a [u8]);

    impl Read for Fragments<'_> {
        fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
            let size = out.len().min(1);
            self.0.read(&mut out[..size])
        }
    }

    #[test]
    fn encodes_exact_sql_bytes_and_decodes_multibyte_fragments() {
        let examples: &[(SqlDumpCharset, &str, &[u8])] = &[
            (
                SqlDumpCharset::Utf8Mb4,
                "SELECT '🙂';",
                b"SELECT '\xf0\x9f\x99\x82';",
            ),
            (
                SqlDumpCharset::Utf8Mb3,
                "SELECT '中';",
                b"SELECT '\xe4\xb8\xad';",
            ),
            (SqlDumpCharset::Ascii, "SELECT 'test';", b"SELECT 'test';"),
            (
                SqlDumpCharset::Latin1,
                "SELECT 'café €';",
                b"SELECT 'caf\xe9 \x80';",
            ),
            (
                SqlDumpCharset::Latin2,
                "SELECT 'Łódź';",
                b"SELECT '\xa3\xf3d\xbc';",
            ),
            (
                SqlDumpCharset::Gbk,
                "SELECT '中文';",
                b"SELECT '\xd6\xd0\xce\xc4';",
            ),
            (
                SqlDumpCharset::Gb18030,
                "SELECT '中💩';",
                b"SELECT '\xd6\xd0\x94\x39\xda\x33';",
            ),
            (
                SqlDumpCharset::Big5,
                "SELECT '中文';",
                b"SELECT '\xa4\xa4\xa4\xe5';",
            ),
        ];
        for &(charset, sql, expected) in examples {
            let mut encoded = Vec::new();
            encode_sql(charset, Fragments(sql.as_bytes()), &mut encoded).unwrap();
            assert_eq!(encoded, expected, "{}", charset.mysql_name());

            let mut utf8 = Vec::new();
            decode_sql(charset, Fragments(&encoded), &mut utf8).unwrap();
            assert_eq!(utf8, sql.as_bytes(), "{}", charset.mysql_name());
        }

        let mut utf8 = Vec::new();
        decode_sql(
            SqlDumpCharset::Latin1,
            vec![0x80; BUFFER_SIZE * 2].as_slice(),
            &mut utf8,
        )
        .unwrap();
        assert_eq!(utf8, "€".repeat(BUFFER_SIZE * 2).as_bytes());
    }

    #[test]
    fn rejects_unrepresentable_text_and_incompatible_declared_bytes() {
        for (charset, input) in [
            (SqlDumpCharset::Ascii, "é"),
            (SqlDumpCharset::Latin1, "中"),
            (SqlDumpCharset::Utf8Mb3, "🙂"),
            (SqlDumpCharset::Gbk, "🙂"),
        ] {
            let error = encode_sql(charset, input.as_bytes(), io::sink()).unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        }

        for (charset, bytes) in [
            (SqlDumpCharset::Utf8Mb4, b"\xe4\xb8".as_slice()),
            (SqlDumpCharset::Gbk, b"\x94\x39\xda\x33"),
            (SqlDumpCharset::Gb18030, b"\xa6\xd9"),
        ] {
            let error = decode_sql(charset, Fragments(bytes), io::sink()).unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        }
    }

    #[test]
    fn recognizes_canonical_dump_markers_and_config_alias() {
        for charset in SqlDumpCharset::ALL {
            assert_eq!(SqlDumpCharset::parse(charset.mysql_name()), Some(charset));
            let header = format!("{DUMP_ENCODING_PREFIX}{}\r\n", charset.mysql_name());
            assert_eq!(parse_marker(header.as_bytes()).unwrap(), Some(charset));
            let name = serde_json::to_string(&charset).unwrap();
            assert_eq!(name, format!("\"{}\"", charset.mysql_name()));
        }
        assert_eq!(
            SqlDumpCharset::parse(" UTF8 "),
            Some(SqlDumpCharset::Utf8Mb3)
        );
        assert_eq!(
            serde_json::from_str::<SqlDumpCharset>("\"utf8\"").unwrap(),
            SqlDumpCharset::Utf8Mb3
        );
        assert_eq!(parse_marker(b"-- old unmarked backup\r\n").unwrap(), None);
    }
}
