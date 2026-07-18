use windows::Win32::Globalization::{
    GetUserDefaultLocaleName, GetUserPreferredUILanguages, MUI_LANGUAGE_NAME,
};
use windows::Win32::Graphics::DirectWrite::{
    DWRITE_FACTORY_TYPE_SHARED, DWriteCreateFactory, IDWriteFactory, IDWriteLocalizedStrings,
};
use windows::core::{BOOL, PCWSTR, PWSTR, Result, w};

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct FontFamily {
    pub family_name: String,
    pub display_name: String,
}

pub fn families() -> Result<Vec<FontFamily>> {
    let factory: IDWriteFactory = unsafe { DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED)? };
    let mut collection = None;
    unsafe { factory.GetSystemFontCollection(&mut collection, false)? };
    let Some(collection) = collection else {
        return Ok(Vec::new());
    };
    let mut user_locales = preferred_ui_locale_names().unwrap_or_default();
    if let Some(locale) = user_locale_name()
        && !user_locales
            .iter()
            .any(|existing| existing.eq_ignore_ascii_case(&locale))
    {
        user_locales.push(locale);
    }

    let mut families = Vec::with_capacity(unsafe { collection.GetFontFamilyCount() } as usize);
    for index in 0..unsafe { collection.GetFontFamilyCount() } {
        let Ok(family) = (unsafe { collection.GetFontFamily(index) }) else {
            continue;
        };
        let Ok(names) = (unsafe { family.GetFamilyNames() }) else {
            continue;
        };
        let Ok(family_name) = matching_name(&names) else {
            continue;
        };
        if family_name.is_empty() {
            continue;
        }
        let display_name =
            display_name(&names, &user_locales).unwrap_or_else(|_| family_name.clone());
        families.push(FontFamily {
            family_name,
            display_name,
        });
    }

    families.sort_by_cached_key(|family| family.display_name.to_lowercase());
    families.dedup_by(|left, right| left.family_name.eq_ignore_ascii_case(&right.family_name));
    Ok(families)
}

fn matching_name(names: &IDWriteLocalizedStrings) -> Result<String> {
    localized_name(names, w!("en-us"))?.map_or_else(|| string_at(names, 0), Ok)
}

fn display_name(names: &IDWriteLocalizedStrings, user_locales: &[String]) -> Result<String> {
    for locale_name in user_locales {
        let locale = wide_string(locale_name);
        if let Some(name) = localized_name(names, PCWSTR(locale.as_ptr()))? {
            return Ok(name);
        }
    }

    if let Some(locale_name) = user_locales
        .iter()
        .find(|locale| locale_starts_with(locale, "zh"))
    {
        let language_fallback = if locale_starts_with(locale_name, "zh-tw")
            || locale_starts_with(locale_name, "zh-hk")
            || locale_starts_with(locale_name, "zh-mo")
        {
            w!("zh-tw")
        } else {
            w!("zh-cn")
        };
        if let Some(name) = localized_name(names, language_fallback)? {
            return Ok(name);
        }
    }

    matching_name(names)
}

fn localized_name(names: &IDWriteLocalizedStrings, locale: PCWSTR) -> Result<Option<String>> {
    let mut index = 0;
    let mut exists = BOOL::default();
    unsafe { names.FindLocaleName(locale, &mut index, &mut exists)? };
    if exists.as_bool() {
        string_at(names, index).map(Some)
    } else {
        Ok(None)
    }
}

fn string_at(names: &IDWriteLocalizedStrings, index: u32) -> Result<String> {
    let length = unsafe { names.GetStringLength(index)? };
    let mut buffer = vec![0; length as usize + 1];
    unsafe { names.GetString(index, &mut buffer)? };
    Ok(String::from_utf16_lossy(&buffer[..length as usize]))
}

fn user_locale_name() -> Option<String> {
    let mut buffer = [0; 85];
    let length = unsafe { GetUserDefaultLocaleName(&mut buffer) };
    (length > 1).then(|| String::from_utf16_lossy(&buffer[..length as usize - 1]))
}

fn preferred_ui_locale_names() -> Result<Vec<String>> {
    let mut language_count = 0;
    let mut buffer_length = 0;
    unsafe {
        GetUserPreferredUILanguages(
            MUI_LANGUAGE_NAME,
            &mut language_count,
            None,
            &mut buffer_length,
        )?
    };
    if buffer_length == 0 {
        return Ok(Vec::new());
    }

    let mut buffer = vec![0; buffer_length as usize];
    unsafe {
        GetUserPreferredUILanguages(
            MUI_LANGUAGE_NAME,
            &mut language_count,
            Some(PWSTR(buffer.as_mut_ptr())),
            &mut buffer_length,
        )?
    };
    Ok(parse_multistring(&buffer))
}

fn parse_multistring(buffer: &[u16]) -> Vec<String> {
    buffer
        .split(|character| *character == 0)
        .take_while(|value| !value.is_empty())
        .map(String::from_utf16_lossy)
        .collect()
}

fn wide_string(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(std::iter::once(0)).collect()
}

fn locale_starts_with(locale: &str, prefix: &str) -> bool {
    locale
        .get(..prefix.len())
        .is_some_and(|value| value.eq_ignore_ascii_case(prefix))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enumerates_installed_font_family_and_display_names() {
        let families = families().expect("DirectWrite should enumerate system fonts");
        assert!(!families.is_empty());
        assert!(
            families.iter().all(|family| {
                !family.family_name.is_empty() && !family.display_name.is_empty()
            })
        );
        assert!(families.windows(2).all(|pair| {
            pair[0].display_name.to_lowercase() <= pair[1].display_name.to_lowercase()
        }));
    }

    #[test]
    fn recognizes_locale_language_prefixes_case_insensitively() {
        assert!(locale_starts_with("zh-CN", "zh"));
        assert!(locale_starts_with("ZH-tw", "zh-tw"));
        assert!(!locale_starts_with("en-SG", "zh"));
    }

    #[test]
    fn parses_windows_language_multistrings() {
        let buffer: Vec<u16> = "zh-CN\0en-US\0\0".encode_utf16().collect();
        assert_eq!(parse_multistring(&buffer), ["zh-CN", "en-US"]);
    }
}
