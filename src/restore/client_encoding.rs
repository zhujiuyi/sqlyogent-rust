use crate::sql_encoding::SqlDumpCharset;
use crate::sql_tools::{EditorToken, editor_tokens, set_assignments};

const UNSUPPORTED_CHANGE: &str = "旧编码还原无法安全执行此客户端字符集设置。支持文件开头的简单 SET NAMES 和 SQLyog 的标准字符集声明。";
const CHARSET_CONFLICT: &str = "脚本的 SET NAMES 声明与所选文件编码不一致。";
const SQLYOG_NAMES_PREFIX: &str = "/*!40101 SET NAMES ";

pub(crate) struct ExplicitClientEncoding {
    charset: SqlDumpCharset,
    leading: bool,
    plain_names_seen: bool,
}

impl ExplicitClientEncoding {
    pub(crate) fn new(charset: SqlDumpCharset) -> Self {
        Self {
            charset,
            leading: true,
            plain_names_seen: false,
        }
    }

    pub(crate) fn query<'a>(&mut self, sql: &'a str, mode: &str) -> Result<&'a str, String> {
        let mut tokens = significant_tokens(sql, mode).peekable();
        let Some(first) = tokens.next() else {
            return Ok(sql);
        };
        if let Some(body) = executable_body(first.text) {
            if let Some(name) = first
                .text
                .strip_prefix(SQLYOG_NAMES_PREFIX)
                .and_then(|text| text.strip_suffix(" */"))
                .filter(|name| ascii_charset_name(name))
                && tokens.peek().is_none()
            {
                self.require_matching_name(name)?;
                // Keep the original executable-comment version condition.
                return Ok("/*!40101 SET NAMES utf8mb4 */");
            }
            self.check_unrecognized_statement(body, mode)?;
            for token in tokens {
                if let Some(executable) = executable_body(token.text) {
                    self.check_unrecognized_statement(executable, mode)?;
                } else {
                    self.check_unrecognized_statement(&sql[token.range.start..], mode)?;
                    break;
                }
            }
            return Ok(sql);
        }
        if first.text.eq_ignore_ascii_case("SET") {
            if let Some(second) = tokens.next()
                && second.text.eq_ignore_ascii_case("NAMES")
            {
                let name = tokens.next().map(|token| token.text).and_then(literal_name);
                if !self.leading || self.plain_names_seen || tokens.next().is_some() {
                    return Err(UNSUPPORTED_CHANGE.into());
                }
                self.require_matching_name(name.ok_or(UNSUPPORTED_CHANGE)?)?;
                self.plain_names_seen = true;
                return Ok("SET NAMES utf8mb4");
            }
            if changes_client_charset(&sql[first.range.end..], mode) {
                return Err(UNSUPPORTED_CHANGE.into());
            }
        } else if !first.text.eq_ignore_ascii_case("USE") {
            self.leading = false;
        }
        Ok(sql)
    }

    fn require_matching_name(&self, name: &str) -> Result<(), String> {
        let declared = SqlDumpCharset::parse(name);
        let utf8 = |charset| matches!(charset, SqlDumpCharset::Utf8Mb3 | SqlDumpCharset::Utf8Mb4);
        if declared
            .is_some_and(|charset| charset == self.charset || utf8(charset) && utf8(self.charset))
        {
            Ok(())
        } else {
            Err(CHARSET_CONFLICT.into())
        }
    }

    fn check_unrecognized_statement(&mut self, sql: &str, mode: &str) -> Result<(), String> {
        if let Some(first) = significant_tokens(sql, mode).next() {
            if first.text.eq_ignore_ascii_case("SET") {
                if changes_client_charset(&sql[first.range.end..], mode) {
                    return Err(UNSUPPORTED_CHANGE.into());
                }
            } else if !first.text.eq_ignore_ascii_case("USE") {
                self.leading = false;
            }
        }
        Ok(())
    }
}

fn significant_tokens<'a>(sql: &'a str, mode: &str) -> impl Iterator<Item = EditorToken<'a>> {
    let has_mode = |name| {
        mode.split(',')
            .any(|mode| mode.trim().eq_ignore_ascii_case(name))
    };
    editor_tokens(
        sql,
        has_mode("NO_BACKSLASH_ESCAPES"),
        has_mode("ANSI_QUOTES"),
    )
    .filter(|token| {
        let text = token.text;
        !(text.starts_with("--")
            || text.starts_with('#')
            || text.starts_with("/*") && executable_body(text).is_none())
    })
}

fn executable_body(comment: &str) -> Option<&str> {
    comment
        .strip_prefix("/*!")
        .or_else(|| comment.strip_prefix("/*M!"))
        .and_then(|body| body.strip_suffix("*/"))
        .map(|body| {
            body.trim_start_matches(|ch: char| ch.is_ascii_digit())
                .trim_ascii()
        })
}

fn changes_client_charset(assignments: &str, mode: &str) -> bool {
    if significant_tokens(assignments, mode).any(|token| executable_body(token.text).is_some()) {
        return true;
    }
    let mut words = significant_tokens(assignments, mode);
    if let Some(first) = words.next()
        && (first.text.eq_ignore_ascii_case("NAMES")
            || (first.text.eq_ignore_ascii_case("CHARACTER")
                && words
                    .next()
                    .is_some_and(|next| next.text.eq_ignore_ascii_case("SET"))))
    {
        return true;
    }
    set_assignments(assignments, mode).iter().any(|assignment| {
        let mut tokens = significant_tokens(assignment, mode).peekable();
        // A single @ targets a user variable. @@ targets a system variable.
        let user_variable = tokens.peek().is_some_and(|token| token.text == "@") && {
            tokens.next();
            tokens.peek().is_none_or(|token| token.text != "@")
        };
        !user_variable
            && tokens.take_while(|token| token.text != "=").any(|token| {
                [
                    "character_set_client",
                    "character_set_connection",
                    "collation_connection",
                ]
                .iter()
                .any(|variable| {
                    token
                        .text
                        .trim_matches(['`', '"'])
                        .eq_ignore_ascii_case(variable)
                })
            })
    })
}

fn literal_name(name: &str) -> Option<&str> {
    let name = name
        .strip_prefix('\'')
        .and_then(|name| name.strip_suffix('\''))
        .or_else(|| {
            name.strip_prefix('"')
                .and_then(|name| name.strip_suffix('"'))
        })
        .unwrap_or(name);
    (ascii_charset_name(name) && !name.eq_ignore_ascii_case("DEFAULT")).then_some(name)
}

fn ascii_charset_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_legacy_charset_only_neutralizes_known_header_statements() {
        let mut codec = ExplicitClientEncoding::new(SqlDumpCharset::Gbk);
        assert_eq!(
            codec.query("SET /* 头部 */ NAMES 'gbk'", "").unwrap(),
            "SET NAMES utf8mb4"
        );
        let routine = "CREATE PROCEDURE p() BEGIN SELECT 'SET NAMES latin1'; END";
        assert_eq!(codec.query(routine, "").unwrap(), routine);
        assert_eq!(
            codec.query("/*!40101 SET NAMES gbk */", "").unwrap(),
            "/*!40101 SET NAMES utf8mb4 */"
        );
        assert!(
            codec
                .query("SET NAMES gbk", "")
                .unwrap_err()
                .contains("无法安全执行")
        );

        let mut codec = ExplicitClientEncoding::new(SqlDumpCharset::Gbk);
        assert!(
            codec
                .query("/*!40101 SET NAMES latin1 */", "")
                .unwrap_err()
                .contains("不一致")
        );
        assert!(codec.query("SET NAMES gbk COLLATE gbk_bin", "").is_err());
        let save = "SET @saved_client = @@SESSION.character_set_client";
        assert_eq!(codec.query(save, "").unwrap(), save);
        assert!(
            codec
                .query("SET @@SESSION.character_set_client = @saved_client", "")
                .is_err()
        );
        assert!(
            codec
                .query("SET @x=1 /*!40101 , character_set_client=gbk */", "")
                .is_err()
        );
    }
}
