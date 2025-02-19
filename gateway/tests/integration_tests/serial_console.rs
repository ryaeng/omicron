// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

// Copyright 2022 Oxide Computer Company

use super::current_simulator_state;
use super::setup;
use super::SpStateExt;
use dropshot::Method;
use futures::prelude::*;
use gateway_messages::SpPort;
use http::uri::Scheme;
use http::StatusCode;
use http::Uri;
use omicron_gateway::http_entrypoints::SpType;
use sp_sim::Gimlet;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite;
use tokio_tungstenite::tungstenite::protocol::Message;

// simulated gimlets expose their serial console over TCP; this function spawns
// a task to read/write on the serial console, returning channels for the caller
async fn sim_sp_serial_console(
    gimlet: &Gimlet,
) -> (mpsc::Sender<Vec<u8>>, mpsc::Receiver<Vec<u8>>) {
    let mut conn =
        TcpStream::connect(gimlet.serial_console_addr("sp3-host-cpu").unwrap())
            .await
            .unwrap();

    let (console_write, mut console_write_rx) = mpsc::channel::<Vec<u8>>(16);
    let (console_read_tx, console_read) = mpsc::channel::<Vec<u8>>(16);
    tokio::spawn(async move {
        let mut buf = [0; 128];
        loop {
            tokio::select! {
                to_write = console_write_rx.recv() => {
                    match to_write {
                        Some(data) => {
                            conn.write_all(&data).await.unwrap();
                        }
                        None => return,
                    }
                }

                read_success = conn.read(&mut buf) => {
                    let n = read_success.unwrap();
                    console_read_tx.send(buf[..n].to_vec()).await.unwrap();
                }
            }
        }
    });

    (console_write, console_read)
}

#[tokio::test]
async fn serial_console_communication() {
    let testctx =
        setup::test_setup("serial_console_communication", SpPort::One).await;
    let client = &testctx.client;
    let simrack = &testctx.simrack;

    // sanity check: we have at least 1 gimlet, and all SPs are enabled
    let sim_state = current_simulator_state(simrack).await;
    assert!(sim_state.iter().any(|sp| sp.info.id.typ == SpType::Sled));
    assert!(sim_state.iter().all(|sp| sp.details.is_enabled()));

    // connect to sled 0's serial console
    let (console_write, mut console_read) =
        sim_sp_serial_console(&simrack.gimlets[0]).await;

    // connect to the MGS websocket for this gimlet
    let url = {
        let mut parts = client
            .url("/sp/sled/0/component/sp3-host-cpu/serial-console/attach")
            .into_parts();
        parts.scheme = Some(Scheme::try_from("ws").unwrap());
        Uri::from_parts(parts).unwrap()
    };
    let (mut ws, _resp) = tokio_tungstenite::connect_async(url).await.unwrap();

    for i in 0..8 {
        let msg_from_mgs = format!("hello from MGS {}", i).into_bytes();
        let msg_from_sp = format!("hello from SP {}", i).into_bytes();

        // confirm messages sent to the websocket are received on the console
        // TCP connection
        ws.send(Message::Binary(msg_from_mgs.clone())).await.unwrap();
        assert_eq!(console_read.recv().await.unwrap(), msg_from_mgs);

        // confirm messages sent to the console TCP connection are received by
        // the websocket
        console_write.send(msg_from_sp.clone()).await.unwrap();
        assert_eq!(
            ws.next().await.unwrap().unwrap(),
            Message::Binary(msg_from_sp)
        );
    }

    testctx.teardown().await;
}

#[tokio::test]
async fn serial_console_detach() {
    let testctx = setup::test_setup("serial_console_detach", SpPort::One).await;
    let client = &testctx.client;
    let simrack = &testctx.simrack;

    // sanity check: we have at least 1 gimlet, and all SPs are enabled
    let sim_state = current_simulator_state(simrack).await;
    assert!(sim_state.iter().any(|sp| sp.info.id.typ == SpType::Sled));
    assert!(sim_state.iter().all(|sp| sp.details.is_enabled()));

    // connect to sled 0's serial console
    let (console_write, mut console_read) =
        sim_sp_serial_console(&simrack.gimlets[0]).await;

    // connect to the MGS websocket for this gimlet
    let attach_url = {
        let mut parts = client
            .url("/sp/sled/0/component/sp3-host-cpu/serial-console/attach")
            .into_parts();
        parts.scheme = Some(Scheme::try_from("ws").unwrap());
        Uri::from_parts(parts).unwrap()
    };
    let (mut ws, _resp) =
        tokio_tungstenite::connect_async(attach_url.clone()).await.unwrap();

    // attempting to connect while the first connection is still open should
    // fail
    let err =
        tokio_tungstenite::connect_async(attach_url.clone()).await.unwrap_err();
    match err {
        tungstenite::Error::Http(resp) => {
            assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        }
        tungstenite::Error::ConnectionClosed
        | tungstenite::Error::AlreadyClosed
        | tungstenite::Error::Io(_)
        | tungstenite::Error::Tls(_)
        | tungstenite::Error::Capacity(_)
        | tungstenite::Error::Protocol(_)
        | tungstenite::Error::SendQueueFull(_)
        | tungstenite::Error::Utf8
        | tungstenite::Error::Url(_)
        | tungstenite::Error::HttpFormat(_) => panic!("unexpected error"),
    }

    // the original websocket should still work
    ws.send(Message::Binary(b"hello".to_vec())).await.unwrap();
    assert_eq!(console_read.recv().await.unwrap(), b"hello");
    console_write.send(b"world".to_vec()).await.unwrap();
    assert_eq!(
        ws.next().await.unwrap().unwrap(),
        Message::Binary(b"world".to_vec())
    );

    // hit the detach endpoint, which should disconnect `ws`
    let detach_url = format!(
        "{}",
        client.url("/sp/sled/0/component/sp3-host-cpu/serial-console/detach")
    );
    client
        .make_request_no_body(Method::POST, &detach_url, StatusCode::NO_CONTENT)
        .await
        .unwrap();
    match ws.next().await {
        Some(Ok(Message::Close(Some(frame)))) => {
            assert_eq!(frame.reason, "serial console was detached");
        }
        other => panic!("unexpected websocket message {:?}", other),
    }

    // we should now be able to rettach
    let (mut ws, _resp) =
        tokio_tungstenite::connect_async(attach_url.clone()).await.unwrap();
    ws.send(Message::Binary(b"hello".to_vec())).await.unwrap();
    assert_eq!(console_read.recv().await.unwrap(), b"hello");
    console_write.send(b"world".to_vec()).await.unwrap();
    assert_eq!(
        ws.next().await.unwrap().unwrap(),
        Message::Binary(b"world".to_vec())
    );

    testctx.teardown().await;
}
