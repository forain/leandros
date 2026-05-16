//! Console commands for DRM and display management
//!
//! This module provides command-line utilities for managing the DRM console,
//! switching display modes, and controlling virtual terminals.

use alloc::{string::{String, ToString}, vec::Vec, format};
use super::drm_console::*;

/// Console command handler
pub struct ConsoleCommands;

impl ConsoleCommands {
    /// Handle console command
    pub fn handle_command(command: &str) -> Result<String, &'static str> {
        let parts: Vec<&str> = command.trim().split_whitespace().collect();
        if parts.is_empty() {
            return Err("Empty command");
        }

        match parts[0] {
            "drm" => Self::handle_drm_command(&parts[1..]),
            "vt" => Self::handle_vt_command(&parts[1..]),
            "mode" => Self::handle_mode_command(&parts[1..]),
            "help" => Ok(Self::help_text()),
            _ => Err("Unknown command"),
        }
    }

    /// Handle DRM-related commands
    fn handle_drm_command(args: &[&str]) -> Result<String, &'static str> {
        if args.is_empty() {
            return Ok("DRM commands: status, init, reset".to_string());
        }

        match args[0] {
            "status" => {
                // Get DRM status
                let console = get_drm_console().lock();
                if console.is_drm_enabled() {
                    let mode_info = if let Some((w, h, r)) = console.get_current_mode() {
                        format!("{}x{}@{}Hz", w, h, r)
                    } else {
                        "No mode set".to_string()
                    };
                    Ok(format!("DRM: Enabled, Mode: {}", mode_info))
                } else {
                    Ok("DRM: Disabled (using legacy framebuffer)".to_string())
                }
            },

            "init" => {
                // Reinitialize DRM
                match init_drm_console() {
                    Ok(()) => Ok("DRM console reinitialized".to_string()),
                    Err(_) => Err("Failed to initialize DRM console"),
                }
            },

            "reset" => {
                // Reset to safe mode
                match drm_console_set_mode(1024, 768, 60) {
                    Ok(()) => Ok("Reset to 1024x768@60Hz".to_string()),
                    Err(_) => Err("Failed to reset mode"),
                }
            },

            _ => Err("Unknown DRM command"),
        }
    }

    /// Handle virtual terminal commands
    fn handle_vt_command(args: &[&str]) -> Result<String, &'static str> {
        if args.is_empty() {
            return Ok("VT commands: switch <id>, list, current".to_string());
        }

        match args[0] {
            "switch" => {
                if args.len() < 2 {
                    return Err("Usage: vt switch <id>");
                }

                let vt_id: usize = args[1].parse().map_err(|_| "Invalid VT ID")?;
                match drm_console_switch_vt(vt_id) {
                    Ok(()) => Ok(format!("Switched to VT {}", vt_id)),
                    Err(_) => Err("Failed to switch VT"),
                }
            },

            "list" => {
                Ok("Available VTs: 0-7".to_string())
            },

            "current" => {
                let console = get_drm_console().lock();
                Ok(format!("Current VT: {}", console.get_active_vt()))
            },

            _ => Err("Unknown VT command"),
        }
    }

    /// Handle display mode commands
    fn handle_mode_command(args: &[&str]) -> Result<String, &'static str> {
        if args.is_empty() {
            return Ok("Mode commands: set <width>x<height>[@refresh], list, current".to_string());
        }

        match args[0] {
            "set" => {
                if args.len() < 2 {
                    return Err("Usage: mode set <width>x<height>[@refresh]");
                }

                let mode_str = args[1];
                let (resolution, refresh) = if let Some(at_pos) = mode_str.find('@') {
                    let (res_part, refresh_part) = mode_str.split_at(at_pos);
                    let refresh: u32 = refresh_part[1..].parse().map_err(|_| "Invalid refresh rate")?;
                    (res_part, refresh)
                } else {
                    (mode_str, 60) // Default 60Hz
                };

                let x_pos = resolution.find('x').ok_or("Invalid resolution format")?;
                let (width_str, height_str) = resolution.split_at(x_pos);
                let height_str = &height_str[1..]; // Remove 'x'

                let width: u32 = width_str.parse().map_err(|_| "Invalid width")?;
                let height: u32 = height_str.parse().map_err(|_| "Invalid height")?;

                match drm_console_set_mode(width, height, refresh) {
                    Ok(()) => Ok(format!("Set mode to {}x{}@{}Hz", width, height, refresh)),
                    Err(_) => Err("Failed to set mode"),
                }
            },

            "list" => {
                let console = get_drm_console().lock();
                let modes = console.get_available_modes();
                drop(console);

                let mut result = "Available modes:\n".to_string();
                for (width, height, refresh) in modes {
                    result.push_str(&format!("  {}x{}@{}Hz\n", width, height, refresh));
                }
                Ok(result)
            },

            "current" => {
                let console = get_drm_console().lock();
                if let Some((width, height, refresh)) = console.get_current_mode() {
                    Ok(format!("Current mode: {}x{}@{}Hz", width, height, refresh))
                } else {
                    Ok("No mode set".to_string())
                }
            },

            "auto" => {
                // Auto-detect best mode
                let modes = {
                    let console = get_drm_console().lock();
                    console.get_available_modes()
                };

                if let Some((width, height, refresh)) = modes.first() {
                    match drm_console_set_mode(*width, *height, *refresh) {
                        Ok(()) => Ok(format!("Auto-set mode to {}x{}@{}Hz", width, height, refresh)),
                        Err(_) => Err("Failed to auto-set mode"),
                    }
                } else {
                    Err("No modes available")
                }
            },

            _ => Err("Unknown mode command"),
        }
    }

    /// Get help text
    fn help_text() -> String {
        "DRM Console Commands:\n\
         drm status       - Show DRM status\n\
         drm init         - Reinitialize DRM\n\
         drm reset        - Reset to safe mode\n\
         vt switch <id>   - Switch virtual terminal (0-7)\n\
         vt list          - List available VTs\n\
         vt current       - Show current VT\n\
         mode set WxH[@R] - Set display mode (e.g., 1920x1080@60)\n\
         mode list        - List available modes\n\
         mode current     - Show current mode\n\
         mode auto        - Auto-select best mode\n\
         help             - Show this help\n".to_string()
    }
}

/// Parse and execute console command
pub fn execute_console_command(command_line: &str) -> String {
    match ConsoleCommands::handle_command(command_line) {
        Ok(result) => result,
        Err(error) => format!("Error: {}", error),
    }
}

/// Handle function key combinations (for VT switching)
pub fn handle_function_key(key_code: u8) -> Result<String, &'static str> {
    match key_code {
        // F1-F8 for VT switching
        1..=8 => {
            let vt_id = (key_code - 1) as usize;
            match drm_console_switch_vt(vt_id) {
                Ok(()) => Ok(format!("Switched to VT {}", vt_id)),
                Err(_) => Err("Failed to switch VT"),
            }
        },

        // F9 for mode cycling
        9 => {
            let modes = {
                let console = get_drm_console().lock();
                console.get_available_modes()
            };

            if let Some((width, height, refresh)) = modes.get(1) {
                match drm_console_set_mode(*width, *height, *refresh) {
                    Ok(()) => Ok(format!("Switched to {}x{}@{}Hz", width, height, refresh)),
                    Err(_) => Err("Failed to switch mode"),
                }
            } else {
                Err("No alternate modes available")
            }
        },

        // F10 for DRM status
        10 => {
            let console = get_drm_console().lock();
            if console.is_drm_enabled() {
                let mode_info = if let Some((w, h, r)) = console.get_current_mode() {
                    format!("{}x{}@{}Hz", w, h, r)
                } else {
                    "No mode set".to_string()
                };
                Ok(format!("DRM: ON, VT: {}, Mode: {}", console.get_active_vt(), mode_info))
            } else {
                Ok("DRM: OFF (Legacy mode)".to_string())
            }
        },

        _ => Err("Unsupported function key"),
    }
}