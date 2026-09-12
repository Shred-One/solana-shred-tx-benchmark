mod shredstream;

use std::{
    collections::HashMap,
    env,
    error::Error,
    fmt,
    fs::OpenOptions,
    future::Future,
    io::{self, IsTerminal, Write},
    os::unix::fs::OpenOptionsExt,
    sync::{Arc, Condvar, Mutex},
    thread,
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
const PROGRESS_INTERVAL: Duration = Duration::from_millis(200);
const MAX_SOURCE_NAME_LEN: usize = 32;
const O_NONBLOCK: i32 = 0o4000;

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

    fn write_results(
        &mut self,
        names: &[String; 2],
        elapsed: Duration,
        output: &mut impl Write,
    ) -> io::Result<()> {
        for values in &mut self.leads_ms {
            values.sort_by(f64::total_cmp);
        }

        let labels = source_labels(names);
        writeln!(
            output,
            "\nBenchmark results ({:.1}s)",
            elapsed.as_secs_f64()
        )?;
        writeln!(
            output,
            "Matched transactions: {} | Only {}: {} | Only {}: {}",
            self.matched,
            labels[0],
            self.unique[0].saturating_sub(self.matched),
            labels[1],
            self.unique[1].saturating_sub(self.matched)
        )?;
        writeln!(output, "{}", self.table(names))
    }

    fn table(&self, names: &[String; 2]) -> String {
        let labels = source_labels(names);
        let mut rows = vec![[
            "Source",
            "Unique tx",
            "First",
            "Win rate",
            "Mean lead",
            "P50 lead",
            "P75 lead",
            "P95 lead",
            "P99 lead",
        ]
        .map(str::to_owned)];
        for (source, label) in labels.into_iter().enumerate() {
            let win_rate = if self.matched == 0 {
                0.0
            } else {
                self.first[source] as f64 * 100.0 / self.matched as f64
            };
            rows.push([
                label,
                self.unique[source].to_string(),
                self.first[source].to_string(),
                format!("{win_rate:.1}%"),
                format_milliseconds(mean(&self.leads_ms[source])),
                format_milliseconds(percentile(&self.leads_ms[source], 0.50)),
                format_milliseconds(percentile(&self.leads_ms[source], 0.75)),
                format_milliseconds(percentile(&self.leads_ms[source], 0.95)),
                format_milliseconds(percentile(&self.leads_ms[source], 0.99)),
            ]);
        }
        render_table(&rows)
    }
}

#[derive(Clone, Copy)]
struct ProgressSnapshot {
    elapsed: Duration,
    unique: [u64; 2],
}

#[derive(Default)]
struct RenderState {
    latest: Option<ProgressSnapshot>,
    finish: bool,
}

struct ProgressReporter {
    state: Option<Arc<(Mutex<RenderState>, Condvar)>>,
    worker: Option<thread::JoinHandle<io::Result<()>>>,
}

impl ProgressReporter {
    fn new(visible: bool, output: impl Write + Send + 'static) -> Self {
        if !visible {
            return Self {
                state: None,
                worker: None,
            };
        }

        let state = Arc::new((Mutex::new(RenderState::default()), Condvar::new()));
        let worker_state = Arc::clone(&state);
        let worker = thread::spawn(move || render_progress(output, worker_state));
        Self {
            state: Some(state),
            worker: Some(worker),
        }
    }

    fn visible(&self) -> bool {
        self.state.is_some()
    }

    fn report(&self, elapsed: Duration, unique: [u64; 2]) {
        let Some(state) = &self.state else {
            return;
        };
        let (lock, changed) = &**state;
        if let Ok(mut state) = lock.try_lock() {
            if !state.finish {
                state.latest = Some(ProgressSnapshot { elapsed, unique });
                changed.notify_one();
            }
        }
    }

    fn finish(mut self) -> io::Result<()> {
        if let Some(state) = &self.state {
            let (lock, changed) = &**state;
            lock.lock()
                .map_err(|_| io::Error::other("progress state lock poisoned"))?
                .finish = true;
            changed.notify_one();
        }
        match self.worker.take() {
            Some(worker) => worker
                .join()
                .map_err(|_| io::Error::other("progress renderer panicked"))?,
            None => Ok(()),
        }
    }
}

fn terminal_progress() -> ProgressReporter {
    if !io::stdout().is_terminal() {
        return ProgressReporter::new(false, io::sink());
    }
    match OpenOptions::new()
        .write(true)
        .custom_flags(O_NONBLOCK)
        .open("/dev/tty")
    {
        Ok(terminal) => ProgressReporter::new(true, terminal),
        Err(error) => {
            eprintln!("Progress display unavailable: {error}");
            ProgressReporter::new(false, io::sink())
        }
    }
}

struct ProgressLine {
    drawn: bool,
}

impl ProgressLine {
    fn new() -> Self {
        Self { drawn: false }
    }

    fn update(
        &mut self,
        output: &mut impl Write,
        elapsed: Duration,
        unique: [u64; 2],
    ) -> io::Result<()> {
        write!(output, "\r\x1b[2K{}", format_progress(elapsed, unique))?;
        output.flush()?;
        self.drawn = true;
        Ok(())
    }

    fn clear(&mut self, output: &mut impl Write) -> io::Result<()> {
        if self.drawn {
            write!(output, "\r\x1b[2K")?;
            output.flush()?;
            self.drawn = false;
        }
        Ok(())
    }
}

fn render_progress(
    mut output: impl Write,
    state: Arc<(Mutex<RenderState>, Condvar)>,
) -> io::Result<()> {
    let mut line = ProgressLine::new();
    loop {
        let (snapshot, finish) = {
            let (lock, changed) = &*state;
            let mut state = lock
                .lock()
                .map_err(|_| io::Error::other("progress state lock poisoned"))?;
            while state.latest.is_none() && !state.finish {
                state = changed
                    .wait(state)
                    .map_err(|_| io::Error::other("progress state lock poisoned"))?;
            }
            (state.latest.take(), state.finish)
        };
        if let Some(snapshot) = snapshot {
            line.update(&mut output, snapshot.elapsed, snapshot.unique)?;
        }
        if finish {
            return line.clear(&mut output);
        }
    }
}

fn format_progress(elapsed: Duration, unique: [u64; 2]) -> String {
    let tenths = elapsed.as_millis() / 100;
    let elapsed = if tenths <= 999_999 {
        format!("{}.{:01}s", tenths / 10, tenths % 10)
    } else {
        ">99999s".to_owned()
    };
    format!(
        "Running {elapsed:>8} | S1 {:>20} | S2 {:>20}",
        unique[0], unique[1]
    )
}

fn sanitize_source_name(name: &str) -> String {
    let mut characters = name.chars();
    let mut sanitized = String::with_capacity(MAX_SOURCE_NAME_LEN);
    for character in characters.by_ref().take(MAX_SOURCE_NAME_LEN) {
        sanitized.push(if character.is_ascii_graphic() || character == ' ' {
            character
        } else {
            '?'
        });
    }
    if characters.next().is_some() {
        sanitized.truncate(MAX_SOURCE_NAME_LEN - 3);
        sanitized.push_str("...");
    }
    sanitized
}

fn source_labels(names: &[String; 2]) -> [String; 2] {
    [
        format!("S1 ({})", sanitize_source_name(&names[0])),
        format!("S2 ({})", sanitize_source_name(&names[1])),
    ]
}

fn render_table(rows: &[[String; 9]]) -> String {
    let widths: [usize; 9] =
        std::array::from_fn(|column| rows.iter().map(|row| row[column].len()).max().unwrap_or(0));
    let mut lines = Vec::with_capacity(rows.len() + 1);
    for (row_index, row) in rows.iter().enumerate() {
        if row_index == 1 {
            lines.push(widths.map(|width| "-".repeat(width)).join("-+-"));
        }
        lines.push(
            row.iter()
                .enumerate()
                .map(|(column, cell)| {
                    let width = widths[column];
                    if column == 0 {
                        format!("{cell:<width$}")
                    } else {
                        format!("{cell:>width$}")
                    }
                })
                .collect::<Vec<_>>()
                .join(" | "),
        );
    }
    lines.join("\n")
}

#[derive(Debug)]
struct MessageError(String);

impl fmt::Display for MessageError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl Error for MessageError {}

async fn collect_until<S>(
    receiver: &mut mpsc::UnboundedReceiver<Event>,
    start: Instant,
    scheduled_cutoff: Instant,
    duration: Duration,
    stats: &mut Stats,
    progress: &ProgressReporter,
    interrupt: S,
) -> Result<Instant, MessageError>
where
    S: Future<Output = io::Result<()>>,
{
    let deadline = tokio::time::sleep_until(scheduled_cutoff.into());
    tokio::pin!(deadline);
    tokio::pin!(interrupt);
    let mut progress_interval = tokio::time::interval(PROGRESS_INTERVAL);
    progress_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            _ = &mut deadline => return Ok(scheduled_cutoff),
            signal = &mut interrupt => {
                signal.map_err(|error| MessageError(format!("failed to listen for Ctrl+C: {error}")))?;
                return Ok(Instant::now());
            }
            _ = progress_interval.tick(), if progress.visible() => {
                progress.report(
                    Instant::now().saturating_duration_since(start).min(duration),
                    stats.unique,
                );
            }
            event = receiver.recv() => match event {
                Some(Event::Transaction { source, id, received_at }) => {
                    if received_at >= start && received_at <= scheduled_cutoff {
                        stats.observe(source, id, received_at);
                    }
                }
                Some(Event::Error { source, message }) => {
                    return Err(MessageError(format!("S{}: {message}", source + 1)));
                }
                Some(Event::Ready(_) | Event::Finished(_)) => {}
                None => return Err(MessageError("both source streams closed".to_owned())),
            }
        }
    }
}

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

    let progress = terminal_progress();
    let progress_visible = progress.visible();
    let mut stats = Stats::default();
    let progress_error;
    let cutoff = match collect_until(
        &mut receiver,
        start,
        scheduled_cutoff,
        config.duration,
        &mut stats,
        &progress,
        tokio::signal::ctrl_c(),
    )
    .await
    {
        Ok(cutoff) => {
            progress_error = progress.finish().err();
            cutoff
        }
        Err(error) => {
            for task in &tasks {
                task.abort();
            }
            let _ = progress.finish();
            return Err(error.into());
        }
    };

    drop(control_sender);
    let drain_result =
        drain_until_finished(&mut receiver, start, cutoff, &mut stats, &config.names).await;
    for task in tasks {
        task.abort();
    }
    drain_result?;
    {
        let mut output = io::stdout().lock();
        if progress_visible {
            write!(output, "\r\x1b[2K")?;
        }
        stats.write_results(
            &config.names,
            cutoff.saturating_duration_since(start),
            &mut output,
        )?;
    }
    if let Some(error) = progress_error {
        eprintln!("Progress display stopped: {error}");
    }
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
            sanitize_source_name(&take(&mut values, "--source-1-name")?),
            sanitize_source_name(&take(&mut values, "--source-2-name")?),
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
    let rank = (values.len() as f64 * percentile).ceil() as usize;
    let index = rank.saturating_sub(1).min(values.len() - 1);
    Some(values[index])
}

fn format_milliseconds(value: Option<f64>) -> String {
    value.map_or_else(|| "-".to_owned(), |value| format!("{value:.3} ms"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        mpsc as std_mpsc,
    };

    #[derive(Clone, Default)]
    struct SharedWriter(Arc<Mutex<Vec<u8>>>);

    impl SharedWriter {
        fn text(&self) -> String {
            String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
        }
    }

    impl Write for SharedWriter {
        fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buffer);
            Ok(buffer.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    struct BlockingWriter {
        output: SharedWriter,
        started: Option<std_mpsc::Sender<()>>,
        gate: Arc<(Mutex<bool>, Condvar)>,
    }

    impl Write for BlockingWriter {
        fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
            if let Some(started) = self.started.take() {
                let _ = started.send(());
                let (lock, changed) = &*self.gate;
                let mut released = lock.lock().unwrap();
                while !*released {
                    released = changed.wait(released).unwrap();
                }
            }
            self.output.write(buffer)
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    struct WouldBlockWriter(Arc<AtomicUsize>);

    impl Write for WouldBlockWriter {
        fn write(&mut self, _buffer: &[u8]) -> io::Result<usize> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Err(io::Error::from(io::ErrorKind::WouldBlock))
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

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
        for p in [0.50, 0.75, 0.95, 0.99] {
            assert_eq!(percentile(&[1.0], p), Some(1.0));
            assert_eq!(percentile(&[], p), None);
        }

        let pair = [1.0, 2.0];
        assert_eq!(percentile(&pair, 0.50), Some(1.0));
        assert_eq!(percentile(&pair, 0.75), Some(2.0));
        assert_eq!(percentile(&pair, 0.95), Some(2.0));
        assert_eq!(percentile(&pair, 0.99), Some(2.0));

        let quartet = [1.0, 2.0, 3.0, 4.0];
        assert_eq!(percentile(&quartet, 0.50), Some(2.0));
        assert_eq!(percentile(&quartet, 0.75), Some(3.0));
        assert_eq!(percentile(&quartet, 0.95), Some(4.0));
        assert_eq!(percentile(&quartet, 0.99), Some(4.0));
    }

    #[test]
    fn formats_table_from_cell_widths_and_sanitizes_names() {
        assert_eq!(PROGRESS_INTERVAL, Duration::from_millis(200));
        let names = [
            "one\n\x1b[31m\u{0301}".to_owned(),
            "source-two-name-that-is-far-too-long-for-a-terminal".to_owned(),
        ];
        let stats = Stats {
            unique: [12, 3_456],
            first: [7, 3],
            matched: 10,
            leads_ms: [vec![60_000.0], vec![12.5]],
            ..Stats::default()
        };
        let table = stats.table(&names);
        let lines = table.lines().collect::<Vec<_>>();
        let widths = lines.iter().map(|line| line.len()).collect::<Vec<_>>();
        let separators = lines
            .iter()
            .filter(|line| line.contains('|'))
            .map(|line| {
                line.match_indices('|')
                    .map(|(index, _)| index)
                    .collect::<Vec<_>>()
            })
            .collect::<Vec<_>>();
        assert!(widths.iter().all(|width| *width == widths[0]));
        assert!(separators
            .iter()
            .all(|positions| *positions == separators[0]));
        assert_eq!(separators[0].len(), 8);
        assert!(!table.contains('\x1b'));
        assert!(table.contains("60000.000 ms"));
        assert!(table.contains("S1 (one??[31m?)"));
        assert!(table.contains("S2 (source-two-name-that-is-far-t...)"));
    }

    #[test]
    fn progress_is_bounded_and_hidden_output_stays_empty() {
        let progress = format_progress(Duration::MAX, [u64::MAX; 2]);
        assert_eq!(progress.len(), 68);
        assert_eq!(
            progress,
            "Running  >99999s | S1 18446744073709551615 | S2 18446744073709551615"
        );

        let output = SharedWriter::default();
        let hidden = ProgressReporter::new(false, output.clone());
        hidden.report(Duration::from_secs(1), [12, 34]);
        hidden.finish().unwrap();
        assert!(output.text().is_empty());
    }

    #[tokio::test]
    async fn clears_progress_after_success_and_ctrl_c() {
        let (sender, mut receiver) = mpsc::unbounded_channel();
        let output = SharedWriter::default();
        let progress = ProgressReporter::new(true, output.clone());
        progress.report(Duration::ZERO, [0, 0]);
        let start = Instant::now();
        let scheduled_cutoff = start + Duration::from_millis(20);
        let mut stats = Stats::default();
        let cutoff = collect_until(
            &mut receiver,
            start,
            scheduled_cutoff,
            Duration::from_millis(20),
            &mut stats,
            &progress,
            std::future::pending(),
        )
        .await
        .unwrap();
        assert_eq!(cutoff, scheduled_cutoff);
        progress.finish().unwrap();
        assert!(output.text().ends_with("\r\x1b[2K"));
        drop(sender);

        let (_sender, mut receiver) = mpsc::unbounded_channel();
        let output = SharedWriter::default();
        let progress = ProgressReporter::new(true, output.clone());
        progress.report(Duration::ZERO, [0, 0]);
        let start = Instant::now();
        let scheduled_cutoff = start + Duration::from_secs(60);
        let cutoff = collect_until(
            &mut receiver,
            start,
            scheduled_cutoff,
            Duration::from_secs(60),
            &mut Stats::default(),
            &progress,
            std::future::ready(Ok(())),
        )
        .await
        .unwrap();
        assert!(cutoff < scheduled_cutoff);
        progress.finish().unwrap();
        assert!(output.text().ends_with("\r\x1b[2K"));
    }

    #[tokio::test]
    async fn blocked_writer_keeps_only_latest_progress_and_does_not_block_events() {
        let output = SharedWriter::default();
        let (started_sender, started_receiver) = std_mpsc::channel();
        let gate = Arc::new((Mutex::new(false), Condvar::new()));
        let progress = ProgressReporter::new(
            true,
            BlockingWriter {
                output: output.clone(),
                started: Some(started_sender),
                gate: Arc::clone(&gate),
            },
        );
        progress.report(Duration::from_secs(1), [1, 1]);
        started_receiver
            .recv_timeout(Duration::from_secs(1))
            .unwrap();

        let (sender, mut receiver) = mpsc::unbounded_channel();
        let start = Instant::now();
        let scheduled_cutoff = start + Duration::from_secs(60);
        for source in 0..2 {
            sender
                .send(Event::Transaction {
                    source,
                    id: [source as u8; 64],
                    received_at: start + Duration::from_millis(1),
                })
                .unwrap();
        }
        sender
            .send(Event::Error {
                source: 0,
                message: "broken".to_owned(),
            })
            .unwrap();

        let mut stats = Stats::default();
        let error = tokio::time::timeout(
            Duration::from_millis(100),
            collect_until(
                &mut receiver,
                start,
                scheduled_cutoff,
                Duration::from_secs(60),
                &mut stats,
                &progress,
                std::future::pending(),
            ),
        )
        .await
        .expect("event consumer blocked on progress writer")
        .unwrap_err();
        assert_eq!(error.to_string(), "S1: broken");
        assert_eq!(stats.unique, [1, 1]);

        progress.report(Duration::from_secs(2), [2, 2]);
        progress.report(Duration::from_secs(3), [3, 3]);
        let (lock, changed) = &*gate;
        *lock.lock().unwrap() = true;
        changed.notify_one();
        progress.finish().unwrap();

        let output = output.text();
        assert!(!output.contains(&format_progress(Duration::from_secs(2), [2, 2])));
        assert!(output.contains(&format_progress(Duration::from_secs(3), [3, 3])));
        assert!(output.ends_with("\r\x1b[2K"));
    }

    #[test]
    fn progress_failure_is_joined_before_results_are_written() {
        let attempts = Arc::new(AtomicUsize::new(0));
        let progress = ProgressReporter::new(true, WouldBlockWriter(Arc::clone(&attempts)));
        progress.report(Duration::from_secs(1), [1, 1]);

        let started = Instant::now();
        let error = progress.finish().unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
        assert!(started.elapsed() < Duration::from_secs(1));

        let attempts_after_finish = attempts.load(Ordering::SeqCst);
        let output = SharedWriter::default();
        let mut stats = Stats::default();
        stats
            .write_results(
                &["one".to_owned(), "two".to_owned()],
                Duration::from_secs(60),
                &mut output.clone(),
            )
            .unwrap();
        thread::sleep(Duration::from_millis(20));
        assert_eq!(attempts.load(Ordering::SeqCst), attempts_after_finish);
        assert!(output.text().contains("Benchmark results (60.0s)"));
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
