//! The octo-sonic sidecar's client: octo-sonic reads a song and says how it sounds, and where it
//! can blend into the next song (its transition profile).

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use octo_core::settings::SettingsStore;
use serde_json::{Value, json};
use tracing::debug;

use crate::services::framework::http::client_builder;

/// How one song sounds: bliss's numbers, and the version of them.
#[derive(Debug, Clone, PartialEq)]
pub struct SonicFeatures {
    pub values: Vec<f32>,
    pub version: i32,
}

/// octo-sonic is up and writes this feature version, and this transition profile version
/// when it makes profiles; `problem` when it cannot see the music folder, so nothing it is sent
/// could be read. With Sounds alike turned off in .env (RADIO_SOUNDS_ALIKE=false) it makes
/// profiles only (`profiles_only`), or, an octo-sonic from before that, reads nothing (`off`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SonicHealth {
    pub version: i32,
    pub transition_version: Option<i32>,
    /// The pass of the profile analysis within its version; songs whose profile comes from a
    /// lower one are read again. None from an octo-sonic that does not say, which reads nothing
    /// again.
    pub transition_revision: Option<i32>,
    pub problem: Option<String>,
    pub off: bool,
    pub profiles_only: bool,
}

/// What octo-sonic is asked to read of a song: how it sounds, its transition profile, or both.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SonicParts {
    pub sound: bool,
    pub transition: bool,
}

impl SonicParts {
    pub const SOUND: SonicParts = SonicParts {
        sound: true,
        transition: false,
    };

    fn names(self) -> Vec<&'static str> {
        let mut names = Vec::new();
        if self.sound {
            names.push("sound");
        }
        if self.transition {
            names.push("transition");
        }
        names
    }
}

/// What octo-sonic said of one song: its features and its transition profile, as asked, or why
/// not, and the HTTP status (403 and 404 are octo-sonic's view of the music folder, not the
/// song).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SonicAnswer {
    pub features: Option<SonicFeatures>,
    pub transition: Option<Value>,
    pub error: Option<String>,
    pub status: u16,
}

/// The connection dropped, or the call timed out, before octo-sonic answered.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct SonicDropped(pub String);

/// What the analysis asks of octo-sonic. A trait so the worker can be tested without one.
#[async_trait]
pub trait ISonicClient: Send + Sync {
    /// The feature version octo-sonic writes and whether it sees the music, or None when it does
    /// not answer.
    async fn health(&self) -> Option<SonicHealth>;

    /// What was asked of the song, or an error to remember: octo-sonic could not read it. A
    /// dropped connection is `Err`; an answer Octo cannot read, or one missing a part asked for,
    /// is that song's error. `bpm`, the song's published tempo, goes with a profile only, where
    /// it can do no more than pick the octave of the tempo the profile finds.
    async fn analyze(
        &self,
        path: &str,
        parts: SonicParts,
        bpm: Option<f64>,
    ) -> Result<SonicAnswer, SonicDropped>;
}

/// The octo-sonic sidecar over HTTP.
pub struct SonicClient {
    /// `octo-sonic`: 11 minutes, longer than the sidecar takes over the longest song it reads.
    http: reqwest::Client,
    /// The radio source settings: the address is read at every call.
    settings: Arc<SettingsStore>,
}

impl SonicClient {
    pub const CLIENT_NAME: &'static str = "octo-sonic";
    pub const TIMEOUT: Duration = Duration::from_secs(11 * 60);
    const HEALTH_TIMEOUT: Duration = Duration::from_secs(5);

    pub fn new(settings: Arc<SettingsStore>) -> Self {
        SonicClient {
            http: client_builder()
                .timeout(Self::TIMEOUT)
                .build()
                .expect("the octo-sonic client builds"),
            settings,
        }
    }

    fn base_url(&self) -> String {
        self.settings
            .current()
            .radio_sources
            .sonic_url
            .trim_end_matches('/')
            .to_string()
    }
}

/// The body as JSON, an empty body as `{}`. `Err` when the body is not JSON.
fn parse(body: &str) -> serde_json::Result<Value> {
    serde_json::from_str(if body.is_empty() { "{}" } else { body })
}

/// A JSON number that is a whole number within `i32`; anything else is None.
fn int32(value: Option<&Value>) -> Option<i32> {
    value?.as_i64().and_then(|n| i32::try_from(n).ok())
}

/// `TryGetProperty(name) && ValueKind == String ? GetString()`.
fn text(value: &Value, name: &str) -> Option<String> {
    value.get(name).and_then(Value::as_str).map(str::to_string)
}

#[async_trait]
impl ISonicClient for SonicClient {
    async fn health(&self) -> Option<SonicHealth> {
        let url = format!("{}/health", self.base_url());
        let answer = async {
            let response = self.http.get(&url).send().await?;
            let status = response.status();
            let body = response.text().await?;
            anyhow::Ok((status, body))
        };
        let (status, body) = match tokio::time::timeout(Self::HEALTH_TIMEOUT, answer).await {
            Ok(Ok(answer)) => answer,
            Ok(Err(e)) => {
                debug!("octo-sonic health check failed: {e}");
                return None;
            }
            Err(_) => {
                debug!("octo-sonic health check failed: no answer in 5 s");
                return None;
            }
        };
        let doc = match parse(&body) {
            Ok(doc @ Value::Object(_)) => doc,
            Ok(_) | Err(_) => {
                debug!("octo-sonic health check failed: an answer that is not its JSON");
                return None;
            }
        };
        let version = int32(doc.get("featuresVersion"))?;
        // An octo-sonic from before profiles leaves it out.
        let transition_version = int32(doc.get("transitionVersion")).filter(|&v| v > 0);
        // An octo-sonic from before revisions leaves it out; it means nothing without profiles.
        let transition_revision = transition_version
            .and(int32(doc.get("transitionRevision")))
            .filter(|&r| r > 0);
        // Without how songs sound there is nothing but profiles; an octo-sonic that leaves the
        // field out reads both.
        let profiles_only =
            transition_version.is_some() && doc.get("soundsAlike") == Some(&Value::Bool(false));
        if status.is_success() {
            return Some(SonicHealth {
                version,
                transition_version,
                transition_revision,
                problem: None,
                off: false,
                profiles_only,
            });
        }
        if doc.get("off") == Some(&Value::Bool(true)) {
            return Some(SonicHealth {
                version,
                transition_version,
                transition_revision,
                problem: None,
                off: true,
                profiles_only: false,
            });
        }
        (status.as_u16() == 503).then(|| SonicHealth {
            version,
            transition_version,
            transition_revision,
            problem: Some(text(&doc, "error").unwrap_or_else(|| "cannot see the music folder".to_string())),
            off: false,
            profiles_only,
        })
    }

    async fn analyze(
        &self,
        path: &str,
        parts: SonicParts,
        bpm: Option<f64>,
    ) -> Result<SonicAnswer, SonicDropped> {
        let url = format!("{}/analyse", self.base_url());
        // The sound alone goes as it always went, so an older octo-sonic reads it the same.
        let mut body = if parts == SonicParts::SOUND {
            json!({ "path": path })
        } else {
            json!({ "path": path, "parts": parts.names() })
        };
        if let Some(bpm) = bpm.filter(|bpm| parts.transition && bpm.is_finite() && *bpm > 0.0) {
            body["bpm"] = json!(bpm);
        }
        let response = self
            .http
            .post(&url)
            .json(&body)
            .send()
            .await
            .map_err(|e| SonicDropped(e.to_string()))?;
        let status = response.status();
        let body = response.text().await.map_err(|e| SonicDropped(e.to_string()))?;
        let unreadable = || SonicAnswer {
            error: Some(format!(
                "octo-sonic answered something Octo could not read (HTTP {})",
                status.as_u16()
            )),
            status: status.as_u16(),
            ..Default::default()
        };
        let Ok(doc) = parse(&body) else {
            debug!("octo-sonic's answer for {path} could not be read");
            return Ok(unreadable());
        };
        if !status.is_success() {
            // An answer that is not an object is unreadable too.
            let Value::Object(_) = &doc else {
                return Ok(unreadable());
            };
            let error = text(&doc, "error");
            return Ok(SonicAnswer {
                error: Some(error.unwrap_or_else(|| format!("HTTP {}", status.as_u16()))),
                status: status.as_u16(),
                ..Default::default()
            });
        }
        let features = doc.get("features").and_then(Value::as_array).and_then(|items| {
            items
                .iter()
                .map(|item| item.as_f64().map(|n| n as f32).filter(|n| n.is_finite()))
                .collect::<Option<Vec<f32>>>()
        });
        let version = doc
            .get("version")
            .and_then(Value::as_i64)
            .and_then(|n| i32::try_from(n).ok());
        let features = features
            .zip(version)
            .map(|(values, version)| SonicFeatures { values, version });
        let transition = doc
            .get("transition")
            .filter(|t| t.is_object() && t.get("version").and_then(Value::as_i64).is_some())
            .cloned();
        if (parts.sound && features.is_none()) || (parts.transition && !parts.sound && transition.is_none()) {
            debug!("octo-sonic's answer for {path} could not be read");
            return Ok(unreadable());
        }
        Ok(SonicAnswer {
            features: features.filter(|_| parts.sound),
            transition: transition.filter(|_| parts.transition),
            error: None,
            status: status.as_u16(),
        })
    }
}

#[cfg(test)]
mod tests {
    use octo_core::settings::{AppSettings, RadioSourceSettings};
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;

    async fn client(server: &MockServer) -> SonicClient {
        SonicClient::new(Arc::new(SettingsStore::for_tests(AppSettings {
            radio_sources: RadioSourceSettings {
                sonic_url: format!("{}/", server.uri()),
                ..Default::default()
            },
            ..Default::default()
        })))
    }

    async fn health(status: u16, body: &str) -> Option<SonicHealth> {
        let server = MockServer::start().await;
        Mock::given(path("/health"))
            .respond_with(ResponseTemplate::new(status).set_body_string(body))
            .mount(&server)
            .await;
        client(&server).await.health().await
    }

    /// Health in each of its states.
    #[tokio::test]
    async fn health_reads_each_state() {
        assert_eq!(
            health(200, r#"{"ok":true,"featuresVersion":2}"#).await,
            Some(SonicHealth {
                version: 2,
                ..Default::default()
            })
        );
        assert_eq!(
            health(200, r#"{"ok":true,"featuresVersion":2,"transitionVersion":1}"#).await,
            Some(SonicHealth {
                version: 2,
                transition_version: Some(1),
                ..Default::default()
            })
        );
        assert_eq!(
            health(
                200,
                r#"{"ok":true,"featuresVersion":2,"transitionVersion":1,"soundsAlike":false}"#
            )
            .await,
            Some(SonicHealth {
                version: 2,
                transition_version: Some(1),
                profiles_only: true,
                ..Default::default()
            })
        );
        assert_eq!(
            health(
                200,
                r#"{"ok":true,"featuresVersion":2,"transitionVersion":1,"soundsAlike":true}"#
            )
            .await
            .map(|h| h.profiles_only),
            Some(false)
        );
        // The pass of the profile analysis, said only beside a profile version.
        assert_eq!(
            health(
                200,
                r#"{"ok":true,"featuresVersion":2,"transitionVersion":1,"transitionRevision":2}"#
            )
            .await,
            Some(SonicHealth {
                version: 2,
                transition_version: Some(1),
                transition_revision: Some(2),
                ..Default::default()
            })
        );
        for odd in [
            r#"{"ok":true,"featuresVersion":2,"transitionRevision":2}"#,
            r#"{"ok":true,"featuresVersion":2,"transitionVersion":1,"transitionRevision":0}"#,
            r#"{"ok":true,"featuresVersion":2,"transitionVersion":1,"transitionRevision":"2"}"#,
        ] {
            assert_eq!(
                health(200, odd).await.and_then(|h| h.transition_revision),
                None,
                "{odd}"
            );
        }
        assert_eq!(
            health(503, r#"{"ok":false,"off":true,"featuresVersion":2}"#).await,
            Some(SonicHealth {
                version: 2,
                off: true,
                ..Default::default()
            })
        );
        assert_eq!(
            health(503, r#"{"ok":false,"featuresVersion":2,"error":"blind"}"#).await,
            Some(SonicHealth {
                version: 2,
                problem: Some("blind".into()),
                ..Default::default()
            })
        );
        assert_eq!(
            health(503, r#"{"featuresVersion":2}"#)
                .await
                .and_then(|h| h.problem),
            Some("cannot see the music folder".into())
        );
        assert_eq!(health(500, r#"{"featuresVersion":2}"#).await, None);
        assert_eq!(health(200, r#"{"ok":true}"#).await, None);
        assert_eq!(health(200, "not json").await, None);
        assert_eq!(health(200, r#"{"featuresVersion":2.5}"#).await, None);
    }

    #[tokio::test]
    async fn analyze_reads_features_errors_and_odd_answers() {
        let server = MockServer::builder().start().await;
        Mock::given(method("POST"))
            .and(path("/analyse"))
            .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"features":[0.5,1],"version":2}"#))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/analyse"))
            .respond_with(ResponseTemplate::new(422).set_body_string(r#"{"error":"too short"}"#))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/analyse"))
            .respond_with(ResponseTemplate::new(500).set_body_string(""))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/analyse"))
            .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"features":"lots"}"#))
            .mount(&server)
            .await;
        let client = client(&server).await;
        let ok = client
            .analyze("/music/a.flac", SonicParts::SOUND, None)
            .await
            .expect("an answer");
        assert_eq!(
            ok.features,
            Some(SonicFeatures {
                values: vec![0.5, 1.0],
                version: 2
            })
        );
        let refused = client
            .analyze("/music/a.flac", SonicParts::SOUND, None)
            .await
            .expect("an answer");
        assert_eq!(
            (refused.error.as_deref(), refused.status),
            (Some("too short"), 422)
        );
        let bare = client
            .analyze("/music/a.flac", SonicParts::SOUND, None)
            .await
            .expect("an answer");
        assert_eq!(bare.error.as_deref(), Some("HTTP 500"));
        let odd = client
            .analyze("/music/a.flac", SonicParts::SOUND, None)
            .await
            .expect("an answer");
        assert_eq!(
            odd.error.as_deref(),
            Some("octo-sonic answered something Octo could not read (HTTP 200)")
        );
        let sent = server.received_requests().await.expect("recorded");
        assert_eq!(sent[0].body, br#"{"path":"/music/a.flac"}"#);
    }

    #[tokio::test]
    async fn a_profile_is_asked_for_by_name_and_read_with_the_features() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/analyse"))
            .respond_with(ResponseTemplate::new(200).set_body_string(
                r#"{"features":[0.5],"version":2,"transition":{"version":1,"durationMs":1000}}"#,
            ))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/analyse"))
            .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"transition":{"durationMs":1}}"#))
            .mount(&server)
            .await;
        let client = client(&server).await;
        let both = SonicParts {
            sound: true,
            transition: true,
        };
        let answer = client
            .analyze("/music/a.flac", both, Some(128.5))
            .await
            .expect("an answer");
        assert_eq!(answer.features.map(|f| f.version), Some(2));
        assert_eq!(answer.transition, Some(json!({"version": 1, "durationMs": 1000})));
        // A profile without its version is not one.
        let only = SonicParts {
            sound: false,
            transition: true,
        };
        let odd = client
            .analyze("/music/a.flac", only, None)
            .await
            .expect("an answer");
        assert!(odd.error.is_some() && odd.transition.is_none());
        // A published tempo has nothing to do with how a song sounds.
        let _ = client
            .analyze("/music/a.flac", SonicParts::SOUND, Some(128.5))
            .await;
        let sent = server.received_requests().await.expect("recorded");
        assert_eq!(sent[2].body, br#"{"path":"/music/a.flac"}"#);
        assert_eq!(
            sent[0].body,
            br#"{"path":"/music/a.flac","parts":["sound","transition"],"bpm":128.5}"#
        );
        assert_eq!(
            sent[1].body,
            br#"{"path":"/music/a.flac","parts":["transition"]}"#
        );
    }

    #[tokio::test]
    async fn a_closed_port_is_a_dropped_connection() {
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .and_then(|l| l.local_addr())
            .expect("a port")
            .port();
        let client = SonicClient::new(Arc::new(SettingsStore::for_tests(AppSettings {
            radio_sources: RadioSourceSettings {
                sonic_url: format!("http://127.0.0.1:{port}"),
                ..Default::default()
            },
            ..Default::default()
        })));
        assert!(
            client
                .analyze("/music/a.flac", SonicParts::SOUND, None)
                .await
                .is_err()
        );
        assert_eq!(client.health().await, None);
    }
}
