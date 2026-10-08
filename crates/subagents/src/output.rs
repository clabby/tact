//! The structured-result contract attached to every child turn.

use crate::error::SubagentError;
use jsonschema::Validator;
use serde_json::Value;

pub(crate) struct OutputContract {
    pub(crate) validator: Validator,
    pub(crate) schema: String,
}

impl OutputContract {
    pub(super) fn compile(schema: &Value) -> Result<Self, SubagentError> {
        let validator = jsonschema::validator_for(schema)
            .map_err(|error| SubagentError::InvalidSchema(Box::new(error)))?;
        Ok(Self {
            validator,
            schema: format!("{schema:#}"),
        })
    }
}

pub(crate) fn completion_instructions(schema: &str, turn_token: u64) -> String {
    format!(
        "Your contractual result is not prose. Before finishing, use Code Mode to call \
         `await tools.submit_result({{ turn_token: {turn_token}, output: ... }})` exactly once \
         with a JSON value matching the output schema below. If validation rejects the value, \
         correct it and retry. A turn that ends without an accepted result fails.\n\nOutput \
         schema:\n{schema}"
    )
}

#[cfg(test)]
mod tests {
    use super::{OutputContract, completion_instructions};
    use serde_json::json;

    #[test]
    fn output_contract_renders_the_schema_for_every_turn() {
        let schema = json!({
            "type": "object",
            "properties": { "report": { "type": "string" } },
            "required": ["report"]
        });

        let contract = OutputContract::compile(&schema).unwrap();
        let instructions = completion_instructions(&contract.schema, 7);

        assert!(instructions.contains("turn_token: 7"));
        assert!(instructions.ends_with(&contract.schema));
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&contract.schema).unwrap(),
            schema
        );
        assert!(contract.validator.is_valid(&json!({ "report": "done" })));
        assert!(!contract.validator.is_valid(&json!({ "report": 1 })));
    }
}
