//! Console-specific DRM properties and configuration
//!
//! This module extends the DRM property system with console-specific properties
//! for controlling text rendering, cursor behavior, and visual effects.

use alloc::{vec::Vec, vec, string::{String, ToString}, format};
use super::drm::*;

/// Console-specific DRM properties
pub struct ConsoleProperties {
    // Text rendering properties
    pub font_size: DrmObjectId,
    pub font_weight: DrmObjectId,
    pub text_color: DrmObjectId,
    pub background_color: DrmObjectId,
    pub anti_aliasing: DrmObjectId,

    // Cursor properties
    pub cursor_style: DrmObjectId,
    pub cursor_blink: DrmObjectId,
    pub cursor_color: DrmObjectId,

    // Console behavior
    pub scroll_speed: DrmObjectId,
    pub auto_wrap: DrmObjectId,
    pub bell_enabled: DrmObjectId,

    // Visual effects
    pub transparency: DrmObjectId,
    pub blur_background: DrmObjectId,
    pub shadow_text: DrmObjectId,
}

impl ConsoleProperties {
    /// Create console-specific properties
    pub fn new() -> Self {
        let mut prop_manager = PropertyManager::new();

        // Font properties
        let font_size = prop_manager.add_property(
            DrmProperty::new_range("CONSOLE_FONT_SIZE".to_string(), 8, 48, 16)
        );

        let font_weight = prop_manager.add_property(
            DrmProperty::new_enum(
                "CONSOLE_FONT_WEIGHT".to_string(),
                vec![
                    DrmEnumValue { value: 0, name: "Normal".to_string() },
                    DrmEnumValue { value: 1, name: "Bold".to_string() },
                    DrmEnumValue { value: 2, name: "Light".to_string() },
                ],
                0
            )
        );

        let text_color = prop_manager.add_property(
            DrmProperty::new_range("CONSOLE_TEXT_COLOR".to_string(), 0, 0xFFFFFF, 0xFFFFFF)
        );

        let background_color = prop_manager.add_property(
            DrmProperty::new_range("CONSOLE_BG_COLOR".to_string(), 0, 0xFFFFFF, 0x000000)
        );

        let anti_aliasing = prop_manager.add_property(
            DrmProperty::new_enum(
                "CONSOLE_ANTI_ALIASING".to_string(),
                vec![
                    DrmEnumValue { value: 0, name: "Off".to_string() },
                    DrmEnumValue { value: 1, name: "Gray".to_string() },
                    DrmEnumValue { value: 2, name: "Subpixel".to_string() },
                ],
                0
            )
        );

        // Cursor properties
        let cursor_style = prop_manager.add_property(
            DrmProperty::new_enum(
                "CONSOLE_CURSOR_STYLE".to_string(),
                vec![
                    DrmEnumValue { value: 0, name: "Block".to_string() },
                    DrmEnumValue { value: 1, name: "Underline".to_string() },
                    DrmEnumValue { value: 2, name: "Bar".to_string() },
                    DrmEnumValue { value: 3, name: "None".to_string() },
                ],
                0
            )
        );

        let cursor_blink = prop_manager.add_property(
            DrmProperty::new_range("CONSOLE_CURSOR_BLINK".to_string(), 0, 1, 1)
        );

        let cursor_color = prop_manager.add_property(
            DrmProperty::new_range("CONSOLE_CURSOR_COLOR".to_string(), 0, 0xFFFFFF, 0xFFFFFF)
        );

        // Behavior properties
        let scroll_speed = prop_manager.add_property(
            DrmProperty::new_range("CONSOLE_SCROLL_SPEED".to_string(), 1, 20, 1)
        );

        let auto_wrap = prop_manager.add_property(
            DrmProperty::new_range("CONSOLE_AUTO_WRAP".to_string(), 0, 1, 1)
        );

        let bell_enabled = prop_manager.add_property(
            DrmProperty::new_range("CONSOLE_BELL_ENABLED".to_string(), 0, 1, 1)
        );

        // Visual effects
        let transparency = prop_manager.add_property(
            DrmProperty::new_range("CONSOLE_TRANSPARENCY".to_string(), 0, 255, 255)
        );

        let blur_background = prop_manager.add_property(
            DrmProperty::new_range("CONSOLE_BLUR_BG".to_string(), 0, 1, 0)
        );

        let shadow_text = prop_manager.add_property(
            DrmProperty::new_range("CONSOLE_TEXT_SHADOW".to_string(), 0, 1, 0)
        );

        Self {
            font_size,
            font_weight,
            text_color,
            background_color,
            anti_aliasing,
            cursor_style,
            cursor_blink,
            cursor_color,
            scroll_speed,
            auto_wrap,
            bell_enabled,
            transparency,
            blur_background,
            shadow_text,
        }
    }

    /// Apply property changes to console
    pub fn apply_properties(&self, console: &mut super::drm_console::DrmConsole,
                           prop_manager: &PropertyManager) -> Result<(), super::DriverError> {
        // Apply font size changes
        if let Some(DrmPropertyValue::Range(size)) = prop_manager.get_property(self.font_size) {
            console.set_font_size(*size as usize);
        }

        // Apply color changes
        if let Some(DrmPropertyValue::Range(color)) = prop_manager.get_property(self.text_color) {
            console.set_text_color(*color as u32);
        }

        if let Some(DrmPropertyValue::Range(color)) = prop_manager.get_property(self.background_color) {
            console.set_background_color(*color as u32);
        }

        // Apply cursor properties
        if let Some(DrmPropertyValue::Enum(style)) = prop_manager.get_property(self.cursor_style) {
            console.set_cursor_style(*style);
        }

        if let Some(DrmPropertyValue::Range(blink)) = prop_manager.get_property(self.cursor_blink) {
            console.set_cursor_blink(*blink != 0);
        }

        // Apply behavior properties
        if let Some(DrmPropertyValue::Range(wrap)) = prop_manager.get_property(self.auto_wrap) {
            console.set_auto_wrap(*wrap != 0);
        }

        Ok(())
    }

    /// Get property configuration as string
    pub fn get_property_info(&self, prop_manager: &PropertyManager) -> String {
        let mut info = String::new();

        info.push_str("Console Properties:\n");

        // Font properties
        if let Some(DrmPropertyValue::Range(size)) = prop_manager.get_property(self.font_size) {
            info.push_str(&format!("  Font Size: {}\n", size));
        }

        if let Some(DrmPropertyValue::Range(color)) = prop_manager.get_property(self.text_color) {
            info.push_str(&format!("  Text Color: #{:06X}\n", color));
        }

        if let Some(DrmPropertyValue::Range(color)) = prop_manager.get_property(self.background_color) {
            info.push_str(&format!("  Background: #{:06X}\n", color));
        }

        // Cursor properties
        if let Some(DrmPropertyValue::Enum(style)) = prop_manager.get_property(self.cursor_style) {
            let style_name = match style {
                0 => "Block",
                1 => "Underline",
                2 => "Bar",
                3 => "None",
                _ => "Unknown",
            };
            info.push_str(&format!("  Cursor: {}\n", style_name));
        }

        info
    }
}

/// Console theme presets
pub struct ConsoleThemes;

impl ConsoleThemes {
    /// Apply a predefined theme
    pub fn apply_theme(theme_name: &str, prop_manager: &mut PropertyManager,
                      console_props: &ConsoleProperties) -> Result<(), &'static str> {
        match theme_name {
            "default" => Self::apply_default_theme(prop_manager, console_props),
            "dark" => Self::apply_dark_theme(prop_manager, console_props),
            "light" => Self::apply_light_theme(prop_manager, console_props),
            "matrix" => Self::apply_matrix_theme(prop_manager, console_props),
            "retro" => Self::apply_retro_theme(prop_manager, console_props),
            "minimal" => Self::apply_minimal_theme(prop_manager, console_props),
            _ => Err("Unknown theme"),
        }
    }

    fn apply_default_theme(prop_manager: &mut PropertyManager,
                          props: &ConsoleProperties) -> Result<(), &'static str> {
        let console_obj = DrmObjectId(0); // Console object ID
        prop_manager.set_property(console_obj, props.text_color, DrmPropertyValue::Range(0xFFFFFF))?;
        prop_manager.set_property(console_obj, props.background_color, DrmPropertyValue::Range(0x000000))?;
        prop_manager.set_property(console_obj, props.font_size, DrmPropertyValue::Range(16))?;
        prop_manager.set_property(console_obj, props.cursor_style, DrmPropertyValue::Enum(0))?; // Block
        prop_manager.set_property(console_obj, props.cursor_blink, DrmPropertyValue::Range(1))?;
        Ok(())
    }

    fn apply_dark_theme(prop_manager: &mut PropertyManager,
                       props: &ConsoleProperties) -> Result<(), &'static str> {
        let console_obj = DrmObjectId(0);
        prop_manager.set_property(console_obj, props.text_color, DrmPropertyValue::Range(0xE0E0E0))?;
        prop_manager.set_property(console_obj, props.background_color, DrmPropertyValue::Range(0x1A1A1A))?;
        prop_manager.set_property(console_obj, props.font_size, DrmPropertyValue::Range(14))?;
        prop_manager.set_property(console_obj, props.cursor_style, DrmPropertyValue::Enum(2))?; // Bar
        prop_manager.set_property(console_obj, props.transparency, DrmPropertyValue::Range(240))?;
        Ok(())
    }

    fn apply_light_theme(prop_manager: &mut PropertyManager,
                        props: &ConsoleProperties) -> Result<(), &'static str> {
        let console_obj = DrmObjectId(0);
        prop_manager.set_property(console_obj, props.text_color, DrmPropertyValue::Range(0x2D2D30))?;
        prop_manager.set_property(console_obj, props.background_color, DrmPropertyValue::Range(0xFFFFFF))?;
        prop_manager.set_property(console_obj, props.font_size, DrmPropertyValue::Range(14))?;
        prop_manager.set_property(console_obj, props.cursor_style, DrmPropertyValue::Enum(1))?; // Underline
        prop_manager.set_property(console_obj, props.shadow_text, DrmPropertyValue::Range(1))?;
        Ok(())
    }

    fn apply_matrix_theme(prop_manager: &mut PropertyManager,
                         props: &ConsoleProperties) -> Result<(), &'static str> {
        let console_obj = DrmObjectId(0);
        prop_manager.set_property(console_obj, props.text_color, DrmPropertyValue::Range(0x00FF41))?;
        prop_manager.set_property(console_obj, props.background_color, DrmPropertyValue::Range(0x000000))?;
        prop_manager.set_property(console_obj, props.font_size, DrmPropertyValue::Range(12))?;
        prop_manager.set_property(console_obj, props.cursor_style, DrmPropertyValue::Enum(0))?; // Block
        prop_manager.set_property(console_obj, props.cursor_color, DrmPropertyValue::Range(0x00FF41))?;
        prop_manager.set_property(console_obj, props.font_weight, DrmPropertyValue::Enum(1))?; // Bold
        Ok(())
    }

    fn apply_retro_theme(prop_manager: &mut PropertyManager,
                        props: &ConsoleProperties) -> Result<(), &'static str> {
        let console_obj = DrmObjectId(0);
        prop_manager.set_property(console_obj, props.text_color, DrmPropertyValue::Range(0xFFB000))?;
        prop_manager.set_property(console_obj, props.background_color, DrmPropertyValue::Range(0x1A0F00))?;
        prop_manager.set_property(console_obj, props.font_size, DrmPropertyValue::Range(20))?;
        prop_manager.set_property(console_obj, props.cursor_style, DrmPropertyValue::Enum(0))?; // Block
        prop_manager.set_property(console_obj, props.cursor_color, DrmPropertyValue::Range(0xFFB000))?;
        Ok(())
    }

    fn apply_minimal_theme(prop_manager: &mut PropertyManager,
                          props: &ConsoleProperties) -> Result<(), &'static str> {
        let console_obj = DrmObjectId(0);
        prop_manager.set_property(console_obj, props.text_color, DrmPropertyValue::Range(0x808080))?;
        prop_manager.set_property(console_obj, props.background_color, DrmPropertyValue::Range(0x000000))?;
        prop_manager.set_property(console_obj, props.font_size, DrmPropertyValue::Range(12))?;
        prop_manager.set_property(console_obj, props.cursor_style, DrmPropertyValue::Enum(3))?; // None
        prop_manager.set_property(console_obj, props.font_weight, DrmPropertyValue::Enum(2))?; // Light
        Ok(())
    }

    /// Get available themes
    pub fn list_themes() -> Vec<&'static str> {
        vec!["default", "dark", "light", "matrix", "retro", "minimal"]
    }
}