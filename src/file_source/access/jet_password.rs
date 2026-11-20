use encoding_rs::EncoderResult;
use jetdb::{FileError, PageReader, format::db_header};
use zeroize::Zeroizing;

use super::open_error;

const JET3_PASSWORD_BYTES: usize = 20;
const JET4_PASSWORD_BYTES: usize = 40;
const JET4_CREATION_DATE: usize = 0x72;

pub(super) fn verify(reader: &mut PageReader, password: Option<&str>) -> Result<(), String> {
    let header = reader.header();
    if header.version.is_accdb() {
        return Ok(());
    }
    let jet3 = header.version.is_jet3();
    let code_page = header.code_page;
    let page = reader.read_page(0).map_err(open_error)?;
    // JetDB has already decoded the common header. Only Jet 4 applies a second mask to this field.
    let (width, mask) = if jet3 {
        (JET3_PASSWORD_BYTES, [0; 4])
    } else {
        let raw = page[JET4_CREATION_DATE..][..8].try_into().unwrap();
        let date = f64::from_le_bytes(raw);
        if !date.is_finite() {
            return Err("旧版 MDB 的密码标记无效。".into());
        }
        (JET4_PASSWORD_BYTES, (date as i32).to_le_bytes())
    };
    let stored = &page[db_header::PASSWORD_JET3..][..width];
    if stored.iter().zip(mask.iter().cycle()).all(|(b, m)| b == m) {
        return Ok(());
    }

    let password = password
        .filter(|password| !password.is_empty())
        .ok_or_else(|| open_error(FileError::PasswordRequired))?;
    let invalid = || open_error(FileError::InvalidPassword);
    if password.contains('\0') {
        return Err(invalid());
    }
    // Encode only the supplied password. Compare the padded bytes without reconstructing the stored password.
    let mut candidate = Zeroizing::new([0; JET4_PASSWORD_BYTES]);
    if jet3 {
        let encoding = codepage::to_encoding_no_replacement(code_page)
            .filter(|encoding| *encoding == encoding.output_encoding())
            .ok_or_else(|| format!("旧版 MDB 的密码代码页 {code_page} 暂不支持。"))?;
        let (result, _, written) = encoding.new_encoder().encode_from_utf8_without_replacement(
            password,
            &mut *candidate,
            true,
        );
        if result != EncoderResult::InputEmpty || written > width {
            return Err(invalid());
        }
    } else {
        for (index, unit) in password.encode_utf16().enumerate() {
            let Some(bytes) = candidate.get_mut(index * 2..index * 2 + 2) else {
                return Err(invalid());
            };
            bytes.copy_from_slice(&unit.to_le_bytes());
        }
    }

    let difference = stored
        .iter()
        .zip(candidate.iter())
        .zip(mask.iter().cycle())
        .fold(0, |diff, ((stored, candidate), mask)| {
            diff | (stored ^ candidate ^ mask)
        });
    if difference == 0 {
        Ok(())
    } else {
        Err(invalid())
    }
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use jetdb::format::JetVersion;

    use super::*;

    #[test]
    fn authenticates_jet3_and_jet4_test_passwords() {
        // Page 0 metadata captured from synthetic DAO 3.6 databases created with CP936
        // and the same test password. Keep only the bytes needed for header validation.
        let jet3 = [
            0xd3, 0x41, 0xd8, 0xdb, 0x54, 0xef, 0x57, 0x02, 0x5d, 0x89, 0x9e, 0x98, 0x5e, 0x11,
            0x87, 0x6f, 0x11, 0xe6, 0x13, 0xb6, 0x8a, 0x60, 0x54, 0x94, 0x7b, 0x36,
        ];
        let jet4 = [
            0xd3, 0x41, 0x53, 0x29, 0x54, 0xef, 0xbb, 0x26, 0xc0, 0x50, 0x91, 0xaa, 0x9d, 0x82,
            0x8d, 0xeb, 0x19, 0xe6, 0x20, 0x03, 0x8a, 0x60, 0x5e, 0x21, 0x7b, 0x36, 0xff, 0xc7,
            0xdf, 0xb1, 0x7d, 0x41, 0x13, 0x43, 0xc5, 0x1a, 0xb1, 0x33, 0x3e, 0xd4, 0x79, 0x5b,
            0x98, 0x00, 0x7c, 0x2a, 0x4c, 0xd7, 0x7c, 0x99, 0x05, 0x13, 0x98, 0xfd, 0x29, 0x44,
            0x4c, 0xfc, 0x29, 0x9f, 0x86, 0x66,
        ];
        for (version, bytes) in [
            (JetVersion::Jet3, jet3.as_slice()),
            (JetVersion::Jet4, &jet4),
        ] {
            let mut page = vec![0; version.format().page_size];
            page[db_header::SIGNATURE..][..db_header::SIGNATURE_JET.len()]
                .copy_from_slice(db_header::SIGNATURE_JET);
            page[db_header::VERSION] = version as u8;
            page[db_header::CODE_PAGE..][..bytes.len()].copy_from_slice(bytes);
            let mut reader = PageReader::open_reader(Cursor::new(page)).unwrap();

            verify(&mut reader, Some("样本密码A19")).unwrap();
        }
    }
}
