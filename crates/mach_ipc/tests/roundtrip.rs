//! End-to-end tests over a real Mach service.
//!
//! Every test drives both ends for real — bind a name, connect to it from
//! another task, exchange typed values — because the interesting failures here
//! (descriptor layout, port right accounting, edge-triggered wakeups) are
//! invisible to anything that stops short of a full round trip.

use futures_lite::StreamExt;
use futures_lite::future::block_on;
use paneru_mach_ipc::{Error, Receiver, Sender};
use serde::{Deserialize, Serialize};

#[derive(Debug, PartialEq, Serialize, Deserialize)]
enum Request {
    Query(String),
    Shout(Vec<u8>),
    Subscribe,
    Nothing,
}

#[derive(Debug, PartialEq, Serialize, Deserialize)]
struct Response {
    ok: bool,
    detail: String,
}

#[derive(Debug, PartialEq, Serialize, Deserialize)]
struct Event {
    seq: u32,
}

/// Service names must not collide between concurrently running tests, and a name
/// left behind by a crashed run must not fail the next one — so each test gets
/// its own, keyed by the test name and this process's pid.
fn service_name(test: &str) -> String {
    format!("com.karinushka.paneru.test.{test}.{}", std::process::id())
}

/// Connects, tolerating the receiver not having bound the name quite yet.
fn connect<T: Serialize>(name: &str) -> Sender<T> {
    loop {
        match Sender::connect(name) {
            Ok(sender) => return sender,
            Err(Error::NotRunning) => std::thread::yield_now(),
            Err(err) => panic!("connect: {err}"),
        }
    }
}

#[test]
fn a_call_gets_its_reply() {
    let name = service_name("call");
    let receiver = Receiver::<Request>::bind(&name).expect("bind");

    let client_name = name.clone();
    let client = std::thread::spawn(move || {
        let sender = connect::<Request>(&client_name);
        block_on(sender.call::<Response>(&Request::Query("displays".into()))).expect("a reply")
    });

    let delivery = block_on(receiver.recv()).expect("receive");
    assert_eq!(delivery.value, Request::Query("displays".into()));
    delivery
        .reply
        .expect("the sender asked for a reply")
        .send(&Response {
            ok: true,
            detail: "two".into(),
        })
        .expect("reply");

    assert_eq!(
        client.join().expect("the client thread"),
        Response {
            ok: true,
            detail: "two".into()
        }
    );
}

/// The reply must survive being moved to another task. This is the property the
/// daemon depends on: its receive loop hands the reply off and goes straight
/// back to receiving, so the answer is almost never produced where the request
/// was read.
#[test]
fn a_reply_can_be_sent_from_another_thread() {
    let name = service_name("offthread");
    let receiver = Receiver::<Request>::bind(&name).expect("bind");

    let client_name = name.clone();
    let client = std::thread::spawn(move || {
        let sender = connect::<Request>(&client_name);
        block_on(sender.call::<Response>(&Request::Nothing)).expect("a reply")
    });

    let delivery = block_on(receiver.recv()).expect("receive");
    let reply = delivery.reply.expect("a reply channel");
    std::thread::spawn(move || {
        reply
            .send(&Response {
                ok: true,
                detail: "elsewhere".into(),
            })
            .expect("reply");
    })
    .join()
    .expect("the replying thread");

    assert_eq!(client.join().expect("the client thread").detail, "elsewhere");
}

/// Payloads travel out of line, so they have to hold well past anything that
/// would fit inline in a Mach message.
#[test]
fn a_large_value_survives() {
    let name = service_name("large");
    let receiver = Receiver::<Request>::bind(&name).expect("bind");

    // Past the inline limit and not a page multiple, so an off-by-one in the
    // descriptor size would show up.
    let big = vec![0xab_u8; 1_000_003];
    let expected = big.clone();

    let client_name = name.clone();
    let client = std::thread::spawn(move || {
        let sender = connect::<Request>(&client_name);
        block_on(sender.call::<Response>(&Request::Shout(big))).expect("a reply")
    });

    let delivery = block_on(receiver.recv()).expect("receive");
    match delivery.value {
        Request::Shout(got) => assert_eq!(got, expected),
        other => panic!("expected a Shout, got {other:?}"),
    }
    delivery.reply.expect("a reply channel").send(&Response {
        ok: true,
        detail: String::new(),
    })
    .expect("reply");

    assert!(client.join().expect("the client thread").ok);
}

/// A fire-and-forget value carries no reply channel, and the receiver must see
/// that rather than trying to answer into nothing.
#[test]
fn a_send_has_no_reply_channel() {
    let name = service_name("oneway");
    let receiver = Receiver::<Request>::bind(&name).expect("bind");

    let client_name = name.clone();
    std::thread::spawn(move || {
        let sender = connect::<Request>(&client_name);
        block_on(sender.send(&Request::Nothing)).expect("send");
    });

    let delivery = block_on(receiver.recv()).expect("receive");
    assert_eq!(delivery.value, Request::Nothing);
    assert!(delivery.reply.is_none());
    assert!(delivery.subscriber.is_none());
}

/// The subscription path: the sender hands over a channel that outlives the
/// value delivering it, and the service pushes to it.
#[test]
fn a_subscriber_receives_pushed_events() {
    let name = service_name("subscribe");
    let receiver = Receiver::<Request>::bind(&name).expect("bind");

    let client_name = name.clone();
    let client = std::thread::spawn(move || {
        let sender = connect::<Request>(&client_name);
        let events = block_on(sender.subscribe::<Event>(&Request::Subscribe)).expect("subscribe");
        block_on(events.recv()).expect("an event").value
    });

    let delivery = block_on(receiver.recv()).expect("receive");
    assert_eq!(delivery.value, Request::Subscribe);
    let subscriber = delivery.subscriber.expect("a subscriber channel");
    subscriber.try_send(&Event { seq: 7 }).expect("push");

    assert_eq!(client.join().expect("the client thread"), Event { seq: 7 });
}

/// A subscription is a stream, which is how `paneru subscribe` consumes it.
#[test]
fn a_subscription_is_a_stream_of_events() {
    let name = service_name("stream");
    let receiver = Receiver::<Request>::bind(&name).expect("bind");

    let client_name = name.clone();
    let client = std::thread::spawn(move || {
        let sender = connect::<Request>(&client_name);
        let mut events =
            block_on(sender.subscribe::<Event>(&Request::Subscribe)).expect("subscribe");
        block_on(async {
            let mut seen = Vec::new();
            while let Some(event) = events.next().await {
                seen.push(event.expect("an event").value.seq);
                if seen.len() == 3 {
                    break;
                }
            }
            seen
        })
    });

    let delivery = block_on(receiver.recv()).expect("receive");
    let subscriber = delivery.subscriber.expect("a subscriber channel");
    for seq in 1..=3 {
        subscriber.try_send(&Event { seq }).expect("push");
    }

    assert_eq!(client.join().expect("the client thread"), vec![1, 2, 3]);
}

/// The receiver is a stream too, and consecutive values must each wake it —
/// this is what catches a wakeup registration that only ever fires once.
#[test]
fn a_receiver_is_a_stream_of_deliveries() {
    let name = service_name("recvstream");
    let mut receiver = Receiver::<Request>::bind(&name).expect("bind");

    let client_name = name.clone();
    std::thread::spawn(move || {
        let sender = connect::<Request>(&client_name);
        for n in 0..5 {
            block_on(sender.send(&Request::Query(n.to_string()))).expect("send");
        }
    });

    let seen = block_on(async {
        let mut seen = Vec::new();
        while let Some(delivery) = receiver.next().await {
            match delivery.expect("a delivery").value {
                Request::Query(n) => seen.push(n),
                other => panic!("unexpected {other:?}"),
            }
            if seen.len() == 5 {
                break;
            }
        }
        seen
    });

    assert_eq!(seen, ["0", "1", "2", "3", "4"]);
}

/// The whole point of the subscriber design: a dead client is reported as
/// [`Error::PeerGone`], so it can be reaped for the right reason rather than
/// inferred from a failed write a slow reader would also produce.
#[test]
fn a_dead_subscriber_is_reported_as_gone() {
    let name = service_name("death");
    let receiver = Receiver::<Request>::bind(&name).expect("bind");

    let client_name = name.clone();
    let client = std::thread::spawn(move || {
        let sender = connect::<Request>(&client_name);
        let events = block_on(sender.subscribe::<Event>(&Request::Subscribe)).expect("subscribe");
        // Dropping the receiving end is exactly what a client exiting does.
        drop(events);
    });

    let delivery = block_on(receiver.recv()).expect("receive");
    let subscriber = delivery.subscriber.expect("a subscriber channel");
    client.join().expect("the client thread");

    match subscriber.try_send(&Event { seq: 1 }) {
        Err(Error::PeerGone) => {}
        other => panic!("expected PeerGone, got {other:?}"),
    }
}

/// A value that does not decode as `T` is one bad client, not a dead service —
/// it must be reported without poisoning the receiver.
#[test]
fn a_mistyped_value_is_reported_and_survivable() {
    let name = service_name("mistyped");
    let receiver = Receiver::<Request>::bind(&name).expect("bind");

    let client_name = name.clone();
    std::thread::spawn(move || {
        // A type the receiver cannot possibly decode as `Request`.
        let wrong = connect::<String>(&client_name);
        block_on(wrong.send(&"not a request at all".to_string())).expect("send");
        let right = connect::<Request>(&client_name);
        block_on(right.send(&Request::Nothing)).expect("send");
    });

    match block_on(receiver.recv()) {
        Err(Error::Decode) => {}
        other => panic!("expected Decode, got {other:?}"),
    }
    // The service is still usable.
    assert_eq!(
        block_on(receiver.recv()).expect("the next value").value,
        Request::Nothing
    );
}

/// A second binder must be refused rather than take the name over — the failure
/// the socket transport could not detect, because it simply unlinked whatever it
/// found at the path.
#[test]
fn a_second_bind_is_refused() {
    let name = service_name("conflict");
    let _first = Receiver::<Request>::bind(&name).expect("bind");

    match Receiver::<Request>::bind(&name) {
        Err(Error::AlreadyRunning) => {}
        Ok(_) => panic!("the second bind should have been refused"),
        Err(err) => panic!("expected AlreadyRunning, got {err:?}"),
    }
}

/// Connecting when nothing is bound is the ordinary "paneru isn't running" case,
/// and has to be distinguishable from every other failure.
#[test]
fn connecting_to_nothing_reports_not_running() {
    match Sender::<Request>::connect(&service_name("absent")) {
        Err(Error::NotRunning) => {}
        Ok(_) => panic!("connected to a service that does not exist"),
        Err(err) => panic!("expected NotRunning, got {err:?}"),
    }
}
