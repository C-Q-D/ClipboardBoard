//! 此文件定义可持久化配置 DTO、默认值、快照身份和统一语义验证规则。

use serde::{Deserialize, Serialize};
use std::fmt;

use crate::image_storage::parse_image_storage_preference;

/// 当前配置文档唯一支持的 schema 版本。
pub(crate) const CURRENT_SCHEMA_VERSION: u64 = 1;

/// 普通历史条数的合法范围。
pub(crate) const MAX_ITEMS_RANGE: std::ops::RangeInclusive<u32> = 1..=100_000;
/// 历史保留天数的合法范围。
pub(crate) const RETENTION_DAYS_RANGE: std::ops::RangeInclusive<u32> = 1..=3_650;
/// 图片空间 MiB 的合法范围。
pub(crate) const IMAGE_QUOTA_MIB_RANGE: std::ops::RangeInclusive<u32> = 16..=10_240;
/// 排除程序规则最大条数，避免配置加载创建无界匹配集合。
pub(crate) const MAX_EXCLUDED_APPS: usize = 64;
/// 单条排除程序规则的 UTF-8 字节上限。
pub(crate) const MAX_EXCLUDED_APP_RULE_BYTES: usize = 512;

/// Windows `MOD_*` 热键修饰位；使用稳定数值让配置模型在非 Windows 目标也可验证。
pub(crate) const HOTKEY_MOD_ALT: u32 = 0x0001;
/// Windows `MOD_CONTROL` 热键修饰位。
pub(crate) const HOTKEY_MOD_CONTROL: u32 = 0x0002;
/// Windows `MOD_SHIFT` 热键修饰位。
pub(crate) const HOTKEY_MOD_SHIFT: u32 = 0x0004;
/// Windows `MOD_WIN` 热键修饰位。
pub(crate) const HOTKEY_MOD_WIN: u32 = 0x0008;
/// Windows `MOD_NOREPEAT` 可选修饰位。
pub(crate) const HOTKEY_MOD_NOREPEAT: u32 = 0x4000;
/// 允许持久化的物理修饰位集合。
const HOTKEY_PHYSICAL_MODIFIERS: u32 =
    HOTKEY_MOD_ALT | HOTKEY_MOD_CONTROL | HOTKEY_MOD_SHIFT | HOTKEY_MOD_WIN;
/// 允许持久化的全部修饰位集合。
const HOTKEY_ALLOWED_MODIFIERS: u32 = HOTKEY_PHYSICAL_MODIFIERS | HOTKEY_MOD_NOREPEAT;
/// 默认 Alt+V 的虚拟键码。
pub(crate) const DEFAULT_HOTKEY_VIRTUAL_KEY: u32 = 0x56;
/// 默认 Alt+V 的修饰位。
pub(crate) const DEFAULT_HOTKEY_MODIFIERS: u32 = HOTKEY_MOD_ALT | HOTKEY_MOD_NOREPEAT;

/// ClipboardBoard 当前已知的全部设置。
///
/// 后续原子只在此 DTO 上兼容增加字段；未知 JSON 字段由持久化层独立保留。
#[derive(Clone, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default)]
pub struct AppSettings {
    /// 历史与图片容量设置。
    pub history: HistorySettings,
    /// 本地隐私与记录策略。
    pub privacy: PrivacySettings,
    /// 可跨重启恢复的全局快捷键配置。
    pub hotkey: HotkeySettings,
}

impl Default for AppSettings {
    /// 返回产品根计划定义的默认配置。
    fn default() -> Self {
        Self {
            history: HistorySettings::default(),
            privacy: PrivacySettings::default(),
            hotkey: HotkeySettings::default(),
        }
    }
}

/// 可持久化的全局快捷键组合；进程内注册 ID 不写入配置。
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default)]
pub struct HotkeySettings {
    /// Windows `MOD_*` 修饰位，只允许物理修饰和 `MOD_NOREPEAT`。
    pub modifiers: u32,
    /// Windows 虚拟键码，范围为 `1..=0xFE`。
    pub virtual_key: u32,
}

impl Default for HotkeySettings {
    /// 返回产品默认 Alt+V 组合。
    fn default() -> Self {
        Self {
            modifiers: DEFAULT_HOTKEY_MODIFIERS,
            virtual_key: DEFAULT_HOTKEY_VIRTUAL_KEY,
        }
    }
}

impl HotkeySettings {
    /// 返回用于配置 UI 和错误信息的规范化名称；顺序固定为 Win、Ctrl、Alt、Shift、主键。
    pub fn label(&self) -> String {
        let mut parts = Vec::with_capacity(5);
        if self.modifiers & HOTKEY_MOD_WIN != 0 {
            parts.push("Win".to_owned());
        }
        if self.modifiers & HOTKEY_MOD_CONTROL != 0 {
            parts.push("Ctrl".to_owned());
        }
        if self.modifiers & HOTKEY_MOD_ALT != 0 {
            parts.push("Alt".to_owned());
        }
        if self.modifiers & HOTKEY_MOD_SHIFT != 0 {
            parts.push("Shift".to_owned());
        }
        parts.push(format_virtual_key(self.virtual_key));
        parts.join(" + ")
    }
}

/// 把常用虚拟键转换为短标签；未知键保留十六进制，避免伪造可读名称。
fn format_virtual_key(virtual_key: u32) -> String {
    match virtual_key {
        0x09 => "Tab".to_owned(),
        0x0D => "Enter".to_owned(),
        0x1B => "Esc".to_owned(),
        0x20 => "Space".to_owned(),
        0x2E => "Delete".to_owned(),
        0x70..=0x7B => format!("F{}", virtual_key - 0x6F),
        0x30..=0x39 | 0x41..=0x5A => char::from_u32(virtual_key).map_or_else(
            || format!("VK_{virtual_key:02X}"),
            |character| character.to_ascii_uppercase().to_string(),
        ),
        _ => format!("VK_{virtual_key:02X}"),
    }
}

impl fmt::Debug for AppSettings {
    /// 只输出有限设置摘要，避免 Debug 递归展开排除规则或配置正文。
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AppSettings")
            .field("history", &self.history)
            .field("privacy", &self.privacy)
            .field("hotkey", &self.hotkey)
            .finish()
    }
}

/// 剪贴板记录隐私设置。
#[derive(Clone, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default)]
pub struct PrivacySettings {
    /// 当前持久化的记录暂停状态。
    pub recording_pause: RecordingPause,
    /// 不进入 ClipboardIO 正文读取的来源程序规则。
    pub excluded_apps: Vec<String>,
}

impl fmt::Debug for PrivacySettings {
    /// 只输出暂停模式和规则数量，不输出规则路径或文件名。
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PrivacySettings")
            .field("recording_pause", &self.recording_pause)
            .field("excluded_apps_count", &self.excluded_apps.len())
            .finish()
    }
}

/// 可跨重启恢复的记录暂停模式。
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "mode", content = "until_unix_millis", rename_all = "snake_case")]
pub enum RecordingPause {
    /// 正常读取剪贴板更新。
    #[default]
    Active,
    /// 暂停到指定 UTC Unix epoch 毫秒。
    UntilUnixMillis(u64),
    /// 无限暂停，必须由用户显式恢复。
    Indefinite,
}

/// 历史记录与图片捕获的限制配置。
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default)]
pub struct HistorySettings {
    /// 普通历史最大条数。
    pub max_items: u32,
    /// 普通历史最长保留天数。
    pub retention_days: u32,
    /// 图片资产空间上限，单位 MiB。
    pub image_quota_mib: u32,
    /// 是否捕获图片内容。
    pub capture_images: bool,
    /// 是否记录来源程序名称。
    pub capture_source_app: bool,
    /// 可选图片资产根；缺省时由图片存储模块解析为应用默认目录。
    pub image_storage_root: Option<String>,
}

impl Default for HistorySettings {
    /// 返回根计划中的历史默认参数。
    fn default() -> Self {
        Self {
            max_items: 2_000,
            retention_days: 30,
            image_quota_mib: 500,
            capture_images: true,
            capture_source_app: true,
            image_storage_root: None,
        }
    }
}

/// 配置快照来自哪个耐久副本。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SettingsLoadSource {
    /// 主配置文件通过完整验证。
    Primary,
    /// 主文件缺失或损坏，已从备份恢复。
    Backup,
    /// 主文件和备份都不存在，使用产品默认值。
    Defaults,
}

/// 对外只读配置快照；revision 在当前进程内单调递增。
#[derive(Clone, Eq, PartialEq)]
pub struct SettingsSnapshot {
    /// 当前已提交配置。
    settings: AppSettings,
    /// 当前快照来源。
    source: SettingsLoadSource,
    /// 当前进程内乐观并发修订号。
    revision: u64,
}

impl fmt::Debug for SettingsSnapshot {
    /// 快照 Debug 只输出来源和 revision，不递归输出完整配置 JSON。
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SettingsSnapshot")
            .field("source", &self.source)
            .field("revision", &self.revision)
            .finish()
    }
}

impl SettingsSnapshot {
    /// 构造 worker 内部快照。
    pub(crate) fn new(settings: AppSettings, source: SettingsLoadSource, revision: u64) -> Self {
        Self {
            settings,
            source,
            revision,
        }
    }

    /// 返回当前配置的只读引用。
    pub fn settings(&self) -> &AppSettings {
        &self.settings
    }

    /// 返回快照耐久来源。
    pub fn source(&self) -> SettingsLoadSource {
        self.source
    }

    /// 返回当前进程内修订号。
    pub fn revision(&self) -> u64 {
        self.revision
    }
}

/// 已知设置语义非法时的稳定字段标识。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ValidationField {
    /// 普通历史条数。
    MaxItems,
    /// 历史保留天数。
    RetentionDays,
    /// 图片空间 MiB。
    ImageQuotaMib,
    /// 排除程序规则集合。
    ExcludedApps,
    /// 图片资产根路径。
    ImageStorageRoot,
    /// 全局快捷键组合。
    Hotkey,
}

/// load 与 save 共用的语义验证器，避免写入无法重新加载的配置。
pub(crate) fn validate_settings(settings: &AppSettings) -> Result<(), ValidationField> {
    if !MAX_ITEMS_RANGE.contains(&settings.history.max_items) {
        return Err(ValidationField::MaxItems);
    }
    if !RETENTION_DAYS_RANGE.contains(&settings.history.retention_days) {
        return Err(ValidationField::RetentionDays);
    }
    if !IMAGE_QUOTA_MIB_RANGE.contains(&settings.history.image_quota_mib) {
        return Err(ValidationField::ImageQuotaMib);
    }
    // 路径保存和启动转换必须复用图片存储模块的唯一解析器，防止两处规则漂移。
    if parse_image_storage_preference(settings.history.image_storage_root.as_deref()).is_err() {
        return Err(ValidationField::ImageStorageRoot);
    }
    validate_hotkey(&settings.hotkey)?;
    validate_excluded_apps(&settings.privacy.excluded_apps)?;
    Ok(())
}

/// 校验持久化快捷键的位掩码、虚拟键和明确系统保留组合。
pub(crate) fn validate_hotkey(settings: &HotkeySettings) -> Result<(), ValidationField> {
    if settings.virtual_key == 0
        || settings.virtual_key > 0xFE
        || matches!(
            settings.virtual_key,
            0x10 | 0x11 | 0x12 | 0x5B | 0x5C | 0x5D | 0xE5 | 0xE7
        )
        || settings.modifiers & !HOTKEY_ALLOWED_MODIFIERS != 0
        || settings.modifiers & HOTKEY_PHYSICAL_MODIFIERS == 0
    {
        return Err(ValidationField::Hotkey);
    }

    let physical = settings.modifiers & HOTKEY_PHYSICAL_MODIFIERS;
    // 明确拒绝会覆盖常用系统行为的组合；其余动态冲突交给 RegisterHotKey 返回值。
    let reserved = (physical == HOTKEY_MOD_ALT && matches!(settings.virtual_key, 0x09 | 0x73))
        || (physical == HOTKEY_MOD_WIN && settings.virtual_key == 0x4C)
        || (physical == HOTKEY_MOD_CONTROL | HOTKEY_MOD_ALT && settings.virtual_key == 0x2E);
    if reserved {
        return Err(ValidationField::Hotkey);
    }
    Ok(())
}

/// 校验排除规则集合的大小、控制字符和 Windows 路径语法。
pub(crate) fn validate_excluded_apps(rules: &[String]) -> Result<(), ValidationField> {
    if rules.len() > MAX_EXCLUDED_APPS
        || rules
            .iter()
            .any(|rule| normalize_excluded_app_rule(rule).is_none())
    {
        return Err(ValidationField::ExcludedApps);
    }
    Ok(())
}

/// 将配置中的单条规则规范化；匹配阶段仍使用 Windows 序号忽略大小写。
pub(crate) fn normalize_excluded_app_rule(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() || trimmed.len() > MAX_EXCLUDED_APP_RULE_BYTES || trimmed.contains('\0') {
        return None;
    }

    let replaced = trimmed.replace('/', "\\");
    if !replaced.contains('\\') && !replaced.contains(':') {
        if replaced == "." || replaced == ".." {
            return None;
        }
        return Some(replaced);
    }
    normalize_absolute_windows_path(&replaced, MAX_EXCLUDED_APP_RULE_BYTES)
}

/// 规范化进程映像的绝对路径；来源路径允许长于配置单项但受 Win32 缓冲区限制。
pub(crate) fn normalize_process_image_path(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() || trimmed.contains('\0') {
        return None;
    }
    normalize_absolute_windows_path(&trimmed.replace('/', "\\"), 32 * 1024)
}

/// 只接受绝对 DOS/UNC 路径并拒绝相对、重复分隔符和扩展前缀。
fn normalize_absolute_windows_path(value: &str, max_bytes: usize) -> Option<String> {
    if value.is_empty() || value.len() > max_bytes || value.starts_with(r"\\?\") {
        return None;
    }

    if let Some(rest) = value.strip_prefix(r"\\") {
        let parts: Vec<&str> = rest.split('\\').collect();
        if parts.len() < 3
            || parts
                .iter()
                .any(|part| part.is_empty() || *part == "." || *part == "..")
        {
            return None;
        }
        return Some(format!(r"\\{}", parts.join("\\")));
    }

    let bytes = value.as_bytes();
    if bytes.len() < 3 || !bytes[0].is_ascii_alphabetic() || bytes[1] != b':' || bytes[2] != b'\\' {
        return None;
    }
    let rest = &value[3..];
    if rest.is_empty() {
        return None;
    }
    let parts: Vec<&str> = rest.split('\\').collect();
    if parts
        .iter()
        .any(|part| part.is_empty() || *part == "." || *part == "..")
    {
        return None;
    }
    Some(format!("{}:\\{}", bytes[0] as char, parts.join("\\")))
}

#[cfg(test)]
mod tests {
    //! 此测试模块验证历史数值范围的闭区间边界。

    use super::{
        normalize_excluded_app_rule, validate_hotkey, validate_settings, AppSettings,
        HistorySettings, HotkeySettings, ValidationField, DEFAULT_HOTKEY_MODIFIERS,
        DEFAULT_HOTKEY_VIRTUAL_KEY, HOTKEY_MOD_ALT, HOTKEY_MOD_CONTROL, HOTKEY_MOD_NOREPEAT,
        HOTKEY_MOD_SHIFT, HOTKEY_MOD_WIN, IMAGE_QUOTA_MIB_RANGE, MAX_EXCLUDED_APPS,
        MAX_ITEMS_RANGE, RETENTION_DAYS_RANGE,
    };

    /// 最小值和最大值均合法，越界值均被统一验证器拒绝。
    #[test]
    fn validates_all_history_numeric_boundaries() {
        for (max_items, retention_days, image_quota_mib, expected) in [
            (
                *MAX_ITEMS_RANGE.start(),
                *RETENTION_DAYS_RANGE.start(),
                *IMAGE_QUOTA_MIB_RANGE.start(),
                true,
            ),
            (
                *MAX_ITEMS_RANGE.end(),
                *RETENTION_DAYS_RANGE.end(),
                *IMAGE_QUOTA_MIB_RANGE.end(),
                true,
            ),
            (0, 30, 500, false),
            (100_001, 30, 500, false),
            (2_000, 0, 500, false),
            (2_000, 3_651, 500, false),
            (2_000, 30, 15, false),
            (2_000, 30, 10_241, false),
        ] {
            let settings = AppSettings {
                history: HistorySettings {
                    max_items,
                    retention_days,
                    image_quota_mib,
                    ..HistorySettings::default()
                },
                ..AppSettings::default()
            };
            assert_eq!(validate_settings(&settings).is_ok(), expected);
        }
    }

    /// 排除规则只接受 basename 或绝对 DOS/UNC 路径，并拒绝相对和扩展前缀。
    #[test]
    fn validates_excluded_app_path_boundaries() {
        for valid in [
            "KeePass.exe",
            "工具.EXE",
            r"C:\Program Files\KeePass\KeePass.exe",
            r"C:/Program Files/KeePass/KeePass.exe",
            r"\\server\share\KeePass.exe",
        ] {
            assert!(normalize_excluded_app_rule(valid).is_some(), "{valid}");
        }
        for invalid in [
            "",
            "  ",
            "C:KeePass.exe",
            r".\KeePass.exe",
            r"..\KeePass.exe",
            r"\KeePass.exe",
            r"C:\..\KeePass.exe",
            r"C:\Program Files\\KeePass.exe",
            r"\\?\C:\KeePass.exe",
            r"\\server\share",
        ] {
            assert!(normalize_excluded_app_rule(invalid).is_none(), "{invalid}");
        }
    }

    /// 规则集合条数和单项边界均会进入同一个稳定字段错误。
    #[test]
    fn rejects_excluded_app_count_overflow() {
        let mut settings = AppSettings::default();
        settings.privacy.excluded_apps = vec!["a.exe".to_owned(); MAX_EXCLUDED_APPS + 1];
        assert!(validate_settings(&settings).is_err());
    }

    /// 图片根设置必须复用 image_storage 的唯一解析器，并映射为稳定字段错误。
    #[test]
    fn validates_image_storage_root_through_shared_parser() {
        let mut settings = AppSettings::default();
        assert!(validate_settings(&settings).is_ok());

        settings.history.image_storage_root = Some(r"D:\ClipboardAssets\Images".to_owned());
        assert!(validate_settings(&settings).is_ok());

        settings.history.image_storage_root = Some("relative\\images".to_owned());
        assert_eq!(
            validate_settings(&settings),
            Err(ValidationField::ImageStorageRoot)
        );
    }

    /// 自定义 Debug 只输出规则数量，不泄露规则字符串。
    #[test]
    fn settings_debug_is_redacted() {
        let mut settings = AppSettings::default();
        settings.privacy.excluded_apps = vec![r"C:\secret\password-manager.exe".to_owned()];
        let debug = format!("{settings:?}");
        assert!(debug.contains("excluded_apps_count"));
        assert!(!debug.contains("password-manager"));
        assert!(!debug.contains("C:\\secret"));
    }

    /// 默认快捷键保持 Alt+V，并且规范化标签不依赖运行时注册 ID。
    #[test]
    fn validates_default_hotkey_and_label() {
        let hotkey = HotkeySettings::default();
        assert_eq!(hotkey.modifiers, DEFAULT_HOTKEY_MODIFIERS);
        assert_eq!(hotkey.virtual_key, DEFAULT_HOTKEY_VIRTUAL_KEY);
        assert_eq!(hotkey.label(), "Alt + V");
        assert!(validate_hotkey(&hotkey).is_ok());
    }

    /// 快捷键只允许物理修饰、有效主键，并拒绝计划明确列出的系统组合。
    #[test]
    fn rejects_hotkey_modifier_vk_and_reserved_boundaries() {
        for settings in [
            HotkeySettings {
                modifiers: 0,
                virtual_key: 0x56,
            },
            HotkeySettings {
                modifiers: HOTKEY_MOD_ALT | 0x20,
                virtual_key: 0x56,
            },
            HotkeySettings {
                modifiers: HOTKEY_MOD_ALT,
                virtual_key: 0,
            },
            HotkeySettings {
                modifiers: HOTKEY_MOD_ALT,
                virtual_key: 0xFF,
            },
            HotkeySettings {
                modifiers: HOTKEY_MOD_ALT,
                virtual_key: 0x09,
            },
            HotkeySettings {
                modifiers: HOTKEY_MOD_ALT,
                virtual_key: 0x73,
            },
            HotkeySettings {
                modifiers: 0x0008,
                virtual_key: 0x4C,
            },
            HotkeySettings {
                modifiers: HOTKEY_MOD_CONTROL | HOTKEY_MOD_ALT,
                virtual_key: 0x2E,
            },
            HotkeySettings {
                modifiers: HOTKEY_MOD_ALT,
                virtual_key: 0x10,
            },
            HotkeySettings {
                modifiers: HOTKEY_MOD_CONTROL,
                virtual_key: 0x11,
            },
            HotkeySettings {
                modifiers: HOTKEY_MOD_SHIFT,
                virtual_key: 0x12,
            },
            HotkeySettings {
                modifiers: HOTKEY_MOD_WIN,
                virtual_key: 0x5B,
            },
            HotkeySettings {
                modifiers: HOTKEY_MOD_WIN,
                virtual_key: 0x5C,
            },
            HotkeySettings {
                modifiers: HOTKEY_MOD_WIN,
                virtual_key: 0x5D,
            },
            HotkeySettings {
                modifiers: HOTKEY_MOD_CONTROL,
                virtual_key: 0xE5,
            },
            HotkeySettings {
                modifiers: HOTKEY_MOD_CONTROL,
                virtual_key: 0xE7,
            },
        ] {
            assert!(validate_hotkey(&settings).is_err(), "{settings:?}");
        }
        assert!(validate_hotkey(&HotkeySettings {
            modifiers: HOTKEY_MOD_CONTROL | HOTKEY_MOD_NOREPEAT,
            virtual_key: 0x4B,
        })
        .is_ok());
    }

    /// 快捷键字段缺失时 serde 必须回退默认组合，满足旧配置向前兼容。
    #[test]
    fn missing_hotkey_fields_use_default() {
        let settings: AppSettings = serde_json::from_value(serde_json::json!({
            "history": {},
            "privacy": {}
        }))
        .expect("旧配置应能解析");
        assert_eq!(settings.hotkey, HotkeySettings::default());
    }
}
