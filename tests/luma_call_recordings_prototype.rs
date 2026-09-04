#[path = "../src/bin/luma_call_recordings/common.rs"]
#[allow(dead_code)]
mod common;
#[path = "../src/bin/luma_call_recordings/protocol.rs"]
#[allow(dead_code)]
mod protocol;

#[test]
fn protocol_self_test_is_hermetic() {
    protocol::self_test().expect("the prototype protocol boundaries should remain self-consistent");
}
