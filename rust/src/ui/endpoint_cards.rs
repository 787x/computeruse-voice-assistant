//! PR13：卡片草稿只在 UI 中；保存通过原工作线程串行提交。
use super::{settings, worker::UiCommand};
use crate::config::{
    self, AppPaths, ModelOption, ModelSource, ProtocolKind, ProviderConfig, Settings,
};
use crate::{AppWindow, EndpointCard, EndpointModel};
use slint::{ComponentHandle, Model, ModelRc, VecModel};

fn row(m: &ModelOption, enabled: bool) -> EndpointModel {
    EndpointModel {
        id: m.id.clone().into(),
        label: m.display_name.clone().into(),
        enabled,
        context: m
            .context_window
            .map(|v| v.to_string())
            .unwrap_or_default()
            .into(),
        output: m
            .max_output_tokens
            .map(|v| v.to_string())
            .unwrap_or_default()
            .into(),
        image: m.image_input.unwrap_or(false),
        image_known: m.image_input.is_some(),
        source: match m.source {
            ModelSource::Manual => "manual",
            ModelSource::Discovered => "server",
            ModelSource::Unverified => "unverified",
        }
        .into(),
        expanded: false,
    }
}
pub(super) fn card(p: &ProviderConfig) -> EndpointCard {
    let mut p = p.clone();
    p.remember_manual_model(&p.model.clone());
    let enabled = p.selectable_models();
    for id in &enabled {
        if !p.models.iter().any(|m| &m.id == id) {
            let mut m = ModelOption::manual(id);
            m.source = ModelSource::Unverified;
            p.models.push(m);
        }
    }
    EndpointCard {
        id: p.id.clone().into(),
        name: p.display_name().into(),
        url: p.base_url_or_default().into(),
        saved_url: p.base_url_or_default().into(),
        catalog_origin: p.models_origin_url.clone().into(),
        default_model: p.model.clone().into(),
        kind: match p.protocol_kind() {
            ProtocolKind::Mimo => 0,
            ProtocolKind::DeepSeek => 1,
            ProtocolKind::OpenaiCompatible => 2,
        },
        thinking: settings::thinking_index(p.thinking),
        effort: settings::reasoning_effort_index(p.reasoning_effort),
        rows: ModelRc::new(VecModel::from(
            p.models
                .iter()
                .map(|m| row(m, enabled.contains(&m.id)))
                .collect::<Vec<_>>(),
        )),
        ..Default::default()
    }
}
pub fn fill(w: &AppWindow, s: &Settings) {
    let mut ids = vec!["mimo".to_string(), "deepseek".to_string()];
    ids.retain(|id| !s.removed_provider_ids.contains(id));
    for p in &s.providers {
        if !ids.contains(&p.id) {
            ids.push(p.id.clone());
        }
    }
    w.set_endpoint_cards(ModelRc::new(VecModel::from(
        ids.iter()
            .map(|id| card(&s.provider(id)))
            .collect::<Vec<_>>(),
    )));
}
fn number(s: &str) -> Result<Option<u64>, String> {
    if s.trim().is_empty() {
        return Ok(None);
    }
    s.trim()
        .parse::<u64>()
        .ok()
        .filter(|n| *n > 0)
        .map(Some)
        .ok_or_else(|| "元数据必须为正整数或留空（未知）".into())
}
fn parse_model(m: EndpointModel) -> Result<ModelOption, String> {
    Ok(ModelOption {
        id: m.id.trim().to_string(),
        display_name: m.label.to_string(),
        source: match m.source.as_str() {
            "manual" => ModelSource::Manual,
            "unverified" => ModelSource::Unverified,
            _ => ModelSource::Discovered,
        },
        context_window: number(&m.context)?,
        max_output_tokens: number(&m.output)?,
        image_input: m.image_known.then_some(m.image),
    })
}
fn header(c: &EndpointCard) -> ProviderConfig {
    let kind = match c.kind {
        0 => ProtocolKind::Mimo,
        1 => ProtocolKind::DeepSeek,
        _ => ProtocolKind::OpenaiCompatible,
    };
    let mut p = Settings::default().provider(&c.id);
    p.id = c.id.to_string();
    p.name = c.name.trim().to_string();
    p.base_url = c.url.to_string();
    p.protocol_kind = Some(kind);
    p.model = c.default_model.trim().to_string();
    p.thinking = settings::thinking_for_index(c.thinking);
    p.reasoning_effort = settings::reasoning_effort_for_index(c.effort);
    p.models_origin_url = c.catalog_origin.to_string();
    // 在线查询允许未完成行；连接测试仍使用当前测试模型已知的能力上限。
    p.models = c
        .rows
        .iter()
        .filter(|m| m.id.trim() == p.model)
        .filter_map(|m| parse_model(m).ok())
        .collect();
    p.invalidate_catalog_for_url();
    p
}
pub fn provider(c: &EndpointCard) -> Result<ProviderConfig, String> {
    let mut p = header(c);
    p.models = c
        .rows
        .iter()
        .map(parse_model)
        .collect::<Result<Vec<_>, String>>()?;
    if p.models.len() > ProviderConfig::MAX_MODELS {
        return Err(format!(
            "每个实例最多保存 {} 个模型，请先减少目录条目",
            ProviderConfig::MAX_MODELS
        ));
    }
    let mut ids = std::collections::BTreeSet::new();
    if p.models
        .iter()
        .any(|m| m.id.is_empty() || !ids.insert(m.id.clone()))
    {
        return Err("模型 ID 不能为空或重复".into());
    }
    p.enabled_models = Some(
        c.rows
            .iter()
            .filter(|m| m.enabled)
            .map(|m| m.id.trim().to_string())
            .collect(),
    );
    p.invalidate_catalog_for_url();
    Ok(p)
}
fn validate_default(p: &ProviderConfig) -> Result<(), String> {
    if !p
        .enabled_models
        .as_ref()
        .is_some_and(|v| v.contains(&p.model))
    {
        return Err("请先选择目录中已启用的默认模型，再删除或禁用旧默认模型".into());
    }
    Ok(())
}
/// 只对当前实例的已保存地址复用旧凭据；显式确认绑定到确切的新 URL。
pub(super) fn resolve_key(
    saved: &Settings,
    secret: &crate::secret::SecretStore,
    p: &ProviderConfig,
    typed: &str,
    confirmed_url: &str,
) -> Result<String, String> {
    if !typed.trim().is_empty() {
        return Ok(typed.to_string());
    }
    let stored = secret
        .load(&p.id)
        .map_err(|e| e.to_string())?
        .unwrap_or_default();
    if !stored.is_empty()
        && saved.provider(&p.id).base_url_or_default() != p.base_url_or_default()
        && confirmed_url.trim().trim_end_matches('/') != p.base_url_or_default()
    {
        return Err("地址已改变：请填写新 Key，或确认将本实例已存 Key 用于此地址".into());
    }
    Ok(stored)
}
pub(super) fn key_presence(
    settings: &Settings,
    secret: &crate::secret::SecretStore,
) -> Vec<String> {
    settings
        .provider_ids()
        .into_iter()
        .filter(|id| secret.has(id))
        .collect()
}
pub(super) fn ensure_not_referenced(
    paths: &AppPaths,
    settings: &Settings,
    id: &str,
) -> Result<(), String> {
    if settings.provider_id == id || settings.model_plans.iter().any(|p| p.provider_id == id) {
        return Err("该实例被全局默认或模型方案（含时段方案）引用，请先更换引用".into());
    }
    #[derive(serde::Deserialize, Default)]
    #[serde(rename_all = "camelCase")]
    struct Reference {
        #[serde(default)]
        model_choice: config::SessionModelChoice,
    }
    let entries = match std::fs::read_dir(paths.sessions_dir()) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e.to_string()),
    };
    for entry in entries {
        let path = entry.map_err(|e| e.to_string())?.path();
        if path.extension().is_some_and(|v| v == "json")
            && path.file_name().is_none_or(|v| v != "index.json")
        {
            let reference: Reference =
                serde_json::from_str(&std::fs::read_to_string(&path).map_err(|e| e.to_string())?)
                    .map_err(|e| format!("无法检查会话引用：{e}"))?;
            if reference.model_choice.provider_id == id {
                return Err("该实例被已保存 Session 的模型选择引用，请先更换会话选择".into());
            }
        }
    }
    Ok(())
}
pub(super) fn save_provider(
    paths: &AppPaths,
    secret: &crate::secret::SecretStore,
    current: &Settings,
    provider: &ProviderConfig,
    key: &str,
    confirmed_url: &str,
) -> Result<Settings, String> {
    resolve_key(current, secret, provider, key, confirmed_url)?;
    validate_default(provider)?;
    let mut next = current.clone();
    if provider.id == next.provider_id {
        next.base_url = provider.base_url.clone();
        next.model = provider.model.clone();
        next.thinking = provider.thinking;
        next.reasoning_effort = provider.reasoning_effort;
    }
    if let Some(existing) = next.providers.iter_mut().find(|p| p.id == provider.id) {
        *existing = provider.clone();
    } else {
        next.providers.push(provider.clone());
    }
    let next = next.normalized()?;
    let old_key = if key.trim().is_empty() {
        None
    } else {
        secret.load(&provider.id).map_err(|e| e.to_string())?
    };
    if !key.trim().is_empty() {
        secret.save(&provider.id, key).map_err(|e| e.to_string())?;
    }
    if let Err(error) = config::save_settings(paths, &next) {
        if !key.trim().is_empty() {
            secret
                .save(&provider.id, old_key.as_deref().unwrap_or_default())
                .map_err(|e| format!("配置保存失败：{error}；Key 恢复失败：{e}"))?;
        }
        return Err(error);
    }
    Ok(next)
}

pub(super) fn delete_provider(
    paths: &AppPaths,
    secret: &crate::secret::SecretStore,
    current: &Settings,
    id: &str,
) -> Result<Settings, String> {
    ensure_not_referenced(paths, current, id)?;
    let mut next = current.clone();
    next.providers.retain(|p| p.id != id);
    if !next.removed_provider_ids.iter().any(|v| v == id) {
        next.removed_provider_ids.push(id.to_string());
    }
    config::save_settings(paths, &next)?;
    if let Err(error) = secret.clear(id) {
        config::save_settings(paths, current)
            .map_err(|rollback| format!("Key 删除失败：{error}；配置恢复失败：{rollback}"))?;
        return Err(error.to_string());
    }
    Ok(next)
}

fn invalidate_draft(c: &mut EndpointCard) {
    if c.catalog_origin.trim().trim_end_matches('/') != c.url.trim().trim_end_matches('/') {
        for (i, mut m) in c.rows.iter().enumerate() {
            if m.source != "manual" {
                m.source = "unverified".into();
                m.context = "".into();
                m.output = "".into();
                m.image_known = false;
                c.rows.set_row_data(i, m);
            }
        }
    }
}
fn merge_catalog(c: &mut EndpointCard, models: &[ModelOption]) {
    invalidate_draft(c);
    let mut rows = c.rows.iter().collect::<Vec<_>>();
    for existing in &mut rows {
        if existing.source != "manual" {
            if let Some(m) = models.iter().find(|m| m.id == existing.id.as_str()) {
                let expanded = existing.expanded;
                let enabled = existing.enabled;
                let label = existing.label.clone();
                *existing = row(m, enabled);
                if !label.is_empty() {
                    existing.label = label;
                }
                existing.expanded = expanded;
            } else {
                existing.source = "unverified".into();
                existing.context = "".into();
                existing.output = "".into();
                existing.image_known = false;
            }
        }
    }
    for m in models {
        if !rows.iter().any(|r| r.id == m.id) && rows.len() < ProviderConfig::MAX_MODELS {
            rows.push(row(m, false));
        }
    }
    c.rows = ModelRc::new(VecModel::from(rows));
    c.catalog_origin = c.url.trim().trim_end_matches('/').into();
}

pub fn install(w: &AppWindow, paths: AppPaths, tx: tokio::sync::mpsc::UnboundedSender<UiCommand>) {
    let weak = w.as_weak();
    w.on_add_endpoint(move |kind| {
        let Some(w) = weak.upgrade() else {
            return;
        };
        let mut p = Settings::default().provider(match kind {
            0 => "mimo",
            1 => "deepseek",
            _ => "openai-compatible",
        });
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        static INSTANCE_SEQUENCE: std::sync::atomic::AtomicU64 =
            std::sync::atomic::AtomicU64::new(0);
        let sequence = INSTANCE_SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        p.id = format!("endpoint-{stamp}-{sequence}");
        p.protocol_kind = Some(match kind {
            0 => ProtocolKind::Mimo,
            1 => ProtocolKind::DeepSeek,
            _ => ProtocolKind::OpenaiCompatible,
        });
        p.name = match kind {
            0 => "Xiaomi MiMo",
            1 => "DeepSeek",
            _ => "OpenAI-compatible",
        }
        .into();
        let mut cards = w.get_endpoint_cards().iter().collect::<Vec<_>>();
        cards.push(card(&p));
        w.set_endpoint_cards(ModelRc::new(VecModel::from(cards)));
    });
    let revision = std::cell::Cell::new(0i32);
    let weak = w.as_weak();
    w.on_endpoint_action(move |op, mut c| {
        let Some(w) = weak.upgrade() else {
            return;
        };
        // 展开行和删除确认只改变 UI；不能作废正在进行的连接测试。
        if !op.starts_with("expand:") && op != "cancel-delete" && op != "delete" {
            revision.set(revision.get().wrapping_add(1));
            c.revision = revision.get();
            if c.status == "busy" {
                c.status = "".into();
            }
        }
        let cards = w.get_endpoint_cards();
        let Some(i) = cards.iter().position(|v| v.id == c.id) else {
            return;
        };
        let result = (|| -> Result<(), String> {
            match op.as_str() {
                "save" => {
                    let p = provider(&c)?;
                    validate_default(&p)?;
                    let _ = tx.send(UiCommand::SaveEndpoint(
                        p,
                        settings::ProviderDraft {
                            api_key: c.key.to_string(),
                            ..Default::default()
                        },
                        c.revision,
                        c.credential_url.to_string(),
                    ));
                }
                "draft" => {
                    c.delete_pending = false;
                    invalidate_draft(&mut c);
                }
                "reuse-key" => {
                    c.credential_url = c.url.clone();
                    c.status = "".into();
                    c.error = "".into();
                    c.error = "".into();
                }
                "cancel-delete" => {
                    c.delete_pending = false;
                }
                "clear-key" => {
                    c.key = "".into();
                    c.status = "".into();
                    c.error = "".into();
                    tx.send(UiCommand::ClearApiKey(c.id.to_string()))
                        .map_err(|_| "后台线程已退出".to_string())?;
                }
                "delete" => {
                    if !c.delete_pending {
                        c.delete_pending = true;
                        cards.set_row_data(i, c);
                        return Ok(());
                    }
                    let _ = tx.send(UiCommand::DeleteEndpoint(c.id.to_string()));
                }
                "cancel" => {
                    let s = config::load_settings(&paths).map_err(|e| e.to_string())?;
                    if s.providers.iter().any(|p| c.id == p.id)
                        || c.id == "mimo"
                        || c.id == "deepseek"
                    {
                        c = card(&s.provider(&c.id));
                    } else {
                        let v = cards.iter().filter(|v| v.id != c.id).collect::<Vec<_>>();
                        w.set_endpoint_cards(ModelRc::new(VecModel::from(v)));
                        return Ok(());
                    }
                }
                "restore" => {
                    let p = Settings::default().provider(match c.kind {
                        0 => "mimo",
                        1 => "deepseek",
                        _ => "openai-compatible",
                    });
                    let defaults = card(&p);
                    c.rows = defaults.rows;
                    c.default_model = defaults.default_model;
                    c.status = "".into();
                    c.error = "".into();
                }
                "add-model" => {
                    let mut rows = c.rows.iter().collect::<Vec<_>>();
                    rows.push(row(&ModelOption::manual(""), true));
                    c.rows = ModelRc::new(VecModel::from(rows));
                    c.status = "".into();
                    c.error = "".into();
                }
                "fetch" | "test" => {
                    let p = header(&c);
                    let test = op == "test";
                    if test && p.model.trim().is_empty() {
                        return Err("请填写测试模型".into());
                    }
                    let saved = config::load_settings(&paths).map_err(|e| e.to_string())?;
                    let key = resolve_key(
                        &saved,
                        &crate::secret::SecretStore::new(paths.secret_file()),
                        &p,
                        &c.key,
                        &c.credential_url,
                    )?;
                    if test {
                        c.status = "busy".into();
                        c.error = "".into();
                    }
                    let weak = w.as_weak();
                    let request_revision = c.revision;
                    let before = (
                        c.id.to_string(),
                        c.url.to_string(),
                        c.key.to_string(),
                        c.default_model.to_string(),
                        c.status.to_string(),
                    );
                    let test = op == "test";
                    std::thread::spawn(move || {
                        let runtime =
                            tokio::runtime::Runtime::new().expect("model request runtime");
                        let result = runtime.block_on(async {
                            if test {
                                let mut s = Settings::default();
                                s.providers.push(p.clone());
                                let result = crate::model_catalog::probe_chat(
                                    &s.profile_of(&p.id, &p.model, p.thinking, p.reasoning_effort),
                                    &key,
                                    15000,
                                )
                                .await;
                                (Some(result.ok()), result.message(), Vec::new())
                            } else {
                                let result = crate::model_catalog::fetch_models(
                                    &p,
                                    &key,
                                    &mut crate::model_catalog::ModelCache::new(),
                                    true,
                                )
                                .await;
                                (
                                    None,
                                    result.error.unwrap_or_default(),
                                    if result.ok { result.models } else { Vec::new() },
                                )
                            }
                        });
                        let _ = weak.upgrade_in_event_loop(move |w| {
                            let cards = w.get_endpoint_cards();
                            // 比较完整草稿；任何编辑都使旧请求失效。
                            if let Some(i) = cards.iter().position(|v| {
                                v.revision == request_revision
                                    && v.id == before.0
                                    && v.url == before.1
                                    && v.key == before.2
                                    && v.default_model == before.3
                                    && v.status == before.4
                            }) {
                                let mut c = cards.row_data(i).unwrap();
                                if let Some(ok) = result.0 {
                                    c.status = if ok { "成功" } else { "失败" }.into();
                                } else if result.1.is_empty() {
                                    merge_catalog(&mut c, &result.2);
                                }
                                if !result.1.is_empty() && result.0 != Some(true) {
                                    c.error = result.1.into();
                                }
                                cards.set_row_data(i, c);
                            }
                        });
                    });
                }
                _ => {
                    if let Some((kind, index)) = op
                        .split_once(':')
                        .and_then(|(kind, i)| i.parse::<usize>().ok().map(|i| (kind, i)))
                    {
                        let mut rows = c.rows.iter().collect::<Vec<_>>();
                        if let Some(model) = rows.get_mut(index) {
                            match kind {
                                "expand" => model.expanded = !model.expanded,
                                "toggle" => {
                                    model.enabled = !model.enabled;
                                    c.status = "".into();
                                    c.error = "".into();
                                }
                                "remove" => {
                                    if model.id == c.default_model {
                                        return Err("请先指定另一已启用的默认模型".into());
                                    }
                                    rows.remove(index);
                                    c.status = "".into();
                                    c.error = "".into();
                                }
                                _ => {}
                            }
                        }
                        c.rows = ModelRc::new(VecModel::from(rows));
                    }
                }
            }
            cards.set_row_data(i, c);
            Ok(())
        })();
        if let Err(e) = result {
            if let Some(mut current) = cards.row_data(i) {
                if let Ok(saved) = config::load_settings(&paths) {
                    current.saved_url = saved.provider(&current.id).base_url_or_default().into();
                }
                current.status = "失败".into();
                current.error = e.into();
                current.delete_pending = false;
                cards.set_row_data(i, current);
            }
        }
    });
}
#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};

    fn isolated() -> (AppPaths, crate::secret::SecretStore) {
        let root = std::env::temp_dir().join(format!(
            "cuava-review-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let paths = AppPaths { root };
        let secret = crate::secret::SecretStore::new(paths.secret_file());
        (paths, secret)
    }
    #[test]
    fn changed_url_requires_exact_consent_for_both_requests_and_save() {
        let (paths, secret) = isolated();
        let mut s = Settings::default();
        let mut p = s.provider("mimo");
        p.id = "mimo-second".into();
        p.models = vec![ModelOption::manual(p.model.clone())];
        p.enabled_models = Some(vec![p.model.clone()]);
        s.providers.push(p.clone());
        secret.save(&p.id, "original-key").unwrap();
        assert_eq!(
            resolve_key(&s, &secret, &p, "", "").unwrap(),
            "original-key"
        );
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let old = p.base_url_or_default();
        p.base_url = format!("http://{}/v1", listener.local_addr().unwrap());
        assert!(resolve_key(&s, &secret, &p, "", &old).is_err());
        assert!(save_provider(&paths, &secret, &s, &p, "", &old).is_err());
        assert_eq!(
            listener.accept().unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
        assert_eq!(
            resolve_key(&s, &secret, &p, "new-key", "").unwrap(),
            "new-key"
        );
        let next = save_provider(&paths, &secret, &s, &p, "", &p.base_url).unwrap();
        let reopened = config::load_settings(&paths).unwrap();
        assert_eq!(reopened.provider(&p.id).base_url, p.base_url);
        assert_eq!(
            resolve_key(&next, &secret, &p, "", "").unwrap(),
            "original-key"
        );
        p.base_url.push_str("/another");
        assert!(resolve_key(&next, &secret, &p, "", &next.provider(&p.id).base_url).is_err());
        std::fs::remove_dir_all(paths.root).unwrap();
    }
    #[test]
    fn failed_save_rolls_back_key_and_keeps_disk_configuration() {
        let (paths, secret) = isolated();
        let s = Settings::default();
        let mut p = s.provider("mimo");
        p.models = vec![ModelOption::manual(p.model.clone())];
        p.enabled_models = Some(vec![p.model.clone()]);
        secret.save(&p.id, "old-key").unwrap();
        config::save_settings(&paths, &s).unwrap();
        let original = std::fs::read(paths.settings_file()).unwrap();
        std::fs::create_dir_all(paths.root.join("settings.json.tmp")).unwrap();
        assert!(save_provider(&paths, &secret, &s, &p, "new-key", "").is_err());
        assert_eq!(secret.load(&p.id).unwrap().unwrap(), "old-key");
        assert_eq!(s.provider(&p.id).base_url, p.base_url);
        assert_eq!(std::fs::read(paths.settings_file()).unwrap(), original);
        std::fs::remove_dir_all(paths.root).unwrap();
    }
    #[test]
    fn catalog_merge_preserves_blank_manual_enabled_and_missing_models() {
        let mut p = Settings::default().provider("mimo");
        p.models_origin_url = p.base_url_or_default();
        let mut server = ModelOption::manual("old");
        server.source = ModelSource::Discovered;
        server.context_window = Some(9999);
        server.image_input = Some(true);
        p.models.push(server);
        p.enabled_models = Some(vec!["old".into(), "missing".into()]);
        let mut c = card(&p);
        assert!(c.rows.iter().any(|m| m.id == "missing"));
        let mut rows = c.rows.iter().collect::<Vec<_>>();
        let mut blank = row(&ModelOption::manual(""), true);
        blank.label = "未完成草稿".into();
        rows.push(blank);
        let mut manual = ModelOption::manual("private");
        manual.context_window = Some(1234);
        rows.push(row(&manual, true));
        c.rows = ModelRc::new(VecModel::from(rows));
        c.url = "http://127.0.0.1:1/v1".into();
        invalidate_draft(&mut c);
        let stale = c.rows.iter().find(|m| m.id == "old").unwrap();
        assert_eq!(stale.source, "unverified");
        assert!(stale.context.is_empty());
        assert!(!stale.image_known);
        assert!(provider(&c).is_err()); // 保存仍拒绝空行；获取只读取 header。
        assert_eq!(header(&c).base_url, c.url.as_str());
        merge_catalog(&mut c, &[ModelOption::manual("online")]);
        assert!(c
            .rows
            .iter()
            .any(|m| m.id.is_empty() && m.label == "未完成草稿"));
        assert!(c
            .rows
            .iter()
            .any(|m| m.id == "old" && m.enabled && m.source == "unverified"));
        assert!(c
            .rows
            .iter()
            .any(|m| m.id == "private" && m.context == "1234" && m.enabled));
        assert_eq!(c.catalog_origin, c.url);
        let mut rows = c
            .rows
            .iter()
            .filter(|m| !m.id.is_empty())
            .collect::<Vec<_>>();
        rows.retain(|m| m.id != "missing");
        c.rows = ModelRc::new(VecModel::from(rows));
        let saved = provider(&c).unwrap();
        let rebuilt = card(&saved);
        assert_eq!(rebuilt.catalog_origin, c.url);
        assert!(rebuilt
            .rows
            .iter()
            .any(|m| m.id == "old" && m.source == "unverified"));
    }
    #[test]
    fn key_presence_is_per_instance_and_does_not_change_session_choice() {
        let (paths, secret) = isolated();
        let mut s = Settings::default();
        for id in ["first-mimo", "second-mimo"] {
            let mut p = s.provider("mimo");
            p.id = id.into();
            s.providers.push(p);
        }
        let mut runtime = super::super::sessions::SessionRuntime::open(&paths);
        runtime.ensure_one(1);
        let choice = config::SessionModelChoice {
            provider_id: "second-mimo".into(),
            model: "private".into(),
            ..Default::default()
        };
        runtime.set_model_choice(choice.clone()).unwrap();
        secret.save("first-mimo", "a").unwrap();
        runtime.set_key_presence(key_presence(&s, &secret));
        assert!(runtime.has_key_for("first-mimo"));
        assert!(!runtime.has_key_for("second-mimo"));
        secret.save("second-mimo", "b").unwrap();
        secret.clear("first-mimo").unwrap();
        runtime.set_key_presence(key_presence(&s, &secret));
        assert!(!runtime.has_key_for("first-mimo"));
        assert!(runtime.has_key_for("second-mimo"));
        assert_eq!(runtime.model_choice(), choice);
        std::fs::remove_dir_all(paths.root).unwrap();
    }
    #[test]
    fn deletion_checks_structured_references_not_body_or_plan_names() {
        let (paths, _) = isolated();
        std::fs::create_dir_all(paths.sessions_dir()).unwrap();
        let mut s = Settings::default();
        std::fs::write(
            paths.sessions_dir().join("test.json"),
            r#"{"modelChoice":{},"history":[{"content":"target"}],"timeline":[{"text":"target"}]}"#,
        )
        .unwrap();
        assert!(ensure_not_referenced(&paths, &s, "target").is_ok());
        std::fs::write(
            paths.sessions_dir().join("test.json"),
            r#"{"modelChoice":{"providerId":"target","model":"x"}}"#,
        )
        .unwrap();
        assert!(ensure_not_referenced(&paths, &s, "target")
            .unwrap_err()
            .contains("Session"));
        std::fs::write(
            paths.sessions_dir().join("test.json"),
            r#"{"modelChoice":{}}"#,
        )
        .unwrap();
        let plan: config::ModelPlan = serde_json::from_value(
            serde_json::json!({"id":"p","name":"target","providerId":"mimo","model":"m"}),
        )
        .unwrap();
        s.model_plans.push(plan);
        assert!(ensure_not_referenced(&paths, &s, "target").is_ok());
        s.model_plans[0].provider_id = "target".into();
        assert!(ensure_not_referenced(&paths, &s, "target")
            .unwrap_err()
            .contains("方案"));
        assert!(ensure_not_referenced(&paths, &s, &s.provider_id).is_err());
        std::fs::remove_dir_all(paths.root).unwrap();
    }

    #[test]
    fn legacy_protocol_migrates_once_and_manual_metadata_survives_discovery() {
        let mut s = Settings::default();
        let mut p = s.provider("openai-compatible");
        p.id = "gateway".into();
        p.base_url = "http://localhost:1/v1".into();
        p.provider_hint = config::ProviderHint::Mimo;
        let mut manual = ModelOption::manual("private-model");
        manual.context_window = Some(32000);
        manual.display_name = "我的模型".into();
        p.models = vec![manual.clone()];
        p.enabled_models = Some(vec![manual.id.clone()]);
        s.providers.push(p);
        let migrated = s.normalized().unwrap();
        let json = serde_json::to_string(&migrated).unwrap();
        let mut restored: Settings = serde_json::from_str(&json).unwrap();
        let p = restored
            .providers
            .iter_mut()
            .find(|p| p.id == "gateway")
            .unwrap();
        assert_eq!(p.protocol_kind, Some(ProtocolKind::Mimo));
        p.provider_hint = config::ProviderHint::DeepSeek;
        assert_eq!(p.capability(), crate::llm::Capability::Mimo);
        let mut remote = manual.clone();
        remote.source = ModelSource::Discovered;
        remote.context_window = Some(999999);
        p.remember_models(&[remote]);
        assert_eq!(p.models[0], manual);
        assert_eq!(p.enabled_models.as_ref().unwrap(), &["private-model"]);
    }

    #[test]
    fn card_roundtrip_keeps_five_instance_credentials_and_model_metadata() {
        let root = std::env::temp_dir().join(format!(
            "cuava-pr13-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let paths = AppPaths { root };
        let secret = crate::secret::SecretStore::new(paths.secret_file());
        let mut settings = Settings::default();
        for i in 0..5 {
            let mut p = settings.provider(if i < 2 {
                "mimo"
            } else if i < 4 {
                "deepseek"
            } else {
                "openai-compatible"
            });
            p.protocol_kind = Some(p.protocol_kind());
            p.id = format!("saved-{i}");
            p.base_url = format!("http://127.0.0.1:{}/v1", 21000 + i);
            p.model = "first".into();
            let mut model = ModelOption::manual("first");
            model.display_name = "自定义显示名".into();
            model.context_window = Some(32000);
            model.max_output_tokens = Some(4000);
            model.image_input = Some(false);
            p.models = vec![model, ModelOption::manual("second")];
            p.enabled_models = Some(vec!["first".into(), "second".into()]);
            let mut draft = card(&p);
            draft.name = format!("名称 {i}").into();
            let converted = provider(&draft).unwrap();
            validate_default(&converted).unwrap();
            assert_eq!(converted.models, p.models);
            assert_eq!(
                header(&draft)
                    .model_meta("first")
                    .unwrap()
                    .max_output_tokens,
                Some(4000)
            );
            secret.save(&p.id, &format!("mock-key-{i}")).unwrap();
            settings.providers.push(converted);
        }
        config::save_settings(&paths, &settings).unwrap();
        let reloaded = config::load_settings(&paths).unwrap();
        for i in 0..5 {
            let id = format!("saved-{i}");
            let p = reloaded.provider(&id);
            assert_eq!(secret.load(&id).unwrap().unwrap(), format!("mock-key-{i}"));
            assert_eq!(p.display_name(), format!("名称 {i}"));
            assert_eq!(p.selectable_models(), vec!["first", "second"]);
            let profile = reloaded.profile_of(&id, "first", p.thinking, p.reasoning_effort);
            assert_eq!(profile.max_output_tokens, Some(4000));
            assert_eq!(profile.image_input, Some(false));
            assert_eq!(profile.model_context_window, Some(32000));
            let mut draft = card(&p);
            draft.default_model = "absent".into();
            assert!(validate_default(&provider(&draft).unwrap()).is_err());
        }
        assert!(secret.load("unknown-instance").unwrap().is_none());
        std::fs::remove_dir_all(&paths.root).unwrap();
    }

    #[tokio::test]
    async fn five_instances_use_their_own_http_endpoint_key_model_and_protocol() {
        let mut settings = Settings::default();
        let mut servers = Vec::new();
        let kinds = [
            ProtocolKind::Mimo,
            ProtocolKind::Mimo,
            ProtocolKind::DeepSeek,
            ProtocolKind::DeepSeek,
            ProtocolKind::OpenaiCompatible,
        ];
        for (i, kind) in kinds.into_iter().enumerate() {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let address = listener.local_addr().unwrap();
            let mut p = settings.provider("mimo");
            p.id = format!("instance-{i}");
            p.name = "任意名称".into();
            p.protocol_kind = Some(kind);
            p.base_url = format!("http://{address}/v1");
            p.model = format!("model-{i}");
            p.thinking = config::ThinkingLevel::Enabled;
            p.reasoning_effort = config::ReasoningEffort::High;
            settings.providers.push(p.clone());
            servers.push(std::thread::spawn(move || {
                for request_index in 0..2 {
                    let (mut stream, _) = listener.accept().unwrap();
                    stream.set_read_timeout(Some(std::time::Duration::from_secs(10))).unwrap();
                    let mut bytes = Vec::new(); let mut buf = [0; 4096];
                    loop {
                        let n = stream.read(&mut buf).unwrap(); assert!(n > 0); bytes.extend_from_slice(&buf[..n]);
                        if let Some(end) = bytes.windows(4).position(|v| v == b"\r\n\r\n") {
                            let headers = String::from_utf8_lossy(&bytes[..end]).to_ascii_lowercase();
                            let len: usize = headers.lines().find_map(|l| l.strip_prefix("content-length:").and_then(|v| v.trim().parse().ok())).unwrap_or(0);
                            if bytes.len() < end + 4 + len {continue;}
                            assert!(headers.contains(&format!("authorization: bearer test-key-{i}")));
                            if request_index == 0 { assert!(headers.starts_with("get /v1/models ")); }
                            else {
                                assert!(headers.starts_with("post /v1/chat/completions "));
                                let body: serde_json::Value = serde_json::from_slice(&bytes[end+4..]).unwrap();
                                assert_eq!(body["model"], p.model);
                                assert_eq!(body["max_tokens"], 4);
                                match kind {
                                    ProtocolKind::Mimo => {assert_eq!(body["thinking"]["type"], "enabled"); assert!(body.get("reasoning_effort").is_none());}
                                    ProtocolKind::DeepSeek => {assert_eq!(body["reasoning_effort"], "high"); assert!(body.get("thinking").is_none());}
                                    ProtocolKind::OpenaiCompatible => {assert!(body.get("thinking").is_none()); assert!(body.get("reasoning_effort").is_none());}
                                }
                            }
                            break;
                        }
                    }
                    let response = if request_index == 0 { format!(r#"{{"data":[{{"id":"model-{i}"}}]}}"#) }
                        else {"data: {\"choices\":[{\"delta\":{\"content\":\"ok\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n".to_string()};
                    write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", response.len(), response).unwrap();
                }
            }));
        }
        for i in 0..5 {
            let p = settings.provider(&format!("instance-{i}"));
            let key = format!("test-key-{i}");
            let mut draft = card(&p);
            let mut rows = draft.rows.iter().collect::<Vec<_>>();
            rows.push(row(&ModelOption::manual(""), true));
            draft.rows = ModelRc::new(VecModel::from(rows));
            assert!(provider(&draft).is_err());
            let request = header(&draft);
            let result = crate::model_catalog::fetch_models(
                &request,
                &key,
                &mut crate::model_catalog::ModelCache::new(),
                true,
            )
            .await;
            assert!(result.ok, "{:?}", result.error);
            merge_catalog(&mut draft, &result.models);
            assert!(draft.rows.iter().any(|m| m.id.is_empty()));
            let mut profile = settings.profile_of(&p.id, &p.model, p.thinking, p.reasoning_effort);
            profile.max_output_tokens = Some(8);
            profile.request_output_tokens = Some(4);
            let result = crate::model_catalog::probe_chat(&profile, &key, 10000).await;
            assert!(result.ok(), "{}", result.message());
        }
        for server in servers {
            server.join().unwrap();
        }
    }
}
