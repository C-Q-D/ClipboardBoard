//! 此文件负责配置副本加载、未知字段保留与耐久发布；所有调用都发生在 SettingsWorker。

use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    process,
    sync::{
        atomic::{AtomicU64, Ordering},
        OnceLock,
    },
    time::{SystemTime, UNIX_EPOCH},
};

use serde_json::{Map, Value};

use super::{
    model::{validate_settings, SettingsSnapshot, ValidationField, CURRENT_SCHEMA_VERSION},
    AppSettings, SettingsError, SettingsLoadSource,
};

#[cfg(windows)]
use super::windows_replace::{self, ReplaceFailureKind};

/// 单份设置文件的最大字节数，阻止损坏文件造成无界分配。
const MAX_SETTINGS_BYTES: u64 = 1024 * 1024;
/// staging 文件名碰撞的最大重试次数。
const STAGING_CREATE_ATTEMPTS: usize = 64;
/// 同一进程内分配 staging token。
static NEXT_STAGING_TOKEN: AtomicU64 = AtomicU64::new(0);

/// worker 内部同时保留对外快照和未知 JSON 文档。
pub(super) struct LoadedSettings {
    /// 对外可观察快照。
    pub snapshot: SettingsSnapshot,
    /// 后续保存时需要原样写回的完整已验证文档。
    pub document: Value,
    /// 主文件是否经过当前统一验证器验证。
    pub primary_valid: bool,
    /// 备份是否经过当前统一验证器验证。
    pub backup_valid: bool,
}

/// 单个耐久副本的验证分类。
enum Candidate {
    /// 路径不存在。
    Missing,
    /// 当前版本可安全使用。
    Valid {
        /// 已知设置。
        settings: AppSettings,
        /// 包含未知字段的完整文档。
        document: Value,
    },
    /// 文件存在，但内容、schema 或语义损坏。
    Corrupt,
    /// 主文件来自未来版本，旧程序不得降级覆盖。
    UnsupportedFutureSchema(u64),
}

/// 从显式配置目录加载主文件、备份或默认值。
pub(super) fn load(directory: &Path) -> Result<LoadedSettings, SettingsError> {
    fs::create_dir_all(directory)?;
    let primary = directory.join("settings.json");
    let backup = directory.join("settings.json.bak");

    match read_candidate(&primary) {
        Candidate::Valid { settings, document } => Ok(LoadedSettings {
            snapshot: SettingsSnapshot::new(settings, SettingsLoadSource::Primary, 0),
            document,
            primary_valid: true,
            backup_valid: matches!(read_candidate(&backup), Candidate::Valid { .. }),
        }),
        Candidate::UnsupportedFutureSchema(version) => {
            Err(SettingsError::UnsupportedSchema(version))
        }
        Candidate::Missing => load_backup_or_default(&backup, false),
        Candidate::Corrupt => load_backup_or_default(&backup, true),
    }
}

/// 保存 compare-and-save 事务；Win32 成功返回后才更新 worker 快照。
pub(super) fn save(
    directory: &Path,
    loaded: &mut LoadedSettings,
    expected_revision: u64,
    settings: AppSettings,
) -> Result<SettingsSnapshot, SettingsError> {
    save_with_publisher(
        directory,
        loaded,
        expected_revision,
        settings,
        None,
        publish,
    )
}

/// 测试专用保存故障点；生产调用始终传 None。
#[derive(Clone, Copy, Eq, PartialEq)]
enum SaveFault {
    /// staging 独占创建后失败。
    Created,
    /// JSON 写入后失败。
    Written,
    /// flush 后失败。
    Flushed,
    /// sync_all 后、Win32 发布前失败。
    Synced,
}

/// 使用注入发布适配器执行完整保存；测试可精确模拟 Win32 后置状态。
fn save_with_publisher<F>(
    directory: &Path,
    loaded: &mut LoadedSettings,
    expected_revision: u64,
    settings: AppSettings,
    fault: Option<SaveFault>,
    publisher: F,
) -> Result<SettingsSnapshot, SettingsError>
where
    F: FnOnce(&Path, &Path, &Path, bool) -> Result<(), PublishFailure>,
{
    if expected_revision != loaded.snapshot.revision() {
        return Err(SettingsError::RevisionConflict {
            expected: expected_revision,
            actual: loaded.snapshot.revision(),
        });
    }
    validate_settings(&settings).map_err(validation_error)?;
    let next_revision = loaded
        .snapshot
        .revision()
        .checked_add(1)
        .ok_or(SettingsError::RevisionExhausted)?;

    let primary = directory.join("settings.json");
    let backup = directory.join("settings.json.bak");
    // 保存前重新验证主文件，防止外部损坏把有效备份覆盖掉。
    loaded.primary_valid = match read_candidate(&primary) {
        Candidate::Valid { .. } => true,
        Candidate::UnsupportedFutureSchema(version) => {
            return Err(SettingsError::UnsupportedSchema(version));
        }
        Candidate::Missing | Candidate::Corrupt => false,
    };

    let document = merge_known_settings(&loaded.document, &settings);
    let serialized = serde_json::to_vec_pretty(&document).map_err(SettingsError::Serialization)?;
    let mut staging = create_staging(directory)?;
    inject_fault(fault, SaveFault::Created)?;
    staging.file_mut().write_all(&serialized)?;
    inject_fault(fault, SaveFault::Written)?;
    staging.file_mut().flush()?;
    inject_fault(fault, SaveFault::Flushed)?;
    staging.file_mut().sync_all()?;
    inject_fault(fault, SaveFault::Synced)?;
    staging.close();

    let publish_result = publisher(&primary, &backup, staging.path(), loaded.primary_valid);
    if let Err(error) = publish_result {
        if matches!(
            error.kind,
            ReplaceFailureKind::UnableToMoveReplacement2 | ReplaceFailureKind::UnknownPostState
        ) {
            staging.preserve();
        }
        // Win32 失败可能改变多个路径，必须重新验证主副本和备份。
        loaded.primary_valid = matches!(read_candidate(&primary), Candidate::Valid { .. });
        loaded.backup_valid = matches!(read_candidate(&backup), Candidate::Valid { .. });
        return Err(SettingsError::Io(error.error));
    }
    staging.mark_published();

    // Win32 成功返回是唯一线性化点：先提交内存状态，再由 worker 发送回执。
    loaded.document = document;
    loaded.primary_valid = true;
    loaded.backup_valid = matches!(read_candidate(&backup), Candidate::Valid { .. });
    loaded.snapshot = SettingsSnapshot::new(settings, SettingsLoadSource::Primary, next_revision);
    Ok(loaded.snapshot.clone())
}

/// 在测试指定阶段返回稳定 IO 错误，验证 RAII 清理和内存事务顺序。
fn inject_fault(fault: Option<SaveFault>, stage: SaveFault) -> Result<(), SettingsError> {
    if fault == Some(stage) {
        Err(SettingsError::Io(std::io::Error::other(
            "ATOM-44 测试注入保存故障",
        )))
    } else {
        Ok(())
    }
}

/// 读取备份；主副本损坏时不允许静默回到默认值。
fn load_backup_or_default(
    backup: &Path,
    primary_was_corrupt: bool,
) -> Result<LoadedSettings, SettingsError> {
    match read_candidate(backup) {
        Candidate::Valid { settings, document } => Ok(LoadedSettings {
            snapshot: SettingsSnapshot::new(settings, SettingsLoadSource::Backup, 0),
            document,
            primary_valid: false,
            backup_valid: true,
        }),
        Candidate::UnsupportedFutureSchema(version) => {
            Err(SettingsError::UnsupportedSchema(version))
        }
        Candidate::Corrupt => Err(SettingsError::UnrecoverableConfiguration),
        Candidate::Missing if primary_was_corrupt => Err(SettingsError::UnrecoverableConfiguration),
        Candidate::Missing => Ok(LoadedSettings {
            snapshot: SettingsSnapshot::new(
                AppSettings::default(),
                SettingsLoadSource::Defaults,
                0,
            ),
            document: Value::Object(Map::new()),
            primary_valid: false,
            backup_valid: false,
        }),
    }
}

/// 有界读取并完整验证一个 JSON 副本。
fn read_candidate(path: &Path) -> Candidate {
    let mut file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Candidate::Missing,
        Err(_) => return Candidate::Corrupt,
    };
    let Ok(metadata) = file.metadata() else {
        return Candidate::Corrupt;
    };
    if metadata.len() > MAX_SETTINGS_BYTES {
        return Candidate::Corrupt;
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    if Read::by_ref(&mut file)
        .take(MAX_SETTINGS_BYTES + 1)
        .read_to_end(&mut bytes)
        .is_err()
        || bytes.len() as u64 > MAX_SETTINGS_BYTES
    {
        return Candidate::Corrupt;
    }
    let Ok(document) = serde_json::from_slice::<Value>(&bytes) else {
        return Candidate::Corrupt;
    };
    let Some(object) = document.as_object() else {
        return Candidate::Corrupt;
    };
    let schema = match object.get("schema_version") {
        None => CURRENT_SCHEMA_VERSION,
        Some(value) => match value.as_u64() {
            Some(version) => version,
            None => return Candidate::Corrupt,
        },
    };
    if schema > CURRENT_SCHEMA_VERSION {
        return Candidate::UnsupportedFutureSchema(schema);
    }
    if schema != CURRENT_SCHEMA_VERSION {
        return Candidate::Corrupt;
    }
    let Ok(settings) = serde_json::from_value::<AppSettings>(document.clone()) else {
        return Candidate::Corrupt;
    };
    if validate_settings(&settings).is_err() {
        return Candidate::Corrupt;
    }
    Candidate::Valid { settings, document }
}

/// 把完整已知 DTO 覆盖到旧文档，同时递归保留 history 中的未知值。
fn merge_known_settings(document: &Value, settings: &AppSettings) -> Value {
    let mut known = serde_json::to_value(settings).expect("AppSettings 的派生序列化不会失败");
    known
        .as_object_mut()
        .expect("AppSettings 必须序列化为对象")
        .insert(
            "schema_version".to_owned(),
            Value::from(CURRENT_SCHEMA_VERSION),
        );
    merge_known_document(document, &known)
}

/// 通用递归合并接缝：已知值覆盖同名旧值，每层对象继续保留未知键。
fn merge_known_document(document: &Value, known: &Value) -> Value {
    let (Some(old), Some(known)) = (document.as_object(), known.as_object()) else {
        return known.clone();
    };
    let mut merged = old.clone();
    for (key, known_value) in known {
        let value = merged.get(key).map_or_else(
            || known_value.clone(),
            |old_value| merge_known_document(old_value, known_value),
        );
        merged.insert(key.clone(), value);
    }
    Value::Object(merged)
}

/// 将内部字段枚举映射为不含值的公共错误。
fn validation_error(field: ValidationField) -> SettingsError {
    SettingsError::InvalidSettings(match field {
        ValidationField::MaxItems => "history.max_items",
        ValidationField::RetentionDays => "history.retention_days",
        ValidationField::ImageQuotaMib => "history.image_quota_mib",
    })
}

/// 返回当前进程固定 nonce。
fn process_instance_nonce() -> u128 {
    static NONCE: OnceLock<u128> = OnceLock::new();
    *NONCE.get_or_init(|| {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |duration| duration.as_nanos())
            ^ u128::from(process::id())
    })
}

/// 独占 staging 文件；只有本次调用拥有且未发布的路径才会被清理。
struct StagingFile {
    /// staging 路径。
    path: PathBuf,
    /// 同步前保持打开的文件。
    file: Option<File>,
    /// 发布成功后禁止 Drop 删除主文件路径。
    published: bool,
    /// 后置状态未知时保留证据。
    preserve: bool,
}

impl StagingFile {
    /// 返回写入句柄。
    fn file_mut(&mut self) -> &mut File {
        self.file.as_mut().expect("staging 文件已经关闭")
    }

    /// 返回 staging 路径。
    fn path(&self) -> &Path {
        &self.path
    }

    /// flush/sync 后关闭句柄，满足 Win32 替换要求。
    fn close(&mut self) {
        self.file.take();
    }

    /// 标记路径已经被原子移动为主文件。
    fn mark_published(&mut self) {
        self.published = true;
    }

    /// 后置状态未知时保留 staging 证据。
    fn preserve(&mut self) {
        self.preserve = true;
    }
}

impl Drop for StagingFile {
    /// 仅删除仍由本次调用明确拥有的未发布 staging。
    fn drop(&mut self) {
        self.file.take();
        if !self.published && !self.preserve {
            let _ = fs::remove_file(&self.path);
        }
    }
}

/// 用实例 nonce、PID 和单调 token 在固定上限内创建 staging。
fn create_staging(directory: &Path) -> Result<StagingFile, SettingsError> {
    create_staging_with(directory, || {
        NEXT_STAGING_TOKEN.fetch_add(1, Ordering::Relaxed)
    })
}

/// 使用可注入 token 源执行有界独占创建，证明碰撞不会无限重试。
fn create_staging_with(
    directory: &Path,
    mut next_token: impl FnMut() -> u64,
) -> Result<StagingFile, SettingsError> {
    for _ in 0..STAGING_CREATE_ATTEMPTS {
        let token = next_token();
        let path = directory.join(format!(
            "settings-{:032x}-{}-{token}.json.tmp",
            process_instance_nonce(),
            process::id()
        ));
        match OpenOptions::new().create_new(true).write(true).open(&path) {
            Ok(file) => {
                return Ok(StagingFile {
                    path,
                    file: Some(file),
                    published: false,
                    preserve: false,
                });
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(SettingsError::Io(error)),
        }
    }
    Err(SettingsError::Io(std::io::Error::new(
        std::io::ErrorKind::AlreadyExists,
        "配置 staging 在有界重试内持续碰撞",
    )))
}

/// 发布失败携带精确后置状态类别和原始 IO 错误。
struct PublishFailure {
    /// ReplaceFileW 文档分类。
    kind: ReplaceFailureKind,
    /// 原始 Windows 错误。
    error: std::io::Error,
}

/// 使用同目录 Windows 原子操作发布 staging。
#[cfg(windows)]
fn publish(
    primary: &Path,
    backup: &Path,
    staging: &Path,
    primary_valid: bool,
) -> Result<(), PublishFailure> {
    if primary.exists() {
        let backup = primary_valid.then_some(backup);
        windows_replace::replace(primary, staging, backup).map_err(|failure| PublishFailure {
            kind: failure.kind,
            error: failure.error,
        })
    } else {
        windows_replace::move_new(staging, primary).map_err(|error| PublishFailure {
            kind: ReplaceFailureKind::UnknownPostState,
            error,
        })
    }
}

/// 非 Windows 构建仅用于保持库可检查；正式产品不使用此分支。
#[cfg(not(windows))]
fn publish(
    primary: &Path,
    _backup: &Path,
    staging: &Path,
    _primary_valid: bool,
) -> Result<(), PublishFailure> {
    fs::rename(staging, primary).map_err(|error| PublishFailure {
        kind: ReplaceFailureKind::UnknownPostState,
        error,
    })
}

#[cfg(not(windows))]
/// 非 Windows 编译所需的最小后置状态类型。
enum ReplaceFailureKind {
    /// 跨平台 rename 错误没有 Win32 后置状态保证。
    UnknownPostState,
    /// 占位以保持跨平台 matches 可编译。
    UnableToMoveReplacement2,
}

#[cfg(test)]
mod tests {
    //! 此测试模块用内部发布 seam 验证 Win32 失败不会提交内存快照。

    use std::{
        fs, io,
        path::PathBuf,
        process,
        sync::{
            atomic::{AtomicU64, Ordering},
            OnceLock,
        },
        time::{SystemTime, UNIX_EPOCH},
    };

    use super::{
        create_staging_with, load, merge_known_document, save, save_with_publisher, PublishFailure,
        ReplaceFailureKind, SaveFault, STAGING_CREATE_ATTEMPTS,
    };
    use crate::settings::{AppSettings, HistorySettings};

    /// 测试临时根 token。
    static NEXT_TEST_TOKEN: AtomicU64 = AtomicU64::new(0);

    /// 返回测试进程固定 nonce。
    fn test_nonce() -> u128 {
        static NONCE: OnceLock<u128> = OnceLock::new();
        *NONCE.get_or_init(|| {
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |duration| duration.as_nanos())
                ^ u128::from(process::id())
        })
    }

    /// 独占创建内部持久化测试根。
    fn temporary_directory(label: &str) -> PathBuf {
        for _ in 0..64 {
            let token = NEXT_TEST_TOKEN.fetch_add(1, Ordering::Relaxed);
            let directory = std::env::temp_dir().join(format!(
                "clipboard-board-atom44-settings-core-{:032x}-{}-{token}-{label}",
                test_nonce(),
                process::id()
            ));
            match fs::create_dir(&directory) {
                Ok(()) => return directory,
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => panic!("创建持久化测试根失败：{error}"),
            }
        }
        panic!("持久化测试根在有界重试内持续碰撞");
    }

    /// 构造合法的非默认配置。
    fn settings() -> AppSettings {
        AppSettings {
            history: HistorySettings {
                max_items: 2_222,
                ..HistorySettings::default()
            },
            ..AppSettings::default()
        }
    }

    /// 通用合并接缝证明未来新增顶层字段覆盖旧同名 raw，同时保留其余未知值。
    #[test]
    fn complete_known_document_wins_over_raw_top_level_values() {
        let old = serde_json::json!({
            "schema_version": 1,
            "future_known": "旧值",
            "unknown_top": {"keep": true},
            "history": {"max_items": 10, "unknown_history": "保留"}
        });
        let known = serde_json::json!({
            "schema_version": 1,
            "future_known": "新值",
            "history": {"max_items": 20}
        });
        let merged = merge_known_document(&old, &known);
        assert_eq!(merged["future_known"], "新值");
        assert_eq!(merged["unknown_top"]["keep"], true);
        assert_eq!(merged["history"]["max_items"], 20);
        assert_eq!(merged["history"]["unknown_history"], "保留");

        let restarted: serde_json::Value =
            serde_json::from_slice(&serde_json::to_vec(&merged).unwrap()).unwrap();
        assert_eq!(restarted, merged);
    }

    /// staging token 连续碰撞只重试固定次数，且不删除碰撞文件。
    #[test]
    fn staging_collision_has_bounded_retries_and_preserves_foreign_file() {
        let directory = temporary_directory("staging-collision");
        let token = 44;
        let collision = directory.join(format!(
            "settings-{:032x}-{}-{token}.json.tmp",
            super::process_instance_nonce(),
            process::id()
        ));
        fs::write(&collision, b"foreign").unwrap();
        let mut attempts = 0;
        let result = create_staging_with(&directory, || {
            attempts += 1;
            token
        });
        assert!(matches!(
            result,
            Err(crate::settings::SettingsError::Io(ref error))
                if error.kind() == io::ErrorKind::AlreadyExists
        ));
        assert_eq!(attempts, STAGING_CREATE_ATTEMPTS);
        assert_eq!(fs::read(&collision).unwrap(), b"foreign");
        fs::remove_dir_all(directory).unwrap();
    }

    /// 1175、1176、1177、其他错误和未知状态按真实路径布局失败并可安全续存、重启。
    #[test]
    fn publish_failures_revalidate_real_layout_and_allow_safe_retry() {
        for (index, kind, start_from_backup, preserve_staging) in [
            (0, ReplaceFailureKind::UnableToRemoveReplaced, false, false),
            (
                1,
                ReplaceFailureKind::UnableToMoveReplacementWithBackup,
                false,
                false,
            ),
            (
                2,
                ReplaceFailureKind::UnableToMoveReplacementWithoutBackup,
                true,
                false,
            ),
            (3, ReplaceFailureKind::UnableToMoveReplacement2, false, true),
            (4, ReplaceFailureKind::OtherDocumented, false, false),
            (5, ReplaceFailureKind::UnknownPostState, false, true),
        ] {
            let directory = temporary_directory(&format!("publish-failure-{index}"));
            let primary = directory.join("settings.json");
            let backup = directory.join("settings.json.bak");
            let a = br#"{"schema_version":1,"history":{"max_items":1111}}"#;
            let recovery = br#"{"schema_version":1,"history":{"max_items":999}}"#;
            let initial_primary: &[u8] = if start_from_backup { b"{broken" } else { a };
            fs::write(&primary, initial_primary).unwrap();
            fs::write(&backup, recovery).unwrap();
            let mut loaded = load(&directory).expect("加载初始状态失败");
            let before = loaded.snapshot.clone();
            let mut staging_path = None;
            let result = save_with_publisher(
                &directory,
                &mut loaded,
                0,
                settings(),
                None,
                |primary, backup, staging, primary_valid| {
                    staging_path = Some(staging.to_path_buf());
                    match kind {
                        ReplaceFailureKind::UnableToMoveReplacementWithoutBackup => {
                            assert!(!primary_valid);
                            fs::remove_file(primary).unwrap();
                        }
                        ReplaceFailureKind::UnableToMoveReplacement2 => {
                            assert!(primary_valid);
                            fs::copy(primary, backup).unwrap();
                            fs::remove_file(primary).unwrap();
                        }
                        ReplaceFailureKind::UnknownPostState => {
                            fs::remove_file(primary).unwrap();
                        }
                        ReplaceFailureKind::OtherDocumented => {
                            assert!(primary_valid);
                            fs::remove_file(backup).unwrap();
                        }
                        ReplaceFailureKind::UnableToRemoveReplaced
                        | ReplaceFailureKind::UnableToMoveReplacementWithBackup => {
                            assert!(primary_valid);
                        }
                    }
                    Err(PublishFailure {
                        kind,
                        error: io::Error::from_raw_os_error(5),
                    })
                },
            );
            assert!(result.is_err());
            assert_eq!(loaded.snapshot, before);
            assert_eq!(loaded.snapshot.revision(), 0);
            assert_eq!(loaded.primary_valid, primary.exists());
            assert_eq!(
                loaded.backup_valid,
                kind != ReplaceFailureKind::OtherDocumented
            );
            if kind == ReplaceFailureKind::OtherDocumented {
                assert!(!backup.exists());
            }
            let staging_path = staging_path.unwrap();
            assert_eq!(staging_path.exists(), preserve_staging);

            let committed = save(&directory, &mut loaded, 0, settings()).expect("失败后续存失败");
            assert_eq!(committed.revision(), 1);
            if kind == ReplaceFailureKind::OtherDocumented {
                assert_eq!(
                    fs::read(&backup).expect("续存必须把有效旧主文件轮换为备份"),
                    a
                );
            }
            let restarted = load(&directory).expect("失败后重启加载失败");
            assert_eq!(restarted.snapshot.settings(), committed.settings());
            assert!(restarted.backup_valid);
            fs::remove_dir_all(directory).expect("清理发布失败测试根失败");
        }
    }

    /// 创建、写入、flush 和 sync 后故障都必须清理 staging 且不提交快照。
    #[test]
    fn pre_publish_faults_clean_staging_and_keep_snapshot() {
        for (index, fault) in [
            SaveFault::Created,
            SaveFault::Written,
            SaveFault::Flushed,
            SaveFault::Synced,
        ]
        .into_iter()
        .enumerate()
        {
            let directory = temporary_directory(&format!("pre-publish-{index}"));
            let mut loaded = load(&directory).expect("加载默认状态失败");
            let before = loaded.snapshot.clone();
            let result = save_with_publisher(
                &directory,
                &mut loaded,
                0,
                settings(),
                Some(fault),
                |_primary, _backup, _staging, _primary_valid| Ok(()),
            );
            assert!(result.is_err());
            assert_eq!(loaded.snapshot, before);
            assert_eq!(
                fs::read_dir(&directory).unwrap().count(),
                0,
                "发布前故障遗留 staging：{index}"
            );
            fs::remove_dir_all(directory).expect("清理发布前故障测试根失败");
        }
    }
}
