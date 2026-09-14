//! dsh 后端进程的生命周期管理。
//!
//! 关键机制(全部实机验证,见 `docs/recon-dsh.md` 第 5 节):
//!
//! * 启动 `dsh --profile dhdesktop --no-open --port 0 --host 127.0.0.1`;
//! * `--port 0` 让系统挑空闲端口,**实际端口会写进就绪行**,所以不需要自己抢端口;
//! * 就绪信号是 stdout 上恰好一行:
//!   `dsh web: http://127.0.0.1:<port>/?token=<token>`
//!   —— 这一行同时给出端口与鉴权令牌,不需要轮询探测;
//! * 停止用 SIGTERM,宽限期内不退再 SIGKILL。

use std::collections::VecDeque;
use std::process::Stdio;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::Mutex;

use super::locate;
use super::paths;

pub const EVENT_STATUS: &str = "backend://status";
pub const EVENT_LOG: &str = "backend://log";

/// 日志环形缓冲上限。
const MAX_LOG_LINES: usize = 800;
/// 首次启动可能要装 profile 依赖,给足时间;超时视为失败并杀掉。
const READY_TIMEOUT: Duration = Duration::from_secs(300);
/// SIGTERM 后的宽限期。
const TERM_GRACE: Duration = Duration::from_secs(8);
/// SIGKILL 后的等待上限。
const KILL_GRACE: Duration = Duration::from_secs(4);

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// 后端状态。序列化成前端可直接判别的 tagged union(`phase` 字段)。
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "phase", rename_all = "camelCase")]
pub enum BackendStatus {
    /// 未运行。
    Idle,
    /// 正在启动(profile 初始化、依赖安装都算在内)。
    Starting {
        program: String,
        source: String,
    },
    /// 已就绪,`url` 可直接加载进窗口。
    Ready {
        url: String,
        port: u16,
        pid: u32,
        started_at_ms: u64,
    },
    /// 正在停止。
    Stopping,
    /// 启动失败(进程没起来 / 超时)。
    Failed {
        message: String,
    },
    /// 起来过,后来退出了。
    Exited {
        code: Option<i32>,
        message: String,
    },
}

impl BackendStatus {
    pub fn is_ready(&self) -> bool {
        matches!(self, BackendStatus::Ready { .. })
    }

    fn ready_url(&self) -> Option<String> {
        match self {
            BackendStatus::Ready { url, .. } => Some(url.clone()),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LogLine {
    pub seq: u64,
    pub stream: String,
    pub text: String,
    pub at_ms: u64,
}

struct Running {
    pid: u32,
}

struct Inner {
    status: BackendStatus,
    running: Option<Running>,
    /// 已 spawn 但还没就绪的 pid,供超时/停止时清理。
    pending_pid: Option<u32>,
    /// 正在被我们主动停止 —— 用于区分「意外退出」和「按预期退出」。
    stopping: bool,
    logs: VecDeque<LogLine>,
    seq: u64,
}

pub struct BackendManager {
    inner: Mutex<Inner>,
    /// 串行化 start/stop/restart。
    ///
    /// 必要性:`inner` 的「检查状态 → 置为 Starting → spawn」之间要经过 await,
    /// 仅靠 `inner` 的锁会有 TOCTOU 窗口 —— 两个并发调用能同时穿过检查,
    /// 结果起出两个 dsh(实测发生过:两个进程相差 33ms)。
    lifecycle: Mutex<()>,
    /// 主窗口原本的地址(我们自己的前端),用于从 dsh 界面切回来。
    home_url: Mutex<Option<url::Url>>,
    /// 无锁的 pid 快照,给退出时的同步清理用。
    pid_snapshot: AtomicU32,
    app: AppHandle,
}

/// 从就绪行里解析出来的信息。
struct ReadyInfo {
    url: String,
    port: u16,
    token: String,
}

/// 解析 `dsh web: http://127.0.0.1:57710/?token=xxx`。
///
/// 前缀按实测固定为 `dsh web: `;解析失败就当作普通日志行,不影响启动。
fn parse_ready_line(line: &str) -> Option<ReadyInfo> {
    const PREFIX: &str = "dsh web: ";
    let idx = line.find(PREFIX)?;
    let raw = line[idx + PREFIX.len()..].trim();
    let parsed = url::Url::parse(raw).ok()?;
    let port = parsed.port()?;
    let token = parsed
        .query_pairs()
        .find(|(k, _)| k == "token")
        .map(|(_, v)| v.to_string())?;
    Some(ReadyInfo {
        url: raw.to_string(),
        port,
        token,
    })
}

// ---- 进程组管理 ----
//
// `dsh`(尤其是 npx 兑底路径)是「npx → npm exec → node dsh」三层进程树。
// 只杀直接子进程会把真正的 dsh 服务器(孙进程)留下变成孤儿 —— 实测确认过:
// 杀掉 npx 后,`node .../.bin/dsh` 仍活着并继续监听端口。
// 因此 spawn 时把子进程放进**独立进程组**,停止时向整个组发信号。

/// 让子进程自成进程组组长(pgid == pid)。仅 Unix。
fn spawn_in_own_group(cmd: &mut Command) {
    use std::os::unix::process::CommandExt;
    cmd.as_std_mut().process_group(0);
}

/// 向整个进程组发信号(负 pid 表示组)。
fn signal_group(pgid: u32, sig: i32) {
    unsafe { libc::kill(-(pgid as i32), sig) };
}

/// 进程组里是否还有活着的进程。
fn group_alive(pgid: u32) -> bool {
    unsafe { libc::kill(-(pgid as i32), 0) == 0 }
}

async fn wait_pid_gone(pid: u32, timeout: Duration) -> bool {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if !group_alive(pid) {
            return true;
        }
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(150)).await;
    }
}

/// 向进程组发 SIGTERM 并等待;返回是否已全部退出。
async fn terminate(pid: u32, grace: Duration) -> bool {
    signal_group(pid, libc::SIGTERM);
    wait_pid_gone(pid, grace).await
}

// ---- 残留清理 ----
//
// 应用被强杀(崩溃 / SIGKILL / 开发期重启)时,Rust 的退出钩子不会执行,
// 子进程组会变成孤儿并继续占端口。所以把进程组 id 落盘,下次启动前先扫一遍。

fn pidfile_write(pgid: u32) {
    let path = paths::pidfile();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(&path, pgid.to_string());
}

fn pidfile_clear() {
    let _ = std::fs::remove_file(paths::pidfile());
}

/// 取出指定进程组里的 pid 列表(`/usr/bin/pgrep -g`)。
fn group_pids(pgid: u32) -> Vec<u32> {
    let Ok(out) = std::process::Command::new("/usr/bin/pgrep")
        .args(["-g", &pgid.to_string()])
        .output()
    else {
        return Vec::new();
    };
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| l.trim().parse().ok())
        .collect()
}

/// 启动前清理上一次遗留的后端。
///
/// 会先用 `ps` 确认目标进程确实是跑我们这个 profile 的 dsh —— 防止 pgid 被系统
/// 复用后误杀无关进程。返回被清理的进程组描述(仅用于记日志)。
fn cleanup_stale_backend() -> Option<String> {
    let text = std::fs::read_to_string(paths::pidfile()).ok()?;
    let Ok(pgid) = text.trim().parse::<u32>() else {
        pidfile_clear();
        return None;
    };
    if !group_alive(pgid) {
        pidfile_clear();
        return None;
    }

    let pids = group_pids(pgid);
    if pids.is_empty() {
        pidfile_clear();
        return None;
    }
    let list = pids
        .iter()
        .map(|p| p.to_string())
        .collect::<Vec<_>>()
        .join(",");
    let Ok(out) = std::process::Command::new("/bin/ps")
        .args(["-o", "command=", "-p", &list])
        .output()
    else {
        return None;
    };
    let listing = String::from_utf8_lossy(&out.stdout).to_string();
    let ours = listing
        .lines()
        .any(|l| l.contains("--profile") && l.contains(paths::PROFILE_NAME));
    if !ours {
        // pgid 被复用了,不是我们的进程。
        pidfile_clear();
        return None;
    }

    signal_group(pgid, libc::SIGTERM);
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while std::time::Instant::now() < deadline {
        if !group_alive(pgid) {
            break;
        }
        std::thread::sleep(Duration::from_millis(150));
    }
    if group_alive(pgid) {
        signal_group(pgid, libc::SIGKILL);
    }
    pidfile_clear();
    Some(format!(
        "已清理上次遗留的 dsh 后端(进程组 {pgid},{} 个进程)",
        pids.len()
    ))
}

impl BackendManager {
    pub fn new(app: AppHandle) -> Self {
        Self {
            inner: Mutex::new(Inner {
                status: BackendStatus::Idle,
                running: None,
                pending_pid: None,
                stopping: false,
                logs: VecDeque::new(),
                seq: 0,
            }),
            lifecycle: Mutex::new(()),
            home_url: Mutex::new(None),
            pid_snapshot: AtomicU32::new(0),
            app,
        }
    }

    /// 记录主窗口的初始地址(我们自己的前端页面)。
    pub async fn remember_home_url(&self, url: url::Url) {
        *self.home_url.lock().await = Some(url);
    }

    pub async fn status(&self) -> BackendStatus {
        self.inner.lock().await.status.clone()
    }

    pub async fn logs(&self) -> Vec<LogLine> {
        self.inner.lock().await.logs.iter().cloned().collect()
    }

    fn emit_status(&self, inner: &mut Inner, status: BackendStatus) {
        inner.status = status.clone();
        // 终端也打一份:从 Finder 启动时看不到,但 tauri dev / 命令行启动时是唯一的
        // 事后诊断线索(前端日志只在窗口里。)
        println!("[dsh] status -> {status:?}");
        let _ = self.app.emit(EVENT_STATUS, status);
    }

    fn emit_log(&self, inner: &mut Inner, stream: &str, text: String) {
        println!("[dsh:{stream}] {text}");
        inner.seq += 1;
        let line = LogLine {
            seq: inner.seq,
            stream: stream.to_string(),
            text,
            at_ms: now_ms(),
        };
        inner.logs.push_back(line.clone());
        while inner.logs.len() > MAX_LOG_LINES {
            inner.logs.pop_front();
        }
        let _ = self.app.emit(EVENT_LOG, line);
    }

    async fn log(&self, stream: &str, text: String) {
        let mut inner = self.inner.lock().await;
        self.emit_log(&mut inner, stream, text);
    }

    /// 把主窗口导航到指定地址;传 `None` 表示切回我们自己的前端页面。
    ///
    /// `reason` 会写进日志 —— 导航会导致窗口重载,出问题时必须能追到是谁触发的。
    async fn navigate_main(&self, target: Option<String>, reason: &str) {
        let url = match target {
            Some(u) => url::Url::parse(&u).ok(),
            None => self.home_url.lock().await.clone(),
        };
        let Some(url) = url else { return };
        let Some(window) = self.app.get_webview_window("main") else {
            return;
        };
        let shown = url.to_string();
        match window.navigate(url) {
            Ok(()) => {
                self.log("system", format!("主窗口导航到 {shown}({reason})"))
                    .await;
                // 回读实际 URL:`navigate()` 返回 Ok 不代表页面真的过去了,
                // 只有回读才能区分「导航被静默丢弃」和「页面加载失败」。
                let probe = self.app.clone();
                tokio::spawn(async move {
                    tokio::time::sleep(Duration::from_secs(3)).await;
                    if let Some(w) = probe.get_webview_window("main") {
                        match w.url() {
                            Ok(u) => println!("[dsh] 主窗口当前 URL = {u}"),
                            Err(e) => println!("[dsh] 回读主窗口 URL 失败: {e}"),
                        }
                    }
                });
            }
            Err(e) => {
                let mut inner = self.inner.lock().await;
                self.emit_log(&mut inner, "system", format!("导航主窗口失败: {e}"));
            }
        }
    }

    /// 把主窗口切到已就绪的 dsh 界面。(切走后前端上下文会重载。)
    pub async fn open_dsh_ui(&self) {
        let target = self.inner.lock().await.status.ready_url();
        self.navigate_main(target, "显式打开 dsh 界面").await;
    }

    /// 启动后端。已在运行或正在启动时直接返回当前状态。
    pub async fn start(self: &Arc<Self>) -> BackendStatus {
        let _guard = self.lifecycle.lock().await;
        self.start_inner().await
    }

    async fn start_inner(self: &Arc<Self>) -> BackendStatus {
        {
            let inner = self.inner.lock().await;
            match &inner.status {
                s if s.is_ready() => return s.clone(),
                BackendStatus::Starting { .. } | BackendStatus::Stopping => {
                    return inner.status.clone()
                }
                _ => {}
            }
        }

        // 清理上次异常退出遗留的后端(强杀 / 崩溃时退出钩子不会执行)。
        if let Some(desc) = cleanup_stale_backend() {
            let mut inner = self.inner.lock().await;
            self.emit_log(&mut inner, "system", desc);
        }

        // 解析启动方式
        let resolved = match locate::resolve() {
            Some(r) => r,
            None => {
                let message = "未找到可用的 dsh:PATH 与常见安装位置都没有,也没有 npx。\
                               请先安装(例如 npm i -g @deepseek-ai/dsh),或用环境变量 DHDESKTOP_DSH_BIN 指定路径。"
                    .to_string();
                let mut inner = self.inner.lock().await;
                self.emit_log(&mut inner, "system", message.clone());
                self.emit_status(&mut inner, BackendStatus::Failed { message: message.clone() });
                return BackendStatus::Failed { message };
            }
        };

        let profile_ready = paths::profile_manifest().is_file();

        {
            let mut inner = self.inner.lock().await;
            self.emit_status(
                &mut inner,
                BackendStatus::Starting {
                    program: resolved.display.clone(),
                    source: resolved.source_label.clone(),
                },
            );
            self.emit_log(
                &mut inner,
                "system",
                format!("使用 {} ({}) 启动 dsh", resolved.display, resolved.source_label),
            );
            self.emit_log(
                &mut inner,
                "system",
                format!(
                    "DSH_HOME={}{}",
                    paths::dsh_home().display(),
                    if profile_ready {
                        format!(",复用已有 profile {}", paths::PROFILE_NAME)
                    } else {
                        format!(",首次启动:从内置模板 {} 初始化 profile", paths::PROFILE_TEMPLATE)
                    }
                ),
            );
        }

        let mut cmd = Command::new(&resolved.program);
        cmd.args(&resolved.args);
        cmd.arg("--profile").arg(paths::PROFILE_NAME);
        if !profile_ready {
            cmd.arg("--from-default-profile").arg(paths::PROFILE_TEMPLATE);
        }
        cmd.args(["--no-open", "--port", "0", "--host", "127.0.0.1"]);
        cmd.env("DSH_HOME", paths::dsh_home());
        // 补全 PATH,否则 Finder 启动时子进程找不到 node(见 locate::child_path_env)。
        if let Some(path) = locate::child_path_env() {
            cmd.env("PATH", path);
        }
        cmd.stdin(Stdio::null());
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());
        // 自成进程组,使停止时能连孙进程一起收掉(见 spawn_in_own_group 的注释)。
        spawn_in_own_group(&mut cmd);

        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => {
                let message = format!("启动 dsh 失败: {e}");
                let mut inner = self.inner.lock().await;
                self.emit_log(&mut inner, "system", message.clone());
                self.emit_status(&mut inner, BackendStatus::Failed { message: message.clone() });
                return BackendStatus::Failed { message };
            }
        };

        let Some(pid) = child.id() else {
            let message = "启动 dsh 失败:拿不到子进程 pid".to_string();
            let mut inner = self.inner.lock().await;
            self.emit_status(&mut inner, BackendStatus::Failed { message: message.clone() });
            return BackendStatus::Failed { message };
        };

        let stdout = child.stdout.take();
        let stderr = child.stderr.take();

        {
            let mut inner = self.inner.lock().await;
            inner.pending_pid = Some(pid);
            inner.stopping = false;
            self.emit_log(&mut inner, "system", format!("dsh 子进程已启动 (pid {pid})"));
        }
        self.pid_snapshot.store(pid, Ordering::SeqCst);
        pidfile_write(pid);

        // ---- stdout:日志 + 就绪信号 ----
        if let Some(stdout) = stdout {
            let me = Arc::clone(self);
            let program = resolved.display.clone();
            tokio::spawn(async move {
                let mut lines = BufReader::new(stdout).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    let ready = parse_ready_line(&line);
                    let mut inner = me.inner.lock().await;
                    me.emit_log(&mut inner, "stdout", line.clone());
                    if let Some(info) = ready {
                        let _ = info.token; // token 已经包含在 url 里,前端直接用 url
                        let started_at_ms = now_ms();
                        inner.running = Some(Running { pid });
                        inner.pending_pid = None;
                        me.emit_status(
                            &mut inner,
                            BackendStatus::Ready {
                                url: info.url.clone(),
                                port: info.port,
                                pid,
                                started_at_ms,
                            },
                        );
                        drop(inner);
                        me.log("system", format!("dsh 后端就绪:{program}")).await;
                        me.navigate_main(Some(info.url), "后端就绪").await;
                    }
                }
            });
        }

        // ---- stderr:只记日志 ----
        if let Some(stderr) = stderr {
            let me = Arc::clone(self);
            tokio::spawn(async move {
                let mut lines = BufReader::new(stderr).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    let mut inner = me.inner.lock().await;
                    me.emit_log(&mut inner, "stderr", line);
                }
            });
        }

        // ---- 就绪超时 ----
        {
            let me = Arc::clone(self);
            tokio::spawn(async move {
                tokio::time::sleep(READY_TIMEOUT).await;
                let mut inner = me.inner.lock().await;
                if matches!(inner.status, BackendStatus::Starting { .. }) {
                    let message = format!(
                        "dsh 启动超时({}s 内没有就绪),详见日志",
                        READY_TIMEOUT.as_secs()
                    );
                    me.emit_log(&mut inner, "system", message.clone());
                    me.emit_status(&mut inner, BackendStatus::Failed { message });
                    inner.pending_pid = None;
                    drop(inner);
                    let _ = terminate(pid, TERM_GRACE).await;
                    signal_group(pid, libc::SIGKILL);
                    me.pid_snapshot.store(0, Ordering::SeqCst);
                    me.navigate_main(None, "启动超时").await;
                }
            });
        }

        // ---- 退出观察 ----
        {
            let me = Arc::clone(self);
            tokio::spawn(async move {
                let status = child.wait().await;
                let code = status.ok().and_then(|s| s.code());
                me.pid_snapshot.compare_exchange(pid, 0, Ordering::SeqCst, Ordering::SeqCst).ok();

                let mut inner = me.inner.lock().await;
                inner.running = None;
                inner.pending_pid = None;
                if inner.stopping {
                    inner.stopping = false;
                    me.emit_log(&mut inner, "system", "dsh 后端已停止".to_string());
                    me.emit_status(&mut inner, BackendStatus::Idle);
                } else if inner.status.is_ready() {
                    let message = format!("dsh 后端已退出{}", code.map(|c| format!("(退出码 {c})")).unwrap_or_default());
                    me.emit_log(&mut inner, "system", message.clone());
                    me.emit_status(&mut inner, BackendStatus::Exited { code, message });
                } else {
                    let message = format!(
                        "dsh 启动失败{} —— 详见下方日志",
                        code.map(|c| format!("(退出码 {c})")).unwrap_or_default()
                    );
                    me.emit_log(&mut inner, "system", message.clone());
                    me.emit_status(&mut inner, BackendStatus::Failed { message });
                }
                drop(inner);
                me.navigate_main(None, "后端退出").await;
            });
        }

        self.inner.lock().await.status.clone()
    }

    /// 停止后端。SIGTERM → 宽限 → SIGKILL。
    pub async fn stop(&self) -> BackendStatus {
        let _guard = self.lifecycle.lock().await;
        self.stop_inner().await
    }

    async fn stop_inner(&self) -> BackendStatus {
        let pid = {
            let mut inner = self.inner.lock().await;
            let pid = inner
                .running
                .as_ref()
                .map(|r| r.pid)
                .or(inner.pending_pid);
            let Some(pid) = pid else {
                inner.running = None;
                inner.pending_pid = None;
                inner.stopping = false;
                self.emit_status(&mut inner, BackendStatus::Idle);
                return BackendStatus::Idle;
            };
            inner.stopping = true;
            self.emit_status(&mut inner, BackendStatus::Stopping);
            pid
        };

        self.log("system", format!("正在停止 dsh 后端 (pid {pid})")).await;

        if !terminate(pid, TERM_GRACE).await {
            self.log(
                "system",
                format!("pid {pid} 在宽限期内未退出,强制 SIGKILL"),
            )
            .await;
            signal_group(pid, libc::SIGKILL);
            wait_pid_gone(pid, KILL_GRACE).await;
        }

        self.pid_snapshot.store(0, Ordering::SeqCst);
        pidfile_clear();
        {
            let mut inner = self.inner.lock().await;
            inner.running = None;
            inner.pending_pid = None;
            inner.stopping = false;
            self.emit_status(&mut inner, BackendStatus::Idle);
        }
        self.navigate_main(None, "已停止").await;
        BackendStatus::Idle
    }

    pub async fn restart(self: &Arc<Self>) -> BackendStatus {
        let _guard = self.lifecycle.lock().await;
        self.stop_inner().await;
        // 等端口/文件锁释放
        tokio::time::sleep(Duration::from_millis(400)).await;
        self.start_inner().await
    }

    /// 应用退出时的同步清理。用无锁 pid 快照,避免在退出路径上 await。
    pub fn kill_now(&self) {
        let pid = self.pid_snapshot.swap(0, Ordering::SeqCst);
        pidfile_clear();
        if pid == 0 {
            return;
        }
        // 向整个进程组发信号:npx 会再拉起孙进程 dsh,只杀直接子进程会留下孤儿。
        signal_group(pid, libc::SIGTERM);
        for _ in 0..8 {
            if !group_alive(pid) {
                return;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        signal_group(pid, libc::SIGKILL);
    }
}
