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

    /// App id used to identify Handy to the desktop portal. Must match an
    /// installed `<app_id>.desktop` file (see `ensure_desktop_entry`).
    const PORTAL_APP_ID: &str = "com.pais.handy";

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

        // GNOME's GlobalShortcuts backend rejects bind requests from
        // applications it cannot identify (logging "invalid app_id"). A
        // non-sandboxed app must announce its app id — matching an installed
        // `.desktop` file — to the portal over the same D-Bus connection that
        // ashpd uses for its portal calls. ashpd shares a single process-wide
        // session connection, so registering here applies to bind_shortcuts
        // below. Best-effort: portals that don't need it simply ignore it.
        ensure_desktop_entry();
        register_host_app_id().await;

        let portal = GlobalShortcuts::new()
            .await
            .map_err(|e| format!("XDG GlobalShortcuts portal is unavailable: {e}"))?;
        // Version 1 of the GlobalShortcuts interface already emits the
        // `Activated`/`Deactivated` signals we rely on for push-to-talk
        // (press → Activated, release → Deactivated). GNOME ships version 1,
        // so only reject a hypothetical version 0.
        let version = portal.version();
        info!("XDG GlobalShortcuts portal version {version}");
        if version < 1 {
            return Err(format!(
                "XDG GlobalShortcuts portal version {version} is too old (need >= 1)"
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

        let shortcuts = portal_shortcuts(app)?;
        if shortcuts.is_empty() {
            return Ok(());
        }

        let request = portal
            .bind_shortcuts(&session, &shortcuts, None, BindShortcutsOptions::default())
            .await
            .map_err(|e| format!("Failed to bind XDG GlobalShortcuts: {e}"))?;
        let response = request
            .response()
            .map_err(|e| format!("XDG GlobalShortcuts binding was not accepted: {e}"))?;
        for shortcut in response.shortcuts() {
            info!(
                "XDG GlobalShortcuts bound '{}' as '{}'",
                shortcut.id(),
                shortcut.trigger_description()
            );
        }

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
        shortcut_to_xdg_trigger(raw).map(|_| ())
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

    /// Announce our app id to the desktop portal so backends (notably GNOME)
    /// can identify Handy and accept global-shortcut binds.
    async fn register_host_app_id() {
        match ashpd::AppID::try_from(PORTAL_APP_ID) {
            Ok(app_id) => match ashpd::register_host_app(app_id).await {
                Ok(()) => info!("Registered app id '{PORTAL_APP_ID}' with the desktop portal"),
                Err(e) => {
                    warn!("Could not register app id '{PORTAL_APP_ID}' with the portal: {e}")
                }
            },
            Err(e) => warn!("Portal app id '{PORTAL_APP_ID}' is invalid: {e}"),
        }
    }

    /// Ensure a `<PORTAL_APP_ID>.desktop` file exists in the user's
    /// applications directory. The portal looks up app info by this file when an
    /// app registers; without it GNOME reports "App info not found" and rejects
    /// the shortcut bind. We only create it when missing and never overwrite a
    /// file installed by a real package.
    fn ensure_desktop_entry() {
        let Some(data_home) = user_data_home() else {
            warn!("Could not determine data dir for portal desktop entry");
            return;
        };
        let apps_dir = data_home.join("applications");
        let desktop_path = apps_dir.join(format!("{PORTAL_APP_ID}.desktop"));
        if desktop_path.exists() {
            return;
        }

        let exec = std::env::current_exe()
            .ok()
            .and_then(|p| p.to_str().map(str::to_owned))
            .unwrap_or_else(|| "handy".to_string());

        let contents = format!(
            "[Desktop Entry]\n\
             Type=Application\n\
             Name=Handy\n\
             Comment=Offline speech-to-text application\n\
             Exec={exec} %U\n\
             Terminal=false\n\
             Categories=Utility;AudioVideo;Audio;\n\
             StartupWMClass={PORTAL_APP_ID}\n\
             NoDisplay=true\n"
        );

        if let Err(e) = std::fs::create_dir_all(&apps_dir) {
            warn!("Could not create {}: {e}", apps_dir.display());
            return;
        }
        if let Err(e) = std::fs::write(&desktop_path, contents) {
            warn!(
                "Could not write portal desktop entry {}: {e}",
                desktop_path.display()
            );
        } else {
            info!("Wrote portal desktop entry {}", desktop_path.display());
        }
    }

    /// `$XDG_DATA_HOME`, falling back to `$HOME/.local/share`.
    fn user_data_home() -> Option<std::path::PathBuf> {
        if let Some(dir) = std::env::var_os("XDG_DATA_HOME") {
            if !dir.is_empty() {
                return Some(std::path::PathBuf::from(dir));
            }
        }
        std::env::var_os("HOME").map(|home| std::path::PathBuf::from(home).join(".local/share"))
    }

    fn portal_state(app: &AppHandle) -> tauri::State<'_, PortalShortcutState> {
        if app.try_state::<PortalShortcutState>().is_none() {
            app.manage(PortalShortcutState::default());
        }
        app.state::<PortalShortcutState>()
    }

    fn portal_shortcuts(app: &AppHandle) -> Result<Vec<NewShortcut>, String> {
        let settings = settings::get_settings(app);
        let defaults = settings::get_default_settings().bindings;

        // Build the list of shortcuts to bind. A single binding that cannot be
        // expressed as an XDG trigger (e.g. a key the portal does not support)
        // must not abort binding of the remaining shortcuts, so skip it with a
        // warning instead of failing the whole portal session.
        let shortcuts = PORTAL_SHORTCUT_IDS
            .iter()
            .filter_map(|id| {
                if *id == "transcribe_with_post_process" && !settings.post_process_enabled {
                    return None;
                }

                settings.bindings.get(*id).or_else(|| defaults.get(*id))
            })
            .filter_map(
                |binding| match shortcut_to_xdg_trigger(&binding.current_binding) {
                    Ok(trigger) => {
                        let shortcut_id = portal_shortcut_id(&binding.id, &trigger);
                        Some(
                            NewShortcut::new(shortcut_id, binding.name.clone())
                                .preferred_trigger(Some(trigger.as_str())),
                        )
                    }
                    Err(e) => {
                        warn!(
                            "Skipping portal shortcut '{}' ({}): {e}",
                            binding.id, binding.current_binding
                        );
                        None
                    }
                },
            )
            .collect();

        Ok(shortcuts)
    }

    /// Build the portal action id for a shortcut. The XDG portal persists user
    /// bindings by app id + shortcut id, and may prefer that persisted value
    /// over a later `preferred_trigger`. Include a stable fingerprint of Handy's
    /// current preferred trigger so changing the hotkey in Handy creates a fresh
    /// portal action instead of reusing a stale compositor-owned binding.
    fn portal_shortcut_id(binding_id: &str, trigger: &str) -> String {
        format!("{}-{:016x}", binding_id, stable_hash(trigger))
    }

    fn base_portal_shortcut_id(shortcut_id: &str) -> Option<&'static str> {
        for base_id in PORTAL_SHORTCUT_IDS {
            if shortcut_id == base_id {
                return Some(base_id);
            }

            if let Some(suffix) = shortcut_id.strip_prefix(base_id) {
                if suffix.starts_with('-') {
                    return Some(base_id);
                }
            }
        }

        None
    }

    fn stable_hash(value: &str) -> u64 {
        const FNV_OFFSET: u64 = 0xcbf29ce484222325;
        const FNV_PRIME: u64 = 0x100000001b3;

        value.bytes().fold(FNV_OFFSET, |hash, byte| {
            (hash ^ u64::from(byte)).wrapping_mul(FNV_PRIME)
        })
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn portal_shortcut_ids_round_trip_to_base_ids() {
            let transcribe_id = portal_shortcut_id("transcribe", "CTRL+ALT+bracketright");

            assert_ne!(transcribe_id, "transcribe");
            assert_eq!(base_portal_shortcut_id(&transcribe_id), Some("transcribe"));
            assert_eq!(base_portal_shortcut_id("transcribe"), Some("transcribe"));
            assert_eq!(
                base_portal_shortcut_id("transcribe_with_post_process-deadbeef"),
                Some("transcribe_with_post_process")
            );
            assert_eq!(base_portal_shortcut_id("transcribe_unknown"), None);
        }

        #[test]
        fn portal_shortcut_ids_change_with_preferred_trigger() {
            assert_ne!(
                portal_shortcut_id("transcribe", "CTRL+ALT+bracketright"),
                portal_shortcut_id("transcribe", "CTRL+ALT+space")
            );
        }
    }

    fn shortcut_to_xdg_trigger(raw: &str) -> Result<String, String> {
        let parts: Vec<&str> = raw
            .split('+')
            .map(str::trim)
            .filter(|part| !part.is_empty())
            .collect();

        if parts.is_empty() {
            return Err("Shortcut cannot be empty".into());
        }

        let mut modifiers = Vec::new();
        let mut key = None;

        for part in parts {
            match part.to_ascii_lowercase().as_str() {
                "ctrl" | "control" => modifiers.push("CTRL"),
                "alt" | "option" => modifiers.push("ALT"),
                "shift" => modifiers.push("SHIFT"),
                "super" | "meta" | "cmd" | "command" | "win" | "windows" => modifiers.push("LOGO"),
                "num" => modifiers.push("NUM"),
                "fn" | "function" => {
                    return Err(
                        "The 'fn' key is not supported by XDG Desktop Portal shortcuts".into(),
                    )
                }
                other => {
                    if key.is_some() {
                        return Err(format!(
                            "Portal shortcuts must contain exactly one main key: '{raw}'"
                        ));
                    }
                    key = Some(xdg_key_name(other)?);
                }
            }
        }

        let key = key.ok_or_else(|| {
            "Portal shortcuts must include a main key (letter, number, F-key, etc.) in addition to modifiers".to_string()
        })?;

        modifiers.dedup();
        modifiers.push(key.as_str());
        Ok(modifiers.join("+"))
    }

    fn xdg_key_name(key: &str) -> Result<String, String> {
        let mapped = match key {
            "space" => "space",
            "enter" | "return" => "Return",
            "esc" | "escape" => "Escape",
            "backspace" => "BackSpace",
            "tab" => "Tab",
            "delete" | "del" => "Delete",
            "insert" | "ins" => "Insert",
            "home" => "Home",
            "end" => "End",
            "pageup" | "page_up" => "Page_Up",
            "pagedown" | "page_down" => "Page_Down",
            "up" | "arrowup" => "Up",
            "down" | "arrowdown" => "Down",
            "left" | "arrowleft" => "Left",
            "right" | "arrowright" => "Right",
            // Punctuation keys, mapped to their xkb keysym names.
            "[" => "bracketleft",
            "]" => "bracketright",
            ";" => "semicolon",
            "'" => "apostrophe",
            "," => "comma",
            "." => "period",
            "/" => "slash",
            "\\" => "backslash",
            "-" => "minus",
            "=" => "equal",
            "`" => "grave",
            key if key.len() == 1 && key.chars().all(|c| c.is_ascii_alphanumeric()) => key,
            key if is_function_key(key) => return Ok(key.to_ascii_uppercase()),
            key if key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') => key,
            _ => return Err(format!("Unsupported portal shortcut key '{key}'")),
        };

        Ok(mapped.to_string())
    }

    fn is_function_key(key: &str) -> bool {
        let Some(number) = key.strip_prefix('f') else {
            return false;
        };
        number
            .parse::<u8>()
            .map(|n| (1..=35).contains(&n))
            .unwrap_or(false)
    }

    fn handle_portal_event(app: &AppHandle, shortcut_id: &str, is_pressed: bool) {
        if settings::get_settings(app).keyboard_implementation != KeyboardImplementation::Portal {
            debug!("Ignoring stale portal shortcut event for '{shortcut_id}'");
            return;
        }

        let Some(binding_id) = base_portal_shortcut_id(shortcut_id) else {
            warn!("Ignoring unknown portal shortcut event for '{shortcut_id}'");
            return;
        };

        handle_shortcut_event(app, binding_id, PORTAL_HOTKEY_LABEL, is_pressed);
    }

    fn is_portal_shortcut(id: &str) -> bool {
        base_portal_shortcut_id(id).is_some()
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
