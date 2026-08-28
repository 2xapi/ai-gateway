#[path = "../src/usage_overlay.rs"]
mod usage_overlay;

use serde_json::json;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use usage_overlay::{
    effective_opacity, load_settings, next_visibility, save_settings, OverlayPosition,
    UsageOverlaySettings, WindowAction, WindowVisibility, DEFAULT_OPACITY,
    DEFAULT_REFRESH_INTERVAL_SECS, MAX_OPACITY, MAX_REFRESH_INTERVAL_SECS, MIN_OPACITY,
    MIN_REFRESH_INTERVAL_SECS,
};

static TEST_ID: AtomicU64 = AtomicU64::new(0);

fn sandbox() -> PathBuf {
    let id = TEST_ID.fetch_add(1, Ordering::Relaxed);
    let path =
        std::env::temp_dir().join(format!("2xapi-usage-overlay-{}-{id}", std::process::id()));
    let _ = std::fs::remove_dir_all(&path);
    std::fs::create_dir_all(&path).unwrap();
    path
}

#[test]
fn defaults_are_disabled_and_safe() {
    let settings = UsageOverlaySettings::default();
    assert!(!settings.enabled);
    assert_eq!(settings.opacity, DEFAULT_OPACITY);
    assert_eq!(
        settings.refresh_interval_secs,
        DEFAULT_REFRESH_INTERVAL_SECS
    );
    assert!(settings.always_on_top);
    assert!(!settings.click_through);
    assert!(settings.restore_full_opacity_on_hover);
    assert!(settings.validate().is_ok());
}

#[test]
fn validation_rejects_opacity_interval_and_invalid_position() {
    let mut settings = UsageOverlaySettings {
        opacity: MIN_OPACITY,
        refresh_interval_secs: MIN_REFRESH_INTERVAL_SECS,
        position: Some(OverlayPosition { x: 0, y: 0 }),
        ..Default::default()
    };
    assert!(settings.validate().is_ok());
    settings.opacity = MAX_OPACITY;
    settings.refresh_interval_secs = MAX_REFRESH_INTERVAL_SECS;
    assert!(settings.validate().is_ok());
    settings.opacity = 0.59;
    settings.refresh_interval_secs = 301;
    settings.position = Some(OverlayPosition { x: i32::MIN, y: 2 });
    let errors = settings.validate().unwrap_err();
    assert_eq!(errors.len(), 3);
}

#[test]
fn validation_rejects_non_finite_opacity() {
    let settings = UsageOverlaySettings {
        opacity: f64::NAN,
        ..Default::default()
    };
    assert!(settings.validate().is_err());
    let settings = UsageOverlaySettings {
        opacity: f64::INFINITY,
        ..Default::default()
    };
    assert!(settings.validate().is_err());
}

#[test]
fn hover_opacity_respects_restore_and_click_through() {
    let mut settings = UsageOverlaySettings {
        opacity: 0.72,
        ..Default::default()
    };
    assert_eq!(effective_opacity(&settings, false), 0.72);
    assert_eq!(effective_opacity(&settings, true), MAX_OPACITY);

    settings.restore_full_opacity_on_hover = false;
    assert_eq!(effective_opacity(&settings, true), 0.72);

    settings.restore_full_opacity_on_hover = true;
    settings.click_through = true;
    assert_eq!(effective_opacity(&settings, true), 0.72);
}

#[test]
fn settings_round_trip_preserves_other_top_level_fields_atomically() {
    let home = sandbox();
    let path = home.join("2xapi-settings.json");
    std::fs::write(
        &path,
        serde_json::to_vec_pretty(&json!({
            "accel": {"mode": "official"},
            "autoRepairBeforeLaunch": true,
            "unknown": [1, 2, 3]
        }))
        .unwrap(),
    )
    .unwrap();

    let settings = UsageOverlaySettings {
        enabled: true,
        opacity: 0.72,
        refresh_interval_secs: 45,
        position: Some(OverlayPosition { x: -20, y: 80 }),
        ..Default::default()
    };
    save_settings(&home, &settings).unwrap();
    let root: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(root["accel"]["mode"], "official");
    assert_eq!(root["autoRepairBeforeLaunch"], true);
    assert_eq!(root["unknown"], json!([1, 2, 3]));
    assert_eq!(load_settings(&home).unwrap(), settings);

    let mut without_position = settings.clone();
    without_position.enabled = false;
    without_position.opacity = 0.75;
    without_position.position = None;
    save_settings(&home, &without_position).unwrap();
    let updated = load_settings(&home).unwrap();
    assert_eq!(updated.opacity, 0.75);
    assert!(!updated.enabled);
    assert_eq!(updated.position, settings.position);

    let temp_prefix = format!(".2xapi-settings.{}.", std::process::id());
    assert!(!std::fs::read_dir(&home)
        .unwrap()
        .filter_map(Result::ok)
        .any(|entry| entry
            .file_name()
            .to_string_lossy()
            .starts_with(&temp_prefix)));
}

#[test]
fn concurrent_saves_leave_a_valid_complete_file() {
    let home = std::sync::Arc::new(sandbox());
    let mut workers = Vec::new();
    for index in 0..8 {
        let home = std::sync::Arc::clone(&home);
        workers.push(std::thread::spawn(move || {
            let settings = UsageOverlaySettings {
                enabled: index % 2 == 0,
                opacity: MIN_OPACITY + (index as f64 * 0.03),
                refresh_interval_secs: MIN_REFRESH_INTERVAL_SECS + index as u64,
                ..Default::default()
            };
            save_settings(&home, &settings).unwrap();
        }));
    }
    for worker in workers {
        worker.join().unwrap();
    }

    let settings = load_settings(&home).unwrap();
    assert!((MIN_OPACITY..=MAX_OPACITY).contains(&settings.opacity));
    assert!((MIN_REFRESH_INTERVAL_SECS..=MAX_REFRESH_INTERVAL_SECS)
        .contains(&settings.refresh_interval_secs));
    let root: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(home.join("2xapi-settings.json")).unwrap())
            .unwrap();
    assert!(root.get("usageOverlay").is_some());
    assert!(!std::fs::read_dir(&*home)
        .unwrap()
        .filter_map(Result::ok)
        .any(|entry| entry
            .file_name()
            .to_string_lossy()
            .starts_with(&format!(".2xapi-settings.{}.", std::process::id()))));
}

#[test]
fn omitted_nested_fields_use_each_field_default() {
    let home = sandbox();
    std::fs::write(
        home.join("2xapi-settings.json"),
        r#"{"usageOverlay":{"enabled":true}}"#,
    )
    .unwrap();
    let settings = load_settings(&home).unwrap();
    assert!(settings.enabled);
    assert_eq!(settings.opacity, DEFAULT_OPACITY);
    assert!(settings.always_on_top);
    assert!(!settings.click_through);
    assert!(settings.restore_full_opacity_on_hover);
    assert_eq!(
        settings.refresh_interval_secs,
        DEFAULT_REFRESH_INTERVAL_SECS
    );
    assert_eq!(settings.position, None);
}

#[test]
fn missing_segment_uses_defaults_and_bad_root_is_rejected() {
    let home = sandbox();
    std::fs::write(
        home.join("2xapi-settings.json"),
        r#"{"accel":{"mode":"official"}}"#,
    )
    .unwrap();
    assert_eq!(
        load_settings(&home).unwrap(),
        UsageOverlaySettings::default()
    );
    std::fs::write(home.join("2xapi-settings.json"), "[]").unwrap();
    assert!(load_settings(&home).is_err());
}

#[test]
fn visibility_actions_are_pure_and_deterministic() {
    assert_eq!(
        next_visibility(WindowAction::Show, WindowVisibility::Hidden),
        WindowVisibility::Visible
    );
    assert_eq!(
        next_visibility(WindowAction::Hide, WindowVisibility::Visible),
        WindowVisibility::Hidden
    );
    assert_eq!(
        next_visibility(WindowAction::Toggle, WindowVisibility::Visible),
        WindowVisibility::Hidden
    );
    assert_eq!(
        next_visibility(WindowAction::Toggle, WindowVisibility::Unknown),
        WindowVisibility::Visible
    );
    assert_eq!(
        next_visibility(WindowAction::ApplySettings, WindowVisibility::Visible),
        WindowVisibility::Visible
    );
}
