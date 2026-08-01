//! 此文件定义可持久化配置 DTO、默认值、快照身份和统一语义验证规则。

use serde::{Deserialize, Serialize};

/// 当前配置文档唯一支持的 schema 版本。
pub(crate) const CURRENT_SCHEMA_VERSION: u64 = 1;

/// 普通历史条数的合法范围。
pub(crate) const MAX_ITEMS_RANGE: std::ops::RangeInclusive<u32> = 1..=100_000;
/// 历史保留天数的合法范围。
pub(crate) const RETENTION_DAYS_RANGE: std::ops::RangeInclusive<u32> = 1..=3_650;
/// 图片空间 MiB 的合法范围。
pub(crate) const IMAGE_QUOTA_MIB_RANGE: std::ops::RangeInclusive<u32> = 16..=10_240;

/// ClipboardBoard 当前已知的全部设置。
///
/// 后续原子只在此 DTO 上兼容增加字段；未知 JSON 字段由持久化层独立保留。
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default)]
pub struct AppSettings {
    /// 历史与图片容量设置。
    pub history: HistorySettings,
    /// 本地隐私与记录策略。
    pub privacy: PrivacySettings,
}

impl Default for AppSettings {
    /// 返回产品根计划定义的默认配置。
    fn default() -> Self {
        Self {
            history: HistorySettings::default(),
            privacy: PrivacySettings::default(),
        }
    }
}

/// 剪贴板记录隐私设置。
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default)]
pub struct PrivacySettings {
    /// 当前持久化的记录暂停状态。
    pub recording_pause: RecordingPause,
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
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SettingsSnapshot {
    /// 当前已提交配置。
    settings: AppSettings,
    /// 当前快照来源。
    source: SettingsLoadSource,
    /// 当前进程内乐观并发修订号。
    revision: u64,
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
    Ok(())
}

#[cfg(test)]
mod tests {
    //! 此测试模块验证历史数值范围的闭区间边界。

    use super::{
        validate_settings, AppSettings, HistorySettings, IMAGE_QUOTA_MIB_RANGE, MAX_ITEMS_RANGE,
        RETENTION_DAYS_RANGE,
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
}
