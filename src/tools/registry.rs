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
    arguments: &'static [ArgumentDefinition],
}

impl OperationDefinition {
    pub(crate) const fn name(self) -> &'static str {
        self.operation.name()
    }

    pub(crate) const fn description(self) -> &'static str {
        self.description
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
        arguments: &[string("path")],
    },
    OperationDefinition {
        operation: WorkspaceOperation::ReadRange,
        description: "Read a bounded one-based line range from one safe UTF-8 file.",
        arguments: &[
            string("path"),
            positive_integer("start_line"),
            bounded_positive_integer("max_lines", 200),
        ],
    },
    OperationDefinition {
        operation: WorkspaceOperation::ListDirectory,
        description: "List immediate entries in a safe workspace directory.",
        arguments: &[string("path")],
    },
    OperationDefinition {
        operation: WorkspaceOperation::CreateDirectory,
        description: "Create one directory inside the workspace.",
        arguments: &[string("path")],
    },
    OperationDefinition {
        operation: WorkspaceOperation::SearchText,
        description: "Find a literal query in immediate regular UTF-8 workspace files, honoring applicable root and nested .gitignore glob rules and negation.",
        arguments: &[string("directory"), bounded_string("query", 4096)],
    },
    OperationDefinition {
        operation: WorkspaceOperation::SearchTreeText,
        description: "Find a literal query recursively in regular UTF-8 workspace files, honoring applicable root and nested .gitignore glob rules and negation, and skipping .git metadata directories.",
        arguments: &[string("directory"), bounded_string("query", 4096)],
    },
    OperationDefinition {
        operation: WorkspaceOperation::CreateFile,
        description:
            "Create one new UTF-8 file inside the workspace without replacing an existing path.",
        arguments: &[string("path"), string("contents")],
    },
    OperationDefinition {
        operation: WorkspaceOperation::ReplaceText,
        description: "Replace one unique exact match in a safe UTF-8 workspace file.",
        arguments: &[
            string("path"),
            non_empty_string("match_text"),
            string("replacement_text"),
        ],
    },
    OperationDefinition {
        operation: WorkspaceOperation::ApplyPatch,
        description: "Apply one bounded single-file unified diff to an existing safe UTF-8 workspace file with exact hunk context.",
        arguments: &[
            string("path"),
            bounded_string("patch", MAX_PATCH_BYTES),
        ],
    },
    OperationDefinition {
        operation: WorkspaceOperation::OfflineCargoTest,
        description: "Run the fixed offline locked Cargo test profile with an optional literal test-name filter, only if the task has host authorization and verified Linux confinement.",
        arguments: &[optional_bounded_string(
            "test_filter",
            crate::tools::process::MAX_TEST_FILTER_BYTES,
        )],
    },
];

pub(crate) fn workspace_operations() -> &'static [OperationDefinition] {
    &WORKSPACE_OPERATIONS
}
