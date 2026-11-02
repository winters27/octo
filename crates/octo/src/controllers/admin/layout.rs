//! `POST /api/admin/layout/preview`: the paths the file layout gives three sample songs and what
//! is wrong with a template, from the server's own layout engine, so the dashboard's File layout
//! card shows exactly what a download gets. Writes nothing.

use axum::extract::Request;
use axum::response::Response;
use axum::routing::post;
use octo_core::common::PathHelper;
use octo_core::common::layout::{Layout, LayoutKind, LayoutValues};
use octo_core::settings::FolderStructure;
use serde::Deserialize;
use serde_json::{Value, json};

use super::json_body::{FromBody, Kind, Member, Shape, bind_optional_body};
use super::request_binding::{ok, split};
use crate::http::routes::RouteSet;

pub fn routes() -> RouteSet {
    RouteSet::new().route("/api/admin/layout/preview", post(preview_layout))
}

/// What the File layout card sends: its fields as they are now, unsaved, and a number the
/// answer carries back so the page draws only the newest answer.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct LayoutPreviewBody {
    seq: i64,
    folder_structure: Option<String>,
    folder_template: Option<String>,
    folder_template_singles: Option<String>,
    folder_template_compilations: Option<String>,
}

impl FromBody for LayoutPreviewBody {
    const SHAPE: &'static Shape = &Shape {
        name: "Octo.Layout.Preview",
        by_constructor: true,
        members: &[
            Member {
                name: "seq",
                kind: Kind::Int64,
            },
            Member {
                name: "folderStructure",
                kind: Kind::Text,
            },
            Member {
                name: "folderTemplate",
                kind: Kind::Text,
            },
            Member {
                name: "folderTemplateSingles",
                kind: Kind::Text,
            },
            Member {
                name: "folderTemplateCompilations",
                kind: Kind::Text,
            },
        ],
    };
}

/// A folder structure as the settings name it (`Flat`, `ByArtist`, `Organized`, `Custom`), in any
/// capitals; Flat, the default, for anything else.
pub(super) fn structure_named(name: Option<&str>) -> FolderStructure {
    match name.map(str::trim).unwrap_or("") {
        name if name.eq_ignore_ascii_case("ByArtist") => FolderStructure::ByArtist,
        name if name.eq_ignore_ascii_case("Organized") => FolderStructure::Organized,
        name if name.eq_ignore_ascii_case("Custom") => FolderStructure::Custom,
        _ => FolderStructure::Flat,
    }
}

/// The songs every preview files: an album track, a single, and a compilation track, the single
/// holding the characters Windows refuses so the example shows what becomes of them.
fn samples() -> [(&'static str, LayoutValues, &'static str); 3] {
    [
        (
            "Album track",
            LayoutValues {
                artist: "Daft Punk".to_string(),
                track_artist: "Daft Punk".to_string(),
                album: "Discovery".to_string(),
                title: "One More Time".to_string(),
                track: Some(1),
                year: Some(2001),
                album_type: Some("Album".to_string()),
                kind: LayoutKind::Album,
                ..LayoutValues::default()
            },
            ".flac",
        ),
        (
            "Single",
            LayoutValues {
                artist: "Billie Eilish".to_string(),
                track_artist: "Billie Eilish".to_string(),
                album: "What Was I Made For? [From The Motion Picture \"Barbie\"]".to_string(),
                title: "What Was I Made For? [From The Motion Picture \"Barbie\"]".to_string(),
                track: Some(1),
                year: Some(2023),
                album_type: Some("Single".to_string()),
                kind: LayoutKind::Single,
                ..LayoutValues::default()
            },
            ".flac",
        ),
        (
            "Compilation track",
            LayoutValues {
                artist: "Miley Cyrus".to_string(),
                track_artist: "Miley Cyrus".to_string(),
                album_artist: Some("Various Artists".to_string()),
                album: "Now That's What I Call Music! 115".to_string(),
                title: "Flowers".to_string(),
                track: Some(3),
                year: Some(2023),
                album_type: Some("Compilation".to_string()),
                kind: LayoutKind::Compilation,
                ..LayoutValues::default()
            },
            ".mp3",
        ),
    ]
}

fn kind_name(kind: LayoutKind) -> &'static str {
    match kind {
        LayoutKind::Album => "Album",
        LayoutKind::Single => "Single",
        LayoutKind::Compilation => "Compilation",
    }
}

/// The template field a kind is filed by under Custom: singles and compilations by their own
/// field when it holds a template, else by the albums field. None for a preset.
fn field_for(
    structure: FolderStructure,
    kind: LayoutKind,
    singles: &str,
    compilations: &str,
) -> Option<&'static str> {
    if structure != FolderStructure::Custom {
        return None;
    }
    let own = |text: &str, field: &'static str| {
        if text.trim().is_empty() {
            "FolderTemplate"
        } else {
            field
        }
    };
    Some(match kind {
        LayoutKind::Album => "FolderTemplate",
        LayoutKind::Single => own(singles, "FolderTemplateSingles"),
        LayoutKind::Compilation => own(compilations, "FolderTemplateCompilations"),
    })
}

/// The preview: `{ seq, problems: [{ field, message }], examples: [{ kind, label, path, field }] }`,
/// `field` naming the template the example was filed by (null for a preset).
async fn preview_layout(request: Request) -> Response {
    let (parts, body) = split(request).await;
    let body: LayoutPreviewBody = match bind_optional_body(&parts.headers, &body) {
        Ok(body) => body.unwrap_or_default(),
        Err(answer) => return answer,
    };
    let structure = structure_named(body.folder_structure.as_deref());
    let singles = body.folder_template_singles.as_deref().unwrap_or("");
    let compilations = body.folder_template_compilations.as_deref().unwrap_or("");
    let layout = Layout::custom_or_preset(
        structure,
        body.folder_template.as_deref().unwrap_or(""),
        singles,
        compilations,
    );
    let problems: Vec<Value> = layout
        .problems()
        .iter()
        .map(|problem| json!({ "field": problem.setting, "message": problem.message }))
        .collect();
    let examples: Vec<Value> = samples()
        .into_iter()
        .map(|(label, values, extension)| {
            let names = layout.render_names(&values, extension);
            json!({
                "kind": kind_name(values.kind),
                "label": label,
                "path": PathHelper::join_names("", &names, extension),
                "field": field_for(structure, values.kind, singles, compilations),
            })
        })
        .collect();
    ok(&json!({ "seq": body.seq, "problems": problems, "examples": examples }))
}

#[cfg(test)]
#[path = "layout_tests.rs"]
mod tests;
