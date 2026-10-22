#![no_main]

//! Feeds arbitrary bytes to every decoder a peer can reach. Each decode must return an error or a
//! value; a panic or a crash is a bug. Run with `cargo +nightly fuzz run decode` from `fuzz/`.

use groovy_core::groups::{GroupEnvelope, GroupPage, ReceivedPage};
use groovy_core::membership::{KeyRecord, Record};
use groovy_core::messages::WireMessage;
use groovy_core::protocol::{FileOffer, Hello, Reply, Request};
use groovy_core::user::DeviceCert;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let _ = postcard::from_bytes::<Request>(data);
    let _ = postcard::from_bytes::<Reply>(data);
    let _ = postcard::from_bytes::<Hello>(data);
    let _ = postcard::from_bytes::<FileOffer>(data);
    let _ = postcard::from_bytes::<WireMessage>(data);
    let _ = postcard::from_bytes::<GroupEnvelope>(data);
    let _ = postcard::from_bytes::<GroupPage>(data);
    let _ = postcard::from_bytes::<ReceivedPage>(data);
    let _ = postcard::from_bytes::<KeyRecord>(data);
    let _ = postcard::from_bytes::<Record>(data);
    let _ = postcard::from_bytes::<DeviceCert>(data);
});
