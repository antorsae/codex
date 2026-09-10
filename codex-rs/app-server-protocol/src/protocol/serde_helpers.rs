use std::path::PathBuf;

use serde::Deserialize;
use serde::Deserializer;
use serde::Serialize;
use serde::Serializer;

#[cfg(test)]
pub(crate) fn nullable_string_schema(
    generator: &mut schemars::r#gen::SchemaGenerator,
) -> schemars::schema::Schema {
    generator.subschema_for::<Option<String>>()
}

pub fn deserialize_empty_path_as_none<'de, D>(deserializer: D) -> Result<Option<PathBuf>, D::Error>
where
    D: Deserializer<'de>,
{
    let path = Option::<PathBuf>::deserialize(deserializer)?;
    Ok(path.filter(|path| !path.as_os_str().is_empty()))
}

pub fn deserialize_double_option<'de, T, D>(deserializer: D) -> Result<Option<Option<T>>, D::Error>
where
    T: Deserialize<'de>,
    D: Deserializer<'de>,
{
    serde_with::rust::double_option::deserialize(deserializer)
}

pub fn serialize_double_option<T, S>(
    value: &Option<Option<T>>,
    serializer: S,
) -> Result<S::Ok, S::Error>
where
    T: Serialize,
    S: Serializer,
{
    serde_with::rust::double_option::serialize(value, serializer)
}

/// Embed an existing RPC response without registering a second, untitled root definition.
#[cfg(test)]
pub(crate) fn nullable_embedded_response_schema<T: schemars::JsonSchema>(
    generator: &mut schemars::r#gen::SchemaGenerator,
) -> schemars::schema::Schema {
    schemars::schema::SchemaObject {
        subschemas: Some(Box::new(schemars::schema::SubschemaValidation {
            any_of: Some(vec![
                T::json_schema(generator),
                <() as schemars::JsonSchema>::json_schema(generator),
            ]),
            ..Default::default()
        })),
        ..Default::default()
    }
    .into()
}
