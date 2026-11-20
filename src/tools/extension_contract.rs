//! Versioned, descriptive contracts for future external tool extensions.
//!
//! These descriptors do not register executable tools or grant authority. Runtime
//! authorization remains with TaskSession and the existing Tools boundary.

use std::collections::HashSet;

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

/// Validate a collection before any future discovery-to-registry bridge.
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

#[cfg(test)]
mod tests {
    use super::*;

    fn descriptor<'a>(namespace: &'a str, name: &'a str) -> ExtensionDescriptor<'a> {
        ExtensionDescriptor {
            version: EXTENSION_CONTRACT_VERSION,
            namespace,
            name,
            effect: ExtensionEffect::ReadWorkspace,
            authority: ExtensionAuthority::Task,
        }
    }

    #[test]
    fn accepts_distinct_namespaced_operations() {
        assert_eq!(
            validate_descriptors(&[descriptor("mcp_a", "read"), descriptor("mcp_b", "read")]),
            Ok(())
        );
    }

    #[test]
    fn rejects_duplicates_and_invalid_identifiers() {
        assert_eq!(
            validate_descriptors(&[descriptor("mcp", "read"), descriptor("mcp", "read")]),
            Err(ExtensionContractError::DuplicateOperation)
        );
        for invalid in ["", "../escape", "bad.name", "with space"] {
            assert_eq!(
                validate_descriptors(&[descriptor("mcp", invalid)]),
                Err(ExtensionContractError::InvalidIdentifier)
            );
        }
    }

    #[test]
    fn rejects_unknown_contract_versions() {
        let mut item = descriptor("mcp", "read");
        item.version += 1;
        assert_eq!(
            validate_descriptors(&[item]),
            Err(ExtensionContractError::UnsupportedVersion)
        );
    }

    #[test]
    fn metadata_does_not_modify_registered_tools() {
        let before = crate::tools::registry::workspace_operations()
            .iter()
            .map(|operation| operation.name())
            .collect::<Vec<_>>();
        validate_descriptors(&[descriptor("external", "read")]).unwrap();
        let after = crate::tools::registry::workspace_operations()
            .iter()
            .map(|operation| operation.name())
            .collect::<Vec<_>>();
        assert_eq!(before, after);
    }
}
