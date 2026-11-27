use crate::brain::provider_bridge::{
    authorize_dispatch_with_arguments, BridgeError, ExtensionTool,
};
use crate::contracts::extensions::{CapabilityGrant, ExtensionAuthority, ExtensionEffect};
use crate::tools::capability_policy::CapabilityContext;
use crate::tools::FileIdentity;
use serde::ser::{SerializeMap, SerializeSeq};
use serde::{Serialize, Serializer};
use serde_json::Value;
use std::time::{Duration, Instant};
use uuid::Uuid;

const EXTENSION_INTENT_VERSION: u32 = 1;
const EXTENSION_INTENT_DOMAIN: &[u8] = b"Krypper.ExtensionIntent\0";
const EXTENSION_WORKSPACE_DOMAIN: &[u8] = b"Krypper.ExtensionWorkspace\0";
const EXTENSION_APPROVAL_LIFETIME: Duration = Duration::from_secs(30);

/// Immutable details a host prompt can show for one proposed extension effect.
#[derive(Clone, Debug, PartialEq)]
pub struct ExtensionApprovalRequest {
    /// Stable identity of the TaskSession that owns this effect.
    pub session_id: Uuid,
    /// Task whose current turn requested the effect.
    pub task_id: Uuid,
    /// Running turn that requested the effect.
    pub turn_id: Uuid,
    /// Registered namespace and operation.
    pub qualified_name: String,
    /// Host-declared effect class.
    pub effect: ExtensionEffect,
    /// Validated argument schema associated with the selected handler.
    pub schema: Value,
    /// Parsed model arguments that will be passed to the selected handler.
    pub arguments: Value,
    /// SHA-256 digest of the opaque workspace identity, with no raw path.
    pub workspace_identity: [u8; 32],
    /// Managed TaskSession workspace revision at approval time.
    pub workspace_revision: u64,
    /// Versioned digest binding every field that authorizes this exact effect.
    pub intent_digest: [u8; 32],
}

/// Host callback that fetches the current task capability snapshot.
pub type CurrentExtensionContext<'a> = dyn FnMut() -> Result<CapabilityContext, ()> + 'a;

/// Host callback that approves or denies one immutable extension request.
pub type ExtensionApprovalPrompt<'a> =
    dyn FnMut(&ExtensionApprovalRequest) -> Result<bool, ()> + 'a;

/// Host-owned callbacks required to approve registered extension effects.
pub struct ExtensionApprovalGate<'a> {
    current_context: &'a mut CurrentExtensionContext<'a>,
    prompt: &'a mut ExtensionApprovalPrompt<'a>,
}

impl<'a> ExtensionApprovalGate<'a> {
    /// Create a gate backed by the host's current authority and confirmation callbacks.
    pub fn new(
        current_context: &'a mut CurrentExtensionContext<'a>,
        prompt: &'a mut ExtensionApprovalPrompt<'a>,
    ) -> Self {
        Self {
            current_context,
            prompt,
        }
    }
}

/// A host-owned handler that receives validated model arguments.
/// Registration is not a capability grant.
pub struct RegisteredExtensionWithArguments<'a, T> {
    /// Validated declaration and host grant for the operation.
    pub tool: ExtensionTool<'a>,
    /// Trusted host handler; it is never chosen from model output.
    pub execute: fn(&Value) -> T,
}

/// Dispatch an argument-aware registered handler for an active, task-owned turn.
///
/// This compatibility entry point deliberately fails closed: a reusable grant
/// does not approve an individual effect. Use
/// [`dispatch_registered_extension_with_approval_for_turn`] with a host-owned
/// approval gate to invoke a handler.
pub fn dispatch_registered_extension_with_arguments_for_turn<T>(
    _session: &crate::task_lifecycle::TaskSession,
    _task_id: Uuid,
    _turn_id: Uuid,
    _registry: &[RegisteredExtensionWithArguments<'_, T>],
    _context: CapabilityContext,
    _name: &str,
    _arguments: &Value,
) -> Result<T, ExtensionDispatchError> {
    Err(ExtensionDispatchError::ApprovalRequired)
}

/// Dispatch one registered handler after a host approves its exact effect.
///
/// The current-context callback is read before the prompt and again at the
/// effect boundary. The host must advance its existing context generation
/// when revoking a grant. The final identity/revision check, one-shot consume,
/// and synchronous handler call are serialized against managed workspace
/// mutations by TaskSession's existing read lease.
#[allow(clippy::too_many_arguments)]
pub fn dispatch_registered_extension_with_approval_for_turn<T>(
    session: &crate::task_lifecycle::TaskSession,
    task_id: Uuid,
    turn_id: Uuid,
    registry: &[RegisteredExtensionWithArguments<'_, T>],
    context: CapabilityContext,
    approval: &mut ExtensionApprovalGate<'_>,
    name: &str,
    arguments: &Value,
) -> Result<T, ExtensionDispatchError> {
    session
        .validate_running_turn(task_id, turn_id)
        .map_err(ExtensionDispatchError::Turn)?;

    let current_context = (approval.current_context)()
        .map_err(|()| ExtensionDispatchError::AuthorizationUnavailable)?;
    if context.task_id != task_id || current_context.task_id != task_id {
        return Err(ExtensionDispatchError::TaskMismatch);
    }
    if current_context != context {
        return Err(ExtensionDispatchError::StaleAuthorization);
    }

    let selected = registry
        .iter()
        .find(|entry| entry.tool.descriptor.qualified_name() == name)
        .ok_or(ExtensionDispatchError::Bridge(BridgeError::UnknownTool))?;
    let tools = registry
        .iter()
        .map(|entry| ExtensionTool {
            descriptor: entry.tool.descriptor.clone(),
            description: entry.tool.description,
            parameters: entry.tool.parameters.clone(),
            grant: entry.tool.grant,
        })
        .collect::<Vec<_>>();
    authorize_dispatch_with_arguments(&tools, context, name, arguments)
        .map_err(ExtensionDispatchError::Bridge)?;

    let (workspace_identity, workspace_revision) = session
        .extension_workspace_snapshot(task_id, turn_id)
        .map_err(ExtensionDispatchError::Turn)?;
    let intent_digest = ExtensionIntent::new(
        session.id(),
        task_id,
        turn_id,
        context,
        selected.tool.grant,
        &selected.tool,
        arguments,
        workspace_identity,
        workspace_revision,
    )
    .digest()?;
    let request = ExtensionApprovalRequest {
        session_id: session.id(),
        task_id,
        turn_id,
        qualified_name: name.to_owned(),
        effect: selected.tool.descriptor.effect,
        schema: selected.tool.parameters.clone(),
        arguments: arguments.clone(),
        workspace_identity: workspace_identity_digest(workspace_identity)?,
        workspace_revision,
        intent_digest,
    };
    let mut permit = OneShotApprovalPermit::issue(intent_digest, Instant::now())?;
    match (approval.prompt)(&request) {
        Ok(true) => {}
        Ok(false) => return Err(ExtensionDispatchError::ApprovalDenied),
        Err(()) => return Err(ExtensionDispatchError::ApprovalPromptFailed),
    }

    let result = session
        .with_extension_workspace_snapshot(
            task_id,
            turn_id,
            workspace_identity,
            workspace_revision,
            || {
                let fresh_context = (approval.current_context)()
                    .map_err(|()| ExtensionDispatchError::AuthorizationUnavailable)?;
                if fresh_context.task_id != task_id {
                    return Err(ExtensionDispatchError::TaskMismatch);
                }
                authorize_dispatch_with_arguments(&tools, fresh_context, name, arguments)
                    .map_err(ExtensionDispatchError::Bridge)?;

                let final_intent = ExtensionIntent::new(
                    session.id(),
                    task_id,
                    turn_id,
                    fresh_context,
                    selected.tool.grant,
                    &selected.tool,
                    arguments,
                    workspace_identity,
                    workspace_revision,
                );
                let final_digest = final_intent.digest()?;
                permit.consume(final_digest, Instant::now())?;
                Ok((selected.execute)(arguments))
            },
        )
        .map_err(ExtensionDispatchError::Turn)?;
    result.ok_or(ExtensionDispatchError::StaleWorkspace)?
}

/// Distinguishes lifecycle failures from capability, approval and workspace denials.
#[derive(Debug)]
pub enum ExtensionDispatchError {
    /// The active turn or its workspace snapshot could not be validated.
    Turn(crate::task_lifecycle::WorkspaceReadError),
    /// The capability context does not match the requested task.
    TaskMismatch,
    /// Capability, schema or registration validation failed.
    Bridge(BridgeError),
    /// No per-effect host approval was supplied.
    ApprovalRequired,
    /// The host denied the exact effect.
    ApprovalDenied,
    /// The host prompt callback failed; no effect was dispatched.
    ApprovalPromptFailed,
    /// The host could not return the authoritative current context.
    AuthorizationUnavailable,
    /// The grant context changed between the pre-prompt check and approval.
    StaleAuthorization,
    /// The task's managed workspace changed after the prompt.
    StaleWorkspace,
    /// The private one-shot approval expired before the effect.
    ExpiredApproval,
    /// The intent changed or the one-shot approval had already been consumed.
    InvalidApproval,
    /// A versioned intent could not be encoded or hashed.
    InvalidIntent,
}

struct ExtensionIntent<'entry, 'tool> {
    session_id: Uuid,
    task_id: Uuid,
    turn_id: Uuid,
    context: CapabilityContext,
    grant: &'entry CapabilityGrant,
    tool: &'entry ExtensionTool<'tool>,
    arguments: &'entry Value,
    workspace_identity: FileIdentity,
    workspace_revision: u64,
}

impl<'entry, 'tool> ExtensionIntent<'entry, 'tool> {
    #[allow(clippy::too_many_arguments)]
    fn new(
        session_id: Uuid,
        task_id: Uuid,
        turn_id: Uuid,
        context: CapabilityContext,
        grant: &'entry CapabilityGrant,
        tool: &'entry ExtensionTool<'tool>,
        arguments: &'entry Value,
        workspace_identity: FileIdentity,
        workspace_revision: u64,
    ) -> Self {
        Self {
            session_id,
            task_id,
            turn_id,
            context,
            grant,
            tool,
            arguments,
            workspace_identity,
            workspace_revision,
        }
    }

    fn digest(&self) -> Result<[u8; 32], ExtensionDispatchError> {
        let schema = canonical_json_bytes(&self.tool.parameters)?;
        let arguments = canonical_json_bytes(self.arguments)?;
        let descriptor = &self.tool.descriptor;
        let mut encoder = CanonicalEncoder::new(EXTENSION_INTENT_DOMAIN);
        encoder.u32(1, EXTENSION_INTENT_VERSION);
        encoder.bytes(2, self.session_id.as_bytes());
        encoder.bytes(3, self.task_id.as_bytes());
        encoder.bytes(4, self.turn_id.as_bytes());
        encoder.u64(5, self.context.generation);
        encoder.u64(6, self.grant.generation);
        encoder.u32(7, descriptor.version);
        encoder.string(8, descriptor.namespace);
        encoder.string(9, descriptor.name);
        encoder.u8(10, effect_tag(descriptor.effect));
        encoder.u8(11, authority_tag(descriptor.authority));
        encoder.string(12, &self.grant.namespace);
        encoder.string(13, &self.grant.operation);
        encoder.u8(14, effect_tag(self.grant.effect));
        encoder.u8(15, u8::from(self.grant.host_approved));
        encoder.bytes(16, &schema);
        encoder.bytes(17, &arguments);
        encoder.identity(18, self.workspace_identity);
        encoder.u64(19, self.workspace_revision);
        encoder.bytes(20, self.grant.task_id.as_bytes());
        encoder.u8(21, u8::from(self.context.task_authorized));
        let digest = ring::digest::digest(&ring::digest::SHA256, &encoder.0);
        digest
            .as_ref()
            .try_into()
            .map_err(|_| ExtensionDispatchError::InvalidIntent)
    }
}

struct OneShotApprovalPermit {
    intent_digest: [u8; 32],
    expires_at: Instant,
    consumed: bool,
}

impl OneShotApprovalPermit {
    fn issue(intent_digest: [u8; 32], now: Instant) -> Result<Self, ExtensionDispatchError> {
        let expires_at = now
            .checked_add(EXTENSION_APPROVAL_LIFETIME)
            .ok_or(ExtensionDispatchError::InvalidIntent)?;
        Ok(Self {
            intent_digest,
            expires_at,
            consumed: false,
        })
    }

    fn consume(
        &mut self,
        intent_digest: [u8; 32],
        now: Instant,
    ) -> Result<(), ExtensionDispatchError> {
        if now >= self.expires_at {
            return Err(ExtensionDispatchError::ExpiredApproval);
        }
        if self.consumed || self.intent_digest != intent_digest {
            return Err(ExtensionDispatchError::InvalidApproval);
        }
        self.consumed = true;
        Ok(())
    }
}

struct CanonicalEncoder(Vec<u8>);

impl CanonicalEncoder {
    fn new(domain: &[u8]) -> Self {
        Self(domain.to_vec())
    }

    fn field(&mut self, tag: u16, value: &[u8]) {
        self.0.extend_from_slice(&tag.to_be_bytes());
        self.0
            .extend_from_slice(&(value.len() as u64).to_be_bytes());
        self.0.extend_from_slice(value);
    }

    fn bytes(&mut self, tag: u16, value: &[u8]) {
        self.field(tag, value);
    }

    fn string(&mut self, tag: u16, value: &str) {
        self.field(tag, value.as_bytes());
    }

    fn u8(&mut self, tag: u16, value: u8) {
        self.field(tag, &[value]);
    }

    fn u32(&mut self, tag: u16, value: u32) {
        self.field(tag, &value.to_be_bytes());
    }

    fn u64(&mut self, tag: u16, value: u64) {
        self.field(tag, &value.to_be_bytes());
    }

    fn identity(&mut self, tag: u16, identity: FileIdentity) {
        let mut encoded = Vec::with_capacity(16);
        encoded.extend_from_slice(&identity.volume.to_be_bytes());
        encoded.extend_from_slice(&identity.file.to_be_bytes());
        self.field(tag, &encoded);
    }
}

fn workspace_identity_digest(identity: FileIdentity) -> Result<[u8; 32], ExtensionDispatchError> {
    let mut encoder = CanonicalEncoder::new(EXTENSION_WORKSPACE_DOMAIN);
    encoder.identity(1, identity);
    ring::digest::digest(&ring::digest::SHA256, &encoder.0)
        .as_ref()
        .try_into()
        .map_err(|_| ExtensionDispatchError::InvalidIntent)
}

fn effect_tag(effect: ExtensionEffect) -> u8 {
    match effect {
        ExtensionEffect::ReadWorkspace => 1,
        ExtensionEffect::MutateWorkspace => 2,
        ExtensionEffect::ExecuteProcess => 3,
        ExtensionEffect::Network => 4,
        ExtensionEffect::ExternalService => 5,
    }
}

fn authority_tag(authority: ExtensionAuthority) -> u8 {
    match authority {
        ExtensionAuthority::Task => 1,
        ExtensionAuthority::TaskAndHostGrant => 2,
    }
}

fn canonical_json_bytes(value: &Value) -> Result<Vec<u8>, ExtensionDispatchError> {
    serde_json::to_vec(&CanonicalJson(value)).map_err(|_| ExtensionDispatchError::InvalidIntent)
}

struct CanonicalJson<'a>(&'a Value);

impl Serialize for CanonicalJson<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match self.0 {
            Value::Null => serializer.serialize_unit(),
            Value::Bool(value) => serializer.serialize_bool(*value),
            Value::Number(value) => value.serialize(serializer),
            Value::String(value) => serializer.serialize_str(value),
            Value::Array(values) => {
                let mut sequence = serializer.serialize_seq(Some(values.len()))?;
                for value in values {
                    sequence.serialize_element(&CanonicalJson(value))?;
                }
                sequence.end()
            }
            Value::Object(values) => {
                let mut entries = values.iter().collect::<Vec<_>>();
                entries.sort_unstable_by_key(|(left, _)| *left);
                let mut object = serializer.serialize_map(Some(entries.len()))?;
                for (key, value) in entries {
                    object.serialize_entry(key, &CanonicalJson(value))?;
                }
                object.end()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        canonical_json_bytes, ExtensionDispatchError, ExtensionIntent, OneShotApprovalPermit,
    };
    use crate::brain::provider_bridge::ExtensionTool;
    use crate::tools::capability_policy::{CapabilityContext, CapabilityGrant};
    use crate::tools::extension_contract::{
        ExtensionAuthority, ExtensionDescriptor, ExtensionEffect, EXTENSION_CONTRACT_VERSION,
    };
    use serde_json::json;
    use std::time::{Duration, Instant};
    use uuid::Uuid;

    #[test]
    fn canonical_json_sorts_object_keys_and_preserves_numeric_types() {
        let first = canonical_json_bytes(&json!({"b": 2, "a": 1})).unwrap();
        let reordered = canonical_json_bytes(&json!({"a": 1, "b": 2})).unwrap();
        assert_eq!(first, reordered);

        let integer = canonical_json_bytes(&json!({"n": 1})).unwrap();
        let float = canonical_json_bytes(&json!({"n": 1.0})).unwrap();
        assert_ne!(integer, float);
    }

    #[test]
    fn extension_intent_binds_arguments_schema_and_workspace_identity() {
        let task_id = Uuid::from_u128(1);
        let grant = CapabilityGrant {
            task_id,
            namespace: "external".into(),
            operation: "read".into(),
            effect: ExtensionEffect::ReadWorkspace,
            generation: 3,
            host_approved: false,
        };
        let context = CapabilityContext {
            task_id,
            generation: 3,
            task_authorized: true,
        };
        let tool = ExtensionTool {
            descriptor: ExtensionDescriptor {
                version: EXTENSION_CONTRACT_VERSION,
                namespace: "external",
                name: "read",
                effect: ExtensionEffect::ReadWorkspace,
                authority: ExtensionAuthority::Task,
            },
            description: "Read",
            parameters: json!({"type":"object","properties":{"path":{"type":"string"}}}),
            grant: &grant,
        };
        let first = ExtensionIntent::new(
            Uuid::from_u128(2),
            task_id,
            Uuid::from_u128(4),
            context,
            &grant,
            &tool,
            &json!({"b": 2, "a": 1}),
            crate::tools::FileIdentity {
                volume: 10,
                file: 20,
            },
            7,
        )
        .digest()
        .unwrap();
        let reordered = ExtensionIntent::new(
            Uuid::from_u128(2),
            task_id,
            Uuid::from_u128(4),
            context,
            &grant,
            &tool,
            &json!({"a": 1, "b": 2}),
            crate::tools::FileIdentity {
                volume: 10,
                file: 20,
            },
            7,
        )
        .digest()
        .unwrap();
        assert_eq!(first, reordered);

        let different_arguments = ExtensionIntent::new(
            Uuid::from_u128(2),
            task_id,
            Uuid::from_u128(4),
            context,
            &grant,
            &tool,
            &json!({"path": "other.txt"}),
            crate::tools::FileIdentity {
                volume: 10,
                file: 20,
            },
            7,
        )
        .digest()
        .unwrap();
        assert_ne!(first, different_arguments);

        let changed_tool = ExtensionTool {
            descriptor: tool.descriptor.clone(),
            description: tool.description,
            parameters: json!({
                "type":"object",
                "properties":{"path":{"type":"string"},"limit":{"type":"integer"}}
            }),
            grant: &grant,
        };
        let changed_schema = ExtensionIntent::new(
            Uuid::from_u128(2),
            task_id,
            Uuid::from_u128(4),
            context,
            &grant,
            &changed_tool,
            &json!({"b": 2, "a": 1}),
            crate::tools::FileIdentity {
                volume: 10,
                file: 20,
            },
            7,
        )
        .digest()
        .unwrap();
        assert_ne!(first, changed_schema);

        let other_identity = ExtensionIntent::new(
            Uuid::from_u128(2),
            task_id,
            Uuid::from_u128(4),
            context,
            &grant,
            &tool,
            &json!({"b": 2, "a": 1}),
            crate::tools::FileIdentity {
                volume: 10,
                file: 21,
            },
            7,
        )
        .digest()
        .unwrap();
        assert_ne!(first, other_identity);

        let other_revision = ExtensionIntent::new(
            Uuid::from_u128(2),
            task_id,
            Uuid::from_u128(4),
            context,
            &grant,
            &tool,
            &json!({"b": 2, "a": 1}),
            crate::tools::FileIdentity {
                volume: 10,
                file: 20,
            },
            8,
        )
        .digest()
        .unwrap();
        assert_ne!(first, other_revision);
    }

    #[test]
    fn approval_permit_expires_and_cannot_be_replayed_or_rebound() {
        let now = Instant::now();
        let digest = [4; 32];
        let mut expired = OneShotApprovalPermit {
            intent_digest: digest,
            expires_at: now - Duration::from_secs(1),
            consumed: false,
        };
        assert!(matches!(
            expired.consume(digest, now),
            Err(ExtensionDispatchError::ExpiredApproval)
        ));

        let mut permit = OneShotApprovalPermit::issue(digest, now).unwrap();
        assert!(matches!(
            permit.consume([5; 32], now),
            Err(ExtensionDispatchError::InvalidApproval)
        ));
        permit.consume(digest, now).unwrap();
        assert!(matches!(
            permit.consume(digest, now),
            Err(ExtensionDispatchError::InvalidApproval)
        ));
    }
}
