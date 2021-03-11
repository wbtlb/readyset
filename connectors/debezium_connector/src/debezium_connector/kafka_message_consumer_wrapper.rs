use rdkafka::client::ClientContext;
use rdkafka::config::ClientConfig;
use rdkafka::consumer::stream_consumer::StreamConsumer;
use rdkafka::consumer::{Consumer, ConsumerContext};
use tracing::info;

// This struct contains all of the boilerplate for creating a kafka consumer.
pub struct KafkaMessageConsumerWrapper {
    pub kafka_consumer: LoggingConsumer,
}

impl KafkaMessageConsumerWrapper {
    pub fn new(
        bootstrap_servers: String,
        topic_names: Vec<String>,
        group_id: String,
        timeout: String,
        eof: bool,
        auto_commit: bool,
    ) -> anyhow::Result<KafkaMessageConsumerWrapper> {
        let context = Context;
        let names: Vec<&str> = topic_names.iter().map(String::as_str).collect();
        info!(
            group_id = group_id.as_str(),
            bootstrap_servers = ?bootstrap_servers,
            "Connecting to Kafka"
        );
        let consumer: LoggingConsumer = ClientConfig::new()
            .set("group.id", &group_id)
            .set("bootstrap.servers", &bootstrap_servers)
            .set("enable.partition.eof", &eof.to_string())
            .set("session.timeout.ms", &timeout)
            .set("enable.auto.commit", &auto_commit.to_string())
            .create_with_context(context)?;

        info!(topics = ?names, "Subscribing to topics");
        consumer.subscribe(names.as_slice())?;

        Ok(KafkaMessageConsumerWrapper {
            kafka_consumer: consumer,
        })
    }
}

pub struct Context;

impl ClientContext for Context {}

impl ConsumerContext for Context {}

pub type LoggingConsumer = StreamConsumer<Context>;
