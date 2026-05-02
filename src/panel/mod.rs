#[cfg(target_os = "windows")]
use crate::config::AppConfig;

#[cfg(not(target_os = "windows"))]
mod native {
    use crate::autostart;
    use crate::config::{config_file_path, AppConfig, PushToTalkHotkey};
    use eframe::egui;
    use std::sync::atomic::{AtomicBool, Ordering};

    static SETTINGS_WINDOW_OPEN: AtomicBool = AtomicBool::new(false);

    pub fn open_settings_window(initial_config: AppConfig) {
        if SETTINGS_WINDOW_OPEN.swap(true, Ordering::SeqCst) {
            log::info!("Settings window already open");
            return;
        }

        std::thread::spawn(move || {
            run_settings_native(initial_config);
            SETTINGS_WINDOW_OPEN.store(false, Ordering::SeqCst);
        });
    }

    fn run_settings_native(initial_config: AppConfig) {
        let native_options = eframe::NativeOptions {
            viewport: egui::ViewportBuilder::default()
                .with_inner_size([520.0, 300.0])
                .with_min_inner_size([480.0, 260.0])
                .with_title("Clicky Settings"),
            ..Default::default()
        };

        let app = SettingsApp::new(initial_config);
        let result = eframe::run_native(
            "Clicky Settings",
            native_options,
            Box::new(|_cc| Ok(Box::new(app))),
        );

        if let Err(error) = result {
            log::error!("Failed to open settings window: {}", error);
        }
    }

    struct SettingsApp {
        push_to_talk_hotkey: PushToTalkHotkey,
        autostart_enabled: bool,
        status_message: Option<String>,
        status_is_error: bool,
    }

    impl SettingsApp {
        fn new(config: AppConfig) -> Self {
            Self {
                push_to_talk_hotkey: config.push_to_talk_hotkey,
                autostart_enabled: config.autostart_enabled,
                status_message: None,
                status_is_error: false,
            }
        }

        fn save(&mut self) -> Result<(), String> {
            let config = AppConfig {
                push_to_talk_hotkey: self.push_to_talk_hotkey,
                autostart_enabled: self.autostart_enabled,
            };

            let config_path =
                config_file_path().ok_or_else(|| "Config directory unavailable".to_string())?;
            if let Some(parent_dir) = config_path.parent() {
                std::fs::create_dir_all(parent_dir)
                    .map_err(|e| format!("Failed to create config directory: {e}"))?;
            }
            let serialized = toml::to_string_pretty(&config)
                .map_err(|e| format!("Failed to serialize config: {e}"))?;
            std::fs::write(&config_path, serialized)
                .map_err(|e| format!("Failed to write config: {e}"))?;

            if autostart::is_supported() {
                autostart::set_enabled(config.autostart_enabled)?;
            }

            Ok(())
        }
    }

    impl eframe::App for SettingsApp {
        fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
            egui::CentralPanel::default().show(ctx, |ui| {
                ui.add_space(8.0);
                ui.heading("Clicky Settings");
                ui.add_space(6.0);
                ui.label("API keys are configured via the backend .env file.");
                if let Some(config_path) = config_file_path() {
                    ui.small(format!("Config file: {}", config_path.display()));
                }
                ui.add_space(16.0);

                ui.label("Push-to-talk hotkey");
                egui::ComboBox::from_id_salt("push_to_talk_hotkey")
                    .selected_text(self.push_to_talk_hotkey.display_name())
                    .show_ui(ui, |ui| {
                        ui.selectable_value(
                            &mut self.push_to_talk_hotkey,
                            PushToTalkHotkey::CtrlSpace,
                            PushToTalkHotkey::CtrlSpace.display_name(),
                        );
                        ui.selectable_value(
                            &mut self.push_to_talk_hotkey,
                            PushToTalkHotkey::CtrlGrave,
                            PushToTalkHotkey::CtrlGrave.display_name(),
                        );
                    });
                ui.small("Saved settings are picked up automatically within about a second.");

                if autostart::is_supported() {
                    ui.add_space(10.0);
                    ui.checkbox(
                        &mut self.autostart_enabled,
                        "Start Clicky automatically on login",
                    );
                }

                ui.add_space(18.0);
                if let Some(message) = &self.status_message {
                    let color = if self.status_is_error {
                        egui::Color32::from_rgb(220, 92, 92)
                    } else {
                        egui::Color32::from_rgb(90, 180, 120)
                    };
                    ui.colored_label(color, message);
                    ui.add_space(10.0);
                }

                ui.horizontal(|ui| {
                    if ui.button("Save").clicked() {
                        match self.save() {
                            Ok(()) => {
                                self.status_message = Some(
                                    "Saved. Settings apply automatically within about a second."
                                        .to_string(),
                                );
                                self.status_is_error = false;
                            }
                            Err(error) => {
                                self.status_message = Some(error);
                                self.status_is_error = true;
                            }
                        }
                    }

                    if ui.button("Save and Close").clicked() {
                        match self.save() {
                            Ok(()) => {
                                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                            }
                            Err(error) => {
                                self.status_message = Some(error);
                                self.status_is_error = true;
                            }
                        }
                    }

                    if ui.button("Close").clicked() {
                        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                    }
                });
            });
        }
    }
}

#[cfg(not(target_os = "windows"))]
pub use native::open_settings_window;

#[cfg(target_os = "windows")]
pub fn open_settings_window(_initial_config: AppConfig) {
    use crate::config::config_file_path;

    let config_path = config_file_path()
        .map(|path| path.display().to_string())
        .unwrap_or_else(|| "%APPDATA%\\clicky-desktop\\config.toml".to_string());

    // On Windows the egui settings panel is not available in this build.
    // Log the config file location so the user can find it — no dialog is shown
    // because there are no API keys to configure in the frontend.
    log::info!(
        "Settings: edit the config file to change hotkey or autostart: {}",
        config_path
    );
}
