use base64::{Engine as _, engine::general_purpose::STANDARD};
use quick_xml::{
    Reader, XmlVersion,
    events::{BytesStart, Event},
};

use super::{Field, MAX_RESPONSE_BYTES, Reply, ResultSet, ServerError, VERSION, is_xml_text};

const MAX_RESULT_SETS: usize = 128;
const MAX_METADATA_BYTES: usize = 1024 * 1024;

pub(crate) fn parse_response(body: &[u8], row_limit: usize) -> Result<Reply, String> {
    if body.len() > MAX_RESPONSE_BYTES {
        return Err("隧道响应超过 64 MiB。".into());
    }
    let mut xml = Xml(Reader::from_reader(body));
    xml.0.config_mut().expand_empty_elements = true;
    let mut first = xml.markup()?;
    if matches!(first, Event::Decl(_)) {
        first = xml.markup()?;
    }
    if !matches!(first, Event::Start(ref start) if start.name().as_ref() == b"xml") {
        return Err("SQLyog PHP 隧道没有返回 XML 查询结果。".into());
    }

    let mut reply = Reply {
        sets: Vec::new(),
        error: None,
        omitted_sets: 0,
    };
    while let Some(result) = xml.child(b"xml")? {
        if result.name().as_ref() != b"result" || reply.error.is_some() {
            return Err(invalid_reply());
        }
        if attribute(&result, "v")?.as_deref() != Some(VERSION) {
            return Err("查询响应的 SQLyog PHP 隧道版本不匹配。".into());
        }
        let retain = reply.sets.len() < MAX_RESULT_SETS;
        match read_result(&mut xml, if retain { row_limit } else { 0 })? {
            Record::Data(result) if retain => reply.sets.push(result),
            Record::Data(_) => reply.omitted_sets += 1,
            Record::Error(error) => reply.error = Some(error),
        }
    }
    if !matches!(xml.markup()?, Event::Eof) || (reply.sets.is_empty() && reply.error.is_none()) {
        return Err(invalid_reply());
    }
    Ok(reply)
}

enum Record {
    Data(ResultSet),
    Error(ServerError),
}

fn read_result(xml: &mut Xml<'_>, row_limit: usize) -> Result<Record, String> {
    let mut error = None;
    let mut server_version = None;
    let mut info = None;
    let mut affected_rows = None;
    let mut insert_id = None;
    let mut columns = None;
    let mut rows = None;
    while let Some((tag, field_prefix)) = xml.result_child()? {
        match tag.name().as_ref() {
            b"e_i" => set_once(&mut error, read_error(xml)?)?,
            b"s_v" => set_once(&mut server_version, xml.text(&tag, MAX_METADATA_BYTES)?)?,
            b"m_i" => set_once(&mut info, xml.text(&tag, MAX_METADATA_BYTES)?)?,
            b"a_r" => {
                let value = xml.text(&tag, MAX_METADATA_BYTES)?;
                let value = if value.trim_ascii() == "-1" {
                    0
                } else {
                    number(&value)?
                };
                set_once(&mut affected_rows, value)?;
            }
            b"i_i" => set_once(
                &mut insert_id,
                number(&xml.text(&tag, MAX_METADATA_BYTES)?)?,
            )?,
            b"f_i" => {
                let fields = read_fields(xml, &tag)?;
                if field_prefix.is_some_and(|prefix| prefix != fields.len() as u64) {
                    return Err(invalid_reply());
                }
                set_once(&mut columns, fields)?;
            }
            b"r_i" => {
                let field_count = columns.as_ref().ok_or_else(invalid_reply)?.len();
                let result = read_rows(xml, &tag, field_count, row_limit)?;
                set_once(&mut rows, result)?;
            }
            _ => return Err(invalid_reply()),
        }
    }
    if let Some(error) = error.ok_or_else(invalid_reply)? {
        return Ok(Record::Error(error));
    }
    let (rows, truncated) = rows.ok_or_else(invalid_reply)?;
    Ok(Record::Data(ResultSet {
        columns: columns.ok_or_else(invalid_reply)?,
        rows,
        affected_rows: affected_rows.ok_or_else(invalid_reply)?,
        last_insert_id: insert_id.filter(|id| *id != 0),
        server_version: server_version.unwrap_or_default(),
        info: info.filter(|message| !message.is_empty()),
        truncated,
    }))
}

fn read_error(xml: &mut Xml<'_>) -> Result<Option<ServerError>, String> {
    let mut code = None;
    let mut message = None;
    while let Some(tag) = xml.child(b"e_i")? {
        let text = xml.text(&tag, MAX_METADATA_BYTES)?;
        match tag.name().as_ref() {
            b"e_n" => set_once(
                &mut code,
                u32::try_from(number(&text)?).map_err(|_| invalid_reply())?,
            )?,
            b"e_d" => set_once(&mut message, text)?,
            _ => return Err(invalid_reply()),
        }
    }
    match (code, message) {
        (None, None) => Ok(None),
        (Some(code), Some(message)) => Ok(Some(ServerError { code, message })),
        _ => Err(invalid_reply()),
    }
}

fn read_fields(xml: &mut Xml<'_>, start: &BytesStart<'_>) -> Result<Vec<Field>, String> {
    let count = attribute(start, "c")?.ok_or_else(invalid_reply)?;
    let count = usize::try_from(number(&count)?).map_err(|_| invalid_reply())?;
    let mut fields = Vec::new();
    while let Some(field) = xml.child(b"f_i")? {
        if field.name().as_ref() != b"f" || fields.len() >= count {
            return Err(invalid_reply());
        }
        let mut name = None;
        let mut table = None;
        let mut data_type = None;
        let mut length = None;
        let mut default = None;
        while let Some(tag) = xml.child(b"f")? {
            let value = xml.text(&tag, MAX_METADATA_BYTES)?;
            match tag.name().as_ref() {
                b"n" => set_once(&mut name, value)?,
                b"t" => set_once(&mut table, value)?,
                b"ty" => set_once(&mut data_type, value)?,
                b"m" => set_once(
                    &mut length,
                    if value.trim_ascii().is_empty() {
                        None
                    } else {
                        Some(number(&value)?)
                    },
                )?,
                b"d" => set_once(&mut default, value)?,
                _ => return Err(invalid_reply()),
            }
        }
        fields.push(Field {
            name: name.ok_or_else(invalid_reply)?,
            table: table.unwrap_or_default(),
            data_type: data_type.unwrap_or_default(),
            max_length: length.flatten(),
            default_text: default.filter(|text| !text.is_empty()),
        });
    }
    if fields.len() != count {
        return Err(invalid_reply());
    }
    Ok(fields)
}

type Rows = Vec<Vec<Option<Vec<u8>>>>;

fn read_rows(
    xml: &mut Xml<'_>,
    start: &BytesStart<'_>,
    field_count: usize,
    row_limit: usize,
) -> Result<(Rows, bool), String> {
    let declared_rows = attribute(start, "c")?.as_deref().map(number).transpose()?;
    let mut visited = 0;
    let mut rows = Vec::new();
    while let Some(row) = xml.child(b"r_i")? {
        if row.name().as_ref() != b"r" || field_count == 0 {
            return Err(invalid_reply());
        }
        let mut width = 0;
        let retain = rows.len() < row_limit;
        let mut cells = Vec::new();
        while let Some(cell) = xml.child(b"r")? {
            if cell.name().as_ref() != b"c" || width >= field_count {
                return Err(invalid_reply());
            }
            let value = read_cell(xml, &cell)?;
            if retain {
                cells.push(value);
            }
            width += 1;
        }
        if width != field_count {
            return Err(invalid_reply());
        }
        if retain {
            rows.push(cells);
        }
        visited += 1;
    }
    if declared_rows.is_some_and(|declared| declared != visited) {
        return Err(invalid_reply());
    }
    let truncated = visited > rows.len() as u64;
    Ok((rows, truncated))
}

fn read_cell(xml: &mut Xml<'_>, tag: &BytesStart<'_>) -> Result<Option<Vec<u8>>, String> {
    let length = number(&attribute(tag, "l")?.ok_or_else(invalid_reply)?)?;
    let value = xml.text(tag, MAX_RESPONSE_BYTES)?;
    let value = value.trim_ascii();
    if length == 0 && value == "(NULL)" {
        return Ok(None);
    }
    if length == 0 && (value == "_" || value.is_empty()) {
        return Ok(Some(Vec::new()));
    }
    let bytes = STANDARD.decode(value).map_err(|_| invalid_reply())?;
    if bytes.len() as u64 != length {
        return Err(invalid_reply());
    }
    Ok(Some(bytes))
}

struct Xml<'a>(Reader<&'a [u8]>);

impl<'a> Xml<'a> {
    fn markup(&mut self) -> Result<Event<'a>, String> {
        loop {
            match self.0.read_event().map_err(|_| invalid_reply())? {
                Event::Comment(_) => {}
                Event::Text(text) if text.iter().all(u8::is_ascii_whitespace) => {}
                event => return Ok(event),
            }
        }
    }

    fn child(&mut self, parent: &[u8]) -> Result<Option<BytesStart<'a>>, String> {
        match self.markup()? {
            Event::Start(start) => Ok(Some(start)),
            Event::End(end) if end.name().as_ref() == parent => Ok(None),
            _ => Err(invalid_reply()),
        }
    }

    fn result_child(&mut self) -> Result<Option<(BytesStart<'a>, Option<u64>)>, String> {
        let mut event = self.markup()?;
        // The 8.05 PHP script can print mysql_num_fields before the f_i element.
        let prefix = if let Event::Text(text) = event {
            let digits = std::str::from_utf8(&text)
                .map_err(|_| invalid_reply())?
                .trim_ascii();
            if digits.is_empty() || !digits.bytes().all(|digit| digit.is_ascii_digit()) {
                return Err(invalid_reply());
            }
            let count = number(digits)?;
            event = self.markup()?;
            Some(count)
        } else {
            None
        };
        match event {
            Event::Start(start) if prefix.is_none() || start.name().as_ref() == b"f_i" => {
                Ok(Some((start, prefix)))
            }
            Event::End(end) if prefix.is_none() && end.name().as_ref() == b"result" => Ok(None),
            _ => Err(invalid_reply()),
        }
    }

    fn text(&mut self, tag: &BytesStart<'_>, limit: usize) -> Result<String, String> {
        let mut value = String::new();
        loop {
            let event = self.0.read_event().map_err(|_| invalid_reply())?;
            let text = match &event {
                Event::Text(text) => text.xml10_content().map_err(|_| invalid_reply())?,
                Event::CData(text) => text.xml10_content().map_err(|_| invalid_reply())?,
                Event::GeneralRef(reference) => {
                    let character =
                        match reference.resolve_char_ref().map_err(|_| invalid_reply())? {
                            Some(character) => character,
                            None => match &reference[..] {
                                b"amp" => '&',
                                b"lt" => '<',
                                b"gt" => '>',
                                b"apos" => '\'',
                                b"quot" => '"',
                                _ => return Err(invalid_reply()),
                            },
                        };
                    let mut buffer = [0; 4];
                    append_text(&mut value, character.encode_utf8(&mut buffer), limit)?;
                    continue;
                }
                Event::Comment(_) => continue,
                Event::End(end) if tag.name() == end.name() => return Ok(value),
                _ => return Err(invalid_reply()),
            };
            append_text(&mut value, &text, limit)?;
        }
    }
}

fn append_text(value: &mut String, text: &str, limit: usize) -> Result<(), String> {
    if text.len() > limit.saturating_sub(value.len()) || !is_xml_text(text) {
        return Err(invalid_reply());
    }
    value.push_str(text);
    Ok(())
}

fn attribute(tag: &BytesStart<'_>, name: &str) -> Result<Option<String>, String> {
    let mut value = None;
    for attribute in tag.attributes() {
        let attribute = attribute.map_err(|_| invalid_reply())?;
        if attribute.key.as_ref() == name.as_bytes() {
            let text = attribute
                .normalized_value(XmlVersion::Implicit1_0)
                .map_err(|_| invalid_reply())?;
            set_once(&mut value, text.into_owned())?;
        }
    }
    Ok(value)
}

fn set_once<T>(slot: &mut Option<T>, value: T) -> Result<(), String> {
    if slot.replace(value).is_some() {
        return Err(invalid_reply());
    }
    Ok(())
}

fn number(value: &str) -> Result<u64, String> {
    value.trim_ascii().parse().map_err(|_| invalid_reply())
}

fn invalid_reply() -> String {
    "SQLyog PHP 隧道返回的 XML 查询结果格式不正确。".into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_archived_php_field_count_before_the_metadata_element() {
        let raw = br#"<xml><result v="8.05"><e_i></e_i><s_v>11.4.13-MariaDB</s_v><m_i></m_i>
<a_r>1</a_r><i_i>0</i_i>2<f_i c="2">
<f><n>label</n><t></t><m>6</m><d></d><ty>varchar</ty></f>
<f><n>payload</n><t></t><m>2</m><d></d><ty>varchar</ty></f>
</f_i><r_i c="1"><r><c l="6">5riv5rm+</c><c l="2">AP8=</c></r></r_i></result></xml>"#;
        let parsed = parse_response(raw, 1).unwrap();
        assert_eq!(parsed.sets[0].columns[0].name, "label");
        assert_eq!(
            parsed.sets[0].rows[0][0].as_deref(),
            Some("港湾".as_bytes())
        );
        assert_eq!(parsed.sets[0].rows[0][1].as_deref(), Some(&[0, 255][..]));
    }

    #[test]
    fn decodes_columns_binary_null_and_empty_text_with_a_row_limit() {
        let response = r#"<xml><result v="8.05"><e_i/>
<s_v>11.4.13-MariaDB</s_v><m_i>info &amp; details</m_i><a_r>2</a_r><i_i>0</i_i>
<f_i c="3">
<f><n>&#x540D;</n><t>demo</t><m>0</m><d/><ty>varchar</ty></f>
<f><n>optional</n><t>demo</t><m>0</m><d/><ty>varchar</ty></f>
<f><n>raw</n><t>demo</t><m>2</m><d/><ty>blob</ty></f>
</f_i><r_i c="2"><r><c l="0">_</c><c l="0">(NULL)</c><c l="2">AP8=</c></r>
<r><c l="1">QQ==</c><c l="1">Qg==</c><c l="1">Qw==</c></r></r_i></result></xml>"#;
        let reply = parse_response(response.as_bytes(), 1).unwrap();
        assert!(reply.error.is_none());
        let set = &reply.sets[0];
        assert_eq!(set.server_version, "11.4.13-MariaDB");
        assert_eq!(set.columns[0].name, "名");
        assert_eq!(set.columns[2].data_type, "blob");
        assert_eq!(set.info.as_deref(), Some("info & details"));
        assert_eq!(
            set.rows,
            vec![vec![Some(Vec::new()), None, Some(vec![0, 255])]]
        );
        assert!(set.truncated);
    }

    #[test]
    fn preserves_previous_results_and_mysql_error_number() {
        let body = br#"<xml><result v="8.05"><e_i/><s_v>5.1.40</s_v><m_i/>
<a_r>1</a_r><i_i>42</i_i><f_i c="0"/><r_i/></result>
<result v="8.05"><e_i><e_n>1146</e_n><e_d>Table &apos;sample&apos; does not exist</e_d></e_i></result></xml>"#;
        let reply = parse_response(body, 10).unwrap();
        assert_eq!(reply.sets[0].affected_rows, 1);
        assert_eq!(reply.sets[0].last_insert_id, Some(42));
        let error = reply.error.unwrap();
        assert_eq!(error.code, 1146);
        assert_eq!(error.message, "Table 'sample' does not exist");
    }
}
