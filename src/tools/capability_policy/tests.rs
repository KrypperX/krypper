use super::*;
use crate::tools::extension_contract::EXTENSION_CONTRACT_VERSION;

fn descriptor(
    effect: ExtensionEffect,
    authority: ExtensionAuthority,
) -> ExtensionDescriptor<'static> {
    ExtensionDescriptor {
        version: EXTENSION_CONTRACT_VERSION,
        namespace: "mcp",
        name: "run",
        effect,
        authority,
    }
}

fn fixture(effect: ExtensionEffect) -> (CapabilityContext, CapabilityGrant) {
    let task_id = Uuid::new_v4();
    (
        CapabilityContext {
            task_id,
            generation: 4,
            task_authorized: true,
        },
        CapabilityGrant {
            task_id,
            namespace: "mcp".into(),
            operation: "run".into(),
            effect,
            generation: 4,
            host_approved: false,
        },
    )
}

#[test]
fn explicit_read_and_mutation_grants_work_without_elevation() {
    for effect in [
        ExtensionEffect::ReadWorkspace,
        ExtensionEffect::MutateWorkspace,
    ] {
        let (context, grant) = fixture(effect);
        assert_eq!(
            evaluate(
                &descriptor(effect, ExtensionAuthority::Task),
                context,
                Some(&grant)
            ),
            Ok(())
        );
    }
}

#[test]
fn no_grant_or_unauthorized_task_fails_closed() {
    let (mut context, grant) = fixture(ExtensionEffect::ReadWorkspace);
    let request = descriptor(ExtensionEffect::ReadWorkspace, ExtensionAuthority::Task);
    assert_eq!(
        evaluate(&request, context, None),
        Err(CapabilityDenial::MissingGrant)
    );
    context.task_authorized = false;
    assert_eq!(
        evaluate(&request, context, Some(&grant)),
        Err(CapabilityDenial::TaskUnauthorized)
    );
}

#[test]
fn task_ownership_and_revocation_are_enforced() {
    let (mut context, grant) = fixture(ExtensionEffect::ReadWorkspace);
    let request = descriptor(ExtensionEffect::ReadWorkspace, ExtensionAuthority::Task);
    context.task_id = Uuid::new_v4();
    assert_eq!(
        evaluate(&request, context, Some(&grant)),
        Err(CapabilityDenial::WrongTask)
    );
    context.task_id = grant.task_id;
    context.generation += 1;
    assert_eq!(
        evaluate(&request, context, Some(&grant)),
        Err(CapabilityDenial::StaleGrant)
    );
}

#[test]
fn wrong_operation_and_effect_cannot_reuse_grants() {
    let (context, grant) = fixture(ExtensionEffect::ReadWorkspace);
    let mut request = descriptor(ExtensionEffect::ReadWorkspace, ExtensionAuthority::Task);
    request.name = "other";
    assert_eq!(
        evaluate(&request, context, Some(&grant)),
        Err(CapabilityDenial::WrongOperation)
    );
    request.name = "run";
    request.effect = ExtensionEffect::MutateWorkspace;
    assert_eq!(
        evaluate(&request, context, Some(&grant)),
        Err(CapabilityDenial::WrongEffect)
    );
}

#[test]
fn privileged_effects_require_host_approval_even_with_misleading_metadata() {
    for effect in [
        ExtensionEffect::ExecuteProcess,
        ExtensionEffect::Network,
        ExtensionEffect::ExternalService,
    ] {
        let (context, mut grant) = fixture(effect);
        let request = descriptor(effect, ExtensionAuthority::Task);
        assert_eq!(
        evaluate(&request, context, Some(&grant)),
        Err(CapabilityDenial::HostApprovalRequired)
    );
        grant.host_approved = true;
        assert_eq!(evaluate(&request, context, Some(&grant)), Ok(()));
    }
}

#[test]
fn explicit_host_prerequisite_is_enforced_for_mutation() {
    let (context, mut grant) = fixture(ExtensionEffect::MutateWorkspace);
    let request = descriptor(
        ExtensionEffect::MutateWorkspace,
        ExtensionAuthority::TaskAndHostGrant,
    );
    assert_eq!(
        evaluate(&request, context, Some(&grant)),
        Err(CapabilityDenial::HostApprovalRequired)
    );
    grant.host_approved = true;
    assert_eq!(evaluate(&request, context, Some(&grant)), Ok(()));
}
