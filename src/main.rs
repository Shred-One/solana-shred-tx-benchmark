mod shredstream;

use std::{
    collections::HashMap,
    env,
    error::Error,
    fmt,
    time::{Duration, Instant},
};

use serde::Deserialize;
use shredstream::{shredstream_proxy_client::ShredstreamProxyClient, SubscribeEntriesRequest};
use solana_hash::Hash;
use solana_transaction::versioned::VersionedTransaction;
use tokio::sync::{mpsc, watch};

type TransactionId = [u8; 64];

const HELP: &str = "\
Compare transaction arrival times from two Jito ShredStream proxies.

Usage:
  solana-shred-tx-benchmark \\
    --source-1-url http://127.0.0.1:19091 --source-1-name NAME \\
    --source-2-url http://127.0.0.1:19092 --source-2-name NAME [--duration SECONDS]

Options:
  --source-1-url URL    First proxy gRPC endpoint
  --source-1-name NAME  Display name for the first source
  --source-2-url URL    Second proxy gRPC endpoint
  --source-2-name NAME  Display name for the second source
  --duration SECONDS    Benchmark duration (default: 60)
  -h, --help            Show this help
";

const START_DELAY: Duration = Duration::from_millis(10);

#[derive(Debug)]
struct Config {
    urls: [String; 2],
    names: [String; 2],
    duration: Duration,
}

#[derive(Deserialize)]
struct Entry {
    #[allow(dead_code)]
    num_hashes: u64,
    #[allow(dead_code)]
    hash: Hash,
    transactions: Vec<VersionedTransaction>,
}

enum Event {
    Ready(usize),
    Transaction {
        source: usize,
        id: TransactionId,
        received_at: Instant,
    },
    Finished(usize),
    Error {
        source: usize,
        message: String,
    },
}

#[derive(Default)]
struct Observation {
    arrivals: [Option<Instant>; 2],
}

#[derive(Default)]
struct Stats {
    observations: HashMap<TransactionId, Observation>,
    unique: [u64; 2],
    first: [u64; 2],
    leads_ms: [Vec<f64>; 2],
    matched: u64,
}

impl Stats {
    fn observe(&mut self, source: usize, id: TransactionId, received_at: Instant) {
        let observation = self.observations.entry(id).or_default();
        if observation.arrivals[source].is_some() {
            return;
        }

        observation.arrivals[source] = Some(received_at);
        self.unique[source] += 1;

        let other = 1 - source;
        if let Some(other_at) = observation.arrivals[other] {
            self.matched += 1;
            let (winner, lead) = if received_at < other_at {
                (source, other_at.duration_since(received_at))
            } else {
                (other, received_at.duration_since(other_at))
            };
            self.first[winner] += 1;
            self.leads_ms[winner].push(lead.as_secs_f64() * 1_000.0);
        }
    }

    fn print(&mut self, names: &[String; 2], elapsed: Duration) {
        for values in &mut self.leads_ms {
            values.sort_by(f64::total_cmp);
        }

        println!("\nBenchmark results ({:.1}s)", elapsed.as_secs_f64());
        println!(
            "Matched transactions: {} | Only {}: {} | Only {}: {}",
            self.matched,
            names[0],
            self.unique[0].saturating_sub(self.matched),
            names[1],
            self.unique[1].saturating_sub(self.matched)
        );
        println!(
            "| Source | Unique tx | First | Win rate | Mean lead | P50 lead | P75 lead | P95 lead | P99 lead |"
        );
        println!("|---|---:|---:|---:|---:|---:|---:|---:|---:|");
        for (source, name) in names.iter().enumerate() {
            let win_rate = if self.matched == 0 {
                0.0
            } else {
                self.first[source] as f64 * 100.0 / self.matched as f64
            };
            println!(
                "| {} | {} | {} | {:.1}% | {} | {} | {} | {} | {} |",
                name.replace('|', "\\|"),
                self.unique[source],
                self.first[source],
                win_rate,
                format_milliseconds(mean(&self.leads_ms[source])),
                format_milliseconds(percentile(&self.leads_ms[source], 0.50)),
                format_milliseconds(percentile(&self.leads_ms[source], 0.75)),
                format_milliseconds(percentile(&self.leads_ms[source], 0.95)),
                format_milliseconds(percentile(&self.leads_ms[source], 0.99)),
            );
        }
    }
}

#[derive(Debug)]
struct MessageError(String);

impl fmt::Display for MessageError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl Error for MessageError {}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let Some(config) = parse_args(env::args().skip(1))? else {
        print!("{HELP}");
        return Ok(());
    };

    let (sender, mut receiver) = mpsc::unbounded_channel();
    let (control_sender, control_receiver) = watch::channel(None);
    let tasks = [
        tokio::spawn(receive(
            0,
            config.urls[0].clone(),
            sender.clone(),
            control_receiver.clone(),
        )),
        tokio::spawn(receive(1, config.urls[1].clone(), sender, control_receiver)),
    ];
    eprintln!("Waiting for both sources to become ready...");
    if let Err(error) = wait_for_ready(&mut receiver, &config.names).await {
        for task in tasks {
            task.abort();
        }
        return Err(error.into());
    }

    let start = Instant::now() + START_DELAY;
    let scheduled_cutoff = start + config.duration;
    control_sender.send(Some((start, scheduled_cutoff)))?;
    eprintln!(
        "Comparing '{}' and '{}' for {} seconds. Press Ctrl+C to stop early.",
        config.names[0],
        config.names[1],
        config.duration.as_secs()
    );

    let deadline = tokio::time::sleep_until(scheduled_cutoff.into());
    tokio::pin!(deadline);
    let mut stats = Stats::default();
    let cutoff = loop {
        tokio::select! {
            _ = &mut deadline => break scheduled_cutoff,
            signal = tokio::signal::ctrl_c() => {
                signal?;
                break Instant::now();
            }
            event = receiver.recv() => match event {
                Some(Event::Transaction { source, id, received_at }) => {
                    if received_at >= start && received_at <= scheduled_cutoff {
                        stats.observe(source, id, received_at);
                    }
                }
                Some(Event::Error { source, message }) => {
                    for task in tasks {
                        task.abort();
                    }
                    return Err(MessageError(format!("{}: {message}", config.names[source])).into());
                }
                Some(Event::Ready(_) | Event::Finished(_)) => {}
                None => return Err(MessageError("both source streams closed".to_owned()).into()),
            }
        }
    };

    drop(control_sender);
    let drain_result =
        drain_until_finished(&mut receiver, start, cutoff, &mut stats, &config.names).await;
    for task in tasks {
        task.abort();
    }
    drain_result?;
    stats.print(&config.names, cutoff.saturating_duration_since(start));
    Ok(())
}

async fn receive(
    source: usize,
    url: String,
    sender: mpsc::UnboundedSender<Event>,
    mut control: watch::Receiver<Option<(Instant, Instant)>>,
) {
    let mut last_error = String::new();
    for _ in 0..20 {
        match ShredstreamProxyClient::connect(url.clone()).await {
            Ok(mut client) => match client.subscribe_entries(SubscribeEntriesRequest {}).await {
                Ok(response) => {
                    let mut stream = response.into_inner();
                    if sender.send(Event::Ready(source)).is_err() {
                        return;
                    }
                    loop {
                        tokio::select! {
                            biased;
                            changed = control.changed() => {
                                if changed.is_err() {
                                    let _ = sender.send(Event::Finished(source));
                                    return;
                                }
                            }
                            result = stream.message() => match result {
                            Ok(Some(message)) => {
                                let received_at = Instant::now();
                                let Some((start, cutoff)) = *control.borrow() else {
                                    continue;
                                };
                                if received_at < start || received_at > cutoff {
                                    continue;
                                }
                                let entries: Vec<Entry> =
                                    match bincode::deserialize(&message.entries) {
                                        Ok(entries) => entries,
                                        Err(error) => {
                                            send_error(
                                                &sender,
                                                source,
                                                format!("invalid entry data: {error}"),
                                            );
                                            return;
                                        }
                                    };
                                for transaction in
                                    entries.into_iter().flat_map(|entry| entry.transactions)
                                {
                                    let Some(signature) = transaction.signatures.first() else {
                                        continue;
                                    };
                                    let id = signature
                                        .as_ref()
                                        .try_into()
                                        .expect("signature is 64 bytes");
                                    if sender
                                        .send(Event::Transaction {
                                            source,
                                            id,
                                            received_at,
                                        })
                                        .is_err()
                                    {
                                        return;
                                    }
                                }
                            }
                            Ok(None) => {
                                send_error(&sender, source, "stream closed".to_owned());
                                return;
                            }
                            Err(error) => {
                                send_error(&sender, source, format!("stream error: {error}"));
                                return;
                            }
                            }
                        }
                    }
                }
                Err(error) => last_error = format!("subscription failed: {error}"),
            },
            Err(error) => last_error = format!("connection failed: {error}"),
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    send_error(&sender, source, format!("{last_error} ({url})"));
}

fn send_error(sender: &mpsc::UnboundedSender<Event>, source: usize, message: String) {
    let _ = sender.send(Event::Error { source, message });
}

async fn wait_for_ready(
    receiver: &mut mpsc::UnboundedReceiver<Event>,
    names: &[String; 2],
) -> Result<(), MessageError> {
    let mut ready = [false; 2];
    while !ready.iter().all(|ready| *ready) {
        match receiver.recv().await {
            Some(Event::Ready(source)) => ready[source] = true,
            Some(Event::Error { source, message }) => {
                return Err(MessageError(format!("{}: {message}", names[source])));
            }
            Some(Event::Transaction { .. } | Event::Finished(_)) => {}
            None => return Err(MessageError("both source streams closed".to_owned())),
        }
    }
    Ok(())
}

async fn drain_until_finished(
    receiver: &mut mpsc::UnboundedReceiver<Event>,
    start: Instant,
    cutoff: Instant,
    stats: &mut Stats,
    names: &[String; 2],
) -> Result<(), MessageError> {
    let mut finished = [false; 2];
    while !finished.iter().all(|finished| *finished) {
        match receiver.recv().await {
            Some(Event::Transaction {
                source,
                id,
                received_at,
            }) if received_at >= start && received_at <= cutoff => {
                stats.observe(source, id, received_at);
            }
            Some(Event::Finished(source)) => finished[source] = true,
            Some(Event::Error { source, message }) => {
                return Err(MessageError(format!("{}: {message}", names[source])));
            }
            Some(Event::Ready(_) | Event::Transaction { .. }) => {}
            None => {
                return Err(MessageError(
                    "source streams closed before draining".to_owned(),
                ))
            }
        }
    }
    Ok(())
}

fn parse_args(arguments: impl Iterator<Item = String>) -> Result<Option<Config>, MessageError> {
    let mut values = HashMap::new();
    let mut arguments = arguments.peekable();
    while let Some(argument) = arguments.next() {
        if argument == "--help" || argument == "-h" {
            return Ok(None);
        }
        if !matches!(
            argument.as_str(),
            "--source-1-url"
                | "--source-1-name"
                | "--source-2-url"
                | "--source-2-name"
                | "--duration"
        ) {
            return Err(MessageError(format!(
                "unknown option: {argument}\n\n{HELP}"
            )));
        }
        let value = arguments
            .next()
            .ok_or_else(|| MessageError(format!("missing value for {argument}")))?;
        values.insert(argument, value);
    }

    let take = |values: &mut HashMap<String, String>, key: &str| {
        values
            .remove(key)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| MessageError(format!("missing required option: {key}")))
    };
    let duration = values
        .remove("--duration")
        .unwrap_or_else(|| "60".to_owned())
        .parse::<u64>()
        .map_err(|_| MessageError("--duration must be a positive integer".to_owned()))?;
    if duration == 0 {
        return Err(MessageError(
            "--duration must be greater than zero".to_owned(),
        ));
    }

    Ok(Some(Config {
        urls: [
            take(&mut values, "--source-1-url")?,
            take(&mut values, "--source-2-url")?,
        ],
        names: [
            take(&mut values, "--source-1-name")?,
            take(&mut values, "--source-2-name")?,
        ],
        duration: Duration::from_secs(duration),
    }))
}

fn mean(values: &[f64]) -> Option<f64> {
    (!values.is_empty()).then(|| values.iter().sum::<f64>() / values.len() as f64)
}

fn percentile(values: &[f64], percentile: f64) -> Option<f64> {
    if values.is_empty() {
        return None;
    }
    let index = ((values.len() - 1) as f64 * percentile).round() as usize;
    Some(values[index])
}

fn format_milliseconds(value: Option<f64>) -> String {
    value.map_or_else(|| "-".to_owned(), |value| format!("{value:.3} ms"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_unique_and_out_of_order_arrivals() {
        let mut stats = Stats::default();
        let start = Instant::now();
        stats.observe(0, [1; 64], start + Duration::from_millis(5));
        stats.observe(0, [1; 64], start + Duration::from_millis(6));
        stats.observe(1, [1; 64], start + Duration::from_millis(2));

        assert_eq!(stats.unique, [1, 1]);
        assert_eq!(stats.matched, 1);
        assert_eq!(stats.first, [0, 1]);
        assert_eq!(stats.leads_ms[1], vec![3.0]);
    }

    #[test]
    fn parses_required_options_and_default_duration() {
        let arguments = [
            "--source-1-url",
            "http://one",
            "--source-1-name",
            "one",
            "--source-2-url",
            "http://two",
            "--source-2-name",
            "two",
        ];
        let config = parse_args(arguments.into_iter().map(str::to_owned))
            .unwrap()
            .unwrap();

        assert_eq!(config.duration, Duration::from_secs(60));
        assert_eq!(config.names, ["one", "two"]);
    }

    #[test]
    fn calculates_nearest_rank_percentiles() {
        let values = (1..=101).map(f64::from).collect::<Vec<_>>();
        assert_eq!(percentile(&values, 0.50), Some(51.0));
        assert_eq!(percentile(&values, 0.75), Some(76.0));
        assert_eq!(percentile(&values, 0.95), Some(96.0));
        assert_eq!(percentile(&values, 0.99), Some(100.0));
        assert_eq!(percentile(&[], 0.99), None);
    }

    #[tokio::test]
    async fn readiness_waits_for_both_sources() {
        let (sender, mut receiver) = mpsc::unbounded_channel();
        let names = ["one".to_owned(), "two".to_owned()];
        sender.send(Event::Ready(0)).unwrap();

        let waiter = tokio::spawn(async move { wait_for_ready(&mut receiver, &names).await });
        tokio::task::yield_now().await;
        assert!(!waiter.is_finished());

        sender.send(Event::Ready(1)).unwrap();
        assert!(waiter.await.unwrap().is_ok());
    }

    #[tokio::test]
    async fn drain_keeps_only_events_at_or_before_cutoff() {
        let (sender, mut receiver) = mpsc::unbounded_channel();
        let names = ["one".to_owned(), "two".to_owned()];
        let start = Instant::now();
        let cutoff = start + Duration::from_millis(5);
        sender
            .send(Event::Transaction {
                source: 0,
                id: [0; 64],
                received_at: start - Duration::from_millis(1),
            })
            .unwrap();
        sender
            .send(Event::Transaction {
                source: 0,
                id: [1; 64],
                received_at: start + Duration::from_millis(1),
            })
            .unwrap();
        sender.send(Event::Finished(0)).unwrap();
        let producer = tokio::spawn(async move {
            tokio::task::yield_now().await;
            sender
                .send(Event::Transaction {
                    source: 1,
                    id: [1; 64],
                    received_at: start + Duration::from_millis(2),
                })
                .unwrap();
            sender
                .send(Event::Transaction {
                    source: 1,
                    id: [2; 64],
                    received_at: start + Duration::from_millis(6),
                })
                .unwrap();
            sender.send(Event::Finished(1)).unwrap();
        });

        let mut stats = Stats::default();
        drain_until_finished(&mut receiver, start, cutoff, &mut stats, &names)
            .await
            .unwrap();
        producer.await.unwrap();

        assert_eq!(stats.unique, [1, 1]);
        assert_eq!(stats.matched, 1);
    }
}
