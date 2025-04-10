//! High-level serializer for converting a Roblox DOM into an XML string.
//! 
//! This module wraps the low-level XML writer machinery (defined in `serializer_core`)
//! along with our property serialization code (e.g. `encode_internal`) into a single
//! convenience function: `to_string`.

use std::borrow::Cow;
use std::collections::{BTreeMap, HashMap};
use std::io::Write;

use rbx_dom_weak::{
    types::{Ref, SharedString, SharedStringHash, Variant, VariantType, WeakDom},
};
use rbx_reflection::{DataType, PropertyKind, PropertySerialization, ReflectionDatabase};

use crate::{
    conversion::ConvertVariant,
    core::find_serialized_property_descriptor,
    error::{EncodeError as NewEncodeError, EncodeErrorKind},
    types::write_value_xml,
};
use crate::serializer_core::{XmlEventWriter, XmlWriteEvent};

/// Serializes the given DOM into XML by writing to the provided output.
/// This is the core function that the high-level API wraps.
pub fn encode_internal<W: Write>(
    output: W,
    tree: &WeakDom,
    ids: &[Ref],
    options: EncodeOptions,
) -> Result<(), NewEncodeError> {
    let mut writer = XmlEventWriter::from_output(output);
    let mut state = EmitState::new(options);

    writer.write(XmlWriteEvent::start_element("roblox").attr("version", "4"))?;

    let mut property_buffer = Vec::new();
    for id in ids {
        serialize_instance(&mut writer, &mut state, tree, *id, &mut property_buffer)?;
    }

    serialize_shared_strings(&mut writer, &mut state)?;

    writer.write(XmlWriteEvent::end_element())?;

    Ok(())
}

/// Describes the strategy that rbx_xml should use when serializing properties.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum EncodePropertyBehavior {
    /// Ignores properties that aren't known by rbx_xml.
    IgnoreUnknown,
    /// Writes unrecognized properties.
    WriteUnknown,
    /// Returns an error if any properties are found that aren't known.
    ErrorOnUnknown,
    /// Completely turns off reflection (properties appear exactly as in the tree).
    NoReflection,
}

/// Options available for serializing an XML-format model or place.
#[derive(Debug, Clone)]
pub struct EncodeOptions<'db> {
    property_behavior: EncodePropertyBehavior,
    database: &'db ReflectionDatabase<'db>,
}

impl<'db> EncodeOptions<'db> {
    /// Constructs an `EncodeOptions` with default values.
    #[inline]
    pub fn new() -> Self {
        EncodeOptions {
            property_behavior: EncodePropertyBehavior::IgnoreUnknown,
            database: rbx_reflection_database::get(),
        }
    }

    /// Override the property behavior.
    #[inline]
    pub fn property_behavior(self, property_behavior: EncodePropertyBehavior) -> Self {
        EncodeOptions { property_behavior, ..self }
    }

    /// Override the reflection database.
    #[inline]
    pub fn reflection_database(self, database: &'db ReflectionDatabase<'db>) -> Self {
        EncodeOptions { database, ..self }
    }

    pub(crate) fn use_reflection(&self) -> bool {
        self.property_behavior != EncodePropertyBehavior::NoReflection
    }
}

impl<'db> Default for EncodeOptions<'db> {
    fn default() -> EncodeOptions<'db> {
        EncodeOptions::new()
    }
}

/// Internal state used when emitting XML.
pub struct EmitState<'db> {
    options: EncodeOptions<'db>,
    /// Maps instance IDs to their generated referent number.
    referent_map: HashMap<Ref, u32>,
    /// The next referent value to use.
    next_referent: u32,
    /// Map of shared strings to output later.
    shared_strings_to_emit: BTreeMap<SharedStringHash, SharedString>,
}

impl<'db> EmitState<'db> {
    pub fn new(options: EncodeOptions<'db>) -> EmitState<'db> {
        EmitState {
            options,
            referent_map: HashMap::new(),
            next_referent: 0,
            shared_strings_to_emit: BTreeMap::new(),
        }
    }

    pub fn map_id(&mut self, id: Ref) -> u32 {
        if let Some(&value) = self.referent_map.get(&id) {
            value
        } else {
            let referent = self.next_referent;
            self.referent_map.insert(id, referent);
            self.next_referent += 1;
            referent
        }
    }

    pub fn add_shared_string(&mut self, value: SharedString) {
        self.shared_strings_to_emit.insert(value.hash(), value);
    }
}

/// Serialize a single instance (and its children) to XML.
fn serialize_instance<'dom, W: Write>(
    writer: &mut XmlEventWriter<W>,
    state: &mut EmitState,
    tree: &'dom WeakDom,
    id: Ref,
    property_buffer: &mut Vec<(&'dom String, &'dom Variant)>,
) -> Result<(), NewEncodeError> {
    let instance = tree.get_by_ref(id).unwrap();
    let mapped_id = state.map_id(id);

    writer.write(
        XmlWriteEvent::start_element("Item")
            .attr("class", &instance.class)
            .attr("referent", &mapped_id.to_string()),
    )?;

    writer.write(XmlWriteEvent::start_element("Properties"))?;

    write_value_xml(
        writer,
        state,
        "Name",
        &Variant::String(instance.name.clone()),
    )?;

    // Collect properties to sort them.
    property_buffer.extend(&instance.properties);
    property_buffer.sort_unstable_by_key(|(key, _)| *key);

    for (property_name, value) in property_buffer.drain(..) {
        let maybe_serialized_descriptor = if state.options.use_reflection() {
            find_serialized_property_descriptor(
                &instance.class,
                property_name,
                state.options.database,
            )
        } else {
            None
        };

        if let Some(serialized_descriptor) = maybe_serialized_descriptor {
            let data_type = match &serialized_descriptor.data_type {
                DataType::Value(data_type) => *data_type,
                DataType::Enum(_enum_name) => VariantType::Enum,
                _ => unimplemented!(),
            };

            let mut serialized_name = serialized_descriptor.name.as_ref();

            let mut converted_value = match value.try_convert_ref(data_type) {
                Ok(val) => val,
                Err(message) => {
                    return Err(
                        writer.error(EncodeErrorKind::UnsupportedPropertyConversion {
                            class_name: instance.class.clone(),
                            property_name: property_name.to_string(),
                            expected_type: data_type,
                            actual_type: value.ty(),
                            message,
                        }),
                    )
                }
            };

            // Handle property migrations.
            if let PropertyKind::Canonical {
                serialization: PropertySerialization::Migrate(migration),
            } = &serialized_descriptor.kind
            {
                if let Ok(new_value) = migration.perform(&converted_value) {
                    converted_value = Cow::Owned(new_value);
                    serialized_name = &migration.new_property_name;
                }
            }

            write_value_xml(writer, state, serialized_name, &converted_value)?;
        } else {
            match state.options.property_behavior {
                EncodePropertyBehavior::IgnoreUnknown => {}
                EncodePropertyBehavior::WriteUnknown | EncodePropertyBehavior::NoReflection => {
                    write_value_xml(writer, state, property_name, value)?;
                }
                EncodePropertyBehavior::ErrorOnUnknown => {
                    return Err(writer.error(EncodeErrorKind::UnknownProperty {
                        class_name: instance.class.clone(),
                        property_name: property_name.clone(),
                    }));
                }
            }
        }
    }

    writer.write(XmlWriteEvent::end_element())?;

    // Recursively serialize all child instances.
    for child_id in instance.children() {
        serialize_instance(writer, state, tree, *child_id, property_buffer)?;
    }

    writer.write(XmlWriteEvent::end_element())?;

    Ok(())
}

/// Serialize all shared strings referenced in the DOM.
fn serialize_shared_strings<W: Write>(
    writer: &mut XmlEventWriter<W>,
    state: &mut EmitState,
) -> Result<(), NewEncodeError> {
    if state.shared_strings_to_emit.is_empty() {
        return Ok(());
    }

    writer.write(XmlWriteEvent::start_element("SharedStrings"))?;

    for value in state.shared_strings_to_emit.values() {
        let full_hash = value.hash();
        let truncated_hash = &full_hash.as_bytes()[..16];

        writer.write(
            XmlWriteEvent::start_element("SharedString")
                .attr("md5", &base64::encode(truncated_hash)),
        )?;

        writer.write_string(&base64::encode(value.data()))?;
        writer.end_element()?;
    }

    writer.end_element()?;
    Ok(())
}

/// Public high-level function which converts a WeakDom into an XML string.
/// The `ids` slice contains the IDs of the top‑level instances to serialize.
/// `options` control property serialization.
pub fn to_string(
    tree: &WeakDom,
    ids: &[Ref],
    options: EncodeOptions,
) -> Result<String, NewEncodeError> {
    let mut output = Vec::new();
    encode_internal(&mut output, tree, ids, options)?;
    let xml_string = String::from_utf8(output).map_err(|e| {
        NewEncodeError::new_from_writer(
            EncodeErrorKind::Custom(format!("UTF-8 conversion error: {}", e)),
            &[],
        )
    })?;
    Ok(xml_string)
}
