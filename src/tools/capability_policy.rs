//! Fail-closed capability decisions for future extension dispatch.
//!
//! The trusted host supplies task identity, grant epoch and approvals. Model
//! output and extension metadata cannot issue grants. Existing TaskSession,
//! registry, workspace and sandbox enforcement remain authoritative.

use super::extension_contract::{ExtensionAuthority, ExtensionDescriptor, ExtensionEffect};
use uuid::Uuid;

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

/// Evaluate one host-issued grant against the requested extension operation.
///
/// This pure decision does not execute a tool. The caller must still enforce
/// all existing runtime authorization, workspace and sandbox constraints.
pub fn evaluate(
    descriptor: &ExtensionDescriptor<'_>,
    context: CapabilityContext,
    grant: Option<&CapabilityGrant>,
) -> Result<(), CapabilityDenial> {
    if !context.task_authorized {
        return Err(CapabilityDenial::TaskUnauthorized);
    }
    let grant = grant.ok_or(CapabilityDenial::MissingGrant)?;
    if grant.task_id != context.task_id {
        return Err(CapabilityDenial::WrongTask);
    }
    if grant.generation != context.generation {
        return Err(CapabilityDenial::StaleGrant);
    }
    if grant.namespace != descriptor.namespace || grant.operation != descriptor.name {
        return Err(CapabilityDenial::WrongOperation);
    }
    if grant.effect != descriptor.effect {
        return Err(CapabilityDenial::WrongEffect);
    }
    // Network and external integrations always require explicit host approval,
    // even if a future descriptor incorrectly declares Task-only authority.
    let elevated = matches!(
        descriptor.effect,
        ExtensionEffect::ExecuteProcess | ExtensionEffect::Network | ExtensionEffect::ExternalService
    );
    if (elevated || descriptor.authority == ExtensionAuthority::TaskAndHostGrant)
        && !grant.host_approved
    {
        return Err(CapabilityDenial::HostApprovalRequired);
    }
    Ok(())
}

#[cfg(test)]
#[path = "capability_policy/tests.rs"]
mod tests;
