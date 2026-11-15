//! The playlist folder endpoints through the pipeline: create, rename, move, the depth and cycle
//! refusals, delete moving contents up, a tombstoned folder, a missing parent, the id shape,
//! Octo's own lists refused, the sign-in, and the answer in JSON and XML.

use serde_json::Value;

use super::held_playlists::{Fixture, ROAD, fixture};

const A: &str = "pfAAAAAAAAAAAAAAAAAAAA";
const B: &str = "pfBBBBBBBBBBBBBBBBBBBB";
const C: &str = "pfCCCCCCCCCCCCCCCCCCCC";
const D: &str = "pfDDDDDDDDDDDDDDDDDDDD";
const E: &str = "pfEEEEEEEEEEEEEEEEEEEE";

async fn folders(f: &Fixture, call: &str) -> Value {
    f.json("winters", call).await
}

fn folder<'a>(answer: &'a Value, id: &str) -> Option<&'a Value> {
    answer["playlistFolders"]["folder"]
        .as_array()?
        .iter()
        .find(|folder| folder["id"] == id)
}

#[tokio::test]
async fn t33_folders_are_made_renamed_moved_and_deleted_with_their_contents_moving_up() {
    let f = fixture().await;
    let made = folders(&f, &format!("savePlaylistFolder?id={A}&name=Trips")).await;
    assert_eq!(made["status"], "ok", "{made}");
    assert_eq!(folder(&made, A).unwrap()["name"], "Trips");
    assert!(made["playlistFolders"]["changed"].as_str().is_some());

    let renamed = folders(&f, &format!("savePlaylistFolder?id={A}&name=Road%20trips")).await;
    assert_eq!(folder(&renamed, A).unwrap()["name"], "Road trips");

    let child = folders(&f, &format!("savePlaylistFolder?id={B}&name=Summer&parentId={A}")).await;
    assert_eq!(folder(&child, B).unwrap()["parent"], A);
    let placed = folders(&f, &format!("setPlaylistFolder?playlistId={ROAD}&folderId={B}")).await;
    assert_eq!(placed["playlistFolders"]["place"][0]["folder"], B);

    // A folder can't go inside itself.
    let cycle = folders(
        &f,
        &format!("savePlaylistFolder?id={A}&name=Road%20trips&parentId={B}"),
    )
    .await;
    assert_eq!(cycle["error"]["code"], 10);
    assert_eq!(cycle["error"]["message"], "A folder can't go inside itself.");

    // Deleting B moves ROAD up into A.
    let deleted = folders(&f, &format!("deletePlaylistFolder?id={B}")).await;
    assert!(folder(&deleted, B).is_none());
    assert_eq!(deleted["playlistFolders"]["place"][0]["folder"], A);

    // A deleted folder is never brought back by a stale save.
    let stale = folders(&f, &format!("savePlaylistFolder?id={B}&name=Summer")).await;
    assert_eq!(stale["error"]["code"], 70);
    assert_eq!(stale["error"]["message"], "There is no such folder.");

    // A missing parent puts the folder under the deleted one's nearest live ancestor.
    let reparented = folders(&f, &format!("savePlaylistFolder?id={C}&name=Later&parentId={B}")).await;
    assert_eq!(folder(&reparented, C).unwrap()["parent"], A);

    // Top level again with an empty folder id.
    let top = folders(&f, &format!("setPlaylistFolder?playlistId={ROAD}&folderId=")).await;
    assert!(
        top["playlistFolders"]["place"]
            .as_array()
            .is_none_or(|places| places.is_empty()),
        "{top}"
    );
}

#[tokio::test]
async fn t33_folder_rules_refuse_with_their_words() {
    let f = fixture().await;
    let shape = folders(&f, "savePlaylistFolder?id=nope&name=Bad").await;
    assert_eq!(shape["error"]["code"], 10);
    assert_eq!(
        shape["error"]["message"],
        "That folder id isn't one Octo can use."
    );
    let name = folders(&f, &format!("savePlaylistFolder?id={A}&name=")).await;
    assert_eq!(
        name["error"]["message"],
        "A folder needs a name of 1 to 100 characters."
    );

    let mut parent: Option<&str> = None;
    for id in [A, B, C, D] {
        let call = match parent {
            Some(parent) => format!("savePlaylistFolder?id={id}&name=Level&parentId={parent}"),
            None => format!("savePlaylistFolder?id={id}&name=Level"),
        };
        assert_eq!(folders(&f, &call).await["status"], "ok");
        parent = Some(id);
    }
    let deep = folders(&f, &format!("savePlaylistFolder?id={E}&name=Five&parentId={D}")).await;
    assert_eq!(deep["error"]["message"], "Folders can go 4 deep.");

    let mix = folders(
        &f,
        &format!("setPlaylistFolder?playlistId=og{}&folderId={A}", "a".repeat(20)),
    )
    .await;
    assert_eq!(mix["error"]["message"], "Octo's own lists stay in From Octo.");
    let liked = f.state.generated_playlists.liked_id("winters");
    let liked = folders(&f, &format!("setPlaylistFolder?playlistId={liked}&folderId={A}")).await;
    assert_eq!(liked["error"]["code"], 10);

    let missing = folders(
        &f,
        "setPlaylistFolder?playlistId=x&folderId=pfZZZZZZZZZZZZZZZZZZZZ",
    )
    .await;
    assert_eq!(missing["error"]["code"], 70);
}

#[tokio::test]
async fn t33_folders_need_a_sign_in_and_answer_in_xml_too() {
    let f = fixture().await;
    let refused = f
        .rest_as(
            "u=winters&t=bad&s=salt&v=1.16.1&c=test",
            "getPlaylistFolders?f=json",
        )
        .await
        .json();
    assert_eq!(refused["subsonic-response"]["error"]["code"], 40);

    assert_eq!(
        folders(&f, &format!("savePlaylistFolder?id={A}&name=Trips")).await["status"],
        "ok"
    );
    assert_eq!(
        folders(&f, &format!("setPlaylistFolder?playlistId={ROAD}&folderId={A}")).await["status"],
        "ok"
    );
    let xml = f.rest("winters", "getPlaylistFolders").await.body;
    assert!(xml.contains("<playlistFolders "), "{xml}");
    assert!(xml.contains(&format!(r#"<folder id="{A}""#)), "{xml}");
    assert!(xml.contains(&format!(r#"playlist="{ROAD}""#)), "{xml}");
}
