//! Compatibility path for shared extension contract types.

#[allow(unused_imports)]
pub use crate::contracts::extensions::{
    validate_descriptors, ExtensionAuthority, ExtensionContractError, ExtensionDescriptor,
    ExtensionEffect, EXTENSION_CONTRACT_VERSION,
};

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
