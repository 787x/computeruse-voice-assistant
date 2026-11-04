//! 设置页探测只使用点击时的草稿；凭据选择与端点身份共用同一条规则。
use crate::config::{ProviderConfig, Settings};
use crate::llm::ProviderProfile;
use crate::secret::SecretStore;
use crate::AppWindow;

#[derive(Clone, PartialEq, Eq)]
pub struct ProbeDraft {
    pub provider_id: String,
    pub base_url: String,
    pub model: String,
    pub api_key: String,
    pub hint: i32,
    pub thinking: i32,
    pub effort: i32,
}
impl std::fmt::Debug for ProbeDraft {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProbeDraft")
            .field("provider", &self.provider_id)
            .field("endpoint", &self.base_url)
            .field("model", &self.model)
            .finish_non_exhaustive()
    }
}
impl ProbeDraft {
    pub fn from_window(window: &AppWindow) -> Self {
        Self {
            provider_id: super::worker::provider_id_for_index(window.get_provider_index()).into(),
            base_url: window.get_base_url().trim().into(),
            model: window.get_model_name().trim().into(),
            api_key: window.get_api_key_input().trim().into(),
            hint: window.get_provider_hint_index(),
            thinking: window.get_thinking_index(),
            effort: window.get_effort_index(),
        }
    }
    pub fn matches_window(&self, window: &AppWindow) -> bool {
        *self == Self::from_window(window)
    }
    pub fn resolve(
        &self,
        settings: &Settings,
        secret: &SecretStore,
    ) -> Result<(ProviderConfig, String), String> {
        let mut provider = settings.provider(&self.provider_id);
        let key = if !self.api_key.is_empty() {
            self.api_key.clone()
        } else if same_endpoint(&self.base_url, &provider.base_url_or_default()) {
            secret
                .load(&self.provider_id)
                .map_err(|_| "无法读取该提供商的 Key，请重新填写".to_string())?
                .unwrap_or_default()
        } else {
            return Err(
                "地址尚未保存，请填写本次测试的 API Key，或先保存端点配置；不会向新地址发送旧 Key"
                    .into(),
            );
        };
        provider.base_url = self.base_url.clone();
        provider.model = self.model.clone();
        provider.provider_hint = super::settings::provider_hint_for_index(self.hint);
        provider.thinking = super::settings::thinking_for_index(self.thinking);
        provider.reasoning_effort = super::settings::reasoning_effort_for_index(self.effort);
        Ok((provider, key))
    }
    pub fn profile(provider: &ProviderConfig) -> ProviderProfile {
        ProviderProfile {
            provider_id: provider.id.clone(),
            capability: provider.capability(),
            base_url: provider.base_url_or_default(),
            model: provider.model.clone(),
            thinking: provider.thinking,
            reasoning_effort: provider.reasoning_effort,
            model_context_window: None,
        }
    }
    pub fn label(&self) -> String {
        format!("{} · {} · {}", self.provider_id, self.base_url, self.model)
    }
}
pub fn same_endpoint(a: &str, b: &str) -> bool {
    a.trim().trim_end_matches('/') == b.trim().trim_end_matches('/')
}
