use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::io::Read;
use std::path::{Path, PathBuf};

use eframe::egui::FontData;
use serde::{Deserialize, Serialize};
use ttf_parser::{Face, Language, PlatformId, name_id};

const MAX_BYTES: u64 = 128 * 1024 * 1024;
const MAX_FACES: u32 = 1024;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(super) struct FontFace {
    pub family: String,
    pub style: String,
    pub path: PathBuf,
    pub index: u32,
}

impl FontFace {
    pub(super) fn label(&self) -> String {
        let style = match self.style.to_ascii_lowercase().as_str() {
            "regular" | "normal" => "常规",
            "bold" => "粗体",
            "italic" | "oblique" => "斜体",
            "bold italic" | "bold oblique" => "粗斜体",
            "thin" => "极细",
            "light" => "细体",
            "medium" => "中等",
            "semibold" | "semi bold" | "demibold" | "demi bold" => "半粗体",
            "black" => "特粗体",
            _ => &self.style,
        };
        format!("{} · {style}", self.family)
    }

    pub(super) fn data(&self) -> Result<FontData, String> {
        let bytes = read_font(&self.path)?;
        let (family, style) = font_names(&bytes, self.index).ok_or("无法读取所选字体。")?;
        if family != self.family || style != self.style {
            return Err("选中的字体已更换，请重新选择。".into());
        }
        let mut data = FontData::from_owned(bytes);
        data.index = self.index;
        Ok(data)
    }
}

pub(super) fn catalog() -> Vec<FontFace> {
    let mut seen_paths = BTreeSet::new();
    let mut faces = BTreeMap::new();
    for path in platform_paths() {
        if !font_extension(&path) {
            continue;
        }
        let Ok(path) = fs::canonicalize(path) else {
            continue;
        };
        if !seen_paths.insert(path.clone()) {
            continue;
        }
        let Ok(bytes) = read_font(&path) else {
            continue;
        };
        let count = ttf_parser::fonts_in_collection(&bytes).unwrap_or(1);
        for index in 0..count.min(MAX_FACES) {
            let Some((family, style)) = font_names(&bytes, index) else {
                continue;
            };
            faces
                .entry((family.to_lowercase(), style.to_lowercase()))
                .or_insert_with(|| FontFace {
                    family,
                    style,
                    path: path.clone(),
                    index,
                });
        }
    }
    faces.into_values().collect()
}

fn font_extension(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| {
            matches!(
                extension.to_ascii_lowercase().as_str(),
                "ttf" | "otf" | "ttc" | "otc"
            )
        })
}

fn read_font(path: &Path) -> Result<Vec<u8>, String> {
    if !path.is_absolute() || !font_extension(path) {
        return Err("字体路径无效。".into());
    }
    let metadata = fs::metadata(path).map_err(|error| format!("无法读取字体文件属性：{error}"))?;
    if !metadata.is_file() {
        return Err("字体路径没有指向文件。".into());
    }
    if metadata.len() > MAX_BYTES {
        return Err("字体文件大小超过 128 MiB。".into());
    }
    let file = File::open(path).map_err(|error| format!("无法打开字体文件：{error}"))?;
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.take(MAX_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("无法读取字体文件：{error}"))?;
    if bytes.len() as u64 > MAX_BYTES {
        return Err("字体文件大小超过 128 MiB。".into());
    }
    Ok(bytes)
}

fn font_names(bytes: &[u8], index: u32) -> Option<(String, String)> {
    let face = Face::parse(bytes, index).ok()?;
    let tables = face.tables();
    // Egui's rasterizer reads outlines, so bitmap-only faces cannot serve as text fonts.
    if tables.cmap.is_none()
        || (tables.glyf.is_none() && tables.cff.is_none() && tables.cff2.is_none())
    {
        return None;
    }
    let family = [
        name_id::TYPOGRAPHIC_FAMILY,
        name_id::FAMILY,
        name_id::FULL_NAME,
    ]
    .into_iter()
    .find_map(|id| localized_name(&face, id))?;
    let style = [name_id::TYPOGRAPHIC_SUBFAMILY, name_id::SUBFAMILY]
        .into_iter()
        .find_map(|id| localized_name(&face, id))
        .unwrap_or_else(|| "Regular".into());
    Some((family, style))
}

fn localized_name(face: &Face<'_>, id: u16) -> Option<String> {
    face.names()
        .into_iter()
        .filter(|name| name.name_id == id)
        .filter_map(|name| {
            let decoded = match (name.platform_id, name.encoding_id) {
                (PlatformId::Macintosh, 0) => encoding_rs::MACINTOSH
                    .decode_without_bom_handling_and_without_replacement(name.name)?
                    .into_owned(),
                (PlatformId::Windows, 10) => encoding_rs::UTF_16BE
                    .decode_without_bom_handling_and_without_replacement(name.name)?
                    .into_owned(),
                _ => name.to_string()?,
            };
            let value = decoded.trim();
            if value.is_empty() || value.chars().any(char::is_control) {
                return None;
            }
            let language = match name.language() {
                Language::Chinese_PeoplesRepublicOfChina => 0,
                Language::Chinese_Singapore => 1,
                Language::Chinese_Taiwan
                | Language::Chinese_HongKongSAR
                | Language::Chinese_MacaoSAR => 2,
                Language::English_UnitedStates | Language::English_UnitedKingdom => 3,
                _ => 4,
            };
            Some((language, value.to_owned()))
        })
        .min_by_key(|(language, _)| *language)
        .map(|(_, name)| name)
}

#[cfg(any(windows, target_os = "macos"))]
fn files_in(directory: &Path, files: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(directory) else {
        return;
    };
    let mut entries: Vec<_> = entries.flatten().collect();
    entries.sort_by_key(fs::DirEntry::file_name);
    for entry in entries {
        let path = entry.path();
        if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
            files_in(&path, files);
        } else if font_extension(&path) {
            files.push(path);
        }
    }
}

#[cfg(windows)]
fn platform_paths() -> Vec<PathBuf> {
    use std::os::windows::process::CommandExt as _;
    use std::process::Command;

    let windows = std::env::var_os("SystemRoot").or_else(|| std::env::var_os("WINDIR"));
    let system = windows
        .as_ref()
        .map(|path| PathBuf::from(path).join("Fonts"));
    let mut files = Vec::new();
    if let Some(dirs) = directories::BaseDirs::new() {
        files_in(
            &dirs.data_local_dir().join("Microsoft/Windows/Fonts"),
            &mut files,
        );
    }
    if let Some(windows) = windows {
        // The registration values also include fonts installed outside the standard folders.
        let powershell =
            PathBuf::from(windows).join(r"System32\WindowsPowerShell\v1.0\powershell.exe");
        let script = r#"
[Console]::OutputEncoding = [Text.UTF8Encoding]::new($false)
foreach ($hive in [Microsoft.Win32.Registry]::CurrentUser, [Microsoft.Win32.Registry]::LocalMachine) {
    $key = $hive.OpenSubKey('SOFTWARE\Microsoft\Windows NT\CurrentVersion\Fonts')
    if ($null -eq $key) { continue }
    try {
        foreach ($name in $key.GetValueNames()) {
            $value = $key.GetValue($name)
            if ($value -is [string]) { [Console]::WriteLine($value) }
        }
    } finally { $key.Dispose() }
}"#;
        if let Ok(output) = Command::new(powershell)
            .args([
                "-NoLogo",
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                script,
            ])
            .creation_flags(0x08000000)
            .output()
            && output.status.success()
        {
            for name in String::from_utf8_lossy(&output.stdout).lines() {
                let path = PathBuf::from(name);
                if path.is_absolute() {
                    files.push(path);
                } else if let Some(directory) = &system {
                    files.push(directory.join(path));
                }
            }
        }
    }
    if let Some(system) = system {
        files_in(&system, &mut files);
    }
    files
}

#[cfg(target_os = "macos")]
fn platform_paths() -> Vec<PathBuf> {
    let mut files = Vec::new();
    if let Some(dirs) = directories::BaseDirs::new() {
        files_in(&dirs.home_dir().join("Library/Fonts"), &mut files);
    }
    for directory in ["/Library/Fonts", "/System/Library/Fonts"] {
        files_in(Path::new(directory), &mut files);
    }
    files
}

#[cfg(not(any(windows, target_os = "macos")))]
fn platform_paths() -> Vec<PathBuf> {
    Vec::new()
}
