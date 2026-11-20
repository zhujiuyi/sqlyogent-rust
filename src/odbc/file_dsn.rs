use std::path::Path;

pub fn validate_file_dsn_path(path: &Path) -> Result<(), String> {
    if path
        .to_str()
        .is_none_or(|path| path.chars().any(char::is_control))
        || !path
            .extension()
            .is_some_and(|extension| extension.eq_ignore_ascii_case("dsn"))
    {
        return Err("请选择有效的 .dsn 文件路径。".into());
    }
    Ok(())
}

pub fn file_dsn_connection(
    path: &Path,
    user: Option<&str>,
    password: Option<&str>,
) -> Result<String, String> {
    validate_file_dsn_path(path)?;
    let path = std::path::absolute(path).map_err(|error| format!("无法定位文件 DSN。{error}"))?;
    if !std::fs::metadata(&path)
        .map_err(|error| format!("无法打开文件 DSN。{error}"))?
        .is_file()
    {
        return Err("请选择普通的 .dsn 文件。".into());
    }
    let path = path.to_str().ok_or("无法以 Unicode 表示文件 DSN 路径。")?;
    if [user, password]
        .into_iter()
        .flatten()
        .any(|value| value.contains('\0'))
    {
        return Err("ODBC 运行时凭据包含无效字符。".into());
    }
    let mut connection = String::new();
    connection_attribute(&mut connection, "FILEDSN", path);
    if let Some(user) = user {
        connection_attribute(&mut connection, "UID", user);
    }
    if let Some(password) = password {
        connection_attribute(&mut connection, "PWD", password);
    }
    Ok(connection)
}

fn connection_attribute(connection: &mut String, key: &str, value: &str) {
    connection.push_str(key);
    connection.push('=');
    if value.contains([';', '{', '}'])
        || value.starts_with(char::is_whitespace)
        || value.ends_with(char::is_whitespace)
    {
        connection.push('{');
        for character in value.chars() {
            if character == '}' {
                connection.push('}');
            }
            connection.push(character);
        }
        connection.push('}');
    } else {
        connection.push_str(value);
    }
    connection.push(';');
}

pub fn installed_odbc_drivers() -> Result<Vec<String>, String> {
    use std::ptr::null_mut;

    use super::{Handle, Manager, NO_DATA, Return, environment};

    type Drivers = unsafe extern "system" fn(
        Handle,
        u16,
        *mut u16,
        i16,
        *mut i16,
        *mut u16,
        i16,
        *mut i16,
    ) -> Return;

    let manager = Manager::load()?;
    let environment = environment(&manager)?;
    let drivers = unsafe { manager._library.get::<Drivers>(b"SQLDriversW") }
        .map_err(|_| "当前 ODBC 驱动管理器无法列出 Unicode 驱动名称。")?;
    let mut direction = 2; // SQL_FETCH_FIRST.
    let mut names = Vec::new();
    loop {
        let mut name = [0u16; 256];
        let mut len = 0;
        let result = unsafe {
            drivers(
                environment.handle,
                direction,
                name.as_mut_ptr(),
                name.len() as i16,
                &mut len,
                null_mut(),
                0,
                null_mut(),
            )
        };
        if result == NO_DATA {
            break;
        }
        environment.check(result, "无法读取已安装的 ODBC 驱动。")?;
        if let Some(units) = name.get(..len as usize)
            && let Ok(name) = String::from_utf16(units)
            && !name.is_empty()
        {
            names.push(name);
        }
        direction = 1; // SQL_FETCH_NEXT.
    }
    names.sort_unstable_by_key(|name| name.to_lowercase());
    names.dedup_by(|a, b| a.eq_ignore_ascii_case(b));
    Ok(names)
}

#[cfg(windows)]
pub fn create_file_dsn(path: &Path, driver: &str, attributes: &str) -> Result<(), String> {
    use std::io::Write;
    use std::os::windows::ffi::OsStrExt;

    use libloading::os::windows::{LOAD_LIBRARY_SEARCH_SYSTEM32, Library};

    const NON_SECRET_ATTRIBUTES: &[&str] = &[
        "SERVER",
        "SERVERNAME",
        "HOST",
        "PORT",
        "DATABASE",
        "DBQ",
        "DEFAULTDIR",
        "DRIVERID",
        "EXTENSIONS",
        "FIL",
        "READONLY",
        "TRUSTED_CONNECTION",
        "INTEGRATED SECURITY",
        "ENCRYPT",
        "TRUSTSERVERCERTIFICATE",
        "SSLMODE",
    ];
    type WriteFileDsn =
        unsafe extern "system" fn(*const u16, *const u16, *const u16, *const u16) -> i32;

    validate_file_dsn_path(path)?;
    if !installed_odbc_drivers()?.iter().any(|name| name == driver) {
        return Err("请选择当前程序架构已安装的 ODBC 驱动。".into());
    }
    if driver.contains(['{', '}']) || driver.chars().any(char::is_control) {
        return Err("驱动名称含有文件 DSN 无法写入的字符。".into());
    }
    let driver = if driver.contains(';') {
        format!("{{{driver}}}")
    } else {
        driver.to_owned()
    };
    let mut entries: Vec<(&str, &str)> = Vec::new();
    for line in attributes
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
    {
        let (key, value) = line.split_once('=').ok_or("每行连接属性须为名称=值。")?;
        let key = key.trim();
        let value = value.trim();
        if !NON_SECRET_ATTRIBUTES
            .iter()
            .any(|allowed| key.eq_ignore_ascii_case(allowed))
        {
            return Err("请只填写服务器、数据库、文件路径或连接模式等支持的配置。用户名和密码应在运行时提供。".into());
        }
        if entries
            .iter()
            .any(|(name, _)| name.eq_ignore_ascii_case(key))
        {
            return Err("每个文件 DSN 连接属性只能填写一次。".into());
        }
        entries.push((key, value));
    }
    // INI values can also be embedded by the manager in a connection string.
    if entries.iter().any(|(_, value)| {
        value.contains([';', '{', '}'])
            || value.chars().any(char::is_control)
            || value.encode_utf16().count() > i16::MAX as usize
    }) {
        return Err("文件 DSN 的属性值不能含分号、大括号或控制字符，且长度须小于 32768。".into());
    }

    let path = std::path::absolute(path).map_err(|error| format!("无法定位文件 DSN。{error}"))?;
    let directory = path.parent().ok_or("无法定位文件 DSN 所在的文件夹。")?;
    let mut temp = tempfile::Builder::new()
        .prefix(".harbor-dsn-")
        .suffix(".dsn")
        .tempfile_in(directory)
        .map_err(|error| format!("无法创建临时文件 DSN。{error}"))?;
    // Preselect the Unicode profile format before the Windows installer writes any attributes.
    temp.write_all(&[0xff, 0xfe])
        .map_err(|error| format!("无法写入临时文件 DSN。{error}"))?;
    let temp = temp.into_temp_path();
    let library = unsafe { Library::load_with_flags("odbccp32.dll", LOAD_LIBRARY_SEARCH_SYSTEM32) }
        .map_err(|_| "无法加载 Windows ODBC 安装程序。")?;
    let write = unsafe { library.get::<WriteFileDsn>(b"SQLWriteFileDSNW") }
        .map_err(|_| "Windows ODBC 安装程序缺少 Unicode 文件 DSN 接口。")?;
    let staged: Vec<_> = temp.as_os_str().encode_wide().chain([0]).collect();
    let section = [b'O' as u16, b'D' as u16, b'B' as u16, b'C' as u16, 0];
    for (key, value) in std::iter::once(("DRIVER", driver.as_str())).chain(entries) {
        let key: Vec<_> = key.encode_utf16().chain([0]).collect();
        let value: Vec<_> = value.encode_utf16().chain([0]).collect();
        if unsafe {
            write(
                staged.as_ptr(),
                section.as_ptr(),
                key.as_ptr(),
                value.as_ptr(),
            )
        } == 0
        {
            return Err("Windows ODBC 安装程序未能写入文件 DSN。".into());
        }
    }
    temp.persist_noclobber(path).map_err(|error| {
        if error.error.kind() == std::io::ErrorKind::AlreadyExists {
            "文件 DSN 已存在，请选择新的文件名。".into()
        } else {
            format!("无法保存文件 DSN。{}", error.error)
        }
    })
}

#[cfg(not(windows))]
pub fn create_file_dsn(_path: &Path, _driver: &str, _attributes: &str) -> Result<(), String> {
    Err("新建文件 DSN 需要 Windows 的 ODBC 安装程序。".into())
}
