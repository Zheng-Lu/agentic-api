use super::support::*;
use crate::executor::{BoxStream, ExecuteRequest, ExecutorError, ExecutorResult};
use crate::types::ResponsePayload;
use either::Either;
use futures::StreamExt;
use serde_json::{Value, json};
use std::{sync::Arc, time::Duration};

#[derive(Clone, Copy)]
pub(super) enum Fault {
    MissingModel,
    WrongModel,
    MissingTerminal,
}

pub(super) fn fault(mut response: Response, fault: Fault) -> Response {
    let mutate = |body: &mut Value| match fault {
        Fault::MissingModel => {
            body.as_object_mut().unwrap().remove("model");
        }
        Fault::WrongModel => body["model"] = json!("not-the-qualified-snapshot"),
        Fault::MissingTerminal => {
            body.as_object_mut().unwrap().remove("status");
        }
    };
    if let Some(body) = &mut response.body {
        mutate(body);
    }
    if let Some(lines) = &mut response.sse {
        let mut events = Vec::new();
        for raw in lines.iter().flat_map(|raw| raw.lines()) {
            let Some(data) = raw.strip_prefix("data:") else {
                continue;
            };
            if data.trim() == "[DONE]" {
                continue;
            }
            let mut event: Value = serde_json::from_str(data).unwrap();
            if matches!(fault, Fault::MissingTerminal) && event["type"] == "response.completed" {
                continue;
            }
            if let Some(body) = event.get_mut("response") {
                mutate(body);
            }
            events.push(format!("data: {event}\n\n"));
        }
        *lines = events;
    }
    response
}

pub(super) async fn assert_failed(result: ExecutorResult<Either<ResponsePayload, BoxStream>>) {
    match result {
        Err(error) => {
            assert_eq!(error.http_status(), http::StatusCode::BAD_GATEWAY);
            assert!(!format!("{error:?} {error}").contains(AUTH));
        }
        Ok(Either::Left(_)) => panic!("fault must not complete"),
        Ok(Either::Right(mut stream)) => {
            let mut errors = 0;
            let mut count = 0;
            while let Some(frame) = tokio::time::timeout(Duration::from_secs(5), stream.next())
                .await
                .unwrap()
            {
                count += 1;
                assert!(count <= 512);
                for data in frame.lines().filter_map(|line| line.strip_prefix("data:")) {
                    if data.trim() == "[DONE]" {
                        continue;
                    }
                    let event: Value = serde_json::from_str(data).unwrap();
                    assert!(event["type"] != "response.completed", "failed round cannot complete");
                    if event["type"] == "error" {
                        errors += 1;
                        assert!(!frame.contains(AUTH));
                    }
                }
            }
            assert_eq!(errors, 1);
        }
    }
}

#[tokio::test]
async fn pinned_execution_bad_round_cannot_persist_or_publish_a_fork() {
    for streaming in [false, true] {
        for failure in [Fault::MissingModel, Fault::WrongModel, Fault::MissingTerminal] {
            let capture = capture("continuation", streaming);
            let fixture = Fixture::new([
                capture.turns[0].response.clone(),
                fault(capture.turns[1].response.clone(), failure),
                capture.turns[2].response.clone(),
            ])
            .await;
            let group = group();
            let source = group.new_session().unwrap();
            let failed = group.new_session().unwrap();
            let survivor = group.new_session().unwrap();
            let first = collect(
                fixture
                    .run(request(&capture.turns[0], false), Some(&source))
                    .await
                    .unwrap(),
            )
            .await;
            let prefix = capture.turns[0].request.body["input"].as_array().unwrap().len() + first.output.len();
            assert_failed(
                fixture
                    .run(child(&capture.turns[1], prefix, &first.id, true), Some(&failed))
                    .await,
            )
            .await;
            assert_eq!(fixture.row_count().await, 0, "failed fork must not be stored");
            let probe = ExecuteRequest::new(request(&capture.turns[0], false), Arc::clone(&fixture.context))
                .with_session(&failed)
                .expect("failed fork released its lease");
            drop(probe);
            collect(
                fixture
                    .run(child(&capture.turns[2], prefix, &first.id, true), Some(&survivor))
                    .await
                    .unwrap(),
            )
            .await;
            assert_eq!(fixture.row_count().await, 1);
            let sent = fixture.requests().await;
            assert_eq!(sent.len(), 3);
            assert_request(&capture.turns[2], &sent[2]);
            fixture.stop().await;
        }
    }
}

#[tokio::test]
async fn pinned_execution_active_drop_discards_partial_round_and_releases_lease() {
    let capture = capture("continuation", true);
    let fixture = Fixture::new([capture.turns[0].response.clone()]).await;
    let group = group();
    let session = group.new_session().unwrap();
    let mut stream = fixture
        .run(request(&capture.turns[0], true), Some(&session))
        .await
        .unwrap()
        .right()
        .unwrap();
    let event = tokio::time::timeout(Duration::from_secs(5), stream.next())
        .await
        .unwrap()
        .unwrap();
    assert!(event.contains("response.created"));
    drop(stream);
    let probe = ExecuteRequest::new(request(&capture.turns[0], false), Arc::clone(&fixture.context))
        .with_session(&session)
        .expect("active stream drop releases lease synchronously");
    drop(probe);
    assert_eq!(fixture.row_count().await, 0);
    assert_eq!(fixture.requests().await.len(), 1);
    let mut missing = request(&capture.turns[0], false);
    missing.previous_response_id = Some("resp_never_published".to_owned());
    assert!(matches!(
        fixture.run(missing, Some(&session)).await,
        Err(ExecutorError::PreviousResponseNotFound { .. })
    ));
    fixture.stop().await;
}
