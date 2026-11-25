//! Fail-closed capability decisions for future extension dispatch.
//!
//! The trusted host supplies task identity, grant epoch and approvals. Model
//! output and extension metadata cannot issue grants. Existing TaskSession,
//! registry, workspace and sandbox enforcement remain authoritative.

pub use crate::contracts::extensions::{CapabilityContext, CapabilityDenial, CapabilityGrant};
use crate::contracts::extensions::{ExtensionAuthority, ExtensionDescriptor, ExtensionEffect};

#[cfg(test)]
use uuid::Uuid;

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
        ExtensionEffect::ExecuteProcess
            | ExtensionEffect::Network
            | ExtensionEffect::ExternalService
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
