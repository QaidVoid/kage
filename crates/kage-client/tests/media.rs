//! One-shot produced-media requests and their request correlation.

use kage_client::{Change, Client, Frame, RequestId, RpcError};
use serde_json::json;

#[test]
fn media_read_answers_keep_the_request_and_session_without_retaining_bytes() {
    let mut client = Client::new();
    let image = client.media_read("first", true);
    let media = client.media_read("second", false);
    let outgoing = client.take_outgoing();
    assert_eq!(outgoing.len(), 2);
    assert_eq!(
        outgoing[0],
        Frame::Request {
            id: RequestId::Number(image),
            method: "_kage/media/read".into(),
            params: json!({"sessionId": "first", "imageOnly": true, "metadataOnly": false}),
        }
    );
    assert_eq!(
        outgoing[1],
        Frame::Request {
            id: RequestId::Number(media),
            method: "_kage/media/read".into(),
            params: json!({"sessionId": "second", "imageOnly": false, "metadataOnly": false}),
        }
    );
    for (request, session, mime) in [
        (media, "second", "audio/mpeg"),
        (image, "first", "image/png"),
    ] {
        let changes = client.handle(Frame::Success {
            id: RequestId::Number(request),
            result: json!({"mime": mime, "data": "Ynl0ZXM=", "sha256": "digest"}),
        });
        assert_eq!(
            changes,
            [Change::MediaRead {
                request,
                session: session.into(),
                mime: mime.into(),
                data: "Ynl0ZXM=".into(),
                sha256: "digest".into(),
            }]
        );
        assert_eq!(changes[0].session_id(), Some(session));
    }
    assert!(client.state().sessions.is_empty());
}

#[test]
fn media_read_unavailable_or_malformed_reports_failure_once() {
    let mut client = Client::new();
    let request = client.media_read("session", true);
    let error = RpcError::method_not_found("_kage/media/read");
    let failure = Frame::Failure {
        id: RequestId::Number(request),
        error: error.clone(),
    };
    assert_eq!(
        client.handle(failure.clone()),
        [Change::Failed { request, error }]
    );
    assert!(client.handle(failure).is_empty());

    let request = client.media_read("session", false);
    let changes = client.handle(Frame::Success {
        id: RequestId::Number(request),
        result: json!({"mime": "image/png", "data": "Ynl0ZXM="}),
    });
    assert!(
        matches!(changes.as_slice(), [Change::Failed { request: failed, .. }] if *failed == request)
    );
    assert!(
        client
            .handle(Frame::Success {
                id: RequestId::Number(request),
                result: json!({"mime": "image/png", "data": "Ynl0ZXM=", "sha256": "digest"}),
            })
            .is_empty()
    );
}

#[test]
fn media_metadata_requests_only_the_digest_and_mime() {
    let mut client = Client::new();
    let request = client.media_metadata("session", false);
    assert_eq!(
        client.take_outgoing(),
        [Frame::Request {
            id: RequestId::Number(request),
            method: "_kage/media/read".into(),
            params: json!({"sessionId": "session", "imageOnly": false, "metadataOnly": true}),
        }]
    );
    assert_eq!(
        client.handle(Frame::Success {
            id: RequestId::Number(request),
            result: json!({"mime": "audio/mpeg", "data": "", "sha256": "digest"}),
        }),
        [Change::MediaRead {
            request,
            session: "session".into(),
            mime: "audio/mpeg".into(),
            data: String::new(),
            sha256: "digest".into(),
        }]
    );
}
