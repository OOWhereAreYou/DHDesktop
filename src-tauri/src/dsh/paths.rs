//! dsh 目录归属解析。
//!
//! **决策(2026-09-15)**:DHDesktop 使用**完全独立**的数据目录,不读也不写用户
//! 机器上那个共享的 `~/.dsh`。
//!
//! 理由:客户端与用户自己的命令行 dsh 互不干涉 —— 一个被改坏(权限、手动删改)
//! 不应该影响另一个。代价是凭据与会话历史不共享,用户若想复用命令行里的配置,
//! 需要自己拷进来。
//!
//! 目录布局(与 `$DSH_HOME` 同结构,因为 dsh 就是这么解释它的):
//!
//! ```text
//! ~/Library/Application Support/com.dhdesktop.app/
//! └── dsh-home/            ← 就是传给子进程的 DSH_HOME
//!     ├── profiles/dhdesktop/   插件与依赖图
//!     ├── settings.yaml         dsh 设置
//!     ├── storages/             会话历史
//!     └── dhdesktop-backend.pid 后端进程组 id(残留清理用)
//! ```

use std::path::{Path, PathBuf};

/// DHDesktop 独占的 profile 名。
///
/// 刻意不使用官方 CLI 的 `web` profile,避免两个客户端互相改变依赖图;
/// 也不使用 `desktop`——官方代码里有 `rejectElectronProfile` 明确禁止 CLI 触碰它。
/// 该 profile 由 `--from-default-profile web` 从内置 web 模板初始化。
pub const PROFILE_NAME: &str = "dhdesktop";

/// 内置模板名(`dsh --from-default-profile <名字>`)。
pub const PROFILE_TEMPLATE: &str = "web";

/// 应用标识符,与 `tauri.conf.json` 的 `identifier` 保持一致。
pub const APP_IDENTIFIER: &str = "com.dhdesktop.app";

/// 显式覆盖数据目录的环境变量(给测试 / 高级用户用)。
///
/// 刻意**不**沿用 `DSH_HOME`:那个变量是用户命令行 dsh 的开关,
/// 用它会破坏「客户端独立」这个决策。
pub const HOME_OVERRIDE_ENV: &str = "DHDESKTOP_DSH_HOME";

pub fn home_dir() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/"))
}

/// 应用自己的数据目录。
pub fn app_data_dir() -> PathBuf {
    #[cfg(target_os = "macos")]
    {
        home_dir()
            .join("Library/Application Support")
            .join(APP_IDENTIFIER)
    }
    #[cfg(target_os = "windows")]
    {
        std::env::var_os("APPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(|| home_dir().join("AppData/Roaming"))
            .join(APP_IDENTIFIER)
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        home_dir().join(".local/share").join(APP_IDENTIFIER)
    }
}

/// 传给 dsh 子进程的 `DSH_HOME`。
pub fn dsh_home() -> PathBuf {
    if let Ok(v) = std::env::var(HOME_OVERRIDE_ENV) {
        let trimmed = v.trim();
        if !trimmed.is_empty() {
            return match trimmed.strip_prefix("~/") {
                Some(rest) => home_dir().join(rest),
                None => PathBuf::from(trimmed),
            };
        }
    }
    app_data_dir().join("dsh-home")
}

pub fn profiles_dir() -> PathBuf {
    dsh_home().join("profiles")
}

pub fn profile_dir() -> PathBuf {
    profiles_dir().join(PROFILE_NAME)
}

/// profile 的清单文件。插件依赖(`dependencies`)与启用列表
/// (`dsh.profile.bundles`)都在这里。
pub fn profile_manifest() -> PathBuf {
    profile_dir().join("package.json")
}

/// 用户补丁层。禁用插件、覆盖某个插件配置都写在这里,
/// 而不是改 bundle 列表(见 recon 笔记第 6 节)。
pub fn profile_patch() -> PathBuf {
    profile_dir().join("cordis.patch.yml")
}

/// 记录当前后端进程组 id 的文件,用于下次启动时清理异常退出遗留的进程。
pub fn pidfile() -> PathBuf {
    dsh_home().join("dhdesktop-backend.pid")
}

/// 应用自己管理的 dsh 运行时目录(`npm install --prefix` 的落点)。
///
/// 存在的理由:启动速度。实测 `npx @latest` 到就绪要 6.9 秒,而直接用 node 跑
/// 同一份代码只要 2.9 秒 —— 差异全在 npm 的解析/registry 检查上。
/// 装一次之后就不再经过 npx。
pub fn runtime_dir() -> PathBuf {
    app_data_dir().join("runtime")
}

/// dsh 的入口脚本(存在即说明运行时已就绪)。
pub fn runtime_entry() -> PathBuf {
    runtime_dir()
        .join("node_modules/@deepseek-ai/dsh/lib/bin.js")
}

/// 目标目录是否可写(会尝试创建并用临时文件实测)。
pub fn is_writable(dir: &Path) -> bool {
    if std::fs::create_dir_all(dir).is_err() {
        return false;
    }
    let probe = dir.join(".dhdesktop-write-probe");
    match std::fs::write(&probe, b"") {
        Ok(()) => {
            let _ = std::fs::remove_file(&probe);
            true
        }
        Err(_) => false,
    }
}
