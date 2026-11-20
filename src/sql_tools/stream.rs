use std::io::{self, Read};

use super::{Quoting, delimiter_command};

const BUFFER_SIZE: usize = 64 * 1024;
type Checkpoint<'a> = dyn FnMut(u64) -> Result<(), String> + 'a;

pub(crate) struct StreamingStatement<'a> {
    pub(crate) sql: &'a str,
    pub(crate) offset: u64,
}

pub(crate) struct StreamingStatementReader<R> {
    input: Window<R>,
    statement: Vec<u8>,
    delimiter: Vec<u8>,
    quoting: Quoting,
    line_start: bool,
    first: bool,
    statement_limit: usize,
}

impl<R: Read> StreamingStatementReader<R> {
    pub(crate) fn new(reader: R, statement_limit: usize) -> Self {
        Self {
            input: Window::new(reader),
            statement: Vec::new(),
            delimiter: vec![b';'],
            quoting: Quoting::default(),
            first: true,
            line_start: true,
            statement_limit,
        }
    }

    pub(crate) fn set_sql_mode(&mut self, mode: &str) {
        self.quoting = Quoting::from_sql_mode(mode);
    }

    pub(crate) fn position(&self) -> u64 {
        self.input.position
    }

    pub(crate) fn followed_by(
        &mut self,
        expected: &[u8],
        checkpoint: &mut Checkpoint<'_>,
    ) -> Result<bool, String> {
        Ok(self
            .input
            .peek(expected.len(), checkpoint)?
            .starts_with(expected))
    }

    pub(crate) fn next_statement(
        &mut self,
        checkpoint: &mut Checkpoint<'_>,
    ) -> Result<Option<StreamingStatement<'_>>, String> {
        self.statement.clear();
        if std::mem::take(&mut self.first)
            && self.input.peek(3, checkpoint)?.starts_with(b"\xef\xbb\xbf")
        {
            self.input.consume(3);
        }
        let mut offset = None;
        loop {
            let position = self.input.position;
            let lookahead = self.delimiter.len().max(10);
            let bytes = self.input.peek(lookahead, checkpoint)?;
            let Some(&byte) = bytes.first() else { break };
            if self.line_start && offset.is_none() && directive_prefix(bytes) {
                self.read_line(true, checkpoint)?;
                let text = std::str::from_utf8(&self.statement)
                    .map_err(|_| "DELIMITER 指令必须使用 UTF-8 编码。".to_owned())?;
                let (delimiter, _) =
                    delimiter_command(text).ok_or("DELIMITER 指令缺少有效分隔符。")?;
                self.delimiter = delimiter.as_bytes().to_vec();
                self.statement.clear();
                continue;
            }
            if bytes.starts_with(&self.delimiter) {
                self.input.consume(self.delimiter.len());
                self.line_start = false;
                if offset.is_some() {
                    break;
                }
                continue;
            }
            if byte.is_ascii_whitespace() {
                // The first delimiter byte may itself be whitespace.
                let width = bytes
                    .iter()
                    .position(|&item| !item.is_ascii_whitespace() || item == self.delimiter[0])
                    .unwrap_or(bytes.len())
                    .max(1);
                self.line_start |= bytes[..width]
                    .iter()
                    .any(|byte| matches!(byte, b'\r' | b'\n'));
                self.consume(width, offset.is_some())?;
                continue;
            }
            self.line_start = false;
            if byte == b'#'
                || bytes.starts_with(b"--")
                    && bytes
                        .get(2)
                        .is_none_or(|byte| byte.is_ascii_whitespace() || byte.is_ascii_control())
            {
                self.read_line(offset.is_some(), checkpoint)?;
                continue;
            }
            if bytes.starts_with(b"/*") {
                if bytes.starts_with(b"/*!") || bytes.starts_with(b"/*M!") {
                    offset.get_or_insert(position);
                }
                self.read_block(offset.is_some(), checkpoint)?;
                continue;
            }
            offset.get_or_insert(position);
            if matches!(byte, b'\'' | b'"' | b'\x60') {
                self.read_quoted(byte, checkpoint)?;
                continue;
            }
            let lead = self.delimiter[0];
            let width = bytes
                .iter()
                .position(|&item| {
                    item == lead
                        || item.is_ascii_whitespace()
                        || matches!(item, b'\'' | b'"' | b'\x60' | b'#' | b'-' | b'/')
                })
                .unwrap_or(bytes.len())
                .max(1);
            self.consume(width, true)?;
        }
        let Some(offset) = offset else {
            return Ok(None);
        };
        let sql = std::str::from_utf8(&self.statement)
            .map_err(|error| format!("转储文件必须使用 UTF-8 编码。{error}"))?
            .trim_ascii_end();
        Ok(Some(StreamingStatement { sql, offset }))
    }

    fn consume(&mut self, width: usize, retain: bool) -> Result<(), String> {
        if retain {
            if width > self.statement_limit.saturating_sub(self.statement.len()) {
                return Err(format!(
                    "单条 SQL 超过当前数据库会话允许的 {} 字节，请拆分该语句。",
                    self.statement_limit
                ));
            }
            self.statement
                .try_reserve(width)
                .map_err(|_| "内存不足，无法读取这条 SQL。")?;
            self.statement
                .extend_from_slice(&self.input.remaining()[..width]);
        }
        self.input.consume(width);
        Ok(())
    }

    fn read_line(&mut self, retain: bool, checkpoint: &mut Checkpoint<'_>) -> Result<(), String> {
        loop {
            let bytes = self.input.peek(1, checkpoint)?;
            if bytes.is_empty() {
                return Ok(());
            }
            let newline = bytes.iter().position(|byte| matches!(byte, b'\r' | b'\n'));
            let width = newline.unwrap_or(bytes.len());
            self.consume(width, retain)?;
            if newline.is_some() {
                return Ok(());
            }
        }
    }

    fn read_block(&mut self, retain: bool, checkpoint: &mut Checkpoint<'_>) -> Result<(), String> {
        self.consume(2, retain)?;
        loop {
            let bytes = self.input.peek(2, checkpoint)?;
            if bytes.is_empty() {
                return Err("SQL 文件中的块注释没有结束。".into());
            }
            let star = bytes.iter().position(|&byte| byte == b'*');
            let width = star.unwrap_or(bytes.len());
            self.consume(width, retain)?;
            if star.is_some() {
                let close = self.input.peek(2, checkpoint)?.starts_with(b"*/");
                self.consume(if close { 2 } else { 1 }, retain)?;
                if close {
                    return Ok(());
                }
            }
        }
    }

    fn read_quoted(&mut self, quote: u8, checkpoint: &mut Checkpoint<'_>) -> Result<(), String> {
        let escapes = self.quoting.backslash
            && quote != b'\x60'
            && !(quote == b'"' && self.quoting.ansi_identifiers);
        self.consume(1, true)?;
        loop {
            let bytes = self.input.peek(1, checkpoint)?;
            if bytes.is_empty() {
                return Err("SQL 文件中的字符串或标识符引号没有结束。".into());
            }
            let token = bytes
                .iter()
                .position(|&byte| byte == quote || escapes && byte == b'\\');
            let width = token.unwrap_or(bytes.len());
            self.consume(width, true)?;
            if token.is_some() {
                let bytes = self.input.peek(2, checkpoint)?;
                if bytes[0] != quote {
                    if bytes.len() < 2 {
                        return Err("SQL 文件中的转义序列没有结束。".into());
                    }
                    self.consume(2, true)?;
                } else if bytes.get(1) == Some(&quote) {
                    self.consume(2, true)?;
                } else {
                    self.consume(1, true)?;
                    return Ok(());
                }
            }
        }
    }
}

fn directive_prefix(bytes: &[u8]) -> bool {
    (bytes
        .get(..9)
        .is_some_and(|word| word.eq_ignore_ascii_case(b"DELIMITER"))
        && bytes.get(9).is_some_and(u8::is_ascii_whitespace))
        || bytes.starts_with(b"\\d") && bytes.get(2).is_some_and(u8::is_ascii_whitespace)
}

struct Window<R> {
    reader: R,
    bytes: Vec<u8>,
    start: usize,
    position: u64,
    eof: bool,
}

impl<R: Read> Window<R> {
    fn new(reader: R) -> Self {
        Self {
            reader,
            bytes: Vec::new(),
            start: 0,
            position: 0,
            eof: false,
        }
    }

    fn remaining(&self) -> &[u8] {
        &self.bytes[self.start..]
    }

    fn consume(&mut self, width: usize) {
        self.start += width;
        self.position += width as u64;
    }

    fn peek(&mut self, needed: usize, checkpoint: &mut Checkpoint<'_>) -> Result<&[u8], String> {
        while self.remaining().len() < needed && !self.eof {
            checkpoint(self.position)?;
            let used = self.remaining().len();
            self.bytes.copy_within(self.start.., 0);
            self.bytes.resize(used + BUFFER_SIZE, 0);
            self.start = 0;
            let read = loop {
                match self.reader.read(&mut self.bytes[used..]) {
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                    Err(error) => return Err(format!("无法读取 SQL 文件。{error}")),
                    Ok(count) => break count,
                }
            };
            self.bytes.truncate(used + read);
            self.eof = read == 0;
        }
        Ok(self.remaining())
    }
}
