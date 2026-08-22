//! Access to the identity of the agent's active session.

use nanocodex::{
    Tool,
    tools::contract::{
        ToolContext, ToolDefinition, ToolInput, ToolOutput, ToolResult, async_trait,
    },
};
use serde_json::json;

pub(crate) struct CurrentSessionTool;

#[async_trait]
impl Tool for CurrentSessionTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition::function(
            "current_session",
            "Returns the active session ID for the calling agent.",
            json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false
            }),
        )
        .with_output_schema(json!({
            "type": "object",
            "properties": {
                "session_id": {
                    "type": "string",
                    "description": "The active session ID for the calling agent."
                }
            },
            "required": ["session_id"],
            "additionalProperties": false
        }))
    }

    async fn execute(&self, input: ToolInput, context: ToolContext<'_>) -> ToolResult {
        let _: EmptyInput = input.decode_json()?;
        Ok(ToolOutput::from_json(
            json!({ "session_id": context.session_id() }),
            true,
        ))
    }
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct EmptyInput {}

#[cfg(test)]
mod tests {
    use super::CurrentSessionTool;
    use nanocodex::{
        Tool,
        tools::contract::{ToolContext, ToolInput, ToolOutputBody},
    };
    use serde_json::{Value, json, value::to_raw_value};

    #[tokio::test]
    async fn returns_the_calling_agents_session_id() {
        let input = ToolInput::Function(to_raw_value(&json!({})).unwrap());
        let context = ToolContext::new("test-model", "active-session", "test-call", &[], 32);

        let output = CurrentSessionTool.execute(input, context).await.unwrap();
        let ToolOutputBody::Text(output) = output.output else {
            panic!("current session returned non-text output");
        };
        let output: Value = serde_json::from_str(&output).unwrap();

        assert_eq!(output, json!({ "session_id": "active-session" }));
    }

    #[test]
    fn definition_has_a_closed_empty_input_and_typed_output() {
        let definition = CurrentSessionTool.definition();

        assert_eq!(definition.name(), "current_session");
        assert_eq!(
            definition.parameters().unwrap().as_value(),
            &json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false
            })
        );
        assert_eq!(
            definition.output_schema().unwrap().as_value()["required"],
            json!(["session_id"])
        );
    }
}
