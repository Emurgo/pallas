use std::fs;
use std::net::{Ipv4Addr, SocketAddrV4};
use std::time::Duration;

use pallas_network::facades::{NodeClient, PeerClient};
use pallas_network::miniprotocols::blockfetch::BlockRequest;
use pallas_network::miniprotocols::handshake::n2c;
use pallas_network::miniprotocols::handshake::n2n::VersionData;
use pallas_network::miniprotocols::localstate::queries::{GenericResponse, Request};
use pallas_network::miniprotocols::localstate::{ClientAcquireRequest, ClientQueryRequest};
use pallas_network::miniprotocols::{
    blockfetch,
    chainsync::{self, NextResponse},
    Point,
};
use pallas_network::miniprotocols::{handshake, localstate};
use pallas_network::multiplexer::{Bearer, Plexer};
use std::path::Path;
use tokio::net::TcpListener;

#[tokio::test]
#[ignore]
pub async fn chainsync_history_happy_path() {
    let mut peer = PeerClient::connect("preview-node.world.dev.cardano.org:30002", 2)
        .await
        .unwrap();

    let client = peer.chainsync();

    let known_point = Point::Specific(
        1654413,
        hex::decode("7de1f036df5a133ce68a82877d14354d0ba6de7625ab918e75f3e2ecb29771c2").unwrap(),
    );

    let (point, _) = client
        .find_intersect(vec![known_point.clone()])
        .await
        .unwrap();

    println!("{:?}", point);

    assert!(matches!(client.state(), chainsync::State::Idle));

    match point {
        Some(point) => assert_eq!(point, known_point),
        None => panic!("expected point"),
    }

    let next = client.request_next().await.unwrap();

    match next {
        NextResponse::RollBackward(point, _) => assert_eq!(point, known_point),
        _ => panic!("expected rollback"),
    }

    assert!(matches!(client.state(), chainsync::State::Idle));

    for _ in 0..10 {
        let next = client.request_next().await.unwrap();

        match next {
            NextResponse::RollForward(_, _) => (),
            _ => panic!("expected roll-forward"),
        }

        assert!(matches!(client.state(), chainsync::State::Idle));
    }

    client.send_done().await.unwrap();

    assert!(matches!(client.state(), chainsync::State::Done));
}

#[tokio::test]
#[ignore]
pub async fn chainsync_tip_happy_path() {
    let mut peer = PeerClient::connect("preview-node.world.dev.cardano.org:30002", 2)
        .await
        .unwrap();

    let client = peer.chainsync();

    client.intersect_tip().await.unwrap();

    assert!(matches!(client.state(), chainsync::State::Idle));

    let next = client.request_next().await.unwrap();

    assert!(matches!(next, NextResponse::RollBackward(..)));

    let mut await_count = 0;

    for _ in 0..4 {
        let next = if client.has_agency() {
            client.request_next().await.unwrap()
        } else {
            await_count += 1;
            client.recv_while_must_reply().await.unwrap()
        };

        match next {
            NextResponse::RollForward(_, _) => (),
            NextResponse::Await => (),
            _ => panic!("expected roll-forward or await"),
        }
    }

    assert!(await_count > 0, "tip was never reached");

    client.send_done().await.unwrap();

    assert!(matches!(client.state(), chainsync::State::Done));
}

#[tokio::test]
#[ignore]
pub async fn blockfetch_happy_path() {
    let mut peer = PeerClient::connect("preview-node.world.dev.cardano.org:30002", 2)
        .await
        .unwrap();

    let client = peer.blockfetch();

    let known_point = Point::Specific(
        1654413,
        hex::decode("7de1f036df5a133ce68a82877d14354d0ba6de7625ab918e75f3e2ecb29771c2").unwrap(),
    );

    let range_ok = client
        .request_range((known_point.clone(), known_point))
        .await;

    assert!(matches!(client.state(), blockfetch::State::Streaming));

    println!("streaming...");

    assert!(matches!(range_ok, Ok(_)));

    for _ in 0..1 {
        let next = client.recv_while_streaming().await.unwrap();

        match next {
            Some(body) => assert_eq!(body.len(), 3251),
            _ => panic!("expected block body"),
        }

        assert!(matches!(client.state(), blockfetch::State::Streaming));
    }

    let next = client.recv_while_streaming().await.unwrap();

    assert!(matches!(next, None));

    client.send_done().await.unwrap();

    assert!(matches!(client.state(), blockfetch::State::Done));
}

#[tokio::test]
#[ignore]
pub async fn blockfetch_server_and_client_happy_path() {
    let block_bodies = vec![
        hex::decode("deadbeefdeadbeef").unwrap(),
        hex::decode("c0ffeec0ffeec0ffee").unwrap(),
    ];

    let point = Point::Specific(
        1337,
        hex::decode("deadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeef").unwrap(),
    );

    let server = tokio::spawn({
        let bodies = block_bodies.clone();
        let point = point.clone();
        async move {
            // server setup

            let server_listener = TcpListener::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 30001))
                .await
                .unwrap();

            let (bearer, _) = Bearer::accept_tcp(server_listener).await.unwrap();

            let mut server_plexer = Plexer::new(bearer);

            let mut server_hs: handshake::Server<VersionData> =
                handshake::Server::new(server_plexer.subscribe_server(0));
            let mut server_bf = blockfetch::Server::new(server_plexer.subscribe_server(3));

            tokio::spawn(async move { server_plexer.run().await });

            server_hs.receive_proposed_versions().await.unwrap();
            server_hs
                .accept_version(10, VersionData::new(0, false))
                .await
                .unwrap();

            // server receives range from client, sends blocks

            let BlockRequest(range_request) = server_bf.recv_while_idle().await.unwrap().unwrap();

            assert_eq!(range_request, (point.clone(), point.clone()));
            assert_eq!(*server_bf.state(), blockfetch::State::Busy);

            server_bf.send_block_range(bodies).await.unwrap();

            assert_eq!(*server_bf.state(), blockfetch::State::Idle);

            // server receives range from client, sends NoBlocks

            let BlockRequest(_) = server_bf.recv_while_idle().await.unwrap().unwrap();

            server_bf.send_block_range(vec![]).await.unwrap();

            assert_eq!(*server_bf.state(), blockfetch::State::Idle);

            assert!(server_bf.recv_while_idle().await.unwrap().is_none());

            assert_eq!(*server_bf.state(), blockfetch::State::Done);
        }
    });

    let client = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(1)).await;

        // client setup

        let mut client_to_server_conn = PeerClient::connect("localhost:30001", 0).await.unwrap();

        let client_bf = client_to_server_conn.blockfetch();

        // client sends request range

        client_bf
            .send_request_range((point.clone(), point.clone()))
            .await
            .unwrap();

        assert!(client_bf.recv_while_busy().await.unwrap().is_some());

        // client receives blocks until idle

        let mut received_bodies = Vec::new();

        while let Some(received_body) = client_bf.recv_while_streaming().await.unwrap() {
            received_bodies.push(received_body)
        }

        assert_eq!(received_bodies, block_bodies);

        // client sends request range

        client_bf
            .send_request_range((point.clone(), point.clone()))
            .await
            .unwrap();

        // recv_while_busy returns None for NoBlocks message
        assert!(client_bf.recv_while_busy().await.unwrap().is_none());

        // client sends done

        client_bf.send_done().await.unwrap();
    });

    _ = tokio::join!(client, server);
}

#[tokio::test]
#[ignore]
pub async fn local_state_query_server_and_client_happy_path() {
    let server = tokio::spawn({
        async move {
            // server setup
            let socket_path = Path::new("node.socket");

            if socket_path.exists() {
                fs::remove_file(&socket_path).unwrap();
            }

            let (bearer, _) = Bearer::accept_unix(socket_path).await.unwrap();

            let mut server_plexer = Plexer::new(bearer);

            let mut server_hs: handshake::Server<n2c::VersionData> =
                handshake::Server::new(server_plexer.subscribe_server(0));

            let mut server_sq: localstate::Server =
                localstate::Server::new(server_plexer.subscribe_server(7));

            tokio::spawn(async move { server_plexer.run().await });

            server_hs.receive_proposed_versions().await.unwrap();
            server_hs
                .accept_version(10, n2c::VersionData::new(0, Some(false)))
                .await
                .unwrap();

            // server receives range from client, sends blocks

            let ClientAcquireRequest(maybe_point) =
                server_sq.recv_while_idle().await.unwrap().unwrap();

            assert_eq!(maybe_point, Some(Point::Origin));
            assert_eq!(*server_sq.state(), localstate::State::Acquiring);

            // server_bf.send_block_range(bodies).await.unwrap();

            server_sq.send_acquired().await.unwrap();

            assert_eq!(*server_sq.state(), localstate::State::Acquired);

            // server receives query from client

            let query = match server_sq.recv_while_acquired().await.unwrap() {
                ClientQueryRequest::Query(q) => q,
                x => panic!("unexpected message from client: {x:?}"),
            };

            assert_eq!(
                query,
                Request::BlockQuery(localstate::queries::BlockQuery::GetStakePools)
            );

            assert_eq!(*server_sq.state(), localstate::State::Querying);

            server_sq
                .send_result(GenericResponse::new(hex::decode("82011A008BD423").unwrap()))
                .await
                .unwrap();

            assert_eq!(*server_sq.state(), localstate::State::Acquired);

            // server receives reaquire from the client

            let maybe_point = match server_sq.recv_while_acquired().await.unwrap() {
                ClientQueryRequest::ReAcquire(p) => p,
                x => panic!("unexpected message from client: {x:?}"),
            };

            assert_eq!(maybe_point, Some(Point::Specific(1337, vec![1, 2, 3])));
            assert_eq!(*server_sq.state(), localstate::State::Acquiring);

            server_sq.send_acquired().await.unwrap();

            // server receives release from the client

            match server_sq.recv_while_acquired().await.unwrap() {
                ClientQueryRequest::Release => (),
                x => panic!("unexpected message from client: {x:?}"),
            };

            assert!(server_sq.recv_while_idle().await.unwrap().is_none());

            assert_eq!(*server_sq.state(), localstate::State::Done);
        }
    });

    let client = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(1)).await;

        // client setup

        let socket_path = "node.socket";

        let mut client_to_server_conn = NodeClient::connect(socket_path, 0).await.unwrap();

        let client_sq = client_to_server_conn.statequery();

        // client sends acquire

        client_sq.send_acquire(Some(Point::Origin)).await.unwrap();

        client_sq.recv_while_acquiring().await.unwrap();

        assert_eq!(*client_sq.state(), localstate::State::Acquired);

        // client sends a BlockQuery

        client_sq
            .send_query(Request::BlockQuery(
                localstate::queries::BlockQuery::GetStakePools,
            ))
            .await
            .unwrap();

        let resp = client_sq.recv_while_querying().await.unwrap();

        assert_eq!(
            resp,
            GenericResponse::new(hex::decode("82011A008BD423").unwrap())
        );

        // client sends a ReAquire

        client_sq
            .send_reacquire(Some(Point::Specific(1337, vec![1, 2, 3])))
            .await
            .unwrap();

        client_sq.recv_while_acquiring().await.unwrap();

        client_sq.send_release().await.unwrap();

        client_sq.send_done().await.unwrap();
    });

    _ = tokio::join!(client, server);
}

// TODO: redo txsubmission client test
