//! 此模块定义剪贴板记录暂停的深模块接口，隐藏门禁计数和配置 RPC 线程。

mod controller;
mod pause;

pub use controller::{
    PauseCommand, PauseCommandSender, PauseControllerError, PauseStatus, PrivacyRuntimeOwner,
    SettingsClientRpcAdapter, SettingsRpcPort,
};
pub use pause::{
    restore_pause, ExcludedAppsError, ExcludedAppsSnapshot, GateMode, GateUpdate, PauseClock,
    PauseTimeError, RecordingGate, RecordingReadPermit, SystemPauseClock,
};
