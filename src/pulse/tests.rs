use super::*;

fn control() -> (PulseControl, mpsc::Receiver<PendingRequest>) {
    let (sender, receiver) = mpsc::sync_channel(REQUEST_QUEUE_CAPACITY);
    (
        PulseControl {
            sender,
            cache: Arc::new(RwLock::new(HashMap::new())),
            changes: watch::channel(0).0,
            topology_changes: watch::channel(0).0,
            epoch: Arc::new(AtomicU64::new(1)),
        },
        receiver,
    )
}

#[tokio::test(start_paused = true)]
async fn stalled_request_times_out_and_later_requests_can_succeed() {
    let (control, receiver) = control();
    let caller = control.clone();
    let stalled = tokio::spawn(async move { caller.set_db("MUSIC", -12.0).await });
    tokio::task::yield_now().await;
    let pending = receiver.try_recv().unwrap();

    let error = tokio::time::timeout(REQUEST_TIMEOUT + Duration::from_secs(1), stalled)
        .await
        .expect("the request deadline must include time spent in the queue")
        .unwrap()
        .unwrap_err();
    assert_eq!(error.to_string(), REQUEST_TIMEOUT_MESSAGE);
    assert!(pending.into_current(1).is_none());

    let caller = control.clone();
    let next = tokio::spawn(async move { caller.list_sinks().await });
    tokio::task::yield_now().await;
    let Request::ListSinks { reply } = receiver.try_recv().unwrap().into_current(1).unwrap() else {
        panic!("unexpected request");
    };
    reply.send(Ok(Vec::new())).unwrap();
    assert!(next.await.unwrap().unwrap().is_empty());
}

#[tokio::test]
async fn cancelled_caller_does_not_leave_a_queued_stream_mutation() {
    let (control, receiver) = control();
    let task = tokio::spawn(async move { control.set_sink_input_mute(42, false).await });
    tokio::task::yield_now().await;
    let pending = receiver.try_recv().unwrap();
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    assert!(pending.into_current(1).is_none());
}

#[tokio::test]
async fn queued_mutation_is_rejected_after_a_reconnect() {
    let (control, receiver) = control();
    let caller = control.clone();
    let task = tokio::spawn(async move { caller.set_sink_input_percent(42, 100.0).await });
    tokio::task::yield_now().await;
    let pending = receiver.try_recv().unwrap();
    control.epoch.fetch_add(1, Ordering::AcqRel);

    assert!(pending.into_current(control.connection_epoch()).is_none());
    let error = task.await.unwrap().unwrap_err();
    assert!(error.to_string().contains("connection changed"));
}

#[tokio::test]
async fn stale_stream_snapshot_cannot_queue_a_mutation_on_a_new_connection() {
    let (control, receiver) = control();
    control.epoch.fetch_add(1, Ordering::AcqRel);
    for result in [
        control.set_sink_input_mute_at_epoch(42, false, 1).await,
        control.set_sink_input_percent_at_epoch(42, 100.0, 1).await,
    ] {
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("connection changed"));
    }
    assert!(matches!(
        receiver.try_recv(),
        Err(mpsc::TryRecvError::Empty)
    ));
}

#[tokio::test]
async fn completed_stream_mutation_from_a_prior_connection_is_not_accepted() {
    let (control, receiver) = control();
    let caller = control.clone();
    let task = tokio::spawn(async move { caller.set_sink_input_mute_at_epoch(42, false, 1).await });
    tokio::task::yield_now().await;
    let Request::SetSinkInputMute { reply, .. } =
        receiver.try_recv().unwrap().into_current(1).unwrap()
    else {
        panic!("unexpected request");
    };
    reply
        .send(Ok(StreamState {
            index: 42,
            sink: 1,
            name: String::new(),
            properties: HashMap::new(),
            volumes_percent: vec![100.0],
            muted: false,
            corked: false,
            has_volume: true,
            volume_writable: true,
        }))
        .unwrap();
    control.epoch.fetch_add(1, Ordering::AcqRel);
    assert!(task
        .await
        .unwrap()
        .unwrap_err()
        .to_string()
        .contains("connection changed"));
}

#[tokio::test]
async fn worker_rejects_expired_requests_even_if_the_caller_is_still_listening() {
    let (reply, receiver) = oneshot::channel();
    let pending = PendingRequest {
        epoch: 1,
        deadline: Instant::now(),
        request: Request::SetSinkMute {
            name: "MUSIC".into(),
            muted: false,
            reply,
        },
    };
    assert!(pending.into_current(1).is_none());
    assert_eq!(
        receiver.await.unwrap().unwrap_err().to_string(),
        REQUEST_TIMEOUT_MESSAGE
    );
}
