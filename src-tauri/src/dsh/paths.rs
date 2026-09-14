//! dsh 目录归属解析。
//!
//! 依据见 `docs/recon-dsh.md` 第 6 节:`$DSH_HOME` 默认 `~/.dsh`,
//! 可用环境变量覆盖;每个 profile 是 `$DSH_HOME/profiles/<名字>/` 下的一个
//! 独立 npm 项目(有自己的 `package.json` / `node_modules` / 补丁层)。

use std::path::PathBuf;

/// DHDesktop 独占的 profile 名。
///
/// 刻意不使用官方 CLI 的 `web` profile,避免两个客户端互相改变依赖图;
/// 也不使用 `desktop`——官方代码里有 `rejectElectronProfile` 明确禁止 CLI 触碰它。
/// 该 profile 由 `--from-default-profile web` 从内置 web 模板初始化。
pub const PROFILE_NAME: &str = "dhdesktop";

/// 内置模板名(`dsh --from-default-profile <名字>`)。
pub const PROFILE_TEMPLATE: &str = "web";

pub fn home_dir() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/"))
}

/// `$DSH_HOME`,默认 `~/.dsh`。
pub fn dsh_home() -> PathBuf {
    match std::env::var("DSH_HOME") {
        Ok(v) if !v.trim().is_empty() => PathBuf::from(v),
        _ => home_dir().join(".dsh"),
    }
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
