//! dsh 集成层:进程托管、目录归属、插件视图。
//!
//! 所有设计依据都在 `docs/recon-dsh.md`(实机侦察笔记)。

pub mod backend;
pub mod locate;
pub mod paths;
pub mod profile;
pub mod proxy;

pub use backend::{BackendManager, BackendStatus, LogLine};
