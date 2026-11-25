//! Shared extension contract data and errors used by Brain and Tools.
//!
//! These values describe host-issued capabilities. They do not register tools,
//! grant authority, or execute operations.

use std::collections::HashSet;

use serde_json::Value;
use uuid::Uuid;

/// The only extension contract format currently understood by Krypper.
pub const EXTENSION_CONTRACT_VERSION: u32 = 1;

/// Declared effects; descriptions are never permissions.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ExtensionEffect {
    ReadWorkspace,
    MutateWorkspace,
    ExecuteProcess,
    Network,
    ExternalService,
}

/// The host-controlled authorization prerequisite for an extension.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ExtensionAuthority {
    Task,
    TaskAndHostGrant,
}

/// A stable description of a proposed extension operation.
///
/// Constructing this value never makes the operation callable by a model.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExtensionDescriptor<'a> {
    pub version: u32,
    pub namespace: &'a str,
    pub name: &'a str,
    pub effect: ExtensionEffect,
    pub authority: ExtensionAuthority,
}

impl ExtensionDescriptor<'_> {
    /// Return the namespaced tool name used in provider calls.
    pub fn qualified_name(&self) -> String {
        format!("{}_{}", self.namespace, self.name)
    }
}

/// Rejection reasons for malformed or ambiguous descriptors.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ExtensionContractError {
    UnsupportedVersion,
    InvalidIdentifier,
    DuplicateOperation,
}

/// Validate a collection before any discovery-to-registry bridge.
///
/// The caller must separately validate tool schemas, task grants and host policy.
pub fn validate_descriptors(
    descriptors: &[ExtensionDescriptor<'_>],
) -> Result<(), ExtensionContractError> {
    let mut seen = HashSet::new();
    for descriptor in descriptors {
        if descriptor.version != EXTENSION_CONTRACT_VERSION {
            return Err(ExtensionContractError::UnsupportedVersion);
        }
        if !valid_identifier(descriptor.namespace) || !valid_identifier(descriptor.name) {
            return Err(ExtensionContractError::InvalidIdentifier);
        }
        if !seen.insert((descriptor.namespace, descriptor.name)) {
            return Err(ExtensionContractError::DuplicateOperation);
        }
    }
    Ok(())
}

fn valid_identifier(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
}

/// A host-issued authorization for one task and one extension operation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CapabilityGrant {
    pub task_id: Uuid,
    pub namespace: String,
    pub operation: String,
    pub effect: ExtensionEffect,
    /// Trusted task grant epoch; changed on revocation.
    pub generation: u64,
    /// Explicit host approval for elevated effects.
    pub host_approved: bool,
}

/// Trusted runtime context for a single dispatch decision.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CapabilityContext {
    pub task_id: Uuid,
    pub generation: u64,
    pub task_authorized: bool,
}

/// Why an extension operation cannot be dispatched.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CapabilityDenial {
    TaskUnauthorized,
    MissingGrant,
    WrongTask,
    StaleGrant,
    WrongOperation,
    WrongEffect,
    HostApprovalRequired,
}

/// A validated declaration and the host-issued grant for one task.
///
/// Hosts own the registry and the grant. Constructing this value does not make
/// the operation executable by a provider.
pub struct ExtensionTool<'a> {
    /// Host-validated descriptor for the operation.
    pub descriptor: ExtensionDescriptor<'a>,
    /// Provider-facing description of the operation.
    pub description: &'a str,
    /// Supported JSON Schema object for model arguments.
    pub parameters: Value,
    /// Host-issued grant for the active task.
    pub grant: &'a CapabilityGrant,
}

/// Errors that prevent an extension from being advertised or dispatched.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BridgeError {
    /// The descriptor collection is malformed.
    InvalidContract(ExtensionContractError),
    /// The host capability check denied the operation.
    Denied(CapabilityDenial),
    /// The declared argument schema is unsupported.
    InvalidSchema,
    /// Model arguments do not match the declared schema.
    InvalidArguments,
    /// The requested name is not registered.
    UnknownTool,
    /// The requested name collides with a built-in or another extension.
    NameCollision,
    /// The declared effect cannot use this in-process bridge.
    UnsupportedEffect,
}
