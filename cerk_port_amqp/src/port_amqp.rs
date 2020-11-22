use amq_protocol_types::LongLongUInt;
use amq_protocol_types::{AMQPValue, ShortString};
use anyhow::{Context, Error, Result};
use cerk::kernel::{
    BrokerEvent, CloudEventMessageRoutingId, CloudEventRoutingArgs, Config, DeliveryGuarantee,
    ProcessingResult,
};
use cerk::runtime::channel::{BoxedReceiver, BoxedSender};
use cerk::runtime::InternalServerId;
use cloudevents::CloudEvent;
use futures_lite::stream::StreamExt;
use futures_lite::{future, FutureExt};
use lapin::message::Delivery;
use lapin::{
    options::*, publisher_confirm::Confirmation, types::FieldTable, BasicProperties, Channel,
    Connection, ConnectionProperties, ExchangeKind,
};
use std::borrow::BorrowMut;
use std::collections::HashMap;
use std::convert::TryFrom;
use std::sync::{Arc, Mutex};

struct PendingDelivery {
    consume_channel_id: String,
    delivery_tag: LongLongUInt,
}

type PendingDeliveries = HashMap<CloudEventMessageRoutingId, PendingDelivery>;

struct AmqpConsumeOptions {
    channel: Option<Channel>,
    ensure_queue: bool,
    bind_to_exchange: Option<String>,
    delivery_guarantee: DeliveryGuarantee,
}

struct AmqpPublishOptions {
    channel: Option<Channel>,
    ensure_exchange: bool,
    delivery_guarantee: DeliveryGuarantee,
}

struct AmqpOptions {
    uri: String,
    consume_channels: HashMap<String, AmqpConsumeOptions>,
    publish_channels: HashMap<String, AmqpPublishOptions>,
}

fn try_get_delivery_option(config: &HashMap<String, Config>) -> Result<DeliveryGuarantee> {
    Ok(match config.get("delivery_guarantee") {
        Some(config) => DeliveryGuarantee::try_from(config)?,
        _ => DeliveryGuarantee::Unspecified,
    })
}

fn build_config(id: &InternalServerId, config: &Config) -> Result<AmqpOptions> {
    match config {
        Config::HashMap(config_map) => {
            let mut options = if let Some(Config::String(uri)) = config_map.get("uri") {
                AmqpOptions {
                    uri: uri.to_string(),
                    consume_channels: HashMap::new(),
                    publish_channels: HashMap::new(),
                }
            } else {
                bail!("No uri option")
            };

            if let Some(Config::Vec(ref consumers)) = config_map.get("consume_channels") {
                for consumer_config in consumers.iter() {
                    if let Config::HashMap(consumer) = consumer_config {
                        let consumer_options = AmqpConsumeOptions {
                            ensure_queue: match consumer.get("ensure_queue") {
                                Some(Config::Bool(b)) => *b,
                                _ => false,
                            },
                            bind_to_exchange: match consumer.get("bind_to_exchange") {
                                Some(Config::String(s)) => Some(s.to_string()),
                                _ => None,
                            },
                            delivery_guarantee: try_get_delivery_option(consumer)?,
                            channel: None,
                        };

                        if let Some(Config::String(name)) = consumer.get("name") {
                            options
                                .consume_channels
                                .insert(name.to_string(), consumer_options);
                        } else {
                            bail!("consume_channels name is not set")
                        }
                    } else {
                        bail!("consume_channels entries have to be of type HashMap")
                    }
                }
            }

            if let Some(Config::Vec(ref publishers)) = config_map.get("publish_channels") {
                for publisher_config in publishers.iter() {
                    if let Config::HashMap(publisher) = publisher_config {
                        let publish_options = AmqpPublishOptions {
                            ensure_exchange: match publisher.get("ensure_exchange") {
                                Some(Config::Bool(b)) => *b,
                                _ => false,
                            },
                            delivery_guarantee: try_get_delivery_option(publisher)?,
                            channel: None,
                        };

                        if let Some(Config::String(name)) = publisher.get("name") {
                            options
                                .publish_channels
                                .insert(name.to_string(), publish_options);
                        } else {
                            bail!("publish_channels name is not set");
                        }
                    } else {
                        bail!("publish_channels entries have to be of type HashMap");
                    }
                }
            }

            Ok(options)
        }
        _ => bail!("{} config has to be of type HashMap"),
    }
}

fn setup_connection(
    id: InternalServerId,
    sender_to_kernel: BoxedSender,
    connection: &Option<Connection>,
    config: Config,
    pending_deliveries: Arc<Mutex<HashMap<String, PendingDelivery>>>,
) -> Result<(Connection, AmqpOptions)> {
    let mut config = match build_config(&id.clone(), &config) {
        Ok(c) => c,
        Err(e) => panic!(e),
    };

    async_global_executor::block_on(async {
        let conn = Connection::connect(
            &config.uri,
            ConnectionProperties::default().with_default_executor(8),
        )
        .await?;

        info!("CONNECTED");

        for (name, channel_options) in config.publish_channels.iter_mut() {
            let channel = conn.create_channel().await?;
            if channel_options.delivery_guarantee.requires_acknowledgment() {
                channel
                    .confirm_select(ConfirmSelectOptions { nowait: false })
                    .await?;
            }
            if channel_options.ensure_exchange {
                let exchange = channel.exchange_declare(
                    name.as_str(),
                    ExchangeKind::Fanout,
                    ExchangeDeclareOptions::default(),
                    FieldTable::default(),
                );
                info!("Declared exchange {:?}", exchange);
            }

            channel_options.channel = Some(channel);
        }

        for (name, channel_options) in config.consume_channels.iter_mut() {
            let channel = conn.create_channel().await?;
            if channel_options.delivery_guarantee.requires_acknowledgment() {
                channel
                    .confirm_select(ConfirmSelectOptions { nowait: false })
                    .await?;
            }
            if channel_options.ensure_queue {
                let queue = channel
                    .queue_declare(
                        name.as_str(),
                        QueueDeclareOptions::default(),
                        FieldTable::default(),
                    )
                    .await?;
                info!("Declared queue {:?}", queue);

                if let Some(exchange) = &channel_options.bind_to_exchange {
                    channel
                        .queue_bind(
                            name.as_str(),
                            exchange.as_str(),
                            "",
                            QueueBindOptions::default(),
                            FieldTable::default(),
                        )
                        .await?;
                }
            }

            let mut consumer = channel
                .basic_consume(
                    name.as_str(),
                    format!("cerk-{}", id.clone()).as_str(),
                    BasicConsumeOptions::default(),
                    FieldTable::default(),
                )
                .await?;
            channel_options.channel = Some(channel);

            let cloned_sender = sender_to_kernel.clone_boxed();
            let cloned_id = id.clone();
            let cloned_delivery_guarantee = channel_options.delivery_guarantee.clone();
            let cloned_name = name.clone();
            let weak_clone = pending_deliveries.clone();
            async_global_executor::spawn(async move {
                info!("will consume");
                while let Some(delivery) = consumer.next().await {
                    receive_message(
                        &cloned_name,
                        &cloned_sender,
                        &cloned_id,
                        weak_clone.clone(),
                        &delivery,
                        &cloned_delivery_guarantee,
                    );
                }
            })
            .detach();
        }

        Ok((conn, config))
    })
}

fn receive_message(
    name: &String,
    sender: &BoxedSender,
    id: &String,
    pending_deliveries: Arc<Mutex<HashMap<String, PendingDelivery>>>,
    delivery: &lapin::Result<(Channel, Delivery)>,
    delivery_guarantee: &DeliveryGuarantee,
) -> Result<()> {
    let (channel, delivery) = delivery.as_ref().expect("error in consumer");
    debug!("{} received CloudEvent on queue {}", id, channel.id());
    let payload_str = std::str::from_utf8(&delivery.data).unwrap();
    match serde_json::from_str::<CloudEvent>(&payload_str) {
        Ok(cloud_event) => {
            debug!("{} deserialized event successfully", id);
            let event_id = get_event_id(&cloud_event, &delivery.delivery_tag);
            info!("size: {}", pending_deliveries.clone().lock().unwrap().len());
            if pending_deliveries
                .clone()
                .lock()
                .unwrap()
                .insert(
                    event_id.to_string(),
                    PendingDelivery {
                        delivery_tag: delivery.delivery_tag.clone(),
                        consume_channel_id: name.to_string(),
                    },
                )
                .is_some()
            {
                error!(
                    "failed event_id={} was already in the table - this should not happen",
                    &event_id
                );
            }
            sender.send(BrokerEvent::IncomingCloudEvent(
                id.clone(),
                event_id,
                cloud_event,
                CloudEventRoutingArgs {
                    delivery_guarantee: delivery_guarantee.clone(),
                },
            ));
        }
        Err(err) => {
            bail!("{} while converting string to CloudEvent: {:?}", id, err);
        }
    }

    Ok(())
}

fn get_event_id(cloud_event: &CloudEvent, delivery_tag: &LongLongUInt) -> String {
    match cloud_event {
        CloudEvent::V0_2(event) => format!("{}--{}", event.event_id(), delivery_tag),
        CloudEvent::V1_0(event) => format!("{}--{}", event.event_id(), delivery_tag),
    }
}

async fn send_cloud_event(cloud_event: &CloudEvent, configurations: &AmqpOptions) -> Result<()> {
    let payload = serde_json::to_string(cloud_event).unwrap();
    for (name, options) in configurations.publish_channels.iter() {
        let result = match options.channel {
            Some(ref channel) => {
                let result = publish_cloud_event(&payload, &name, channel).await;
                if let Ok(result) = result {
                    if !options.delivery_guarantee.requires_acknowledgment() || result.is_ack() {
                        Ok(())
                    } else {
                        // todo foramt does not work -> with &'static str -> wait for refactoring to other error type
                        // Err(format!("Message was not acknowledged: {:?}", result).as_str())
                        Err(anyhow!("Message was not acknowledged, but channel delivery_guarantee requires it"))
                    }
                } else {
                    Err(anyhow!("message was not sent successful"))
                }
            }
            None => Err(anyhow!("channel to exchange is closed")),
        };
        result?
    }
    Ok(())
}

async fn publish_cloud_event(
    payload: &String,
    name: &String,
    channel: &Channel,
) -> Result<Confirmation> {
    let confirmation = channel
        .basic_publish(
            name.as_str(),
            "",
            BasicPublishOptions {
                mandatory: true,
                immediate: false,
            },
            Vec::from(payload.as_str()),
            BasicProperties::default()
                .with_delivery_mode(2) //persistent
                .with_content_type(ShortString::from(
                    "application/cloudevents+json; charset=UTF-8",
                )),
        )
        .await?
        .await?;
    Ok(confirmation)
}

async fn ack_nack_pending_event(
    configuration_option: &Option<AmqpOptions>,
    pending_deliveries: &mut HashMap<String, PendingDelivery>,
    event_id: &String,
    result: ProcessingResult,
) -> Result<()> {
    let pending_event = pending_deliveries
        .get(event_id)
        .with_context(|| format!("pending delivery with id={} not found", event_id))?;
    let configuration_option = configuration_option
        .as_ref()
        .and_then(|o| Some(Ok(o)))
        .unwrap_or(Err(anyhow!("configuration_option is not set")))?;
    let channel_options = configuration_option
        .consume_channels
        .get(&pending_event.consume_channel_id)
        .context("channel not found to ack/nack pending delivery")?;
    let channel = channel_options
        .channel
        .as_ref()
        .context("channel not open")?;
    match result {
        ProcessingResult::Successful => {
            channel
                .basic_ack(pending_event.delivery_tag, BasicAckOptions::default())
                .await?
        }
        ProcessingResult::TransientError => {
            channel
                .basic_nack(
                    pending_event.delivery_tag,
                    BasicNackOptions {
                        multiple: false,
                        requeue: true,
                    },
                )
                .await?
        }
        ProcessingResult::PermanentError => {
            channel
                .basic_nack(
                    pending_event.delivery_tag,
                    BasicNackOptions {
                        multiple: false,
                        requeue: false,
                    },
                )
                .await?
        }
    };
    Ok(())
}

/// This port publishes and/or subscribe CloudEvents to/from an AMQP broker with protocol version v0.9.1.
///
/// The port is implemented with [lapin](https://github.com/CleverCloud/lapin).
///
/// # Content Modes
///
/// The port supports the structured content mode with the JSON event format.
/// However, it does not support the binary content mode.
///
/// <https://github.com/cloudevents/spec/blob/master/amqp-protocol-binding.md#2-use-of-cloudevents-attributes>
///
/// # Examples
///
/// * [Sequence to AMQP to Printer](https://github.com/ce-rust/cerk/tree/master/examples/src/sequence_to_amqp_to_printer/)
/// * [AMQP to Printer](https://github.com/ce-rust/cerk/tree/master/examples/src/amqp_to_printer/)
///
pub fn port_amqp_start(id: InternalServerId, inbox: BoxedReceiver, sender_to_kernel: BoxedSender) {
    let mut connection_option: Option<Connection> = None;
    let mut configuration_option: Option<AmqpOptions> = None;
    let mut pending_deliveries: PendingDeliveries = HashMap::new();
    let arc_pending_deliveries: Arc<Mutex<HashMap<String, PendingDelivery>>> =
        Arc::new(Mutex::new(pending_deliveries));

    info!("start amqp port with id {}", id);

    loop {
        match inbox.receive() {
            BrokerEvent::Init => {
                info!("{} initiated", id);
            }
            BrokerEvent::ConfigUpdated(config, _) => {
                info!("{} received ConfigUpdated", &id);
                let result = setup_connection(
                    id.clone(),
                    sender_to_kernel.clone_boxed(),
                    &connection_option,
                    config,
                    arc_pending_deliveries.clone(),
                );
                if result.is_err() {
                    warn!("{} was not able to establish a connection", &id);
                }
                if let Ok(as_ok) = result {
                    connection_option = Some(as_ok.0);
                    configuration_option = Some(as_ok.1);
                } else {
                    connection_option = None;
                    configuration_option = None;
                }
            }
            BrokerEvent::OutgoingCloudEvent(event_id, cloud_event, _, args) => {
                debug!("{} CloudEvent received", &id);
                if let Some(configuration) = configuration_option.as_ref() {
                    let result = future::block_on(send_cloud_event(&cloud_event, configuration));
                    let result = match result {
                        Ok(_) => {
                            info!("sent cloud event to queue");
                            ProcessingResult::Successful
                        }
                        Err(e) => {
                            error!("{} was not able to send CloudEvent {}", &id, e);
                            // todo transient or permanent?
                            ProcessingResult::TransientError
                        }
                    };
                    if args.delivery_guarantee.requires_acknowledgment() {
                        sender_to_kernel.send(BrokerEvent::OutgoingCloudEventProcessed(
                            id.clone(),
                            event_id,
                            result,
                        ));
                    }
                } else {
                    error!("received CloudEvent before connection was  set up - message will not be delivered")
                }
            }
            BrokerEvent::IncomingCloudEventProcessed(event_id, result) => {
                let result = future::block_on(ack_nack_pending_event(
                    &configuration_option,
                    arc_pending_deliveries.lock().unwrap().borrow_mut(),
                    &event_id,
                    result,
                ));
                match result {
                    Ok(()) => debug!("IncomingCloudEventProcessed was ack/nack successful"),
                    Err(err) => warn!("IncomingCloudEventProcessed was not ack/nack {:?}", err),
                };
            }
            broker_event => warn!("event {} not implemented", broker_event),
        }
    }
}
