#![warn(clippy::undocumented_unsafe_blocks)]
#![warn(unsafe_op_in_unsafe_fn)]

#[cfg(test)]
pub(crate) static CURRENT_DIR_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

pub mod agent_loop;
mod brain;
pub mod catalog_data;
pub mod catalog_sources;
pub mod cli;
pub mod core;
pub mod harness;
pub mod memory;
pub mod routing;
pub mod streaming;
pub mod task_lifecycle;
pub(crate) mod tools;

pub use tools::process::{OfflineCargoPolicy, ProcessOutcome, ProcessResource, ProcessResult};

pub use brain::{
    BrainReflectionError, ModelError, ModelProvider, OllamaDecisionSource, WorkspacePlan,
    WorkspacePlanError,
};

/// Host-facing API for provider-authorized extensions and existing Harness dispatch.
///
/// The decision sources re-exported here (`GeminiDecisionSource`,
/// `ResponsesDecisionSource`, `AnthropicDecisionSource` and
/// `OpenAiCompatibleDecisionSource`) take a caller-supplied endpoint through their
/// `with_endpoint_and_extensions` constructors. Endpoints must use `https`;
/// plaintext `http` is accepted only for loopback development servers
/// (`127.0.0.1`, `::1` and `localhost`). Any other origin, userinfo in the
/// authority, or a non-http(s) scheme is rejected with `ModelError::InvalidEndpoint`
/// before the API key is attached, so no request and no credential is sent.
pub mod extensions {
    pub use crate::brain::provider_bridge::{
        anthropic_declarations, authorize_dispatch, authorized_declarations, gemini_declarations,
        responses_declarations, BridgeError, ExtensionTool,
    };
    pub use crate::brain::{
        build_anthropic_request_with_extensions, build_chat_request_with_extensions,
        build_compatible_chat_request_with_extensions, build_gemini_request_with_extensions,
        build_responses_request_with_extensions, AnthropicDecisionSource, GeminiDecisionSource,
        ModelError, ModelProvider, OpenAiCompatibleDecisionSource, OpenAiCompatibleProvider,
        ResponsesDecisionSource, ResponsesProvider,
    };
    pub use crate::harness::execution::{
        dispatch_registered_extension_with_arguments_for_turn,
        run_goal_turn_with_budget_and_extensions, run_goal_turn_with_extensions,
        ExtensionDispatchError, RegisteredExtensionWithArguments, TurnExtensions,
    };
    pub use crate::tools::capability_policy::{
        CapabilityContext, CapabilityDenial, CapabilityGrant,
    };
    pub use crate::tools::extension_contract::{
        ExtensionAuthority, ExtensionContractError, ExtensionDescriptor, ExtensionEffect,
        EXTENSION_CONTRACT_VERSION,
    };
}

/// Opt-in, bounded planning for safe model fallback and provider switching.
pub mod fallback;
