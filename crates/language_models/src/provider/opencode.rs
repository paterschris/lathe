use anyhow::Result;
use collections::BTreeMap;
use credentials_provider::CredentialsProvider;
use futures::{FutureExt, StreamExt, future::BoxFuture};
use gpui::{App, AsyncApp, Context, Entity, SharedString, Task, TaskExt, Window};
use http_client::HttpClient;
use language_model::{
    ApiKeyState, AuthenticateError, EnvVar, IconOrSvg, InlineDescription, LanguageModel,
    LanguageModelCompletionError, LanguageModelCompletionEvent, LanguageModelEffortLevel,
    LanguageModelId, LanguageModelName, LanguageModelProvider, LanguageModelProviderId,
    LanguageModelProviderName, LanguageModelProviderState, LanguageModelRequest,
    LanguageModelToolChoice, ProviderSettingsView, RateLimiter, ReasoningEffort,
    SubPageProviderSettings, env_var,
};
use opencode::{ApiProtocol, OPENCODE_API_URL, OpenCodeSubscription};
pub use settings::OpenCodeApiProtocol;
pub use settings::OpenCodeAvailableModel as AvailableModel;
use settings::{Settings, SettingsStore};
use std::sync::{Arc, LazyLock};
use strum::IntoEnumIterator;
use ui::{ButtonLink, ConfiguredApiCard, List, ListBulletItem, prelude::*};
use ui_input::InputField;
use util::ResultExt;

use crate::provider::anthropic::{AnthropicEventMapper, into_anthropic};
use crate::provider::google::{GoogleEventMapper, into_google};
use crate::provider::open_ai::{
    ChatCompletionMaxTokensParameter, OpenAiEventMapper, OpenAiResponseEventMapper, into_open_ai,
    into_open_ai_response,
};

fn normalize_reasoning_effort(effort: &str) -> Option<ReasoningEffort> {
    match effort.trim().to_ascii_lowercase().as_str() {
        "none" => Some(ReasoningEffort::None),
        "minimal" => Some(ReasoningEffort::Minimal),
        "low" => Some(ReasoningEffort::Low),
        "medium" => Some(ReasoningEffort::Medium),
        "high" => Some(ReasoningEffort::High),
        "xhigh" => Some(ReasoningEffort::XHigh),
        "max" => Some(ReasoningEffort::Max),
        _ => None,
    }
}

fn reasoning_effort_display(effort: ReasoningEffort) -> (&'static str, &'static str) {
    match effort {
        ReasoningEffort::None => ("None", "none"),
        ReasoningEffort::Minimal => ("Minimal", "minimal"),
        ReasoningEffort::Low => ("Low", "low"),
        ReasoningEffort::Medium => ("Medium", "medium"),
        ReasoningEffort::High => ("High", "high"),
        ReasoningEffort::XHigh => ("XHigh", "xhigh"),
        ReasoningEffort::Max => ("Max", "max"),
    }
}

const PROVIDER_ID: LanguageModelProviderId = LanguageModelProviderId::new("opencode");
const PROVIDER_NAME: LanguageModelProviderName = LanguageModelProviderName::new("OpenCode Zen");

const API_KEY_ENV_VAR_NAME: &str = "OPENCODE_API_KEY";
static API_KEY_ENV_VAR: LazyLock<EnvVar> = env_var!(API_KEY_ENV_VAR_NAME);

#[derive(Default, Clone, Debug, PartialEq)]
pub struct OpenCodeSettings {
    pub api_url: String,
    pub available_models: Vec<AvailableModel>,
    pub show_zen_models: bool,
    pub show_go_models: bool,
    pub show_free_models: bool,
}

pub struct OpenCodeLanguageModelProvider {
    http_client: Arc<dyn HttpClient>,
    state: Entity<State>,
}

pub struct State {
    api_key_state: ApiKeyState,
    credentials_provider: Arc<dyn CredentialsProvider>,
}

impl State {
    fn is_authenticated(&self) -> bool {
        self.api_key_state.has_key()
    }

    fn set_api_key(&mut self, api_key: Option<String>, cx: &mut Context<Self>) -> Task<Result<()>> {
        let credentials_provider = self.credentials_provider.clone();
        let api_url = OpenCodeLanguageModelProvider::api_url(cx);
        self.api_key_state.store(
            api_url,
            api_key,
            |this| &mut this.api_key_state,
            credentials_provider,
            cx,
        )
    }

    fn authenticate(&mut self, cx: &mut Context<Self>) -> Task<Result<(), AuthenticateError>> {
        let credentials_provider = self.credentials_provider.clone();
        let api_url = OpenCodeLanguageModelProvider::api_url(cx);
        self.api_key_state.load_if_needed(
            api_url,
            |this| &mut this.api_key_state,
            credentials_provider,
            cx,
        )
    }
}

impl OpenCodeLanguageModelProvider {
    pub fn new(
        http_client: Arc<dyn HttpClient>,
        credentials_provider: Arc<dyn CredentialsProvider>,
        cx: &mut App,
    ) -> Self {
        let state = cx.new(|cx| {
            cx.observe_global::<SettingsStore>(|this: &mut State, cx| {
                let credentials_provider = this.credentials_provider.clone();
                let api_url = Self::api_url(cx);
                this.api_key_state.handle_url_change(
                    api_url,
                    |this| &mut this.api_key_state,
                    credentials_provider,
                    cx,
                );
                cx.notify();
            })
            .detach();
            State {
                api_key_state: ApiKeyState::new(Self::api_url(cx), (*API_KEY_ENV_VAR).clone()),
                credentials_provider,
            }
        });

        Self { http_client, state }
    }

    fn create_language_model(
        &self,
        model: opencode::Model,
        subscription: OpenCodeSubscription,
    ) -> Arc<dyn LanguageModel> {
        Arc::new(OpenCodeLanguageModel {
            id: LanguageModelId::from(model.id().to_string()),
            model,
            subscription,
            state: self.state.clone(),
            http_client: self.http_client.clone(),
            request_limiter: RateLimiter::new(4),
        })
    }

    pub fn settings(cx: &App) -> &OpenCodeSettings {
        &crate::AllLanguageModelSettings::get_global(cx).opencode
    }

    fn subscription_enabled(subscription: OpenCodeSubscription, cx: &App) -> bool {
        let settings = Self::settings(cx);
        match subscription {
            OpenCodeSubscription::Zen => settings.show_zen_models,
            OpenCodeSubscription::Go => settings.show_go_models,
            OpenCodeSubscription::Free => settings.show_free_models,
        }
    }

    fn api_url(cx: &App) -> SharedString {
        let api_url = &Self::settings(cx).api_url;
        if api_url.is_empty() {
            OPENCODE_API_URL.into()
        } else {
            SharedString::new(api_url.as_str())
        }
    }
}

impl LanguageModelProviderState for OpenCodeLanguageModelProvider {
    type ObservableEntity = State;

    fn observable_entity(&self) -> Option<Entity<Self::ObservableEntity>> {
        Some(self.state.clone())
    }
}

impl LanguageModelProvider for OpenCodeLanguageModelProvider {
    fn id(&self) -> LanguageModelProviderId {
        PROVIDER_ID
    }

    fn name(&self) -> LanguageModelProviderName {
        PROVIDER_NAME
    }

    fn icon(&self) -> IconOrSvg {
        IconOrSvg::Icon(IconName::AiOpenCode)
    }

    fn default_model(&self, _cx: &App) -> Option<Arc<dyn LanguageModel>> {
        Some(self.create_language_model(opencode::Model::default(), OpenCodeSubscription::Zen))
    }

    fn default_fast_model(&self, _cx: &App) -> Option<Arc<dyn LanguageModel>> {
        Some(self.create_language_model(opencode::Model::default_fast(), OpenCodeSubscription::Zen))
    }

    fn provided_models(&self, cx: &App) -> Vec<Arc<dyn LanguageModel>> {
        let mut models: BTreeMap<String, (opencode::Model, OpenCodeSubscription)> =
            BTreeMap::default();

        for model in opencode::Model::iter() {
            if !matches!(model, opencode::Model::Custom { .. }) {
                for subscription in model.available_subscriptions().iter().copied() {
                    if !Self::subscription_enabled(subscription, cx) {
                        continue;
                    }
                    let key = format!("{}/{}", subscription.id_prefix(), model.id());
                    models.insert(key, (model.clone(), subscription));
                }
            }
        }

        for model in &Self::settings(cx).available_models {
            let protocol = match model.protocol {
                Some(OpenCodeApiProtocol::Anthropic) => ApiProtocol::Anthropic,
                Some(OpenCodeApiProtocol::OpenAiResponses) => ApiProtocol::OpenAiResponses,
                Some(OpenCodeApiProtocol::OpenAiChat) => ApiProtocol::OpenAiChat,
                Some(OpenCodeApiProtocol::Google) => ApiProtocol::Google,
                None => ApiProtocol::OpenAiChat, // default fallback
            };
            let subscription = match model.subscription {
                Some(settings::OpenCodeModelSubscription::Go) => OpenCodeSubscription::Go,
                Some(settings::OpenCodeModelSubscription::Free) => OpenCodeSubscription::Free,
                Some(settings::OpenCodeModelSubscription::Zen) | None => OpenCodeSubscription::Zen,
            };
            if !Self::subscription_enabled(subscription, cx) {
                continue;
            }
            let custom_model = opencode::Model::Custom {
                name: model.name.clone(),
                display_name: model.display_name.clone(),
                max_tokens: model.max_tokens,
                max_output_tokens: model.max_output_tokens,
                protocol,
                reasoning_effort_levels: model.reasoning_effort_levels.clone(),
                custom_model_api_url: model.custom_model_api_url.clone(),
                interleaved_reasoning: model.interleaved_reasoning,
            };
            let key = format!("{}/{}", subscription.id_prefix(), model.name);
            models.insert(key, (custom_model, subscription));
        }

        models
            .into_values()
            .map(|(model, subscription)| self.create_language_model(model, subscription))
            .collect()
    }

    fn is_authenticated(&self, cx: &App) -> bool {
        self.state.read(cx).is_authenticated()
    }

    fn authenticate(&self, cx: &mut App) -> Task<Result<(), AuthenticateError>> {
        self.state.update(cx, |state, cx| state.authenticate(cx))
    }

    fn settings_view(&self, _cx: &mut App) -> Option<ProviderSettingsView> {
        let state = self.state.clone();
        Some(ProviderSettingsView::SubPage(
            SubPageProviderSettings::new(move |window, cx| {
                cx.new(|cx| ConfigurationView::new(state.clone(), window, cx))
                    .into()
            })
            .description(InlineDescription::Text(
                "To use OpenCode models in Zed, you need an API key.".into(),
            )),
        ))
    }
}

pub struct OpenCodeLanguageModel {
    id: LanguageModelId,
    model: opencode::Model,
    subscription: OpenCodeSubscription,
    state: Entity<State>,
    http_client: Arc<dyn HttpClient>,
    request_limiter: RateLimiter,
}

impl OpenCodeLanguageModel {
    /// Returns the base API URL (e.g., "https://opencode.ai/zen").
    fn base_api_url(&self, cx: &AsyncApp) -> SharedString {
        self.state
            .read_with(cx, |_, cx| OpenCodeLanguageModelProvider::api_url(cx))
    }

    fn api_key(&self, cx: &AsyncApp) -> Option<Arc<str>> {
        self.state.read_with(cx, |state, cx| {
            let api_url = OpenCodeLanguageModelProvider::api_url(cx);
            state.api_key_state.key(&api_url)
        })
    }

    fn stream_anthropic(
        &self,
        request: anthropic::Request,
        cx: &AsyncApp,
    ) -> BoxFuture<
        'static,
        Result<
            futures::stream::BoxStream<
                'static,
                Result<anthropic::Event, anthropic::AnthropicError>,
            >,
            LanguageModelCompletionError,
        >,
    > {
        let http_client = self.http_client.clone();
        // Anthropic crate appends /v1/messages to api_url
        let api_url = self.base_api_url(cx);
        let api_key = self.api_key(cx);

        let future = self.request_limiter.stream(async move {
            let Some(api_key) = api_key else {
                return Err(LanguageModelCompletionError::NoApiKey {
                    provider: PROVIDER_NAME,
                });
            };
            let request = anthropic::stream_completion(
                http_client.as_ref(),
                &api_url,
                &api_key,
                request,
                None,
            );
            let response = request.await?;
            Ok(response)
        });

        async move { Ok(future.await?.boxed()) }.boxed()
    }

    fn stream_openai_chat(
        &self,
        request: open_ai::Request,
        cx: &AsyncApp,
    ) -> BoxFuture<
        'static,
        Result<futures::stream::BoxStream<'static, Result<open_ai::ResponseStreamEvent>>>,
    > {
        let http_client = self.http_client.clone();
        // OpenAI crate appends /chat/completions to api_url, so we pass base + "/v1"
        let base_url = self.base_api_url(cx);
        let api_url: SharedString = format!("{base_url}/v1").into();
        let api_key = self.api_key(cx);
        let provider_name = PROVIDER_NAME.0.to_string();

        let future = self.request_limiter.stream(async move {
            let Some(api_key) = api_key else {
                return Err(LanguageModelCompletionError::NoApiKey {
                    provider: PROVIDER_NAME,
                });
            };
            let request = open_ai::stream_completion(
                http_client.as_ref(),
                &provider_name,
                &api_url,
                &api_key,
                request,
            );
            let response = request.await?;
            Ok(response)
        });

        async move { Ok(future.await?.boxed()) }.boxed()
    }

    fn stream_openai_response(
        &self,
        request: open_ai::responses::Request,
        cx: &AsyncApp,
    ) -> BoxFuture<
        'static,
        Result<futures::stream::BoxStream<'static, Result<open_ai::responses::StreamEvent>>>,
    > {
        let http_client = self.http_client.clone();
        // Responses crate appends /responses to api_url, so we pass base + "/v1"
        let base_url = self.base_api_url(cx);
        let api_url: SharedString = format!("{base_url}/v1").into();
        let api_key = self.api_key(cx);
        let provider_name = PROVIDER_NAME.0.to_string();

        let future = self.request_limiter.stream(async move {
            let Some(api_key) = api_key else {
                return Err(LanguageModelCompletionError::NoApiKey {
                    provider: PROVIDER_NAME,
                });
            };
            let request = open_ai::responses::stream_response(
                http_client.as_ref(),
                &provider_name,
                &api_url,
                &api_key,
                request,
                vec![],
            );
            let response = request.await?;
            Ok(response)
        });

        async move { Ok(future.await?.boxed()) }.boxed()
    }

    fn stream_google_zen(
        &self,
        request: google_ai::GenerateContentRequest,
        cx: &AsyncApp,
    ) -> BoxFuture<
        'static,
        Result<futures::stream::BoxStream<'static, Result<google_ai::GenerateContentResponse>>>,
    > {
        let http_client = self.http_client.clone();
        let api_url = self.base_api_url(cx);
        let api_key = self.api_key(cx);

        let future = self.request_limiter.stream(async move {
            let Some(api_key) = api_key else {
                return Err(LanguageModelCompletionError::NoApiKey {
                    provider: PROVIDER_NAME,
                });
            };
            let request = opencode::stream_generate_content(
                http_client.as_ref(),
                &api_url,
                &api_key,
                request,
            );
            let response = request.await?;
            Ok(response)
        });

        async move { Ok(future.await?.boxed()) }.boxed()
    }
}

impl LanguageModel for OpenCodeLanguageModel {
    fn id(&self) -> LanguageModelId {
        self.id.clone()
    }

    fn name(&self) -> LanguageModelName {
        LanguageModelName::from(self.model.display_name().to_string())
    }

    fn provider_id(&self) -> LanguageModelProviderId {
        PROVIDER_ID
    }

    fn provider_name(&self) -> LanguageModelProviderName {
        PROVIDER_NAME
    }

    fn supports_tools(&self) -> bool {
        self.model.supports_tools()
    }

    fn supports_images(&self) -> bool {
        self.model.supports_images()
    }

    fn supports_thinking(&self) -> bool {
        self.model
            .supported_reasoning_effort_levels()
            .is_some_and(|levels| levels.iter().any(|effort| *effort != ReasoningEffort::None))
    }

    fn supports_disabling_thinking(&self) -> bool {
        self.model
            .supported_reasoning_effort_levels()
            .is_some_and(|levels| levels.contains(&ReasoningEffort::None))
    }

    fn supported_effort_levels(&self) -> Vec<LanguageModelEffortLevel> {
        self.model
            .supported_reasoning_effort_levels()
            .map(|levels| {
                let levels = levels
                    .into_iter()
                    .filter(|effort| *effort != ReasoningEffort::None)
                    .collect::<Vec<_>>();
                if levels.is_empty() {
                    return Vec::new();
                }
                let default_index = levels.len() - 1;
                levels
                    .into_iter()
                    .enumerate()
                    .map(|(i, effort)| {
                        let (name, value) = reasoning_effort_display(effort);
                        LanguageModelEffortLevel {
                            name: name.into(),
                            value: value.into(),
                            is_default: i == default_index,
                        }
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    fn supports_tool_choice(&self, choice: LanguageModelToolChoice) -> bool {
        match choice {
            LanguageModelToolChoice::Auto | LanguageModelToolChoice::Any => true,
            LanguageModelToolChoice::None => {
                // Google models don't support None tool choice
                self.model.protocol(self.subscription) != ApiProtocol::Google
            }
        }
    }

    fn telemetry_id(&self) -> String {
        format!("opencode/{}", self.model.id())
    }

    fn max_token_count(&self) -> u64 {
        self.model.max_token_count(self.subscription)
    }

    fn max_output_tokens(&self) -> Option<u64> {
        self.model.max_output_tokens(self.subscription)
    }

    fn stream_completion(
        &self,
        request: LanguageModelRequest,
        cx: &AsyncApp,
    ) -> BoxFuture<
        'static,
        Result<
            futures::stream::BoxStream<
                'static,
                Result<LanguageModelCompletionEvent, LanguageModelCompletionError>,
            >,
            LanguageModelCompletionError,
        >,
    > {
        match self.model.protocol(self.subscription) {
            ApiProtocol::Anthropic => {
                let mode = if self.supports_thinking() && request.thinking_allowed {
                    anthropic::AnthropicModelMode::AdaptiveThinking
                } else {
                    anthropic::AnthropicModelMode::Default
                };
                let anthropic_request = match into_anthropic(
                    request,
                    self.model.id().to_string(),
                    1.0,
                    self.model
                        .max_output_tokens(self.subscription)
                        .unwrap_or(8192),
                    mode,
                    anthropic::completion::AnthropicPromptCacheMode::Automatic,
                ) {
                    Ok(request) => request,
                    Err(error) => return async move { Err(error.into()) }.boxed(),
                };
                let stream = self.stream_anthropic(anthropic_request, cx);
                async move {
                    let mapper = AnthropicEventMapper::new(PROVIDER_NAME);
                    Ok(mapper.map_stream(stream.await?).boxed())
                }
                .boxed()
            }
            ApiProtocol::OpenAiChat => {
                let reasoning_effort = if request.thinking_allowed {
                    request
                        .thinking_effort
                        .as_deref()
                        .and_then(normalize_reasoning_effort)
                } else {
                    None
                };
                let openai_request = match into_open_ai(
                    request,
                    self.model.id(),
                    true,
                    false,
                    self.model.max_output_tokens(self.subscription),
                    ChatCompletionMaxTokensParameter::MaxCompletionTokens,
                    reasoning_effort,
                    self.model.interleaved_reasoning(),
                ) {
                    Ok(request) => request,
                    Err(error) => return async move { Err(error.into()) }.boxed(),
                };
                let stream = self.stream_openai_chat(openai_request, cx);
                async move {
                    let mapper = OpenAiEventMapper::new();
                    Ok(mapper.map_stream(stream.await?).boxed())
                }
                .boxed()
            }
            ApiProtocol::OpenAiResponses => {
                let supports_none_reasoning_effort = self
                    .model
                    .supported_reasoning_effort_levels()
                    .is_some_and(|levels| levels.contains(&ReasoningEffort::None));
                let response_request = into_open_ai_response(
                    request,
                    self.model.id(),
                    true,
                    false,
                    self.model.max_output_tokens(self.subscription),
                    None,
                    supports_none_reasoning_effort,
                );
                let stream = self.stream_openai_response(response_request, cx);
                async move {
                    let mapper = OpenAiResponseEventMapper::new();
                    Ok(mapper.map_stream(stream.await?).boxed())
                }
                .boxed()
            }
            ApiProtocol::Google => {
                let mode = if self.supports_thinking() && request.thinking_allowed {
                    google_ai::GoogleModelMode::Thinking {
                        budget_tokens: None,
                    }
                } else {
                    google_ai::GoogleModelMode::Default
                };
                let google_request = match into_google(request, self.model.id().to_string(), mode) {
                    Ok(request) => request,
                    Err(error) => return async move { Err(error.into()) }.boxed(),
                };
                let stream = self.stream_google_zen(google_request, cx);
                async move {
                    let mapper = GoogleEventMapper::new();
                    Ok(mapper.map_stream(stream.await?.boxed()).boxed())
                }
                .boxed()
            }
        }
    }
}

struct ConfigurationView {
    api_key_editor: Entity<InputField>,
    state: Entity<State>,
    load_credentials_task: Option<Task<()>>,
}

impl ConfigurationView {
    fn new(state: Entity<State>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let api_key_editor = cx.new(|cx| {
            InputField::new(window, cx, "sk-00000000000000000000000000000000").label("API key")
        });

        cx.observe(&state, |_, _, cx| {
            cx.notify();
        })
        .detach();

        let load_credentials_task = Some(cx.spawn_in(window, {
            let state = state.clone();
            async move |this, cx| {
                if let Some(task) = Some(state.update(cx, |state, cx| state.authenticate(cx))) {
                    let _ = task.await;
                }
                this.update(cx, |this, cx| {
                    this.load_credentials_task = None;
                    cx.notify();
                })
                .log_err();
            }
        }));

        Self {
            api_key_editor,
            state,
            load_credentials_task,
        }
    }

    fn save_api_key(&mut self, _: &menu::Confirm, window: &mut Window, cx: &mut Context<Self>) {
        let api_key = self.api_key_editor.read(cx).text(cx).trim().to_string();
        if api_key.is_empty() {
            return;
        }

        self.api_key_editor
            .update(cx, |editor, cx| editor.set_text("", window, cx));

        let state = self.state.clone();
        cx.spawn_in(window, async move |_, cx| {
            state
                .update(cx, |state, cx| state.set_api_key(Some(api_key), cx))
                .await
        })
        .detach_and_log_err(cx);
    }

    fn reset_api_key(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.api_key_editor
            .update(cx, |editor, cx| editor.set_text("", window, cx));

        let state = self.state.clone();
        cx.spawn_in(window, async move |_, cx| {
            state
                .update(cx, |state, cx| state.set_api_key(None, cx))
                .await
        })
        .detach_and_log_err(cx);
    }

    fn should_render_editor(&self, cx: &mut Context<Self>) -> bool {
        !self.state.read(cx).is_authenticated()
    }
}

impl Render for ConfigurationView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let env_var_set = self.state.read(cx).api_key_state.is_from_env_var();
        let configured_card_label = if env_var_set {
            format!("API key set in {API_KEY_ENV_VAR_NAME} environment variable")
        } else {
            let api_url = OpenCodeLanguageModelProvider::api_url(cx);
            if api_url == OPENCODE_API_URL {
                "API key configured".to_string()
            } else {
                format!("API key configured for {}", api_url)
            }
        };

        let is_editing = self.should_render_editor(cx);

        let api_key_control = if is_editing {
            self.api_key_editor.clone().into_any_element()
        } else {
            ConfiguredApiCard::new("opencode-reset-key", configured_card_label)
                .disabled(env_var_set)
                .when(env_var_set, |this| {
                    this.tooltip_label(format!(
                        "To reset your API key, unset the {API_KEY_ENV_VAR_NAME} environment variable."
                    ))
                })
                .on_click(cx.listener(|this, _, window, cx| this.reset_api_key(window, cx)))
                .into_any_element()
        };

        let api_key_section = v_flex()
            .on_action(cx.listener(Self::save_api_key))
            .child(Label::new(
                "To use OpenCode models in Zed, you need an API key:",
            ).color(Color::Muted))
            .child(
                List::new()
                    .child(
                        ListBulletItem::new("")
                            .child(Label::new("Sign in and get your key at").color(Color::Muted))
                            .child(ButtonLink::new(
                                "OpenCode Console",
                                "https://opencode.ai/auth",
                            )),
                    )
                    .when(is_editing, |this| {
                        this.child(ListBulletItem::new(
                            "Paste your API key below and hit enter to start using OpenCode",
                        ).label_color(Color::Muted))
                    }),
            )
            .child(api_key_control)
            .child(
                Label::new(format!(
                    "You can also set the {API_KEY_ENV_VAR_NAME} environment variable and restart Zed."
                ))
                .size(LabelSize::Small)
                .color(Color::Muted).mt_1p5(),
            )
            .into_any_element();

        if self.load_credentials_task.is_some() {
            Label::new("Loading Credentials…").into_any_element()
        } else {
            v_flex().size_full().child(api_key_section).into_any()
        }
    }
}
