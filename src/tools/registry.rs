use super::MAX_PATCH_BYTES;
use serde_json::{json, Map, Value};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum WorkspaceOperation {
    OfflineCargoTest,
    ReadFile,
    ReadRange,
    ListDirectory,
    CreateDirectory,
    SearchText,
    SearchTreeText,
    CreateFile,
    ReplaceText,
    ApplyPatch,
}

impl WorkspaceOperation {
    const fn name(self) -> &'static str {
        match self {
            Self::OfflineCargoTest => "offline_cargo_test",
            Self::ReadFile => "read_file",
            Self::ReadRange => "read_range",
            Self::ListDirectory => "list_directory",
            Self::CreateDirectory => "create_directory",
            Self::SearchText => "search_text",
            Self::SearchTreeText => "search_tree_text",
            Self::CreateFile => "create_file",
            Self::ReplaceText => "replace_text",
            Self::ApplyPatch => "apply_patch",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum EffectClass {
    ReadOnlyWorkspaceAccess,
    WorkspaceMutation,
    FixedSandboxedOfflineCargoTest,
}

impl EffectClass {
    pub(crate) const fn as_description(self) -> &'static str {
        match self {
            Self::ReadOnlyWorkspaceAccess => "read-only workspace access",
            Self::WorkspaceMutation => "workspace mutation",
            Self::FixedSandboxedOfflineCargoTest => "fixed sandboxed offline Cargo test",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum AuthorizationPrerequisite {
    ExistingTaskAuthorization,
    ExistingTaskAuthorizationHostAuthorizationAndVerifiedConfinement,
}

impl AuthorizationPrerequisite {
    pub(crate) const fn as_description(self) -> &'static str {
        match self {
            Self::ExistingTaskAuthorization => {
                "usable only within the existing task authorization"
            }
            Self::ExistingTaskAuthorizationHostAuthorizationAndVerifiedConfinement => {
                "usable only within the existing task authorization and additionally requires the existing host authorization and verified Linux confinement"
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ArgumentSchema {
    String,
    NonEmptyString,
    BoundedString { max_length: usize },
    PositiveInteger,
    BoundedPositiveInteger { maximum: usize },
}

impl ArgumentSchema {
    fn as_json(self) -> Value {
        match self {
            Self::String => json!({"type":"string"}),
            Self::NonEmptyString => json!({"type":"string","minLength":1}),
            Self::BoundedString { max_length } => {
                json!({"type":"string","minLength":1,"maxLength":max_length})
            }
            Self::PositiveInteger => json!({"type":"integer","minimum":1}),
            Self::BoundedPositiveInteger { maximum } => {
                json!({"type":"integer","minimum":1,"maximum":maximum})
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ArgumentDefinition {
    name: &'static str,
    schema: ArgumentSchema,
    required: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct OperationDefinition {
    operation: WorkspaceOperation,
    description: &'static str,
    effect_class: EffectClass,
    authorization_prerequisite: AuthorizationPrerequisite,
    arguments: &'static [ArgumentDefinition],
}

impl OperationDefinition {
    pub(crate) const fn name(self) -> &'static str {
        self.operation.name()
    }

    pub(crate) const fn description(self) -> &'static str {
        self.description
    }

    pub(crate) const fn effect_class(self) -> EffectClass {
        self.effect_class
    }

    /// Adds descriptive effect and authorization metadata for provider-facing tool declarations.
    ///
    /// Execution continues to use the existing TaskSession and Tools authorization checks.
    pub(crate) fn provider_description(self) -> String {
        format!(
            "{} Effect: {}. Authorization prerequisite: {}. Existing runtime authorization checks remain authoritative and fail closed; this metadata does not grant access.",
            self.description(),
            self.effect_class.as_description(),
            self.authorization_prerequisite.as_description()
        )
    }

    pub(crate) fn parameters(self) -> Value {
        let properties = self
            .arguments
            .iter()
            .map(|argument| (argument.name.to_owned(), argument.schema.as_json()))
            .collect::<Map<_, _>>();
        let mut required = self
            .arguments
            .iter()
            .filter(|argument| argument.required)
            .map(|argument| argument.name)
            .collect::<Vec<_>>();
        required.sort_unstable();

        json!({
            "type":"object",
            "properties":properties,
            "required":required,
            "additionalProperties":false
        })
    }
}

const fn string(name: &'static str) -> ArgumentDefinition {
    ArgumentDefinition {
        name,
        schema: ArgumentSchema::String,
        required: true,
    }
}

const fn non_empty_string(name: &'static str) -> ArgumentDefinition {
    ArgumentDefinition {
        name,
        schema: ArgumentSchema::NonEmptyString,
        required: true,
    }
}

const fn bounded_string(name: &'static str, max_length: usize) -> ArgumentDefinition {
    ArgumentDefinition {
        name,
        schema: ArgumentSchema::BoundedString { max_length },
        required: true,
    }
}

const fn optional_bounded_string(name: &'static str, max_length: usize) -> ArgumentDefinition {
    ArgumentDefinition {
        name,
        schema: ArgumentSchema::BoundedString { max_length },
        required: false,
    }
}

const fn positive_integer(name: &'static str) -> ArgumentDefinition {
    ArgumentDefinition {
        name,
        schema: ArgumentSchema::PositiveInteger,
        required: true,
    }
}

const fn bounded_positive_integer(name: &'static str, maximum: usize) -> ArgumentDefinition {
    ArgumentDefinition {
        name,
        schema: ArgumentSchema::BoundedPositiveInteger { maximum },
        required: true,
    }
}

const WORKSPACE_OPERATIONS: [OperationDefinition; 10] = [
    OperationDefinition {
        operation: WorkspaceOperation::ReadFile,
        description: "Read one safe UTF-8 workspace file.",
        effect_class: EffectClass::ReadOnlyWorkspaceAccess,
        authorization_prerequisite: AuthorizationPrerequisite::ExistingTaskAuthorization,
        arguments: &[string("path")],
    },
    OperationDefinition {
        operation: WorkspaceOperation::ReadRange,
        description: "Read a bounded one-based line range from one safe UTF-8 file.",
        effect_class: EffectClass::ReadOnlyWorkspaceAccess,
        authorization_prerequisite: AuthorizationPrerequisite::ExistingTaskAuthorization,
        arguments: &[
            string("path"),
            positive_integer("start_line"),
            bounded_positive_integer("max_lines", 200),
        ],
    },
    OperationDefinition {
        operation: WorkspaceOperation::ListDirectory,
        description: "List immediate entries in a safe workspace directory.",
        effect_class: EffectClass::ReadOnlyWorkspaceAccess,
        authorization_prerequisite: AuthorizationPrerequisite::ExistingTaskAuthorization,
        arguments: &[string("path")],
    },
    OperationDefinition {
        operation: WorkspaceOperation::CreateDirectory,
        description: "Create one directory inside the workspace.",
        effect_class: EffectClass::WorkspaceMutation,
        authorization_prerequisite: AuthorizationPrerequisite::ExistingTaskAuthorization,
        arguments: &[string("path")],
    },
    OperationDefinition {
        operation: WorkspaceOperation::SearchText,
        description: "Find a literal query in immediate regular UTF-8 workspace files, honoring applicable root and nested .gitignore glob rules and negation.",
        effect_class: EffectClass::ReadOnlyWorkspaceAccess,
        authorization_prerequisite: AuthorizationPrerequisite::ExistingTaskAuthorization,
        arguments: &[string("directory"), bounded_string("query", 4096)],
    },
    OperationDefinition {
        operation: WorkspaceOperation::SearchTreeText,
        description: "Find a literal query recursively in regular UTF-8 workspace files, honoring applicable root and nested .gitignore glob rules and negation, and skipping .git metadata directories.",
        effect_class: EffectClass::ReadOnlyWorkspaceAccess,
        authorization_prerequisite: AuthorizationPrerequisite::ExistingTaskAuthorization,
        arguments: &[string("directory"), bounded_string("query", 4096)],
    },
    OperationDefinition {
        operation: WorkspaceOperation::CreateFile,
        description:
            "Create one new UTF-8 file inside the workspace without replacing an existing path.",
        effect_class: EffectClass::WorkspaceMutation,
        authorization_prerequisite: AuthorizationPrerequisite::ExistingTaskAuthorization,
        arguments: &[string("path"), string("contents")],
    },
    OperationDefinition {
        operation: WorkspaceOperation::ReplaceText,
        description: "Replace one unique exact match in a safe UTF-8 workspace file.",
        effect_class: EffectClass::WorkspaceMutation,
        authorization_prerequisite: AuthorizationPrerequisite::ExistingTaskAuthorization,
        arguments: &[
            string("path"),
            non_empty_string("match_text"),
            string("replacement_text"),
        ],
    },
    OperationDefinition {
        operation: WorkspaceOperation::ApplyPatch,
        description: "Apply one bounded single-file unified diff to an existing safe UTF-8 workspace file with exact hunk context.",
        effect_class: EffectClass::WorkspaceMutation,
        authorization_prerequisite: AuthorizationPrerequisite::ExistingTaskAuthorization,
        arguments: &[
            string("path"),
            bounded_string("patch", MAX_PATCH_BYTES),
        ],
    },
    OperationDefinition {
        operation: WorkspaceOperation::OfflineCargoTest,
        description: "Run the fixed offline locked Cargo test profile with an optional literal test-name filter.",
        effect_class: EffectClass::FixedSandboxedOfflineCargoTest,
        authorization_prerequisite: AuthorizationPrerequisite::ExistingTaskAuthorizationHostAuthorizationAndVerifiedConfinement,
        arguments: &[optional_bounded_string(
            "test_filter",
            crate::tools::process::MAX_TEST_FILTER_BYTES,
        )],
    },
];

pub(crate) fn workspace_operations() -> &'static [OperationDefinition] {
    &WORKSPACE_OPERATIONS
}

pub(crate) fn effect_class_for(name: &str) -> Option<EffectClass> {
    workspace_operations()
        .iter()
        .find(|operation| operation.name() == name)
        .map(OperationDefinition::effect_class)
}

#[cfg(test)]
mod tests {
    use super::{workspace_operations, AuthorizationPrerequisite, EffectClass};

    #[test]
    fn registered_operations_have_exactly_one_effect_and_authorization_prerequisite() {
        let actual = workspace_operations()
            .iter()
            .map(|operation| {
                (
                    operation.name(),
                    operation.effect_class,
                    operation.authorization_prerequisite,
                )
            })
            .collect::<Vec<_>>();

        assert_eq!(
            actual,
            vec![
                (
                    "read_file",
                    EffectClass::ReadOnlyWorkspaceAccess,
                    AuthorizationPrerequisite::ExistingTaskAuthorization,
                ),
                (
                    "read_range",
                    EffectClass::ReadOnlyWorkspaceAccess,
                    AuthorizationPrerequisite::ExistingTaskAuthorization,
                ),
                (
                    "list_directory",
                    EffectClass::ReadOnlyWorkspaceAccess,
                    AuthorizationPrerequisite::ExistingTaskAuthorization,
                ),
                (
                    "create_directory",
                    EffectClass::WorkspaceMutation,
                    AuthorizationPrerequisite::ExistingTaskAuthorization,
                ),
                (
                    "search_text",
                    EffectClass::ReadOnlyWorkspaceAccess,
                    AuthorizationPrerequisite::ExistingTaskAuthorization,
                ),
                (
                    "search_tree_text",
                    EffectClass::ReadOnlyWorkspaceAccess,
                    AuthorizationPrerequisite::ExistingTaskAuthorization,
                ),
                (
                    "create_file",
                    EffectClass::WorkspaceMutation,
                    AuthorizationPrerequisite::ExistingTaskAuthorization,
                ),
                (
                    "replace_text",
                    EffectClass::WorkspaceMutation,
                    AuthorizationPrerequisite::ExistingTaskAuthorization,
                ),
                (
                    "apply_patch",
                    EffectClass::WorkspaceMutation,
                    AuthorizationPrerequisite::ExistingTaskAuthorization,
                ),
                (
                    "offline_cargo_test",
                    EffectClass::FixedSandboxedOfflineCargoTest,
                    AuthorizationPrerequisite::ExistingTaskAuthorizationHostAuthorizationAndVerifiedConfinement,
                ),
            ]
        );
    }

    #[test]
    fn provider_descriptions_identify_metadata_as_descriptive_only() {
        for operation in workspace_operations() {
            let description = operation.provider_description();

            assert!(description.contains(operation.effect_class.as_description()));
            assert!(description.contains(operation.authorization_prerequisite.as_description()));
            assert!(description
                .contains("runtime authorization checks remain authoritative and fail closed"));
            assert!(description.contains("this metadata does not grant access"));
        }
    }
}
