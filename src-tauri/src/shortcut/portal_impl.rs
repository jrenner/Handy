//! XDG Desktop Portal global-shortcut implementation for Linux Wayland.

#[cfg(target_os = "linux")]
pub(super) use linux::{
    init_shortcuts, register_cancel_shortcut, register_shortcut, stop_shortcuts,
    unregister_cancel_shortcut, unregister_shortcut, validate_shortcut,
};

#[cfg(target_os = "linux")]
mod linux {
    use std::sync::Mutex;

    use ashpd::desktop::global_shortcuts::{BindShortcutsOptions, GlobalShortcuts, NewShortcut};
    use ashpd::desktop::CreateSessionOptions;
    use futures_util::StreamExt;
    use log::{debug, error, info, warn};
    use tauri::{AppHandle, Manager};
    use tokio::sync::oneshot;

    use crate::settings::{self, KeyboardImplementation, ShortcutBinding};

    use super::super::handler::handle_shortcut_event;

    const PORTAL_HOTKEY_LABEL: &str = "xdg-desktop-portal";
    const PORTAL_SHORTCUT_IDS: [&str; 2] = ["transcribe", "transcribe_with_post_process"];

    struct PortalShortcutState {
        shutdown: Mutex<Option<oneshot::Sender<()>>>,
    }

    impl Default for PortalShortcutState {
        fn default() -> Self {
            Self {
                shutdown: Mutex::new(None),
            }
        }
    }

    impl PortalShortcutState {
        fn stop(&self) {
            if let Ok(mut shutdown) = self.shutdown.lock() {
                if let Some(sender) = shutdown.take() {
                    let _ = sender.send(());
                }
            }
        }

        fn replace_shutdown(&self, sender: oneshot::Sender<()>) -> Result<(), String> {
            let mut shutdown = self
                .shutdown
                .lock()
                .map_err(|_| "Failed to lock portal shortcut state")?;
            *shutdown = Some(sender);
            Ok(())
        }
    }

    pub(crate) async fn init_shortcuts(app: &AppHandle) -> Result<(), String> {
        stop_shortcuts(app);

        let portal = GlobalShortcuts::new()
            .await
            .map_err(|e| format!("XDG GlobalShortcuts portal is unavailable: {e}"))?;
        if portal.version() < 2 {
            return Err(format!(
                "XDG GlobalShortcuts portal version {} does not support release events",
                portal.version()
            ));
        }

        let mut activated = portal.receive_activated().await.map_err(|e| {
            format!("Failed to subscribe to portal shortcut activation events: {e}")
        })?;
        let mut deactivated = portal.receive_deactivated().await.map_err(|e| {
            format!("Failed to subscribe to portal shortcut deactivation events: {e}")
        })?;

        let session = portal
            .create_session(CreateSessionOptions::default())
            .await
            .map_err(|e| format!("Failed to create XDG GlobalShortcuts session: {e}"))?;

        let shortcuts = portal_shortcuts(app);
        if shortcuts.is_empty() {
            return Ok(());
        }

        let request = portal
            .bind_shortcuts(&session, &shortcuts, None, BindShortcutsOptions::default())
            .await
            .map_err(|e| format!("Failed to bind XDG GlobalShortcuts: {e}"))?;
        request
            .response()
            .map_err(|e| format!("XDG GlobalShortcuts binding was not accepted: {e}"))?;

        let (shutdown_sender, mut shutdown_receiver) = oneshot::channel();
        let state = portal_state(app);
        state.replace_shutdown(shutdown_sender)?;

        let app = app.clone();
        tauri::async_runtime::spawn(async move {
            info!("XDG GlobalShortcuts portal initialized");

            loop {
                tokio::select! {
                    _ = &mut shutdown_receiver => break,
                    event = activated.next() => match event {
                        Some(event) => handle_portal_event(&app, event.shortcut_id(), true),
                        None => break,
                    },
                    event = deactivated.next() => match event {
                        Some(event) => handle_portal_event(&app, event.shortcut_id(), false),
                        None => break,
                    },
                }
            }

            if let Err(e) = session.close().await {
                warn!("Failed to close XDG GlobalShortcuts session: {e}");
            }
        });

        Ok(())
    }

    pub(crate) fn register_shortcut(
        app: &AppHandle,
        binding: ShortcutBinding,
    ) -> Result<(), String> {
        if is_portal_shortcut(&binding.id) {
            restart_shortcuts(app);
        }
        Ok(())
    }

    pub(crate) fn unregister_shortcut(
        app: &AppHandle,
        binding: ShortcutBinding,
    ) -> Result<(), String> {
        if is_portal_shortcut(&binding.id) {
            restart_shortcuts(app);
        }
        Ok(())
    }

    pub(crate) fn validate_shortcut(raw: &str) -> Result<(), String> {
        if raw.trim().is_empty() {
            Err("Shortcut cannot be empty".into())
        } else {
            Ok(())
        }
    }

    pub(crate) fn register_cancel_shortcut(_app: &AppHandle) {}

    pub(crate) fn unregister_cancel_shortcut(_app: &AppHandle) {}

    fn restart_shortcuts(app: &AppHandle) {
        let app = app.clone();
        tauri::async_runtime::spawn(async move {
            if let Err(e) = init_shortcuts(&app).await {
                error!("Failed to restart XDG GlobalShortcuts portal: {e}");
            }
        });
    }

    pub(crate) fn stop_shortcuts(app: &AppHandle) {
        if let Some(state) = app.try_state::<PortalShortcutState>() {
            state.stop();
        }
    }

    fn portal_state(app: &AppHandle) -> tauri::State<'_, PortalShortcutState> {
        if app.try_state::<PortalShortcutState>().is_none() {
            app.manage(PortalShortcutState::default());
        }
        app.state::<PortalShortcutState>()
    }

    fn portal_shortcuts(app: &AppHandle) -> Vec<NewShortcut> {
        let settings = settings::get_settings(app);
        let defaults = settings::get_default_settings().bindings;

        PORTAL_SHORTCUT_IDS
            .iter()
            .filter_map(|id| {
                if *id == "transcribe_with_post_process" && !settings.post_process_enabled {
                    return None;
                }

                let binding = settings.bindings.get(*id).or_else(|| defaults.get(*id))?;
                Some(NewShortcut::new(binding.id.clone(), binding.name.clone()))
            })
            .collect()
    }

    fn handle_portal_event(app: &AppHandle, shortcut_id: &str, is_pressed: bool) {
        if settings::get_settings(app).keyboard_implementation != KeyboardImplementation::Portal {
            debug!("Ignoring stale portal shortcut event for '{shortcut_id}'");
            return;
        }

        handle_shortcut_event(app, shortcut_id, PORTAL_HOTKEY_LABEL, is_pressed);
    }

    fn is_portal_shortcut(id: &str) -> bool {
        PORTAL_SHORTCUT_IDS.contains(&id)
    }
}

#[cfg(not(target_os = "linux"))]
pub(super) async fn init_shortcuts(_app: &tauri::AppHandle) -> Result<(), String> {
    Err("XDG Desktop Portal shortcuts are only supported on Linux".into())
}

#[cfg(not(target_os = "linux"))]
pub(super) fn register_shortcut(
    _app: &tauri::AppHandle,
    _binding: crate::settings::ShortcutBinding,
) -> Result<(), String> {
    Ok(())
}

#[cfg(not(target_os = "linux"))]
pub(super) fn unregister_shortcut(
    _app: &tauri::AppHandle,
    _binding: crate::settings::ShortcutBinding,
) -> Result<(), String> {
    Ok(())
}

#[cfg(not(target_os = "linux"))]
pub(super) fn validate_shortcut(raw: &str) -> Result<(), String> {
    if raw.trim().is_empty() {
        Err("Shortcut cannot be empty".into())
    } else {
        Ok(())
    }
}

#[cfg(not(target_os = "linux"))]
pub(super) fn register_cancel_shortcut(_app: &tauri::AppHandle) {}

#[cfg(not(target_os = "linux"))]
pub(super) fn unregister_cancel_shortcut(_app: &tauri::AppHandle) {}

#[cfg(not(target_os = "linux"))]
pub(super) fn stop_shortcuts(_app: &tauri::AppHandle) {}
