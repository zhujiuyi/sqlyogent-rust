use std::borrow::Cow;
use std::collections::BTreeMap;
use std::io::Read;
use std::net::Ipv6Addr;
use std::num::{NonZeroU16, NonZeroU32};
use std::path::Path;
use std::str::FromStr;

use reqwest::Url;

use crate::database::HTTP_CONTENT_TYPES;

use super::state::{AdvancedProfile, HttpProfile, Profile};

pub(super) struct ImportReport {
    pub profiles: Vec<Profile>,
    pub first_imported: Option<usize>,
    skipped: Vec<(u32, &'static str)>,
    review: Vec<(u32, &'static str)>,
}

impl ImportReport {
    pub(super) fn summary(&self) -> String {
        let added = self
            .first_imported
            .map_or(0, |first| self.profiles.len() - first);
        let mut summary = format!(
            "已导入 {added} 个 SQLyog 连接，跳过 {} 个。",
            self.skipped.len()
        );
        if added > 0 {
            summary.push_str("连接时请重新输入所需的密码。");
        }
        append_details(&mut summary, "跳过", &self.skipped);
        append_details(&mut summary, "请检查", &self.review);
        summary
    }
}

fn append_details(summary: &mut String, label: &str, details: &[(u32, &'static str)]) {
    const VISIBLE_DETAILS: usize = 5;
    for (number, reason) in details.iter().take(VISIBLE_DETAILS) {
        summary.push_str(&format!("\n{label} Connection {number}：{reason}"));
    }
    if details.len() > VISIBLE_DETAILS {
        summary.push_str(&format!(
            "\n另有 {} 条{label}提示。",
            details.len() - VISIBLE_DETAILS
        ));
    }
}

pub(super) fn read_profiles(path: &Path, current: &[Profile]) -> Result<ImportReport, String> {
    const FILE_LIMIT: u64 = 8 * 1024 * 1024;
    let read_error = |error| format!("无法读取 SQLyog 连接文件。{error}");
    let file = std::fs::File::open(path).map_err(read_error)?;
    let mut bytes = Vec::new();
    file.take(FILE_LIMIT + 1)
        .read_to_end(&mut bytes)
        .map_err(read_error)?;
    if bytes.len() as u64 > FILE_LIMIT {
        return Err("SQLyog 连接文件的大小上限为 8 MiB。".into());
    }
    let text = decode_ini(&bytes)?;
    parse_profiles(&text, current)
}

fn parse_profiles(text: &str, current: &[Profile]) -> Result<ImportReport, String> {
    let sections = legacy_sections(text);
    if sections.is_empty() {
        return Err("找不到 SQLyog 连接。请选择 SQLyog 导出的 .sycs 文件或 sqlyog.ini。".into());
    }

    let mut report = ImportReport {
        profiles: current.to_vec(),
        first_imported: None,
        skipped: Vec::new(),
        review: Vec::new(),
    };
    for section in sections {
        match section.profile() {
            Ok((profile, review)) => {
                let label = profile.label.clone();
                if let Some(index) = add_profile(&mut report.profiles, profile) {
                    report.first_imported.get_or_insert(index);
                    if report.profiles[index].label != label {
                        report
                            .review
                            .push((section.number, "连接名称相同，已为导入的连接加编号。"));
                    }
                    report
                        .review
                        .extend(review.into_iter().map(|reason| (section.number, reason)));
                } else {
                    report.skipped.push((section.number, "相同配置已存在。"));
                }
            }
            Err(reason) => report.skipped.push((section.number, reason)),
        }
    }
    Ok(report)
}

fn add_profile(profiles: &mut Vec<Profile>, mut profile: Profile) -> Option<usize> {
    let label = profile.label.clone();
    let mut suffix = 0;
    loop {
        let mut conflicts = profiles
            .iter()
            .filter(|item| item.label.eq_ignore_ascii_case(&profile.label))
            .peekable();
        if conflicts.peek().is_none() {
            let index = profiles.len();
            profiles.push(profile);
            return Some(index);
        }
        if conflicts.any(|item| *item == profile) {
            return None;
        }
        suffix += 1;
        profile.label = format!("{label}_({suffix})");
    }
}

struct LegacySection<'a> {
    number: u32,
    fields: BTreeMap<String, &'a str>,
}

fn legacy_sections(text: &str) -> Vec<LegacySection<'_>> {
    let mut sections = Vec::<LegacySection<'_>>::new();
    let mut active = false;
    for line in text.lines().map(str::trim) {
        if line.starts_with('[') {
            active = false;
            if let Some(number) = connection_number(line) {
                sections.push(LegacySection {
                    number,
                    fields: BTreeMap::new(),
                });
                active = true;
            }
            continue;
        }
        if !active || line.starts_with([';', '#']) {
            continue;
        }
        if let Some((key, value)) = line.split_once('=') {
            let key = key.trim().to_ascii_lowercase();
            // The allowlist excludes database, SSH, web and proxy passwords.
            if matches!(
                key.as_str(),
                "name"
                    | "host"
                    | "user"
                    | "port"
                    | "database"
                    | "compressedprotocol"
                    | "defaulttimeout"
                    | "waittimeoutvalue"
                    | "keep_alive"
                    | "sqlmode_global"
                    | "sqlmode_value"
                    | "init_command"
                    | "readonly"
                    | "tunnel"
                    | "http"
                    | "httptime"
                    | "httpencode"
                    | "contenttype"
                    | "isproxy"
                    | "proxy"
                    | "proxyport"
                    | "proxyuser"
                    | "is401"
                    | "401user"
                    | "ssh"
                    | "sshhost"
                    | "sshuser"
                    | "sshport"
                    | "sshforport"
                    | "sshpasswordradio"
                    | "sshprivateradio"
                    | "sshprivatekeypath"
                    | "sslchecked"
                    | "sshauth"
                    | "ca"
                    | "client_cert"
                    | "client_key"
                    | "cipher"
            ) && let Some(section) = sections.last_mut()
            {
                let value = value.trim();
                let value = value
                    .strip_prefix('"')
                    .and_then(|quoted| quoted.strip_suffix('"'))
                    .unwrap_or(value);
                section.fields.entry(key).or_insert(value);
            }
        }
    }
    sections
}

fn connection_number(line: &str) -> Option<u32> {
    let section = line.strip_prefix('[')?.strip_suffix(']')?;
    let mut words = section.split_ascii_whitespace();
    if !words.next()?.eq_ignore_ascii_case("connection") {
        return None;
    }
    let number = words.next()?.parse().ok()?;
    words.next().is_none().then_some(number)
}

impl LegacySection<'_> {
    fn value(&self, key: &str) -> &str {
        self.fields.get(key).copied().unwrap_or_default()
    }

    fn required(&self, key: &str, missing: &'static str) -> Result<&str, &'static str> {
        let value = self.value(key);
        (!value.is_empty()).then_some(value).ok_or(missing)
    }

    fn number<T: FromStr>(
        &self,
        key: &str,
        invalid: &'static str,
    ) -> Result<Option<T>, &'static str> {
        let value = self.value(key);
        if value.is_empty() {
            return Ok(None);
        }
        value.parse().map(Some).map_err(|_| invalid)
    }

    fn flag(&self, key: &str, default: bool) -> Result<bool, &'static str> {
        Ok(self
            .number::<i32>(key, "旧连接的开关值无效。")?
            .map_or(default, |value| value != 0))
    }

    fn port(&self, key: &str, default: u16, invalid: &'static str) -> Result<u16, &'static str> {
        self.number::<NonZeroU16>(key, invalid)
            .map(|port| port.map_or(default, NonZeroU16::get))
    }

    fn profile(&self) -> Result<(Profile, Vec<&'static str>), &'static str> {
        let label = self.required("name", "缺少连接名称。")?;
        let host = self.required("host", "缺少数据库主机。")?;
        let user = self.required("user", "缺少数据库用户名。")?;
        if !valid_host(host) {
            return Err("数据库主机格式无效。");
        }
        if self.flag("readonly", false)? {
            return Err("当前连接配置不支持 SQLyog 的只读模式。");
        }

        // In the original INI, Tunnel is the flag and Http is the URL.
        let http = self.flag("tunnel", false)?;
        let ssh = self.flag("ssh", false)?;
        let tls = self.flag("sslchecked", false)?;
        if http && (ssh || tls) {
            return Err("HTTP 隧道无法与数据库 TLS 或 SSH 同时启用。");
        }
        let mut databases = self
            .value("database")
            .split(';')
            .map(str::trim)
            .filter(|name| !name.is_empty());
        let first = databases.next().unwrap_or_default();
        let schema = if databases.next().is_none() {
            first
        } else {
            ""
        };
        let mut profile = Profile {
            label: label.to_owned(),
            host: host.to_owned(),
            port: self.port("port", 3306, "数据库端口无效。")?,
            user: user.to_owned(),
            schema: schema.to_owned(),
            ssh,
            tls,
            advanced: AdvancedProfile {
                displayed_databases: self.value("database").to_owned(),
                ..AdvancedProfile::default()
            },
            ..Profile::default()
        };
        let mut review = Vec::new();
        if !self.value("init_command").is_empty() {
            review.push("旧初始化命令需要核对后重新填写。");
        }
        if http {
            profile.http = Some(self.http()?);
        } else {
            let advanced = &mut profile.advanced;
            advanced.compression = self.flag("compressedprotocol", false)?;
            if !self.flag("defaulttimeout", true)? {
                advanced.wait_timeout = Some(
                    self.number::<NonZeroU32>("waittimeoutvalue", "会话空闲超时无效。")?
                        .map_or(28_800, NonZeroU32::get),
                );
            }
            advanced.keep_alive = self
                .number::<u32>("keep_alive", "保活间隔无效。")?
                .filter(|&seconds| seconds > 0);
            if !self.flag("sqlmode_global", true)? {
                advanced.sql_mode = Some(self.value("sqlmode_value").to_owned());
            }
        }
        if ssh {
            self.apply_ssh(&mut profile, &mut review)?;
        }
        if tls {
            self.apply_tls(&mut profile, &mut review)?;
        }
        Ok((profile, review))
    }

    fn http(&self) -> Result<HttpProfile, &'static str> {
        let url = self.required("http", "缺少 HTTP 隧道地址。")?;
        let url = Url::parse(url).map_err(|_| "HTTP 隧道地址无效。")?;
        if !matches!(url.scheme(), "http" | "https") || !url.has_host() {
            return Err("HTTP 隧道地址需要使用 HTTP 或 HTTPS。");
        }
        if !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
        {
            return Err("HTTP 隧道地址含认证信息、查询参数或片段，请手动填写。");
        }
        let content_type = match self.value("contenttype") {
            "" => 0,
            value => HTTP_CONTENT_TYPES
                .iter()
                .position(|candidate| candidate.eq_ignore_ascii_case(value))
                .ok_or("此 HTTP 内容类型不受支持。")?,
        };
        let web_user = if self.flag("is401", false)? {
            self.required("401user", "HTTP 隧道启用了网页认证，但缺少用户名。")?
        } else {
            ""
        };
        let mut profile = HttpProfile {
            url: url.into(),
            content_type,
            force_base64: self.flag("httpencode", false)?,
            timeout: Some(
                self.number::<NonZeroU32>("httptime", "HTTP 请求超时无效。")?
                    .map_or(30, NonZeroU32::get),
            ),
            web_user: web_user.to_owned(),
            ..HttpProfile::default()
        };
        if self.flag("isproxy", false)? {
            let host = self.required("proxy", "缺少 HTTP 代理主机。")?;
            if !valid_host(host) {
                return Err("HTTP 代理主机格式无效。");
            }
            let port = self.port("proxyport", 808, "HTTP 代理端口无效。")?;
            profile.proxy_url = if host.parse::<Ipv6Addr>().is_ok() {
                format!("http://[{host}]:{port}")
            } else {
                format!("http://{host}:{port}")
            };
            profile.proxy_user = self.value("proxyuser").to_owned();
        }
        Ok(profile)
    }

    fn apply_ssh(
        &self,
        profile: &mut Profile,
        review: &mut Vec<&'static str>,
    ) -> Result<(), &'static str> {
        let host = self.required("sshhost", "缺少 SSH 主机。")?;
        if !valid_host(host) {
            return Err("SSH 主机格式无效。");
        }
        profile.ssh_host = host.to_owned();
        profile.ssh_port = self.port("sshport", 22, "SSH 端口无效。")?;
        profile.ssh_user = self.required("sshuser", "缺少 SSH 用户名。")?.to_owned();
        profile.ssh_local_port = self
            .number::<u16>("sshforport", "SSH 本地转发端口无效。")?
            .filter(|&port| port > 0);
        profile.ssh_password_auth =
            self.flag("sshpasswordradio", !self.flag("sshprivateradio", false)?)?;
        if !profile.ssh_password_auth {
            profile.ssh_key = self
                .required("sshprivatekeypath", "缺少 SSH 私钥路径。")?
                .to_owned();
            if Path::new(&profile.ssh_key)
                .extension()
                .is_some_and(|extension| extension.eq_ignore_ascii_case("ppk"))
            {
                review.push("连接前请核对本机上的 .ppk 私钥路径及私钥口令。");
            } else {
                review.push("连接前请核对本机上的 SSH 私钥路径。");
            }
        }
        review.push("连接前请在本机的 known_hosts 中确认 SSH 主机指纹。");
        Ok(())
    }

    fn apply_tls(
        &self,
        profile: &mut Profile,
        review: &mut Vec<&'static str>,
    ) -> Result<(), &'static str> {
        if !self.value("cipher").is_empty() {
            return Err("此 TLS 配置指定了旧版密码套件，请手动配置。");
        }
        profile.ca = self.value("ca").to_owned();
        if self.flag("sshauth", false)? {
            profile.client_identity = self
                .required("client_cert", "缺少 TLS 客户端证书。")?
                .to_owned();
            profile.client_private_key = self
                .required("client_key", "缺少 TLS 客户端私钥。")?
                .to_owned();
        }
        if !profile.ca.is_empty() || !profile.client_identity.is_empty() {
            review.push("连接前请核对本机上的 CA、客户端证书和私钥路径。");
        }
        Ok(())
    }
}

fn valid_host(value: &str) -> bool {
    let ip = value
        .strip_prefix('[')
        .and_then(|host| host.strip_suffix(']'))
        .unwrap_or(value);
    ip.parse::<Ipv6Addr>().is_ok()
        || (!value.is_empty()
            && !value.starts_with('-')
            && value
                .chars()
                .all(|c| c.is_alphanumeric() || matches!(c, '-' | '_' | '.')))
}

fn decode_ini(bytes: &[u8]) -> Result<Cow<'_, str>, String> {
    const ENCODING_ERROR: &str = "无法识别文件编码。请使用 UTF-8 或带 BOM 的 UTF-16 保存文件。";
    if let [0xff, 0xfe, rest @ ..] | [0xfe, 0xff, rest @ ..] = bytes {
        let little_endian = bytes[0] == 0xff;
        let (words, remainder) = rest.as_chunks::<2>();
        if !remainder.is_empty() {
            return Err(ENCODING_ERROR.into());
        }
        let words = words
            .iter()
            .map(|&word| {
                if little_endian {
                    u16::from_le_bytes(word)
                } else {
                    u16::from_be_bytes(word)
                }
            })
            .collect::<Vec<_>>();
        return String::from_utf16(&words)
            .map(Cow::Owned)
            .map_err(|_| ENCODING_ERROR.into());
    }
    if let Some(utf8) = bytes.strip_prefix(&[0xef, 0xbb, 0xbf]) {
        return std::str::from_utf8(utf8)
            .map(Cow::Borrowed)
            .map_err(|_| ENCODING_ERROR.into());
    }
    if let Ok(utf8) = std::str::from_utf8(bytes) {
        return Ok(Cow::Borrowed(utf8));
    }
    #[cfg(windows)]
    if let Some(ansi) = decode_windows_ansi(bytes) {
        return Ok(Cow::Owned(ansi));
    }
    Err(ENCODING_ERROR.into())
}

#[cfg(windows)]
fn decode_windows_ansi(bytes: &[u8]) -> Option<String> {
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn MultiByteToWideChar(
            code_page: u32,
            flags: u32,
            bytes: *const u8,
            byte_count: i32,
            units: *mut u16,
            unit_count: i32,
        ) -> i32;
    }
    const CP_ACP: u32 = 0;
    const MB_ERR_INVALID_CHARS: u32 = 8;
    let count = i32::try_from(bytes.len()).ok()?;
    // An ANSI string cannot expand to more UTF-16 units than its byte count.
    let mut units = vec![0; bytes.len()];
    let written = unsafe {
        MultiByteToWideChar(
            CP_ACP,
            MB_ERR_INVALID_CHARS,
            bytes.as_ptr(),
            count,
            units.as_mut_ptr(),
            count,
        )
    };
    if written <= 0 {
        return None;
    }
    String::from_utf16(&units[..written as usize]).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn imports_saved_connections_and_keeps_existing_profiles() {
        let ini = r#"
[GENERALPREF]
Name=Ignored
[Connection 2]
Name=报表库
Host=db.example.net
Port=3307
User=reader
Password=fixture-db-secret
Database=warehouse;reporting
compressedprotocol=1
defaulttimeout=0
waittimeoutvalue=1200
keep_alive=20
sqlmode_global=0
sqlmode_value=STRICT_TRANS_TABLES
init_command=SELECT 'fixture-init-command'
[Connection 4]
Name=SSH 连接
Host=127.0.0.1
User=reader
SSH=1
SshHost=bastion.example.net
SshUser=operator
SshPort=2222
SshForPort=3310
SshPasswordRadio=1
SshPwd=fixture-ssh-secret
[Connection 6]
Name=密钥 SSH
Host=127.0.0.1
User=reader
SSH=1
SshHost=bastion.example.net
SshUser=operator
SshPasswordRadio=0
SshPrivateRadio=1
SSHPrivateKeyPath=C:\Keys\example.ppk
[Connection 8]
Name=HTTPS
Host=localhost
User=reader
Database=reporting
Tunnel=1
Http=https://example.net/SQLyogTunnel.php
HTTPTime=65
HttpEncode=1
ContentType=text/plain
IsProxy=1
Proxy=proxy.example.net
ProxyPort=8080
ProxyUser=proxy-login
ProxyPwd=fixture-proxy-secret
Is401=1
401User=web-login
401Pwd=fixture-web-secret
[Connection 10]
Name=TLS
Host=db.example.net
User=reader
SslChecked=1
SshAuth=1
CA=ca.pem
Client_Cert=client.pem
Client_Key=key.pem
"#;
        let current = [Profile {
            label: "报表库".into(),
            ..Profile::default()
        }];
        let report = parse_profiles(ini, &current).unwrap();
        assert_eq!(report.first_imported, Some(1));
        assert_eq!(report.profiles.len(), 6);
        assert_eq!(report.profiles[0].host, "127.0.0.1");
        assert!(report.skipped.is_empty());

        let native = &report.profiles[1];
        assert_eq!(native.label, "报表库_(1)");
        assert_eq!(native.port, 3307);
        assert!(native.schema.is_empty());
        assert_eq!(native.advanced.displayed_databases, "warehouse;reporting");
        assert!(native.advanced.compression);
        assert_eq!(native.advanced.wait_timeout, Some(1200));
        assert_eq!(native.advanced.keep_alive, Some(20));
        assert_eq!(
            native.advanced.sql_mode.as_deref(),
            Some("STRICT_TRANS_TABLES")
        );
        assert!(native.advanced.init_file.is_empty());

        let ssh = &report.profiles[2];
        assert!(ssh.ssh && ssh.ssh_password_auth);
        assert_eq!(ssh.ssh_host, "bastion.example.net");
        assert_eq!(ssh.ssh_port, 2222);
        assert_eq!(ssh.ssh_user, "operator");
        assert_eq!(ssh.ssh_local_port, Some(3310));
        let key = &report.profiles[3];
        assert!(key.ssh && !key.ssh_password_auth);
        assert_eq!(key.ssh_key, r"C:\Keys\example.ppk");

        let http = report.profiles[4].http.as_ref().unwrap();
        assert_eq!(report.profiles[4].schema, "reporting");
        assert_eq!(http.url, "https://example.net/SQLyogTunnel.php");
        assert_eq!(HTTP_CONTENT_TYPES[http.content_type], "text/plain");
        assert!(http.force_base64);
        assert_eq!(http.timeout, Some(65));
        assert_eq!(http.web_user, "web-login");
        assert_eq!(http.proxy_url, "http://proxy.example.net:8080");
        assert_eq!(http.proxy_user, "proxy-login");

        let tls = &report.profiles[5];
        assert!(tls.tls);
        assert_eq!(tls.ca, "ca.pem");
        assert_eq!(tls.client_identity, "client.pem");
        assert_eq!(tls.client_private_key, "key.pem");
        let serialized = serde_json::to_string(&report.profiles).unwrap();
        assert!(!serialized.contains("fixture-"));
        let summary = report.summary();
        assert!(!summary.contains("fixture-"));
        assert!(summary.contains(".ppk"));
        assert!(summary.contains("旧初始化命令"));

        let repeat = parse_profiles(ini, &report.profiles).unwrap();
        assert_eq!(repeat.first_imported, None);
        assert_eq!(repeat.skipped.len(), 5);
        assert_eq!(repeat.profiles.len(), report.profiles.len());
    }

    #[test]
    fn reads_unicode_sql_yog_export() {
        let binary = std::env::current_exe().unwrap();
        let temp = tempfile::tempdir_in(binary.parent().unwrap()).unwrap();
        let path = temp.path().join("connections.sycs");
        let ini = "[Connection 1]\r\nName=生产库\r\nHost=localhost\r\nUser=reader\r\n";
        let bytes = [0xff, 0xfe]
            .into_iter()
            .chain(ini.encode_utf16().flat_map(u16::to_le_bytes))
            .collect::<Vec<_>>();
        std::fs::write(&path, bytes).unwrap();
        let report = read_profiles(&path, &[]).unwrap();
        assert_eq!(report.profiles[0].label, "生产库");
        assert_eq!(report.profiles[0].port, 3306);
    }
}
