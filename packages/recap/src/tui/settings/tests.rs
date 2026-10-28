//! Offline connection-selection regression tests.
use super::*;

#[test]
fn endpoint_selection_round_trips_user_state_and_generation_options() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("selection.json");
    let mut app = App::new(crate::application::TuiOptions::default());
    app.apply_selection(Selection {
        endpoint: Some("http://127.0.0.1:19191/v1".into()),
        profile: Some("configured-auth-reference".into()),
        auth_profile: Some("configured-account".into()),
        model: Some("vision-model".into()),
        ..Selection::default()
    });
    crate::connections::save_preferences(&path, &app.current_selection()).unwrap();
    let saved = crate::connections::load_preferences(&path).unwrap();
    let mut reopened = App::new(crate::application::TuiOptions::default());
    reopened.apply_selection(saved.clone());
    let request = reopened.options.create_request(
        "video.mp4".into(),
        None,
        None,
        bcode_recap::contracts::Depth::Standard,
    );
    assert_eq!(request.model.endpoint, saved.endpoint);
    assert_eq!(request.model.profile, saved.profile);
    assert_eq!(request.model.auth_profile, saved.auth_profile);
    assert_eq!(request.model.model, saved.model);
    assert_eq!(reopened.current_selection(), saved);
}
