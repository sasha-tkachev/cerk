use cerk::kernel::{BrokerEvent, CloudEventRoutingArgs, Config, IncomingCloudEvent};
use cerk::runtime::channel::{BoxedReceiver, BoxedSender};
use cerk::runtime::{InternalServerFn, InternalServerFnRefStatic, InternalServerId};
use cloudevents::Event;
use serde_json;
use std::io::{BufRead, BufReader};
use std::os::unix::net::{UnixListener, UnixStream};
use std::time::Duration;

fn liten_to_stream(
    id: &InternalServerId,
    listener: &UnixListener,
    mut stream: Option<BufReader<UnixStream>>,
    sender_to_kernel: &BoxedSender,
    max_tries: usize,
) -> Option<BufReader<UnixStream>> {
    if max_tries == 0 {
        panic!("too many failures while trying to connect to stream");
    }
    debug!("listen to stream...");
    match stream.as_mut() {
        None => match listener.accept() {
            Ok((socket, _)) => {
                let stream = BufReader::new(socket);
                liten_to_stream(id, listener, Some(stream), sender_to_kernel, max_tries - 1)
            }
            Err(err) => std::panic::panic_any(err),
        },
        Some(stream) => {
            let mut line = String::new();

            loop {
                match stream.read_line(&mut line) {
                    Ok(0) => break,
                    Err(err) => {
                        error!("{} read_line error {:?}", id, err);
                        break;
                    }
                    Ok(_) => {
                        debug!("{} received new line", id);
                        match serde_json::from_str::<Event>(&line) {
                            Ok(cloud_event) => {
                                debug!("{} deserialized event successfully", id);
                                sender_to_kernel.send(BrokerEvent::IncomingCloudEvent(
                                    IncomingCloudEvent {
                                        routing_id: id.clone(),
                                        incoming_id: id.clone(),
                                        cloud_event,
                                        args: CloudEventRoutingArgs::default(),
                                    },
                                ))
                            }
                            Err(err) => {
                                error!("{} while converting string to CloudEvent: {:?}", id, err);
                            }
                        }
                    }
                }
                line.clear();
            }
            None
        }
    }
}

/// This is the main function to start the port.
///
/// This port reads CloudEvents form a UNIX Socket and sens them to the Kernel.
///
/// # Configurations
///
/// The Socket expects a `Config::String` as configuration.
/// The string should be a file path where the UNIX Socket should be created.
///
/// e.g. `Config::String(String::from("path/to/the/socket"))`
///
/// # Examples
///
/// * [UNIX Socket Example](https://github.com/ce-rust/cerk/tree/master/examples/examples/src/unix_socket)
///
/// # Limitations
///
/// * **reliability** this port does not support any `DeliveryGuarantee` other then `BestEffort` and so does never send a `IncomingCloudEventProcessed` message
///
/// # open issues
///
/// * https://github.com/ce-rust/cerk/issues/25
///
pub fn port_input_unix_socket_json_start(
    id: InternalServerId,
    inbox: BoxedReceiver,
    sender_to_kernel: BoxedSender,
) {
    info!("start input JSON over unix socket port with id {}", id);
    let mut listener: Option<UnixListener> = None;
    let mut stream: Option<BufReader<UnixStream>> = None;

    loop {
        if let Some(broker_event) = inbox.receive_timeout(Duration::from_millis(100)) {
            match broker_event {
                BrokerEvent::Init => {
                    info!("{} initiated", id);
                }
                BrokerEvent::ConfigUpdated(config, _) => {
                    info!("{} received ConfigUpdated", id);
                    match config {
                        Config::String(socket_path) => {
                            listener = Some(UnixListener::bind(socket_path).unwrap());
                        }
                        _ => error!("{} received invalid config", id),
                    };
                }
                broker_event => warn!("event {} not implemented", broker_event),
            }
        }

        if let Some(listener) = listener.as_ref() {
            stream = liten_to_stream(&id, listener, stream, &sender_to_kernel, 10);
        }
    }
}

/// This is the pointer for the main function to start the port.
pub static PORT_INPUT_UNIX_SOCKET: InternalServerFnRefStatic =
    &(port_input_unix_socket_json_start as InternalServerFn);
