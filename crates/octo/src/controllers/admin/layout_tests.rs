//! The File layout card's preview through the assembled pipeline, and the save check on the
//! merged settings.

use axum::http::{Method, StatusCode};
use octo_core::json::dom::Node;
use octo_core::settings::{AppSettings, JsonObject, SubsonicSettings};
use serde_json::Value;

use super::super::settings::layout_refusal;
use super::super::test_app::{app, send};
use crate::app::AppState;

async fn preview(body: &str) -> Value {
    let state = AppState::for_tests(AppSettings::default());
    let reply = send(
        &app(state),
        Method::POST,
        "/api/admin/layout/preview",
        &[("X-Octo-Admin", "1")],
        Some(body),
    )
    .await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text());
    reply.json()
}

fn paths(answer: &Value) -> Vec<String> {
    answer["examples"]
        .as_array()
        .expect("examples")
        .iter()
        .map(|example| example["path"].as_str().expect("a path").to_string())
        .collect()
}

#[tokio::test]
async fn a_preset_preview_shows_the_three_songs_as_a_download_files_them() {
    let answer = preview(r#"{"seq": 7, "folderStructure": "Organized"}"#).await;
    assert_eq!(answer["seq"], 7);
    assert_eq!(answer["problems"].as_array().map(Vec::len), Some(0));
    assert_eq!(
        paths(&answer),
        [
            "Daft Punk/Discovery/01 - One More Time.flac",
            "Billie Eilish/What Was I Made For\u{FF1F} [From The Motion Picture \u{FF02}Barbie\u{FF02}]/01 - What Was I Made For\u{FF1F} [From The Motion Picture \u{FF02}Barbie\u{FF02}].flac",
            "Miley Cyrus/Now That's What I Call Music! 115/03 - Flowers.mp3",
        ]
    );
    let labels: Vec<&str> = answer["examples"]
        .as_array()
        .expect("examples")
        .iter()
        .map(|example| example["label"].as_str().expect("a label"))
        .collect();
    assert_eq!(labels, ["Album track", "Single", "Compilation track"]);
}

#[tokio::test]
async fn a_custom_preview_files_each_kind_by_its_template() {
    let answer = preview(
        r#"{"seq": 1, "folderStructure": "Custom",
            "folderTemplate": "{Artist}/{Album} ({Year})/{Track:00} - {Title}",
            "folderTemplateSingles": "{Artist}/Singles/{Title}",
            "folderTemplateCompilations": "{Album Artist}/{Album}/{Track:00} - {Track Artist} - {Title}"}"#,
    )
    .await;
    assert_eq!(answer["problems"].as_array().map(Vec::len), Some(0));
    let paths = paths(&answer);
    assert_eq!(paths[0], "Daft Punk/Discovery (2001)/01 - One More Time.flac");
    assert!(paths[1].starts_with("Billie Eilish/Singles/"), "{}", paths[1]);
    assert_eq!(
        paths[2],
        "Various Artists/Now That's What I Call Music! 115/03 - Miley Cyrus - Flowers.mp3"
    );
}

#[tokio::test]
async fn a_broken_template_names_its_field_and_the_examples_show_the_stand_in() {
    let answer = preview(
        r#"{"seq": 3, "folderStructure": "Custom", "folderTemplate": "{Artst}/{Title}",
            "folderTemplateSingles": "{Artist}/{Title}.flac"}"#,
    )
    .await;
    let problems = answer["problems"].as_array().expect("problems");
    assert_eq!(problems.len(), 2);
    assert_eq!(problems[0]["field"], "FolderTemplate");
    assert!(
        problems[0]["message"]
            .as_str()
            .expect("words")
            .contains("{Artst}")
    );
    assert_eq!(problems[1]["field"], "FolderTemplateSingles");
    assert_eq!(paths(&answer)[0], "Daft Punk/Discovery/01 - One More Time.flac");
}

/// Each example names the template it was filed by, so the card can tell which field to fix.
#[tokio::test]
async fn each_example_names_the_template_it_was_filed_by() {
    let fields = |answer: &Value| -> Vec<Value> {
        answer["examples"]
            .as_array()
            .expect("examples")
            .iter()
            .map(|example| example["field"].clone())
            .collect()
    };
    let preset = preview(r#"{"seq": 1, "folderStructure": "Flat"}"#).await;
    assert_eq!(fields(&preset), [Value::Null, Value::Null, Value::Null]);

    let custom = preview(
        r#"{"seq": 2, "folderStructure": "Custom", "folderTemplate": "{Artist}/{Title}",
            "folderTemplateSingles": "  ", "folderTemplateCompilations": "Various/{Album}/{Title}"}"#,
    )
    .await;
    assert_eq!(
        fields(&custom),
        ["FolderTemplate", "FolderTemplate", "FolderTemplateCompilations"]
    );
}

fn patch(json: &str) -> JsonObject {
    match Node::parse(json).expect("JSON") {
        Node::Object(map) => map,
        _ => panic!("an object"),
    }
}

/// The form posts every field it shows, templates included, on any layout.
#[test]
fn a_preset_save_with_blank_templates_is_accepted() {
    let saved = SubsonicSettings::default();
    for structure in ["Flat", "ByArtist", "Organized"] {
        let body = format!(
            r#"{{"Subsonic": {{"FolderStructure": "{structure}", "FolderTemplate": "", "FolderTemplateSingles": "{{Broken"}}}}"#
        );
        assert_eq!(layout_refusal(&patch(&body), &saved), None, "{structure}");
    }
}

#[test]
fn custom_with_a_broken_template_is_refused_with_its_field() {
    let saved = SubsonicSettings::default();
    let refused = layout_refusal(
        &patch(r#"{"Subsonic": {"folderstructure": "custom", "FolderTemplate": "{Artist}/{Album}"}}"#),
        &saved,
    );
    let (field, words) = refused.expect("refused");
    assert_eq!(field, "FolderTemplate");
    assert!(words.starts_with("Albums: Put {Title}"), "{words}");

    // Switching to Custom alone is checked against the templates already saved.
    let broken = SubsonicSettings {
        folder_template_compilations: "{Nope}/{Title}".to_string(),
        ..SubsonicSettings::default()
    };
    let (field, words) =
        layout_refusal(&patch(r#"{"Subsonic": {"FolderStructure": "Custom"}}"#), &broken).expect("refused");
    assert_eq!(field, "FolderTemplateCompilations");
    assert!(words.starts_with("Compilations: "), "{words}");

    // The saved default is the Organized template, so Custom starts from a good one.
    assert_eq!(
        layout_refusal(&patch(r#"{"Subsonic": {"FolderStructure": "Custom"}}"#), &saved),
        None
    );
}
