use super::*;

#[test]
fn management_transport_uses_one_second_and_hooks_use_two_hundred_ms() {
    for (op, timeout) in [("status", 1000), ("advice", 200)] {
        let (stream, mut peer) = UnixStream::pair().unwrap();
        let witness = stream.try_clone().unwrap();
        peer.set_read_timeout(Some(Duration::from_secs(60))).unwrap();
        peer.set_write_timeout(Some(Duration::from_secs(60))).unwrap();
        // Queue the response before entering RPC: no other thread must run
        // during the short transport window. Socket options are shared by dup.
        let response = Response { text: "ready".into(), ..Response::default() };
        writeln!(peer, "{}", serde_json::to_string(&response).unwrap()).unwrap();
        let request = Request {
            op: op.into(),
            scope: "test".into(),
            session: "test".into(),
            seq: 0,
            level: Level::Learn,
            data: Value::Null,
        };
        let response = rpc_on_stream(stream, &request).unwrap();
        assert_eq!(response.text, "ready");
        let expected = Some(Duration::from_millis(timeout));
        assert_eq!(witness.read_timeout().unwrap(), expected, "{op} read deadline");
        assert_eq!(witness.write_timeout().unwrap(), expected, "{op} write deadline");
        let mut line = String::new();
        BufReader::new(peer).read_line(&mut line).unwrap();
        assert_eq!(serde_json::from_str::<Request>(&line).unwrap().op, op);
    }
}

#[test]
fn stalled_service_does_not_hold_coding_hook() {
    let (tx, pending) = mpsc::sync_channel(8);
    let mut hooks = Hooks {
        tx,
        wait: Some(|response, timeout| {
            assert_eq!(timeout, Duration::from_millis(50));
            assert!(matches!(response.try_recv(), Err(mpsc::TryRecvError::Empty)));
            // Advance the injected wait to its deadline while the service
            // remains pending. No scheduling or wall-clock assertion involved.
            Err(mpsc::RecvTimeoutError::Timeout)
        }),
        level: Level::Learn,
        scope: "test".into(),
        session: "test".into(),
        seq: 0,
        cwd: PathBuf::new(),
        root: PathBuf::new(),
        delivered: Vec::new(),
        preferences: String::new(),
        turn_advice: Vec::new(),
        metrics: Metrics::default(),
    };
    assert!(hooks.advice(0).is_empty());
    assert_eq!(hooks.metrics.hooks, 1);
    assert_eq!(hooks.metrics.timeouts, 1);
    let (request, response) = pending.try_recv().unwrap();
    assert_eq!(request.op, "advice");
    assert!(response.send(Ok(Response::default())).is_err(), "timed-out waiter must be dropped");
}
