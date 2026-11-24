//! The exact function names exposed in one model request.

use serde::Deserialize;
use std::collections::BTreeSet;

use super::errors::ModelError;

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct AdvertisedToolNames(BTreeSet<String>);

impl AdvertisedToolNames {
    /// Read names from the serialized request that is about to be sent.
    ///
    /// Supported layouts cover chat completions, Responses, Anthropic and
    /// Gemini. Gemini's explicit `allowedFunctionNames` list narrows the
    /// declarations and is treated as part of the advertised set.
    pub(crate) fn from_request(request: &[u8]) -> Result<Self, ModelError> {
        let request: RequestToolEnvelope =
            serde_json::from_slice(request).map_err(|_| ModelError::RequestEncoding)?;
        let mut names = BTreeSet::new();
        let mut has_gemini_declarations = false;
        for declaration in request.tools {
            match (
                declaration.function,
                declaration.name,
                declaration.function_declarations,
            ) {
                (Some(function), None, None) => {
                    insert_name(&mut names, &function.name)?;
                }
                (None, Some(name), None) => {
                    insert_name(&mut names, &name)?;
                }
                (None, None, Some(functions)) => {
                    if functions.is_empty() {
                        return Err(ModelError::RequestEncoding);
                    }
                    for function in functions {
                        insert_name(&mut names, &function.name)?;
                    }
                    has_gemini_declarations = true;
                }
                _ => return Err(ModelError::RequestEncoding),
            }
        }

        if has_gemini_declarations {
            let allowed_names = request
                .tool_config
                .and_then(|tool_config| tool_config.function_calling_config)
                .and_then(|config| config.allowed_function_names)
                .ok_or(ModelError::RequestEncoding)?;
            let mut allowed = BTreeSet::new();
            for allowed_name in allowed_names {
                if !names.contains(&allowed_name) || !allowed.insert(allowed_name) {
                    return Err(ModelError::RequestEncoding);
                }
            }
            names = allowed;
        }

        Ok(Self(names))
    }

    pub(crate) fn contains(&self, name: &str) -> bool {
        self.0.contains(name)
    }

    #[cfg(test)]
    pub(crate) fn from_names<T: Into<String>>(names: impl IntoIterator<Item = T>) -> Self {
        Self(names.into_iter().map(Into::into).collect())
    }

    #[cfg(test)]
    pub(crate) fn all_builtins() -> Self {
        let names = crate::tools::registry::workspace_operations()
            .iter()
            .map(|operation| operation.name().to_owned())
            .chain(std::iter::once("complete_task".to_owned()));
        Self(names.collect())
    }
}

#[derive(Deserialize)]
struct RequestToolEnvelope {
    tools: Vec<ToolDeclaration>,
    #[serde(rename = "toolConfig")]
    tool_config: Option<GeminiToolConfig>,
}

#[derive(Deserialize)]
struct ToolDeclaration {
    function: Option<NamedFunction>,
    name: Option<String>,
    #[serde(rename = "functionDeclarations")]
    function_declarations: Option<Vec<NamedFunction>>,
}

#[derive(Deserialize)]
struct NamedFunction {
    name: String,
}

#[derive(Deserialize)]
struct GeminiToolConfig {
    #[serde(rename = "functionCallingConfig")]
    function_calling_config: Option<GeminiFunctionCallingConfig>,
}

#[derive(Deserialize)]
struct GeminiFunctionCallingConfig {
    #[serde(rename = "allowedFunctionNames")]
    allowed_function_names: Option<Vec<String>>,
}

fn insert_name(names: &mut BTreeSet<String>, name: &str) -> Result<(), ModelError> {
    if name.is_empty() || !names.insert(name.to_owned()) {
        return Err(ModelError::RequestEncoding);
    }
    Ok(())
}
