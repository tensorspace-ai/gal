//! Counters and gauges, and the Prometheus text format they are read out in.
//!
//! Everything here already existed as state inside `AppState`; none of it was
//! reachable from outside the process. That is the gap this closes. An operator
//! could not see how many connections were open, how many waves were resident,
//! or — the one that matters most — how often a client was being disconnected
//! for overflowing its queue, because every one of those safety valves fires
//! silently and is only visible as a user complaint.
//!
//! Hand-rolled rather than pulling in a metrics crate: the values are counters
//! and gauges over atomics, the exposition format is a dozen lines of text, and
//! this keeps the dependency list short enough to keep auditing.
//!
//! There are no histograms. Request latency is reported per request in the
//! access log, which answers "what is slow" well enough at this size; adding
//! buckets is worth doing when someone needs a quantile across a fleet.

use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};

use dashmap::DashMap;

#[derive(Default)]
pub struct Metrics {
    // --- counters, monotonic ---
    pub http_requests: DashMap<u16, AtomicU64>,
    pub ws_commands: DashMap<&'static str, AtomicU64>,
    pub ws_connections_opened: AtomicU64,
    /// A client disconnected because its outbound queue overflowed. It had
    /// already missed messages by then, so a rising rate here means people are
    /// silently resynchronising, and it is the single most useful number in
    /// this file.
    pub ws_slow_client_disconnects: AtomicU64,
    pub ws_command_panics: AtomicU64,
    pub ws_frames_unparseable: AtomicU64,
    pub ops_applied: AtomicU64,
    /// An op the server would not take: refused by mode, over a document limit,
    /// or written against a revision it can no longer transform from.
    pub ops_refused: AtomicU64,
    /// An op that was applied in memory and would not persist, so it was rolled
    /// back. Never zero for long without something being wrong with the disk.
    pub ops_persist_failures: AtomicU64,
    pub waves_loaded: AtomicU64,
    pub waves_evicted_after_panic: AtomicU64,
    pub rate_limited: DashMap<&'static str, AtomicU64>,

    // --- gauges, up and down ---
    pub ws_connections_active: AtomicI64,
    pub waves_resident: AtomicI64,
}

fn bump(counter: &AtomicU64) {
    counter.fetch_add(1, Ordering::Relaxed);
}

impl Metrics {
    pub fn command(&self, name: &'static str) {
        bump(self.ws_commands.entry(name).or_default().value());
    }

    pub fn http_response(&self, status: u16) {
        bump(self.http_requests.entry(status).or_default().value());
    }

    pub fn rate_limit(&self, limiter: &'static str) {
        bump(self.rate_limited.entry(limiter).or_default().value());
    }

    pub fn connection_opened(&self) {
        bump(&self.ws_connections_opened);
        self.ws_connections_active.fetch_add(1, Ordering::Relaxed);
    }

    pub fn connection_closed(&self) {
        self.ws_connections_active.fetch_sub(1, Ordering::Relaxed);
    }

    pub fn wave_loaded(&self) {
        bump(&self.waves_loaded);
        self.waves_resident.fetch_add(1, Ordering::Relaxed);
    }

    pub fn wave_evicted(&self) {
        self.waves_resident.fetch_sub(1, Ordering::Relaxed);
    }

    /// The Prometheus text exposition format, version 0.0.4.
    pub fn render(&self) -> String {
        let mut out = String::with_capacity(2048);

        let counter = |out: &mut String, name: &str, help: &str, value: u64| {
            out.push_str(&format!(
                "# HELP {name} {help}\n# TYPE {name} counter\n{name} {value}\n"
            ));
        };
        let gauge = |out: &mut String, name: &str, help: &str, value: i64| {
            out.push_str(&format!(
                "# HELP {name} {help}\n# TYPE {name} gauge\n{name} {value}\n"
            ));
        };
        let load = |v: &AtomicU64| v.load(Ordering::Relaxed);

        // Labelled families are written whole, since a Prometheus family's HELP
        // and TYPE lines must appear once and precede all of its samples.
        out.push_str(
            "# HELP gal_http_responses_total HTTP responses by status code.\n\
             # TYPE gal_http_responses_total counter\n",
        );
        for entry in self.http_requests.iter() {
            out.push_str(&format!(
                "gal_http_responses_total{{status=\"{}\"}} {}\n",
                entry.key(),
                load(entry.value())
            ));
        }

        out.push_str(
            "# HELP gal_ws_commands_total WebSocket commands handled, by command.\n\
             # TYPE gal_ws_commands_total counter\n",
        );
        for entry in self.ws_commands.iter() {
            out.push_str(&format!(
                "gal_ws_commands_total{{command=\"{}\"}} {}\n",
                entry.key(),
                load(entry.value())
            ));
        }

        out.push_str(
            "# HELP gal_rate_limited_total Requests refused by a rate limiter.\n\
             # TYPE gal_rate_limited_total counter\n",
        );
        for entry in self.rate_limited.iter() {
            out.push_str(&format!(
                "gal_rate_limited_total{{limiter=\"{}\"}} {}\n",
                entry.key(),
                load(entry.value())
            ));
        }

        counter(
            &mut out,
            "gal_ws_connections_opened_total",
            "WebSocket connections accepted.",
            load(&self.ws_connections_opened),
        );
        counter(
            &mut out,
            "gal_ws_slow_client_disconnects_total",
            "Clients disconnected for overflowing their outbound queue. Each one \
             missed messages and had to resynchronise.",
            load(&self.ws_slow_client_disconnects),
        );
        counter(
            &mut out,
            "gal_ws_command_panics_total",
            "Commands that panicked. Always a bug.",
            load(&self.ws_command_panics),
        );
        counter(
            &mut out,
            "gal_ws_unparseable_frames_total",
            "Frames that were not a message this server defines.",
            load(&self.ws_frames_unparseable),
        );
        counter(
            &mut out,
            "gal_ops_applied_total",
            "Edits applied to a document and persisted.",
            load(&self.ops_applied),
        );
        counter(
            &mut out,
            "gal_ops_refused_total",
            "Edits the server declined: mode, document limits, or a revision it \
             could no longer transform from.",
            load(&self.ops_refused),
        );
        counter(
            &mut out,
            "gal_ops_persist_failures_total",
            "Edits applied in memory that would not persist and were rolled back.",
            load(&self.ops_persist_failures),
        );
        counter(
            &mut out,
            "gal_waves_loaded_total",
            "Waves read from storage into memory.",
            load(&self.waves_loaded),
        );
        counter(
            &mut out,
            "gal_waves_evicted_after_panic_total",
            "Waves thrown out of memory because a command panicked while holding \
             them.",
            load(&self.waves_evicted_after_panic),
        );
        gauge(
            &mut out,
            "gal_ws_connections_active",
            "WebSocket connections open right now.",
            self.ws_connections_active.load(Ordering::Relaxed),
        );
        gauge(
            &mut out,
            "gal_waves_resident",
            "Waves held in memory right now.",
            self.waves_resident.load(Ordering::Relaxed),
        );

        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_exposition_is_well_formed() {
        let m = Metrics::default();
        m.connection_opened();
        m.connection_opened();
        m.connection_closed();
        m.command("submit");
        m.command("submit");
        m.command("open");
        m.http_response(200);
        m.rate_limit("auth");
        m.wave_loaded();

        let text = m.render();
        assert!(text.contains("gal_ws_commands_total{command=\"submit\"} 2"));
        assert!(text.contains("gal_ws_commands_total{command=\"open\"} 1"));
        assert!(text.contains("gal_http_responses_total{status=\"200\"} 1"));
        assert!(text.contains("gal_rate_limited_total{limiter=\"auth\"} 1"));
        assert!(text.contains("gal_ws_connections_active 1"));
        assert!(text.contains("gal_waves_resident 1"));

        // Every family declares its type once, before its samples. A duplicated
        // HELP line makes a scrape fail outright rather than lose one series.
        let mut families: Vec<&str> = text
            .lines()
            .filter_map(|l| l.strip_prefix("# HELP "))
            .map(|l| l.split(' ').next().unwrap())
            .collect();
        let before = families.len();
        families.sort_unstable();
        families.dedup();
        assert_eq!(before, families.len(), "a metric family is declared twice");

        for line in text
            .lines()
            .filter(|l| !l.starts_with('#') && !l.is_empty())
        {
            assert!(
                line.split(' ').count() == 2,
                "not a name/value sample: {line}"
            );
        }
    }

    /// A gauge that only counts up is a gauge that lies after the first eviction.
    #[test]
    fn gauges_come_back_down() {
        let m = Metrics::default();
        m.wave_loaded();
        m.wave_loaded();
        m.wave_evicted();
        m.connection_opened();
        m.connection_closed();
        assert!(m.render().contains("gal_waves_resident 1"));
        assert!(m.render().contains("gal_ws_connections_active 0"));
    }
}
