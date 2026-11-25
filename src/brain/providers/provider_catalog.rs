//! Trusted built-in provider metadata. This stays separate from models.dev's
//! descriptive model records, which are never executable routing metadata.

use super::{
    AdapterId, DecisionProtocol, ModelProvider, OpenAiCompatibleProvider, ProviderCapabilities,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum AuthFieldRole {
    ApiKey,
    CloudflareAccountId,
    CloudflareGatewayId,
    AnthropicWorkspaceId,
}

impl AuthFieldRole {
    pub(crate) const fn request_header(self) -> Option<&'static str> {
        match self {
            Self::CloudflareGatewayId => Some("cf-aig-gateway-id"),
            Self::AnthropicWorkspaceId => Some("anthropic-workspace-id"),
            Self::ApiKey | Self::CloudflareAccountId => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ProviderAuthField {
    pub(crate) role: AuthFieldRole,
    pub(crate) name: &'static str,
    pub(crate) required: bool,
    pub(crate) secret: bool,
    pub(crate) setup_prompt: &'static str,
    pub(crate) missing_message: Option<&'static str>,
    pub(crate) invalid_utf8_message: Option<&'static str>,
    pub(crate) help_note: Option<&'static str>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ProviderEndpoint {
    LocalOllama,
    Fixed(&'static str),
    GeminiModelPath {
        base: &'static str,
        suffix: &'static str,
    },
    CloudflareAccountPath {
        prefix: &'static str,
        suffix: &'static str,
    },
}

impl ProviderEndpoint {
    pub(crate) fn help_url(self) -> Option<String> {
        match self {
            Self::LocalOllama => None,
            Self::Fixed(url) => Some(url.to_owned()),
            Self::GeminiModelPath { base, suffix } => Some(format!("{base}/{{model}}:{suffix}")),
            Self::CloudflareAccountPath { prefix, suffix } => {
                Some(format!("{prefix}<account-id>{suffix}"))
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ProviderDescriptor {
    pub(crate) provider: ModelProvider,
    pub(crate) selector: &'static str,
    pub(crate) aliases: &'static [&'static str],
    pub(crate) label: &'static str,
    pub(crate) setup_label: &'static str,
    pub(crate) protocol: DecisionProtocol,
    pub(crate) native_decision_streaming: bool,
    pub(crate) adapter_id: AdapterId,
    pub(crate) endpoint: ProviderEndpoint,
    pub(crate) auth_fields: &'static [ProviderAuthField],
    pub(crate) catalog_provider_ids: &'static [&'static str],
    pub(crate) help_note: Option<&'static str>,
    pub(crate) postscript: &'static [&'static str],
}

impl ProviderDescriptor {
    pub(crate) fn auth_field(self, role: AuthFieldRole) -> Option<&'static ProviderAuthField> {
        self.auth_fields.iter().find(|field| field.role == role)
    }
}

const GEMINI_KEY: &[ProviderAuthField] = &[ProviderAuthField {
    role: AuthFieldRole::ApiKey,
    name: "GEMINI_API_KEY",
    required: true,
    secret: true,
    setup_prompt: "Gemini API key: ",
    missing_message: Some("GEMINI_API_KEY must be set and nonempty for --provider gemini"),
    invalid_utf8_message: None,
    help_note: None,
}];
const OPENAI_KEY: &[ProviderAuthField] = &[ProviderAuthField {
    role: AuthFieldRole::ApiKey,
    name: "OPENAI_API_KEY",
    required: true,
    secret: true,
    setup_prompt: "OpenAI API key: ",
    missing_message: Some("OPENAI_API_KEY must be set and nonempty for --provider openai"),
    invalid_utf8_message: None,
    help_note: None,
}];
const XAI_KEY: &[ProviderAuthField] = &[ProviderAuthField {
    role: AuthFieldRole::ApiKey,
    name: "XAI_API_KEY",
    required: true,
    secret: true,
    setup_prompt: "xAI API key: ",
    missing_message: Some("XAI_API_KEY must be set and nonempty for --provider xai"),
    invalid_utf8_message: None,
    help_note: None,
}];
const ANTHROPIC_FIELDS: &[ProviderAuthField] = &[
    ProviderAuthField {
        role: AuthFieldRole::ApiKey,
        name: "ANTHROPIC_API_KEY",
        required: true,
        secret: true,
        setup_prompt: "Anthropic API key: ",
        missing_message: Some(
            "ANTHROPIC_API_KEY must be set and nonempty for --provider anthropic",
        ),
        invalid_utf8_message: None,
        help_note: None,
    },
    ProviderAuthField {
        role: AuthFieldRole::AnthropicWorkspaceId,
        name: "ANTHROPIC_WORKSPACE_ID",
        required: false,
        secret: false,
        setup_prompt: "Anthropic Workspace ID (optional; Enter to skip): ",
        missing_message: None,
        invalid_utf8_message: Some("ANTHROPIC_WORKSPACE_ID must be valid UTF-8"),
        help_note: Some(
            "is sent as anthropic-workspace-id (required by Anthropic for keys not scoped to a workspace)",
        ),
    },
];
const CLOUDFLARE_FIELDS: &[ProviderAuthField] = &[
    ProviderAuthField {
        role: AuthFieldRole::CloudflareAccountId,
        name: "CLOUDFLARE_ACCOUNT_ID",
        required: true,
        secret: false,
        setup_prompt: "Cloudflare Account ID: ",
        missing_message: Some("CLOUDFLARE_ACCOUNT_ID must be set for --provider cloudflare"),
        invalid_utf8_message: None,
        help_note: None,
    },
    ProviderAuthField {
        role: AuthFieldRole::ApiKey,
        name: "CLOUDFLARE_API_TOKEN",
        required: true,
        secret: true,
        setup_prompt: "Cloudflare API token: ",
        missing_message: Some(
            "CLOUDFLARE_API_TOKEN must be set and nonempty for --provider cloudflare",
        ),
        invalid_utf8_message: None,
        help_note: None,
    },
];
const CLOUDFLARE_GATEWAY_FIELDS: &[ProviderAuthField] = &[
    ProviderAuthField {
        role: AuthFieldRole::CloudflareAccountId,
        name: "CLOUDFLARE_ACCOUNT_ID",
        required: true,
        secret: false,
        setup_prompt: "Cloudflare Account ID: ",
        missing_message: Some(
            "CLOUDFLARE_ACCOUNT_ID must be set for --provider cloudflare-ai-gateway",
        ),
        invalid_utf8_message: None,
        help_note: None,
    },
    ProviderAuthField {
        role: AuthFieldRole::ApiKey,
        name: "CLOUDFLARE_API_TOKEN",
        required: true,
        secret: true,
        setup_prompt: "Cloudflare API token: ",
        missing_message: Some(
            "CLOUDFLARE_API_TOKEN must be set and nonempty for --provider cloudflare-ai-gateway",
        ),
        invalid_utf8_message: None,
        help_note: None,
    },
    ProviderAuthField {
        role: AuthFieldRole::CloudflareGatewayId,
        name: "CLOUDFLARE_AI_GATEWAY_ID",
        required: false,
        secret: false,
        setup_prompt: "AI Gateway ID [default]: ",
        missing_message: None,
        invalid_utf8_message: Some("CLOUDFLARE_AI_GATEWAY_ID must be valid UTF-8"),
        help_note: Some("same endpoint with cf-aig-gateway-id (default: default)"),
    },
];
const GROQ_KEY: &[ProviderAuthField] = &[ProviderAuthField {
    role: AuthFieldRole::ApiKey,
    name: "GROQ_API_KEY",
    required: true,
    secret: true,
    setup_prompt: "Groq API key: ",
    missing_message: Some("GROQ_API_KEY must be set and nonempty for --provider groq"),
    invalid_utf8_message: None,
    help_note: None,
}];
const OPENROUTER_KEY: &[ProviderAuthField] = &[ProviderAuthField {
    role: AuthFieldRole::ApiKey,
    name: "OPENROUTER_API_KEY",
    required: true,
    secret: true,
    setup_prompt: "OpenRouter API key: ",
    missing_message: Some("OPENROUTER_API_KEY must be set and nonempty for --provider openrouter"),
    invalid_utf8_message: None,
    help_note: None,
}];
const DEEPSEEK_KEY: &[ProviderAuthField] = &[ProviderAuthField {
    role: AuthFieldRole::ApiKey,
    name: "DEEPSEEK_API_KEY",
    required: true,
    secret: true,
    setup_prompt: "DeepSeek API key: ",
    missing_message: Some("DEEPSEEK_API_KEY must be set and nonempty for --provider deepseek"),
    invalid_utf8_message: None,
    help_note: None,
}];
const ZAI_KEY: &[ProviderAuthField] = &[ProviderAuthField {
    role: AuthFieldRole::ApiKey,
    name: "ZAI_API_KEY",
    required: true,
    secret: true,
    setup_prompt: "Z.AI API key: ",
    missing_message: Some("ZAI_API_KEY must be set and nonempty for --provider zai"),
    invalid_utf8_message: None,
    help_note: None,
}];
const MISTRAL_KEY: &[ProviderAuthField] = &[ProviderAuthField {
    role: AuthFieldRole::ApiKey,
    name: "MISTRAL_API_KEY",
    required: true,
    secret: true,
    setup_prompt: "Mistral API key: ",
    missing_message: Some("MISTRAL_API_KEY must be set and nonempty for --provider mistral"),
    invalid_utf8_message: None,
    help_note: None,
}];

const BUILT_IN_PROVIDERS: &[ProviderDescriptor] = &[
    ProviderDescriptor {
        provider: ModelProvider::Ollama,
        selector: "ollama",
        aliases: &[],
        label: "Ollama",
        setup_label: "Ollama (local)",
        protocol: DecisionProtocol::OllamaChat,
        native_decision_streaming: true,
        adapter_id: AdapterId::OLLAMA,
        endpoint: ProviderEndpoint::LocalOllama,
        auth_fields: &[],
        catalog_provider_ids: &[],
        help_note: None,
        postscript: &[],
    },
    ProviderDescriptor {
        provider: ModelProvider::Gemini,
        selector: "gemini",
        aliases: &[],
        label: "Gemini",
        setup_label: "Google Gemini",
        protocol: DecisionProtocol::GeminiGenerateContent,
        native_decision_streaming: false,
        adapter_id: AdapterId::GEMINI,
        endpoint: ProviderEndpoint::GeminiModelPath {
            base: "https://generativelanguage.googleapis.com/v1beta/models",
            suffix: "generateContent",
        },
        auth_fields: GEMINI_KEY,
        catalog_provider_ids: &["google"],
        help_note: Some(
            "Gemini model name must be one safe path component; no custom URL is accepted.",
        ),
        postscript: &[],
    },
    ProviderDescriptor {
        provider: ModelProvider::OpenAi,
        selector: "openai",
        aliases: &[],
        label: "OpenAI",
        setup_label: "OpenAI",
        protocol: DecisionProtocol::Responses,
        native_decision_streaming: true,
        adapter_id: AdapterId::RESPONSES,
        endpoint: ProviderEndpoint::Fixed("https://api.openai.com/v1/responses"),
        auth_fields: OPENAI_KEY,
        catalog_provider_ids: &["openai"],
        help_note: None,
        postscript: &[],
    },
    ProviderDescriptor {
        provider: ModelProvider::Xai,
        selector: "xai",
        aliases: &[],
        label: "xAI",
        setup_label: "xAI",
        protocol: DecisionProtocol::Responses,
        native_decision_streaming: true,
        adapter_id: AdapterId::RESPONSES,
        endpoint: ProviderEndpoint::Fixed("https://api.x.ai/v1/responses"),
        auth_fields: XAI_KEY,
        catalog_provider_ids: &["xai"],
        help_note: None,
        postscript: &[],
    },
    ProviderDescriptor {
        provider: ModelProvider::Anthropic,
        selector: "anthropic",
        aliases: &[],
        label: "Anthropic",
        setup_label: "Anthropic",
        protocol: DecisionProtocol::AnthropicMessages,
        native_decision_streaming: true,
        adapter_id: AdapterId::ANTHROPIC,
        endpoint: ProviderEndpoint::Fixed("https://api.anthropic.com/v1/messages"),
        auth_fields: ANTHROPIC_FIELDS,
        catalog_provider_ids: &["anthropic"],
        help_note: None,
        postscript: &[],
    },
    ProviderDescriptor {
        provider: ModelProvider::OpenAiCompatible(OpenAiCompatibleProvider::Cloudflare),
        selector: "cloudflare",
        aliases: &[],
        label: "Cloudflare Workers AI",
        setup_label: "Cloudflare Workers AI (direct)",
        protocol: DecisionProtocol::CompatibleChat,
        native_decision_streaming: false,
        adapter_id: AdapterId::OPENAI_COMPATIBLE,
        endpoint: ProviderEndpoint::CloudflareAccountPath {
            prefix: "https://api.cloudflare.com/client/v4/accounts/",
            suffix: "/ai/v1/chat/completions",
        },
        auth_fields: CLOUDFLARE_FIELDS,
        catalog_provider_ids: &["cloudflare-workers-ai"],
        help_note: None,
        postscript: &[],
    },
    ProviderDescriptor {
        provider: ModelProvider::OpenAiCompatible(OpenAiCompatibleProvider::CloudflareAiGateway),
        selector: "cloudflare-ai-gateway",
        aliases: &[],
        label: "Cloudflare AI Gateway",
        setup_label: "Cloudflare AI Gateway",
        protocol: DecisionProtocol::CompatibleChat,
        native_decision_streaming: false,
        adapter_id: AdapterId::OPENAI_COMPATIBLE,
        endpoint: ProviderEndpoint::CloudflareAccountPath {
            prefix: "https://api.cloudflare.com/client/v4/accounts/",
            suffix: "/ai/v1/chat/completions",
        },
        auth_fields: CLOUDFLARE_GATEWAY_FIELDS,
        catalog_provider_ids: &["cloudflare-ai-gateway", "cloudflare-workers-ai"],
        help_note: None,
        postscript: &[],
    },
    ProviderDescriptor {
        provider: ModelProvider::OpenAiCompatible(OpenAiCompatibleProvider::Groq),
        selector: "groq",
        aliases: &[],
        label: "Groq",
        setup_label: "Groq",
        protocol: DecisionProtocol::CompatibleChat,
        native_decision_streaming: false,
        adapter_id: AdapterId::OPENAI_COMPATIBLE,
        endpoint: ProviderEndpoint::Fixed("https://api.groq.com/openai/v1/chat/completions"),
        auth_fields: GROQ_KEY,
        catalog_provider_ids: &["groq"],
        help_note: None,
        postscript: &[],
    },
    ProviderDescriptor {
        provider: ModelProvider::OpenAiCompatible(OpenAiCompatibleProvider::OpenRouter),
        selector: "openrouter",
        aliases: &[],
        label: "OpenRouter",
        setup_label: "OpenRouter",
        protocol: DecisionProtocol::CompatibleChat,
        native_decision_streaming: false,
        adapter_id: AdapterId::OPENAI_COMPATIBLE,
        endpoint: ProviderEndpoint::Fixed("https://openrouter.ai/api/v1/chat/completions"),
        auth_fields: OPENROUTER_KEY,
        catalog_provider_ids: &["openrouter"],
        help_note: None,
        postscript: &[
            "OpenRouter may select an upstream provider for the requested model even when fallback",
            "providers are disabled in the request.",
        ],
    },
    ProviderDescriptor {
        provider: ModelProvider::OpenAiCompatible(OpenAiCompatibleProvider::DeepSeek),
        selector: "deepseek",
        aliases: &[],
        label: "DeepSeek",
        setup_label: "DeepSeek",
        protocol: DecisionProtocol::CompatibleChat,
        native_decision_streaming: false,
        adapter_id: AdapterId::OPENAI_COMPATIBLE,
        endpoint: ProviderEndpoint::Fixed("https://api.deepseek.com/chat/completions"),
        auth_fields: DEEPSEEK_KEY,
        catalog_provider_ids: &["deepseek"],
        help_note: None,
        postscript: &[],
    },
    ProviderDescriptor {
        provider: ModelProvider::OpenAiCompatible(OpenAiCompatibleProvider::Zai),
        selector: "zai",
        aliases: &[],
        label: "Z.AI",
        setup_label: "Z.AI",
        protocol: DecisionProtocol::CompatibleChat,
        native_decision_streaming: false,
        adapter_id: AdapterId::OPENAI_COMPATIBLE,
        endpoint: ProviderEndpoint::Fixed("https://api.z.ai/api/paas/v4/chat/completions"),
        auth_fields: ZAI_KEY,
        catalog_provider_ids: &["zai"],
        help_note: None,
        postscript: &[],
    },
    ProviderDescriptor {
        provider: ModelProvider::OpenAiCompatible(OpenAiCompatibleProvider::Mistral),
        selector: "mistral",
        aliases: &[],
        label: "Mistral",
        setup_label: "Mistral",
        protocol: DecisionProtocol::CompatibleChat,
        native_decision_streaming: false,
        adapter_id: AdapterId::OPENAI_COMPATIBLE,
        endpoint: ProviderEndpoint::Fixed("https://api.mistral.ai/v1/chat/completions"),
        auth_fields: MISTRAL_KEY,
        catalog_provider_ids: &["mistral"],
        help_note: None,
        postscript: &[],
    },
];

pub(crate) fn all() -> &'static [ProviderDescriptor] {
    BUILT_IN_PROVIDERS
}

pub(crate) fn by_selector(selector: &str) -> Option<&'static ProviderDescriptor> {
    BUILT_IN_PROVIDERS.iter().find(|descriptor| {
        descriptor.selector == selector || descriptor.aliases.contains(&selector)
    })
}

pub(crate) fn by_provider(provider: ModelProvider) -> &'static ProviderDescriptor {
    BUILT_IN_PROVIDERS
        .iter()
        .find(|descriptor| descriptor.provider == provider)
        .expect("every built-in provider has a descriptor")
}

pub(crate) fn capabilities(provider: ModelProvider) -> ProviderCapabilities {
    let descriptor = by_provider(provider);
    ProviderCapabilities {
        protocol: descriptor.protocol,
        native_decision_streaming: descriptor.native_decision_streaming,
    }
}

pub(crate) fn catalog_matches(selector: &str, catalog_provider: &str) -> bool {
    by_selector(selector)
        .is_some_and(|descriptor| descriptor.catalog_provider_ids.contains(&catalog_provider))
}

pub(crate) fn is_credential_name(name: &str) -> bool {
    BUILT_IN_PROVIDERS
        .iter()
        .flat_map(|descriptor| descriptor.auth_fields)
        .any(|field| field.name == name)
        || profile_credential_name_is_valid(name)
}

fn profile_credential_name_is_valid(name: &str) -> bool {
    let Some(profile_id) = name
        .strip_prefix("KRYPPER_PROVIDER_")
        .and_then(|name| name.strip_suffix("_API_KEY"))
    else {
        return false;
    };
    if profile_id.len() > 32 || profile_id.is_empty() {
        return false;
    }
    if !profile_id.as_bytes()[0].is_ascii_uppercase() {
        return false;
    }
    let mut expect_alphanumeric = true;
    for byte in profile_id.bytes() {
        if byte == b'_' {
            if expect_alphanumeric {
                return false;
            }
            expect_alphanumeric = true;
        } else if byte.is_ascii_uppercase() || byte.is_ascii_digit() {
            expect_alphanumeric = false;
        } else {
            return false;
        }
    }
    !expect_alphanumeric
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn descriptors_preserve_selectors_and_enumeration_order() {
        let expected = [
            "ollama",
            "gemini",
            "openai",
            "xai",
            "anthropic",
            "cloudflare",
            "cloudflare-ai-gateway",
            "groq",
            "openrouter",
            "deepseek",
            "zai",
            "mistral",
        ];
        assert_eq!(
            all()
                .iter()
                .map(|descriptor| descriptor.selector)
                .collect::<Vec<_>>(),
            expected
        );
        for descriptor in all() {
            assert_eq!(
                by_selector(descriptor.selector).unwrap().provider,
                descriptor.provider
            );
            assert_eq!(
                descriptor.provider.capabilities().protocol,
                descriptor.protocol
            );
        }
        assert!(by_selector("unknown-provider").is_none());
        assert!(by_selector("OpenAI").is_none());
    }

    #[test]
    fn aliases_and_catalog_ids_are_unambiguous_and_do_not_become_selectors() {
        let mut selectors = std::collections::BTreeSet::new();
        let mut catalog_ids = std::collections::BTreeSet::new();
        for descriptor in all() {
            assert!(selectors.insert(descriptor.selector));
            for alias in descriptor.aliases {
                assert!(selectors.insert(alias));
                assert_eq!(by_selector(alias).unwrap().provider, descriptor.provider);
            }
            for catalog_id in descriptor.catalog_provider_ids {
                assert!(catalog_ids.insert((descriptor.selector, *catalog_id)));
            }
        }
        assert!(by_selector("google").is_none());
        assert!(catalog_matches("gemini", "google"));
        assert!(!catalog_matches("gemini", "google-vertex"));
        assert!(!catalog_matches("unknown-provider", "unknown-provider"));
    }

    #[test]
    fn adapter_mapping_is_present_and_protocols_are_coalesced() {
        let openai = by_selector("openai").unwrap();
        let xai = by_selector("xai").unwrap();
        assert_eq!(openai.adapter_id, AdapterId::RESPONSES);
        assert_eq!(xai.adapter_id, AdapterId::RESPONSES);
        for descriptor in all() {
            assert!(!descriptor.adapter_id.0.is_empty());
            for field in descriptor.auth_fields {
                assert!(is_credential_name(field.name));
                assert_eq!(field.required, field.missing_message.is_some());
                assert_eq!(field.secret, field.role == AuthFieldRole::ApiKey);
            }
        }
        assert!(!is_credential_name("UNKNOWN_PROVIDER_API_KEY"));
        assert!(is_credential_name("KRYPPER_PROVIDER_MY_WORKER_API_KEY"));
        assert!(!is_credential_name("KRYPPER_PROVIDER_1WORKER_API_KEY"));
        assert!(!is_credential_name("KRYPPER_PROVIDER_MY__WORKER_API_KEY"));
        assert!(!is_credential_name("KRYPPER_PROVIDER_MY_WORKER_SECRET"));
    }
}
