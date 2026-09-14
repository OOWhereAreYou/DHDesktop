//! 定位可用的 `dsh` 可执行文件,以及产出环境诊断报告。
//!
//! 现实约束(见 recon 笔记第 7 节):从 Finder 启动的 macOS GUI 应用只会拿到
//! 极窄的 `PATH`(`/usr/bin:/bin:/usr/sbin:/sbin`),而用户的 `dsh` 很可能装在
//! Homebrew、nvm、pnpm 等目录里。所以这里除了 `PATH` 还会主动探测一批约定位置。

use std::path::{Path, PathBuf};

use serde::Serialize;

use super::paths;

/// dsh 是从哪里找到的。
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum Source {
    /// 用户显式通过 `DHDESKTOP_DSH_BIN` 指定。
    EnvOverride,
    /// 来自 `PATH`。
    Path,
    /// 来自我们主动探测的常见安装位置。
    KnownLocation,
    /// 都没找到,退化到 `npx`(首次需下载约 290MB,体验差,只能算兜底)。
    Npx,
}

impl Source {
    fn label(&self) -> &'static str {
        match self {
            Source::EnvOverride => "环境变量 DHDESKTOP_DSH_BIN",
            Source::Path => "PATH",
            Source::KnownLocation => "常见安装位置",
            Source::Npx => "npx 兜底(首次需下载)",
        }
    }
}

/// 找到的启动方式。`program` + `args` 就是实际要 spawn 的命令前缀。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ResolvedDsh {
    pub program: String,
    pub args: Vec<String>,
    pub source: Source,
    pub source_label: String,
    pub display: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Probe {
    pub dir: String,
    pub has_dsh: bool,
    pub has_node: bool,
}

/// 环境诊断报告,直接给前端诊断页用。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EnvReport {
    pub platform: String,
    pub dsh_home: String,
    pub dsh_home_exists: bool,
    pub dsh_home_writable: bool,
    /// 目录属主 uid。与 `current_uid` 不一致时,极可能是曾被 `sudo` 用过,
    /// 这会让普通用户完全无法启动 dsh(本机就遇到过)。
    pub dsh_home_owner: Option<u32>,
    pub current_uid: Option<u32>,
    pub profile: String,
    pub profile_dir: String,
    pub profile_manifest: String,
    pub profile_ready: bool,
    pub probes: Vec<Probe>,
    pub resolved: Option<ResolvedDsh>,
    pub node: Option<String>,
}

fn path_dirs() -> Vec<PathBuf> {
    match std::env::var_os("PATH") {
        Some(p) => std::env::split_paths(&p)
            .filter(|d| !d.as_os_str().is_empty())
            .collect(),
        None => Vec::new(),
    }
}

/// 主动探测的候选目录(不含 `PATH`)。
fn known_dirs() -> Vec<PathBuf> {
    let home = paths::home_dir();
    let mut dirs = vec![
        PathBuf::from("/opt/homebrew/bin"),
        PathBuf::from("/usr/local/bin"),
        PathBuf::from("/usr/bin"),
        PathBuf::from("/bin"),
        home.join(".local/bin"),
        home.join("Library/pnpm"),
        home.join(".bun/bin"),
        home.join(".volta/bin"),
        home.join(".npm-global/bin"),
    ];

    // nvm:~/.nvm/versions/node/<版本>/bin(按版本号降序,优先新版本)
    if let Ok(entries) = std::fs::read_dir(home.join(".nvm/versions/node")) {
        let mut versions: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
        versions.sort();
        versions.reverse();
        for v in versions {
            dirs.push(v.join("bin"));
        }
    }

    // fnm:~/.local/share/fnm/aliases/<别名>/bin
    if let Ok(entries) = std::fs::read_dir(home.join(".local/share/fnm/aliases")) {
        for e in entries.flatten() {
            dirs.push(e.path().join("bin"));
        }
    }

    // asdf:~/.asdf/shims
    dirs.push(home.join(".asdf/shims"));

    dirs
}

fn find_in(dirs: &[PathBuf], exe: &str) -> Option<PathBuf> {
    dirs.iter().map(|d| d.join(exe)).find(|c| c.is_file())
}

fn resolve_in(dirs: &[PathBuf], source: Source) -> Option<ResolvedDsh> {
    let exe = find_in(dirs, "dsh")?;
    Some(ResolvedDsh {
        program: exe.display().to_string(),
        args: Vec::new(),
        display: exe.display().to_string(),
        source_label: source.label().to_string(),
        source,
    })
}

/// 找出一种可用的启动方式;全部失败则返回 `None`。
pub fn resolve() -> Option<ResolvedDsh> {
    // 1. 用户显式指定优先
    if let Ok(v) = std::env::var("DHDESKTOP_DSH_BIN") {
        if !v.trim().is_empty() {
            let p = PathBuf::from(&v);
            if p.is_file() {
                return Some(ResolvedDsh {
                    program: p.display().to_string(),
                    args: Vec::new(),
                    display: p.display().to_string(),
                    source_label: Source::EnvOverride.label().to_string(),
                    source: Source::EnvOverride,
                });
            }
        }
    }

    // 2. PATH
    if let Some(r) = resolve_in(&path_dirs(), Source::Path) {
        return Some(r);
    }

    // 3. 常见安装位置
    if let Some(r) = resolve_in(&known_dirs(), Source::KnownLocation) {
        return Some(r);
    }

    // 4. npx 兜底
    let all: Vec<PathBuf> = path_dirs().into_iter().chain(known_dirs()).collect();
    if let Some(npx) = find_in(&all, "npx") {
        return Some(ResolvedDsh {
            program: npx.display().to_string(),
            args: vec!["-y".into(), "@deepseek-ai/dsh@latest".into()],
            display: format!("{} -y @deepseek-ai/dsh@latest", npx.display()),
            source_label: Source::Npx.label().to_string(),
            source: Source::Npx,
        });
    }

    None
}

/// 给子进程使用的 `PATH`:保留原有 `PATH`(优先),再补上我们探测到的目录。
///
/// 必要性:`dsh` 与 `npx` 都是 `#!/usr/bin/env node` 脚本。应用从 Finder 启动时
/// `PATH` 只有 `/usr/bin:/bin:/usr/sbin:/sbin`,连 `node` 都找不到,子进程会直接死。
/// 顺带也让 dsh 内部调用的 `git`、`rg` 等更容易被找到。
pub fn child_path_env() -> Option<String> {
    let mut dirs = path_dirs();
    for d in known_dirs() {
        if !dirs.contains(&d) {
            dirs.push(d);
        }
    }
    if dirs.is_empty() {
        return None;
    }
    std::env::join_paths(dirs)
        .ok()
        .map(|s| s.to_string_lossy().into_owned())
}

fn writable(dir: &Path) -> bool {
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

#[cfg(unix)]
fn owner_uid(path: &Path) -> Option<u32> {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata(path).ok().map(|m| m.uid())
}

#[cfg(not(unix))]
fn owner_uid(_path: &Path) -> Option<u32> {
    None
}

#[cfg(unix)]
fn current_uid() -> Option<u32> {
    Some(unsafe { libc::getuid() })
}

#[cfg(not(unix))]
fn current_uid() -> Option<u32> {
    None
}

/// 产出环境诊断报告。
pub fn env_report() -> EnvReport {
    let home = paths::dsh_home();
    let probes: Vec<Probe> = path_dirs()
        .into_iter()
        .chain(known_dirs())
        .map(|d| Probe {
            has_dsh: d.join("dsh").is_file(),
            has_node: d.join("node").is_file(),
            dir: d.display().to_string(),
        })
        .collect();

    let all_dirs: Vec<PathBuf> = path_dirs().into_iter().chain(known_dirs()).collect();

    EnvReport {
        platform: format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH),
        dsh_home_exists: home.exists(),
        dsh_home_writable: writable(&home),
        dsh_home_owner: owner_uid(&home),
        current_uid: current_uid(),
        dsh_home: home.display().to_string(),
        profile: paths::PROFILE_NAME.to_string(),
        profile_dir: paths::profile_dir().display().to_string(),
        profile_manifest: paths::profile_manifest().display().to_string(),
        profile_ready: paths::profile_manifest().is_file(),
        probes,
        resolved: resolve(),
        node: find_in(&all_dirs, "node").map(|p| p.display().to_string()),
    }
}
