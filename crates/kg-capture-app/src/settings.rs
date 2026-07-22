use std::env;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use thiserror::Error;
use toml_edit::{DocumentMut, Item, Table, TableLike, Value, value};

pub const CURRENT_VERSION: u32 = 1;

#[derive(Clone, Debug, PartialEq)]
pub struct Settings {
    pub version: u32,
    pub executable_path: String,
    pub lyrics: LyricsSettings,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            version: CURRENT_VERSION,
            executable_path: String::new(),
            lyrics: LyricsSettings::default(),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct LyricsSettings {
    pub background: String,
    pub text: String,
    pub highlight: String,
    pub font: FontSetting,
    pub alignment: AlignmentSetting,
    pub active_font_size: f32,
    pub candidate_font_size: f32,
    pub show_previous_line: bool,
    pub candidate_line_count: usize,
}

impl Default for LyricsSettings {
    fn default() -> Self {
        Self {
            background: "#292B2F".into(),
            text: "#F5F5F5".into(),
            highlight: "#FFD54F".into(),
            font: FontSetting::Preferred,
            alignment: AlignmentSetting::Center,
            active_font_size: 38.0,
            candidate_font_size: 24.0,
            show_previous_line: true,
            candidate_line_count: 3,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub enum FontSetting {
    #[default]
    Preferred,
    System,
    Named(String),
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum AlignmentSetting {
    Left,
    #[default]
    Center,
    Right,
}

#[derive(Debug, Error)]
pub enum SettingsError {
    #[error("读取设置文件 {path} 失败: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("解析设置文件 {path} 失败: {reason}")]
    Parse { path: PathBuf, reason: String },
    #[error(
        "设置文件 {path} 来自较新的版本（版本 {found}，当前支持 {supported}），为避免覆盖新设置，已停止自动保存"
    )]
    NewerVersion {
        path: PathBuf,
        found: u32,
        supported: u32,
    },
    #[error("写入设置文件 {path} 失败: {source}")]
    Write {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
}

pub fn load() -> Result<Option<Settings>, SettingsError> {
    let path = settings_path();
    match load_from(&path)? {
        Some(settings) => Ok(Some(settings)),
        None => load_legacy_from(&legacy_settings_path()),
    }
}

pub fn save(settings: &Settings) -> Result<(), SettingsError> {
    save_to(&settings_path(), settings)?;
    let _ = fs::remove_file(legacy_settings_path());
    Ok(())
}

fn settings_directory() -> PathBuf {
    env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(env::temp_dir)
        .join("kg-capture")
}

fn settings_path() -> PathBuf {
    settings_directory().join("settings.toml")
}

fn legacy_settings_path() -> PathBuf {
    settings_directory().join("settings.conf")
}

fn load_from(path: &Path) -> Result<Option<Settings>, SettingsError> {
    let contents = match fs::read_to_string(path) {
        Ok(contents) => contents,
        Err(source) if source.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(source) => {
            return Err(SettingsError::Read {
                path: path.to_owned(),
                source,
            });
        }
    };
    let document = contents
        .parse::<DocumentMut>()
        .map_err(|error| SettingsError::Parse {
            path: path.to_owned(),
            reason: error.to_string(),
        })?;
    let root = document.as_table();
    let file_version = optional_u32(root, "version")
        .map_err(|reason| parse_error(path, reason))?
        .unwrap_or(0);
    if file_version > CURRENT_VERSION {
        return Err(SettingsError::NewerVersion {
            path: path.to_owned(),
            found: file_version,
            supported: CURRENT_VERSION,
        });
    }

    let mut settings = Settings::default();
    if let Some(executable_path) =
        optional_string(root, "executable_path").map_err(|reason| parse_error(path, reason))?
    {
        settings.executable_path = executable_path;
    }
    if let Some(lyrics) =
        optional_table(root, "lyrics").map_err(|reason| parse_error(path, reason))?
    {
        apply_lyrics_settings(path, lyrics, &mut settings.lyrics)?;
    }
    Ok(Some(settings))
}

fn apply_lyrics_settings(
    path: &Path,
    table: &dyn TableLike,
    settings: &mut LyricsSettings,
) -> Result<(), SettingsError> {
    if let Some(background) =
        optional_string(table, "background").map_err(|reason| parse_error(path, reason))?
    {
        settings.background = background;
    }
    if let Some(text) =
        optional_string(table, "text").map_err(|reason| parse_error(path, reason))?
    {
        settings.text = text;
    }
    if let Some(highlight) =
        optional_string(table, "highlight").map_err(|reason| parse_error(path, reason))?
    {
        settings.highlight = highlight;
    }
    if let Some(alignment) =
        optional_string(table, "alignment").map_err(|reason| parse_error(path, reason))?
    {
        settings.alignment = match alignment.as_str() {
            "left" => AlignmentSetting::Left,
            "center" => AlignmentSetting::Center,
            "right" => AlignmentSetting::Right,
            _ => {
                return Err(parse_error(
                    path,
                    format!("lyrics.alignment 包含未知值“{alignment}”"),
                ));
            }
        };
    }
    if let Some(size) =
        optional_f32(table, "active_font_size").map_err(|reason| parse_error(path, reason))?
    {
        settings.active_font_size = size;
    }
    if let Some(size) =
        optional_f32(table, "candidate_font_size").map_err(|reason| parse_error(path, reason))?
    {
        settings.candidate_font_size = size;
    }
    if let Some(show) =
        optional_bool(table, "show_previous_line").map_err(|reason| parse_error(path, reason))?
    {
        settings.show_previous_line = show;
    }
    if let Some(count) =
        optional_usize(table, "candidate_line_count").map_err(|reason| parse_error(path, reason))?
    {
        settings.candidate_line_count = count;
    }

    if let Some(font_kind) =
        optional_string(table, "font").map_err(|reason| parse_error(path, reason))?
    {
        settings.font = match font_kind.as_str() {
            "preferred" => FontSetting::Preferred,
            "system" => FontSetting::System,
            "named" => {
                let family = optional_string(table, "font_family")
                    .map_err(|reason| parse_error(path, reason))?
                    .ok_or_else(|| parse_error(path, "命名字体缺少 lyrics.font_family"))?;
                FontSetting::Named(family)
            }
            _ => {
                return Err(parse_error(
                    path,
                    format!("lyrics.font 包含未知值“{font_kind}”"),
                ));
            }
        };
    }
    Ok(())
}

fn optional_table<'a>(
    table: &'a dyn TableLike,
    key: &str,
) -> Result<Option<&'a dyn TableLike>, String> {
    match table.get(key) {
        None => Ok(None),
        Some(item) => item
            .as_table_like()
            .map(Some)
            .ok_or_else(|| format!("{key} 必须是表")),
    }
}

fn optional_value<'a>(
    table: &'a dyn TableLike,
    key: &str,
) -> Result<Option<&'a Value>, String> {
    match table.get(key) {
        None => Ok(None),
        Some(item) => item
            .as_value()
            .map(Some)
            .ok_or_else(|| format!("{key} 必须是值")),
    }
}

fn optional_string(table: &dyn TableLike, key: &str) -> Result<Option<String>, String> {
    optional_value(table, key)?
        .map(|value| {
            value
                .as_str()
                .map(str::to_owned)
                .ok_or_else(|| format!("{key} 必须是字符串"))
        })
        .transpose()
}

fn optional_u32(table: &dyn TableLike, key: &str) -> Result<Option<u32>, String> {
    optional_value(table, key)?
        .map(|value| {
            value
                .as_integer()
                .and_then(|value| u32::try_from(value).ok())
                .ok_or_else(|| format!("{key} 必须是非负 32 位整数"))
        })
        .transpose()
}

fn optional_usize(table: &dyn TableLike, key: &str) -> Result<Option<usize>, String> {
    optional_value(table, key)?
        .map(|value| {
            value
                .as_integer()
                .and_then(|value| usize::try_from(value).ok())
                .ok_or_else(|| format!("{key} 必须是非负整数"))
        })
        .transpose()
}

fn optional_bool(table: &dyn TableLike, key: &str) -> Result<Option<bool>, String> {
    optional_value(table, key)?
        .map(|value| {
            value
                .as_bool()
                .ok_or_else(|| format!("{key} 必须是布尔值"))
        })
        .transpose()
}

fn optional_f32(table: &dyn TableLike, key: &str) -> Result<Option<f32>, String> {
    optional_value(table, key)?
        .map(|value| {
            let value = value
                .as_float()
                .or_else(|| value.as_integer().map(|value| value as f64))
                .ok_or_else(|| format!("{key} 必须是数值"))?;
            let value = value as f32;
            if value.is_finite() {
                Ok(value)
            } else {
                Err(format!("{key} 必须是有限数值"))
            }
        })
        .transpose()
}

fn parse_error(path: &Path, reason: impl Into<String>) -> SettingsError {
    SettingsError::Parse {
        path: path.to_owned(),
        reason: reason.into(),
    }
}

fn existing_file_version(path: &Path) -> Result<Option<u32>, SettingsError> {
    let contents = match fs::read_to_string(path) {
        Ok(contents) => contents,
        Err(source) if source.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(source) => {
            return Err(SettingsError::Read {
                path: path.to_owned(),
                source,
            });
        }
    };
    let document = contents
        .parse::<DocumentMut>()
        .map_err(|error| parse_error(path, error.to_string()))?;
    let version = optional_u32(document.as_table(), "version")
        .map_err(|reason| parse_error(path, reason))?
        .unwrap_or(0);
    Ok(Some(version))
}

fn save_to(path: &Path, settings: &Settings) -> Result<(), SettingsError> {
    if let Some(found) = existing_file_version(path)?
        && found > CURRENT_VERSION
    {
        return Err(SettingsError::NewerVersion {
            path: path.to_owned(),
            found,
            supported: CURRENT_VERSION,
        });
    }

    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent).map_err(|source| SettingsError::Write {
        path: path.to_owned(),
        source,
    })?;

    let mut document = DocumentMut::new();
    document["version"] = value(i64::from(CURRENT_VERSION));
    document["executable_path"] = value(settings.executable_path.as_str());
    document["lyrics"] = Item::Table(Table::new());
    document["lyrics"]["background"] = value(settings.lyrics.background.as_str());
    document["lyrics"]["text"] = value(settings.lyrics.text.as_str());
    document["lyrics"]["highlight"] = value(settings.lyrics.highlight.as_str());
    match &settings.lyrics.font {
        FontSetting::Preferred => document["lyrics"]["font"] = value("preferred"),
        FontSetting::System => document["lyrics"]["font"] = value("system"),
        FontSetting::Named(family) => {
            document["lyrics"]["font"] = value("named");
            document["lyrics"]["font_family"] = value(family.as_str());
        }
    }
    let alignment = match settings.lyrics.alignment {
        AlignmentSetting::Left => "left",
        AlignmentSetting::Center => "center",
        AlignmentSetting::Right => "right",
    };
    document["lyrics"]["alignment"] = value(alignment);
    document["lyrics"]["active_font_size"] =
        value(f64::from(settings.lyrics.active_font_size));
    document["lyrics"]["candidate_font_size"] =
        value(f64::from(settings.lyrics.candidate_font_size));
    document["lyrics"]["show_previous_line"] = value(settings.lyrics.show_previous_line);
    document["lyrics"]["candidate_line_count"] = value(
        i64::try_from(settings.lyrics.candidate_line_count)
            .expect("candidate line count is clamped to a small value"),
    );

    write_atomically(path, document.to_string().as_bytes())
}

fn write_atomically(path: &Path, contents: &[u8]) -> Result<(), SettingsError> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    static TEMP_FILE_ID: AtomicU64 = AtomicU64::new(0);
    let temp_path = parent.join(format!(
        ".settings-{}-{}.tmp",
        std::process::id(),
        TEMP_FILE_ID.fetch_add(1, Ordering::Relaxed)
    ));
    let result = (|| {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp_path)?;
        file.write_all(contents)?;
        file.sync_all()?;
        drop(file);
        replace_file(&temp_path, path)
    })();

    if let Err(source) = result {
        let _ = fs::remove_file(&temp_path);
        return Err(SettingsError::Write {
            path: path.to_owned(),
            source,
        });
    }
    Ok(())
}

fn load_legacy_from(path: &Path) -> Result<Option<Settings>, SettingsError> {
    let contents = match fs::read_to_string(path) {
        Ok(contents) => contents,
        Err(source) if source.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(source) => {
            return Err(SettingsError::Read {
                path: path.to_owned(),
                source,
            });
        }
    };
    let mut settings = Settings::default();
    let mut file_version = 0;

    for (index, raw_line) in contents.lines().enumerate() {
        let line_number = index + 1;
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (key, value) = line
            .split_once('=')
            .ok_or_else(|| parse_error(path, format!("第 {line_number} 行缺少“=”分隔符")))?;
        let key = key.trim();
        let value = value.trim();
        let invalid = |reason: String| {
            parse_error(path, format!("第 {line_number} 行无效: {reason}"))
        };

        match key {
            "version" => {
                file_version = value
                    .parse()
                    .map_err(|error| invalid(format!("版本号无效: {error}")))?;
            }
            "executable_path" => {
                settings.executable_path =
                    decode_legacy_string(value).map_err(|error| invalid(error.into()))?;
            }
            "lyrics.background" => settings.lyrics.background = value.into(),
            "lyrics.text" => settings.lyrics.text = value.into(),
            "lyrics.highlight" => settings.lyrics.highlight = value.into(),
            "lyrics.font" => {
                settings.lyrics.font =
                    parse_legacy_font(value).map_err(|error| invalid(error.into()))?;
            }
            "lyrics.alignment" => {
                settings.lyrics.alignment = match value {
                    "left" => AlignmentSetting::Left,
                    "center" => AlignmentSetting::Center,
                    "right" => AlignmentSetting::Right,
                    _ => return Err(invalid(format!("未知的对齐方式“{value}”"))),
                };
            }
            "lyrics.active_font_size" => {
                settings.lyrics.active_font_size = parse_legacy_f32(value)
                    .map_err(|error| invalid(format!("播放中字号无效: {error}")))?;
            }
            "lyrics.candidate_font_size" => {
                settings.lyrics.candidate_font_size = parse_legacy_f32(value)
                    .map_err(|error| invalid(format!("候选字号无效: {error}")))?;
            }
            "lyrics.show_previous_line" => {
                settings.lyrics.show_previous_line = value
                    .parse()
                    .map_err(|error| invalid(format!("上一句开关无效: {error}")))?;
            }
            "lyrics.candidate_line_count" => {
                settings.lyrics.candidate_line_count = value
                    .parse()
                    .map_err(|error| invalid(format!("候选条目数无效: {error}")))?;
            }
            _ => {}
        }
    }

    if file_version > CURRENT_VERSION {
        return Err(SettingsError::NewerVersion {
            path: path.to_owned(),
            found: file_version,
            supported: CURRENT_VERSION,
        });
    }
    Ok(Some(settings))
}

fn parse_legacy_font(value: &str) -> Result<FontSetting, &'static str> {
    match value {
        "preferred" => Ok(FontSetting::Preferred),
        "system" => Ok(FontSetting::System),
        _ => value
            .strip_prefix("named:")
            .ok_or("未知的字体设置")
            .and_then(decode_legacy_string)
            .map(FontSetting::Named),
    }
}

fn parse_legacy_f32(value: &str) -> Result<f32, String> {
    let value = value
        .parse::<f32>()
        .map_err(|error| error.to_string())?;
    if value.is_finite() {
        Ok(value)
    } else {
        Err("必须是有限数值".into())
    }
}

fn decode_legacy_string(value: &str) -> Result<String, &'static str> {
    if !value.len().is_multiple_of(2) {
        return Err("字符串编码长度无效");
    }
    let bytes = value
        .as_bytes()
        .chunks_exact(2)
        .map(|pair| {
            let pair = std::str::from_utf8(pair).map_err(|_| "字符串编码不是 ASCII")?;
            u8::from_str_radix(pair, 16).map_err(|_| "字符串编码不是十六进制")
        })
        .collect::<Result<Vec<_>, _>>()?;
    String::from_utf8(bytes).map_err(|_| "字符串编码不是 UTF-8")
}

#[cfg(windows)]
fn replace_file(source: &Path, destination: &Path) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt;

    use windows::Win32::Storage::FileSystem::{
        MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
    };
    use windows::core::PCWSTR;

    let source: Vec<u16> = source
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let destination: Vec<u16> = destination
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    unsafe {
        MoveFileExW(
            PCWSTR(source.as_ptr()),
            PCWSTR(destination.as_ptr()),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    }
    .map_err(|_| io::Error::last_os_error())
}

#[cfg(not(windows))]
fn replace_file(source: &Path, destination: &Path) -> io::Result<()> {
    fs::rename(source, destination)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_path(name: &str, extension: &str) -> PathBuf {
        static TEST_ID: AtomicU64 = AtomicU64::new(0);
        env::temp_dir().join(format!(
            "kg-capture-settings-{name}-{}-{}.{}",
            std::process::id(),
            TEST_ID.fetch_add(1, Ordering::Relaxed),
            extension
        ))
    }

    #[test]
    fn missing_and_unknown_fields_support_schema_updates() {
        let path = test_path("migration", "toml");
        fs::write(
            &path,
            "version = 0\nfuture_option = true\n\n[lyrics]\nbackground = \"#010203\"\n",
        )
        .unwrap();

        let settings = load_from(&path).unwrap().unwrap();
        assert_eq!(settings.version, CURRENT_VERSION);
        assert_eq!(settings.lyrics.background, "#010203");
        assert_eq!(
            settings.lyrics.candidate_line_count,
            LyricsSettings::default().candidate_line_count
        );
        let _ = fs::remove_file(path);
    }

    #[test]
    fn accepts_inline_lyrics_table() {
        let path = test_path("inline", "toml");
        fs::write(
            &path,
            r##"version = 1
lyrics = { background = "#010203", alignment = "right" }
"##,
        )
        .unwrap();

        let settings = load_from(&path).unwrap().unwrap();
        assert_eq!(settings.lyrics.background, "#010203");
        assert_eq!(settings.lyrics.alignment, AlignmentSetting::Right);
        let _ = fs::remove_file(path);
    }

    #[test]
    fn refuses_to_overwrite_settings_from_a_newer_version() {
        let path = test_path("future", "toml");
        let future_contents = "version = 999\nfuture_option = true\n";
        fs::write(&path, future_contents).unwrap();

        assert!(matches!(
            load_from(&path),
            Err(SettingsError::NewerVersion { found: 999, .. })
        ));
        assert!(matches!(
            save_to(&path, &Settings::default()),
            Err(SettingsError::NewerVersion { found: 999, .. })
        ));
        assert_eq!(fs::read_to_string(&path).unwrap(), future_contents);
        let _ = fs::remove_file(path);
    }

    #[test]
    fn rejects_non_finite_font_sizes() {
        let path = test_path("non-finite", "toml");
        fs::write(
            &path,
            "version = 1\n\n[lyrics]\nactive_font_size = nan\n",
        )
        .unwrap();

        assert!(matches!(
            load_from(&path),
            Err(SettingsError::Parse { .. })
        ));
        let _ = fs::remove_file(path);
    }

    #[test]
    fn saves_standard_toml_and_loads_it() {
        let path = test_path("round-trip", "toml");
        let settings = Settings {
            executable_path: r"C:\Program Files\WeSing\WeSing.exe".into(),
            lyrics: LyricsSettings {
                font: FontSetting::Named("微软雅黑".into()),
                alignment: AlignmentSetting::Right,
                ..LyricsSettings::default()
            },
            ..Settings::default()
        };

        save_to(&path, &settings).unwrap();
        let contents = fs::read_to_string(&path).unwrap();
        assert!(contents.contains("[lyrics]"));
        assert!(contents.contains(r#"font_family = "微软雅黑""#));
        assert_eq!(load_from(&path).unwrap(), Some(settings));
        let _ = fs::remove_file(path);
    }

    #[test]
    fn loads_the_legacy_conf_format() {
        let path = test_path("legacy", "conf");
        fs::write(
            &path,
            concat!(
                "version=1\n",
                "executable_path=433a5c576553696e675c576553696e672e657865\n",
                "lyrics.font=named:e5beaee8bdaFE99b85e9bb91\n",
            ),
        )
        .unwrap();

        let settings = load_legacy_from(&path).unwrap().unwrap();
        assert_eq!(settings.executable_path, r"C:\WeSing\WeSing.exe");
        assert_eq!(
            settings.lyrics.font,
            FontSetting::Named("微软雅黑".into())
        );
        let _ = fs::remove_file(path);
    }
}
