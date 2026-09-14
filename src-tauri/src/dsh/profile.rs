//! profile 的插件视图(只读)。
//!
//! 依据见 `docs/recon-dsh.md` 第 6 节:`profiles/<名字>/package.json` 里
//! `dependencies` 是已安装的插件,`dsh.profile.bundles` 是启用顺序;
//! 禁用/覆盖走同目录的 `cordis.patch.yml` 补丁层。

use std::collections::BTreeMap;

use serde::Serialize;

use super::paths;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EnabledEntry {
    pub name: String,
    /// 是否是官方内置 bundle(`@deepseek-ai/*`),用来和用户装的第三方插件区分。
    pub builtin: bool,
    /// 该条目是否在 `dependencies` 里有对应版本(内置 bundle 通常没有)。
    pub installed_version: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PluginSnapshot {
    pub profile: String,
    pub profile_dir: String,
    pub manifest_path: String,
    pub patch_path: String,
    /// profile 是否已初始化(未初始化时首次启动会从模板创建)。
    pub exists: bool,
    /// 启用顺序列表。
    pub enabled: Vec<EnabledEntry>,
    /// 已安装依赖(第三方插件 + 其它包)。
    pub dependencies: BTreeMap<String, String>,
    /// 已安装但**未启用**的依赖(差集),这是「装了但没开」的候选。
    pub installed_not_enabled: Vec<String>,
    /// `cordis.patch.yml` 的原始内容,便于界面上展示/后续编辑。
    pub patch: String,
}

/// 读取一次 profile 现状。文件不存在不算错误,返回 `exists: false`。
pub fn snapshot() -> Result<PluginSnapshot, String> {
    let profile_dir = paths::profile_dir();
    let manifest = paths::profile_manifest();
    let patch_path = paths::profile_patch();
    let exists = manifest.is_file();

    let mut enabled: Vec<EnabledEntry> = Vec::new();
    let mut dependencies: BTreeMap<String, String> = BTreeMap::new();

    if exists {
        let text = std::fs::read_to_string(&manifest)
            .map_err(|e| format!("读取 {} 失败: {e}", manifest.display()))?;
        let value: serde_json::Value = serde_json::from_str(&text)
            .map_err(|e| format!("解析 {} 失败: {e}", manifest.display()))?;

        if let Some(list) = value
            .pointer("/dsh/profile/bundles")
            .and_then(|b| b.as_array())
        {
            for item in list {
                if let Some(name) = item.as_str() {
                    enabled.push(EnabledEntry {
                        name: name.to_string(),
                        builtin: name.starts_with("@deepseek-ai/"),
                        installed_version: None,
                    });
                }
            }
        }

        if let Some(map) = value.get("dependencies").and_then(|d| d.as_object()) {
            for (k, v) in map {
                dependencies.insert(k.clone(), v.as_str().unwrap_or("").to_string());
            }
        }

        for entry in enabled.iter_mut() {
            entry.installed_version = dependencies.get(&entry.name).cloned();
        }
    }

    let enabled_names: std::collections::BTreeSet<&str> =
        enabled.iter().map(|e| e.name.as_str()).collect();
    let installed_not_enabled: Vec<String> = dependencies
        .keys()
        .filter(|k| !enabled_names.contains(k.as_str()))
        .cloned()
        .collect();

    let patch = std::fs::read_to_string(&patch_path).unwrap_or_default();

    Ok(PluginSnapshot {
        profile: paths::PROFILE_NAME.to_string(),
        profile_dir: profile_dir.display().to_string(),
        manifest_path: manifest.display().to_string(),
        patch_path: patch_path.display().to_string(),
        exists,
        enabled,
        dependencies,
        installed_not_enabled,
        patch,
    })
}
