#[macro_use]
extern crate log;
use cerk::runtime::InternalServerId;
use cerk::runtime::{BoxedReceiver, BoxedSender};

pub fn start_routing(id: InternalServerId, inbox: BoxedReceiver, sender_to_kernel: BoxedSender) {
    info!("start broadcast router");
}
