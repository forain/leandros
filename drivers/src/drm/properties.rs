//! DRM property system for dynamic configuration

use alloc::{vec::Vec, vec, collections::BTreeMap, string::{String, ToString}};
use super::core::{DrmObject, DrmObjectId, DrmObjectType};

/// Property types
#[repr(u32)]
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum DrmPropertyType {
    Range = 1,        // Range of values
    Enum = 2,         // Enumerated values
    Blob = 3,         // Binary data
    Bitmask = 4,      // Bitmask values
    Object = 5,       // Reference to another object
    SignedRange = 6,  // Signed range of values
}

/// Property flags
pub mod property_flags {
    pub const PENDING: u32 = 1 << 0;     // Value change is pending
    pub const RANGE: u32 = 1 << 1;       // Range property
    pub const IMMUTABLE: u32 = 1 << 2;   // Cannot be changed
    pub const ENUM: u32 = 1 << 3;        // Enumerated property
    pub const BLOB: u32 = 1 << 4;        // Blob property
    pub const BITMASK: u32 = 1 << 5;     // Bitmask property
    pub const ATOMIC: u32 = 1 << 31;     // Can be set atomically
}

/// Property value
#[derive(Debug, Clone)]
pub enum DrmPropertyValue {
    Range(u64),               // Numeric value in range
    Enum(u32),                // Enumerated value index
    Blob(Vec<u8>),           // Binary data
    Bitmask(u64),            // Bitmask value
    Object(DrmObjectId),     // Reference to object
    SignedRange(i64),        // Signed numeric value
}

/// Enumerated property value
#[derive(Debug, Clone)]
pub struct DrmEnumValue {
    pub value: u64,
    pub name: String,
}

/// Property definition
pub struct DrmProperty {
    id: DrmObjectId,
    pub name: String,
    pub property_type: DrmPropertyType,
    pub flags: u32,
    pub values: Vec<u64>,         // For ranges and enums
    pub enum_values: Vec<DrmEnumValue>, // For enum properties
    pub blob_ids: Vec<u32>,       // For blob properties
    pub current_value: Option<DrmPropertyValue>,
}

impl DrmProperty {
    /// Create a range property
    pub fn new_range(name: String, min: u64, max: u64, initial: u64) -> Self {
        Self {
            id: DrmObjectId::new(),
            name,
            property_type: DrmPropertyType::Range,
            flags: property_flags::RANGE | property_flags::ATOMIC,
            values: vec![min, max],
            enum_values: Vec::new(),
            blob_ids: Vec::new(),
            current_value: Some(DrmPropertyValue::Range(initial)),
        }
    }

    /// Create a signed range property
    pub fn new_signed_range(name: String, min: i64, max: i64, initial: i64) -> Self {
        Self {
            id: DrmObjectId::new(),
            name,
            property_type: DrmPropertyType::SignedRange,
            flags: property_flags::RANGE | property_flags::ATOMIC,
            values: vec![min as u64, max as u64],
            enum_values: Vec::new(),
            blob_ids: Vec::new(),
            current_value: Some(DrmPropertyValue::SignedRange(initial)),
        }
    }

    /// Create an enum property
    pub fn new_enum(name: String, enum_values: Vec<DrmEnumValue>, initial_index: usize) -> Self {
        let values: Vec<u64> = enum_values.iter().map(|e| e.value).collect();
        let enum_len = enum_values.len();

        Self {
            id: DrmObjectId::new(),
            name,
            property_type: DrmPropertyType::Enum,
            flags: property_flags::ENUM | property_flags::ATOMIC,
            values,
            enum_values,
            blob_ids: Vec::new(),
            current_value: if initial_index < enum_len {
                Some(DrmPropertyValue::Enum(initial_index as u32))
            } else {
                None
            },
        }
    }

    /// Create a blob property
    pub fn new_blob(name: String, data: Vec<u8>) -> Self {
        Self {
            id: DrmObjectId::new(),
            name,
            property_type: DrmPropertyType::Blob,
            flags: property_flags::BLOB | property_flags::ATOMIC,
            values: Vec::new(),
            enum_values: Vec::new(),
            blob_ids: Vec::new(),
            current_value: Some(DrmPropertyValue::Blob(data)),
        }
    }

    /// Create a bitmask property
    pub fn new_bitmask(name: String, supported_bits: Vec<u64>, initial: u64) -> Self {
        Self {
            id: DrmObjectId::new(),
            name,
            property_type: DrmPropertyType::Bitmask,
            flags: property_flags::BITMASK | property_flags::ATOMIC,
            values: supported_bits,
            enum_values: Vec::new(),
            blob_ids: Vec::new(),
            current_value: Some(DrmPropertyValue::Bitmask(initial)),
        }
    }

    /// Create an object reference property
    pub fn new_object(name: String, object_type: DrmObjectType, initial: Option<DrmObjectId>) -> Self {
        Self {
            id: DrmObjectId::new(),
            name,
            property_type: DrmPropertyType::Object,
            flags: property_flags::ATOMIC,
            values: vec![object_type as u64],
            enum_values: Vec::new(),
            blob_ids: Vec::new(),
            current_value: initial.map(DrmPropertyValue::Object),
        }
    }

    /// Validate and set property value
    pub fn set_value(&mut self, value: DrmPropertyValue) -> Result<(), &'static str> {
        // Check if property is immutable
        if self.flags & property_flags::IMMUTABLE != 0 {
            return Err("Property is immutable");
        }

        // Validate value type matches property type
        match (&self.property_type, &value) {
            (DrmPropertyType::Range, DrmPropertyValue::Range(v)) => {
                if self.values.len() >= 2 {
                    let min = self.values[0];
                    let max = self.values[1];
                    if *v < min || *v > max {
                        return Err("Value out of range");
                    }
                }
            },
            (DrmPropertyType::SignedRange, DrmPropertyValue::SignedRange(v)) => {
                if self.values.len() >= 2 {
                    let min = self.values[0] as i64;
                    let max = self.values[1] as i64;
                    if *v < min || *v > max {
                        return Err("Value out of signed range");
                    }
                }
            },
            (DrmPropertyType::Enum, DrmPropertyValue::Enum(idx)) => {
                if *idx as usize >= self.enum_values.len() {
                    return Err("Invalid enum index");
                }
            },
            (DrmPropertyType::Bitmask, DrmPropertyValue::Bitmask(mask)) => {
                // Check if all set bits are supported
                let mut supported_mask = 0u64;
                for &bit in &self.values {
                    supported_mask |= 1u64 << bit;
                }
                if *mask & !supported_mask != 0 {
                    return Err("Unsupported bits in mask");
                }
            },
            (DrmPropertyType::Blob, DrmPropertyValue::Blob(_)) => {
                // Blob values are always valid
            },
            (DrmPropertyType::Object, DrmPropertyValue::Object(_)) => {
                // Object references would need additional validation
            },
            _ => {
                return Err("Type mismatch");
            }
        }

        self.current_value = Some(value);
        Ok(())
    }

    /// Get current value
    pub fn get_value(&self) -> Option<&DrmPropertyValue> {
        self.current_value.as_ref()
    }
}

impl DrmObject for DrmProperty {
    fn id(&self) -> DrmObjectId { self.id }
    fn object_type(&self) -> DrmObjectType { DrmObjectType::Property }
}

/// Object property attachment
pub struct ObjectProperty {
    pub object_id: DrmObjectId,
    pub property_id: DrmObjectId,
    pub value: DrmPropertyValue,
}

/// Property manager for DRM objects
pub struct PropertyManager {
    properties: BTreeMap<DrmObjectId, DrmProperty>,
    object_properties: BTreeMap<DrmObjectId, Vec<DrmObjectId>>, // Object -> Properties
}

impl PropertyManager {
    pub fn new() -> Self {
        Self {
            properties: BTreeMap::new(),
            object_properties: BTreeMap::new(),
        }
    }

    /// Add a property definition
    pub fn add_property(&mut self, property: DrmProperty) -> DrmObjectId {
        let id = property.id();
        self.properties.insert(id, property);
        id
    }

    /// Attach property to an object
    pub fn attach_property(&mut self, object_id: DrmObjectId, property_id: DrmObjectId) {
        let properties = self.object_properties.entry(object_id).or_insert_with(Vec::new);
        if !properties.contains(&property_id) {
            properties.push(property_id);
        }
    }

    /// Set property value for an object
    pub fn set_property(&mut self, object_id: DrmObjectId, property_id: DrmObjectId,
                       value: DrmPropertyValue) -> Result<(), &'static str> {
        // Check if property is attached to object
        if let Some(properties) = self.object_properties.get(&object_id) {
            if !properties.contains(&property_id) {
                return Err("Property not attached to object");
            }
        } else {
            return Err("Object has no properties");
        }

        // Set the value
        if let Some(property) = self.properties.get_mut(&property_id) {
            property.set_value(value)
        } else {
            Err("Property not found")
        }
    }

    /// Get property value for an object
    pub fn get_property(&self, property_id: DrmObjectId) -> Option<&DrmPropertyValue> {
        self.properties.get(&property_id)?.get_value()
    }

    /// Get all properties for an object
    pub fn get_object_properties(&self, object_id: DrmObjectId) -> Vec<DrmObjectId> {
        self.object_properties.get(&object_id).cloned().unwrap_or_default()
    }

    /// Get property definition
    pub fn get_property_def(&self, property_id: DrmObjectId) -> Option<&DrmProperty> {
        self.properties.get(&property_id)
    }
}

/// Standard DRM properties
pub struct StandardProperties;

impl StandardProperties {
    /// Create standard CRTC properties
    pub fn create_crtc_properties() -> Vec<DrmProperty> {
        vec![
            DrmProperty::new_object(
                "ACTIVE".to_string(),
                DrmObjectType::Crtc,
                None
            ),
            DrmProperty::new_object(
                "MODE_ID".to_string(),
                DrmObjectType::Mode,
                None
            ),
        ]
    }

    /// Create standard connector properties
    pub fn create_connector_properties() -> Vec<DrmProperty> {
        let dpms_values = vec![
            DrmEnumValue { value: 0, name: "On".to_string() },
            DrmEnumValue { value: 1, name: "Standby".to_string() },
            DrmEnumValue { value: 2, name: "Suspend".to_string() },
            DrmEnumValue { value: 3, name: "Off".to_string() },
        ];

        vec![
            DrmProperty::new_enum("DPMS".to_string(), dpms_values, 0),
            DrmProperty::new_object(
                "CRTC_ID".to_string(),
                DrmObjectType::Crtc,
                None
            ),
        ]
    }

    /// Create standard plane properties
    pub fn create_plane_properties() -> Vec<DrmProperty> {
        let plane_types = vec![
            DrmEnumValue { value: 0, name: "Overlay".to_string() },
            DrmEnumValue { value: 1, name: "Primary".to_string() },
            DrmEnumValue { value: 2, name: "Cursor".to_string() },
        ];

        vec![
            DrmProperty::new_enum("type".to_string(), plane_types, 1),
            DrmProperty::new_object(
                "FB_ID".to_string(),
                DrmObjectType::Mode, // Framebuffer
                None
            ),
            DrmProperty::new_object(
                "CRTC_ID".to_string(),
                DrmObjectType::Crtc,
                None
            ),
            DrmProperty::new_signed_range("CRTC_X".to_string(), -2048, 2048, 0),
            DrmProperty::new_signed_range("CRTC_Y".to_string(), -2048, 2048, 0),
            DrmProperty::new_range("CRTC_W".to_string(), 0, 4096, 0),
            DrmProperty::new_range("CRTC_H".to_string(), 0, 4096, 0),
            DrmProperty::new_range("SRC_X".to_string(), 0, 4096 << 16, 0),
            DrmProperty::new_range("SRC_Y".to_string(), 0, 4096 << 16, 0),
            DrmProperty::new_range("SRC_W".to_string(), 0, 4096 << 16, 0),
            DrmProperty::new_range("SRC_H".to_string(), 0, 4096 << 16, 0),
        ]
    }
}