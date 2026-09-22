//! Tact's supported model roster and input parsing.

use nanocodex::Model;
use serde::{Deserialize, Deserializer, de};

pub(crate) const SUPPORTED_MODELS: [Model; 3] = [Model::Luna, Model::Sol, Model::Astra];

pub(crate) fn parse(value: &str) -> Result<Model, String> {
    match value {
        "gpt-6-luna" | "luna" => Ok(Model::Luna),
        "gpt-6-sol" | "sol" => Ok(Model::Sol),
        "gpt-6-astra" | "astra" => Ok(Model::Astra),
        _ => Err(format!(
            "invalid model {value:?}; expected gpt-6-luna, gpt-6-sol, or gpt-6-astra"
        )),
    }
}

pub(crate) fn deserialize_optional<'de, D>(deserializer: D) -> Result<Option<Model>, D::Error>
where
    D: Deserializer<'de>,
{
    Option::<String>::deserialize(deserializer)?
        .map(|value| parse(&value).map_err(de::Error::custom))
        .transpose()
}

pub(crate) const fn name(model: Model) -> &'static str {
    match model {
        Model::Luna => "Luna",
        Model::Sol => "Sol",
        Model::Astra => "Astra",
        _ => model.as_str(),
    }
}

#[cfg(test)]
mod tests {
    use super::parse;
    use nanocodex::Model;

    #[test]
    fn accepts_current_model_ids_and_short_names() {
        for (value, expected) in [
            ("gpt-6-luna", Model::Luna),
            ("luna", Model::Luna),
            ("gpt-6-sol", Model::Sol),
            ("sol", Model::Sol),
            ("gpt-6-astra", Model::Astra),
            ("astra", Model::Astra),
        ] {
            assert_eq!(parse(value), Ok(expected));
        }
    }

    #[test]
    fn rejects_retired_model_ids() {
        for value in ["gpt-5.6-luna", "gpt-5.6-sol", "gpt-5.6-terra", "terra"] {
            assert!(parse(value).is_err(), "retired model {value} was accepted");
        }
    }
}
