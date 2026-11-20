//! SQLyog's XML protocol for the 8.05 PHP tunnel. HTTP authentication and
//! transport belong to the caller. Each query request opens a fresh session.

use std::fmt::Write;

use base64::{Engine as _, engine::general_purpose::STANDARD};

mod response;

pub(crate) use response::parse_response;

pub(crate) const VERSION: &str = "8.05";
pub(crate) const VERSION_QUERY: &str = "app=sqlyog&version=8.05";
pub(crate) const VERSION_BODY: &[u8] = b"garbagedata";
pub(crate) const CONTENT_TYPES: [&str; 4] = [
    "text/xml",
    "text/html",
    "text/plain",
    "application/x-www-form-urlencoded",
];
pub(crate) const MAX_RESPONSE_BYTES: usize = 64 * 1024 * 1024;
pub(crate) const MAX_REQUEST_BYTES: usize = 64 * 1024 * 1024;

pub(crate) struct ConnectInfo<'a> {
    pub host: &'a str,
    pub port: u16,
    pub user: &'a str,
    pub password: &'a str,
    pub database: &'a str,
    pub charset: &'a str,
}

pub(crate) struct RequestBody {
    pub bytes: Vec<u8>,
    /// Send the `Base64: yes` header when the connection fields are encoded.
    pub base64_header: bool,
}

#[derive(Debug)]
pub(crate) struct Reply {
    pub sets: Vec<ResultSet>,
    pub error: Option<ServerError>,
    pub omitted_sets: usize,
}

#[derive(Debug)]
pub(crate) struct ResultSet {
    pub columns: Vec<Field>,
    pub rows: Vec<Vec<Option<Vec<u8>>>>,
    pub affected_rows: u64,
    pub last_insert_id: Option<u64>,
    pub server_version: String,
    pub info: Option<String>,
    pub truncated: bool,
}

#[derive(Debug, Default)]
pub(crate) struct Field {
    pub name: String,
    pub table: String,
    pub data_type: String,
    pub max_length: Option<u64>,
    pub default_text: Option<String>,
}

#[derive(Debug)]
pub(crate) struct ServerError {
    pub code: u32,
    pub message: String,
}

pub(crate) fn check_version(body: &[u8]) -> Result<(), String> {
    let version = std::str::from_utf8(body)
        .ok()
        .and_then(|text| text.trim_ascii().strip_prefix("TunnelVersion:"));
    match version {
        Some(VERSION) => Ok(()),
        Some("PHP_VERSION_ERROR") => Err("隧道服务器的 PHP 版本不受支持。".into()),
        Some("PHP_MODULE_NOT_INSTALLED") => Err("隧道服务器缺少所需的 PHP 扩展。".into()),
        Some(_) => Err("SQLyog PHP 隧道版本不匹配，需要 8.05 协议。".into()),
        None => Err("该地址没有返回 SQLyog PHP 隧道版本。".into()),
    }
}

/// `batch` tells the PHP endpoint to execute a script in one connection and
/// return the results of the last statement. The content type changes only
/// the HTTP header. All four legacy modes send these same raw XML bytes.
pub(crate) fn encode_query(
    connection: &ConnectInfo<'_>,
    sql: &str,
    batch: bool,
    force_base64: bool,
) -> Result<RequestBody, String> {
    if connection.port == 0 || connection.host.is_empty() || connection.user.is_empty() {
        return Err("请填写隧道端的数据库主机、端口和用户名。".into());
    }
    if connection.database.contains('`') {
        return Err("SQLyog PHP 隧道的默认数据库名称不可包含反引号。".into());
    }
    if !connection.charset.is_empty()
        && connection.charset != "[default]"
        && !connection
            .charset
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
    {
        return Err("隧道字符集名称无效。".into());
    }
    let port = connection.port.to_string();
    let fields = [
        ("host", connection.host),
        ("user", connection.user),
        ("password", connection.password),
        ("db", connection.database),
        ("charset", connection.charset),
        ("port", port.as_str()),
    ];
    let base64_header = force_base64 || fields.iter().any(|(_, field)| !is_xml_text(field));
    let query_encoded = base64_header || !is_xml_text(sql);
    let mut xml = String::from("<xml><connect_info>");
    for (tag, field) in fields {
        write!(xml, "<{tag} e='{}'>", u8::from(base64_header)).unwrap();
        write_value(&mut xml, field, base64_header)?;
        write!(xml, "</{tag}>").unwrap();
    }
    write!(
        xml,
        "</connect_info><query_info><query b='{}' e='{}'>",
        u8::from(batch),
        u8::from(query_encoded)
    )
    .unwrap();
    write_value(&mut xml, sql, query_encoded)?;
    write!(
        xml,
        "</query><querylen>{}</querylen></query_info></xml>",
        sql.len()
    )
    .unwrap();
    if xml.len() > MAX_REQUEST_BYTES {
        return Err("隧道请求超过 64 MiB。".into());
    }
    Ok(RequestBody {
        bytes: xml.into_bytes(),
        base64_header,
    })
}

fn is_xml_text(text: &str) -> bool {
    text.chars().all(|character| matches!(character,
        '\t' | '\n' | '\r' | '\u{20}'..='\u{d7ff}' | '\u{e000}'..='\u{fffd}' | '\u{10000}'..='\u{10ffff}'
    ))
}

fn write_value(xml: &mut String, value: &str, encoded: bool) -> Result<(), String> {
    if value.len() > MAX_REQUEST_BYTES.saturating_sub(xml.len()) {
        return Err("隧道请求超过 64 MiB。".into());
    }
    if encoded {
        let encoded_len = value.len().div_ceil(3) * 4;
        if encoded_len > MAX_REQUEST_BYTES - xml.len() {
            return Err("隧道请求超过 64 MiB。".into());
        }
        STANDARD.encode_string(value, xml);
    } else {
        for character in value.chars() {
            match character {
                '&' => xml.push_str("&amp;"),
                '<' => xml.push_str("&lt;"),
                '>' => xml.push_str("&gt;"),
                '\'' => xml.push_str("&apos;"),
                '"' => xml.push_str("&quot;"),
                '\r' => xml.push_str("&#13;"),
                _ => xml.push(character),
            }
            if xml.len() > MAX_REQUEST_BYTES {
                return Err("隧道请求超过 64 MiB。".into());
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sends_legacy_fields_and_preserves_multibyte_query_length() {
        let connection = ConnectInfo {
            host: "localhost",
            port: 3307,
            user: "fixture",
            password: "a&b",
            database: "库",
            charset: "utf8",
        };
        let sql = "SELECT '<测试>'";
        let request = encode_query(&connection, sql, false, false).unwrap();
        assert!(!request.base64_header);
        assert_eq!(
            String::from_utf8(request.bytes).unwrap(),
            "<xml><connect_info><host e='0'>localhost</host><user e='0'>fixture</user><password e='0'>a&amp;b</password><db e='0'>库</db><charset e='0'>utf8</charset><port e='0'>3307</port></connect_info><query_info><query b='0' e='0'>SELECT &apos;&lt;测试&gt;&apos;</query><querylen>17</querylen></query_info></xml>"
        );

        let request = encode_query(&connection, "SELECT 1", true, true).unwrap();
        assert!(request.base64_header);
        let body = String::from_utf8(request.bytes).unwrap();
        assert!(body.contains("<port e='1'>MzMwNw==</port>"));
        assert!(body.contains("<query b='1' e='1'>U0VMRUNUIDE=</query><querylen>8</querylen>"));
    }

    #[test]
    fn checks_the_version_probe_response() {
        assert!(check_version(b"TunnelVersion:8.05").is_ok());
        assert!(
            check_version(b"TunnelVersion:PHP_MODULE_NOT_INSTALLED")
                .unwrap_err()
                .contains("扩展")
        );
        assert!(check_version(b"<html>sign in</html>").is_err());
    }
}
