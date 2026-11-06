//! A request profiler a test switches on to see what a client asked of a fake.
//!
//! While recording, every request becomes a [`Hit`]: its method, path and query, the
//! label of the route that served it, its status, sizes and duration. The [`Profile`]
//! that [`Profiler::stop`] returns counts the hits per label and lists the identical
//! requests a client made more than once, which is where a client usually wastes calls.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use axum::Router;
use axum::extract::{MatchedPath, Request};
use axum::middleware::{self, Next};
use axum::response::Response;
use http::header::CONTENT_LENGTH;

/// A label a fake attaches to a response it serves, which the profiler uses instead of the
/// route template: a spec-driven fake that routes everything through one fallback
/// handler names the operation, such as `repos/get`, this way.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Label(pub String);

/// One request a client made while the profiler was recording.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hit {
    pub method: String,
    pub path: String,
    pub query: Option<String>,
    /// The method and the [`Label`] the fake gave the response, else the route template that
    /// served it, such as `GET /repos/{owner}/{repo}`, else `(unmatched)`.
    pub label: String,
    pub status: u16,
    /// The body sizes, when the request or response declared them.
    pub request_bytes: Option<u64>,
    pub response_bytes: Option<u64>,
    pub duration: Duration,
}

impl Hit {
    /// The request as a client wrote it: method, path and query.
    pub fn request(&self) -> String {
        match &self.query {
            Some(query) => format!("{} {}?{query}", self.method, self.path),
            None => format!("{} {}", self.method, self.path),
        }
    }
}

/// Switches recording on and off and collects the hits; cheap to clone and share.
#[derive(Clone, Default)]
pub struct Profiler(Arc<Mutex<Recording>>);

#[derive(Default)]
struct Recording {
    on: bool,
    hits: Vec<Hit>,
}

impl Profiler {
    /// A profiler that is not recording.
    pub fn new() -> Self {
        Self::default()
    }

    /// Starts recording from scratch, dropping any hits recorded before.
    pub fn start(&self) {
        let mut recording = self.lock();
        recording.on = true;
        recording.hits.clear();
    }

    /// Stops recording and returns what was recorded.
    pub fn stop(&self) -> Profile {
        let mut recording = self.lock();
        recording.on = false;
        Profile {
            hits: std::mem::take(&mut recording.hits),
        }
    }

    pub fn is_recording(&self) -> bool {
        self.lock().on
    }

    /// `router` with every request recorded while the profiler is on. Apply it to the
    /// finished router, so that the route templates it labels hits with are known.
    pub fn layer(&self, router: Router) -> Router {
        let profiler = self.clone();
        router.layer(middleware::from_fn(move |request: Request, next: Next| {
            let profiler = profiler.clone();
            async move { profiler.record(request, next).await }
        }))
    }

    async fn record(&self, request: Request, next: Next) -> Response {
        if !self.is_recording() {
            return next.run(request).await;
        }
        let method = request.method().to_string();
        let path = request.uri().path().to_owned();
        let query = request.uri().query().map(str::to_owned);
        let template = request
            .extensions()
            .get::<MatchedPath>()
            .map(|matched| matched.as_str().to_owned());
        let request_bytes = declared_length(request.headers());
        let started = Instant::now();
        let response = next.run(request).await;
        let named = response
            .extensions()
            .get::<Label>()
            .map(|label| label.0.clone());
        let hit = Hit {
            label: format!(
                "{method} {}",
                named.or(template).as_deref().unwrap_or("(unmatched)")
            ),
            method,
            path,
            query,
            status: response.status().as_u16(),
            request_bytes,
            response_bytes: declared_length(response.headers())
                .or_else(|| axum::body::HttpBody::size_hint(response.body()).exact()),
            duration: started.elapsed(),
        };
        let mut recording = self.lock();
        if recording.on {
            recording.hits.push(hit);
        }
        drop(recording);
        response
    }

    fn lock(&self) -> MutexGuard<'_, Recording> {
        self.0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

fn declared_length(headers: &http::HeaderMap) -> Option<u64> {
    headers.get(CONTENT_LENGTH)?.to_str().ok()?.parse().ok()
}

/// The requests recorded between [`Profiler::start`] and [`Profiler::stop`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Profile {
    hits: Vec<Hit>,
}

/// How often one label was hit, and how long it took in total.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LabelStats {
    pub label: String,
    pub count: usize,
    pub total: Duration,
}

/// An identical request made more than once.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Repeat {
    pub request: String,
    pub count: usize,
}

impl Profile {
    /// Every hit, in the order the requests finished.
    pub fn hits(&self) -> &[Hit] {
        &self.hits
    }

    /// The hits per label, most hit first, then by label.
    pub fn by_label(&self) -> Vec<LabelStats> {
        let mut stats: BTreeMap<&str, LabelStats> = BTreeMap::new();
        for hit in &self.hits {
            let entry = stats.entry(&hit.label).or_insert_with(|| LabelStats {
                label: hit.label.clone(),
                count: 0,
                total: Duration::ZERO,
            });
            entry.count += 1;
            entry.total += hit.duration;
        }
        let mut stats: Vec<LabelStats> = stats.into_values().collect();
        stats.sort_by(|a, b| b.count.cmp(&a.count).then_with(|| a.label.cmp(&b.label)));
        stats
    }

    /// The identical requests made more than once, most repeated first, then by request.
    pub fn repeats(&self) -> Vec<Repeat> {
        let mut counts: BTreeMap<String, usize> = BTreeMap::new();
        for hit in &self.hits {
            *counts.entry(hit.request()).or_default() += 1;
        }
        let mut repeats: Vec<Repeat> = counts
            .into_iter()
            .filter(|(_, count)| *count > 1)
            .map(|(request, count)| Repeat { request, count })
            .collect();
        repeats.sort_by(|a, b| {
            b.count
                .cmp(&a.count)
                .then_with(|| a.request.cmp(&b.request))
        });
        repeats
    }

    /// A Markdown report: the total, the hits per label, and the repeated requests.
    pub fn report(&self) -> String {
        let mut report = format!(
            "{} requests\n\n| count | total ms | route |\n|---:|---:|---|\n",
            self.hits.len()
        );
        for stats in self.by_label() {
            let _ = writeln!(
                report,
                "| {} | {} | `{}` |",
                stats.count,
                stats.total.as_millis(),
                stats.label
            );
        }
        let repeats = self.repeats();
        if !repeats.is_empty() {
            report.push_str("\nRepeated requests:\n\n| count | request |\n|---:|---|\n");
            for repeat in repeats {
                let _ = writeln!(report, "| {} | `{}` |", repeat.count, repeat.request);
            }
        }
        report
    }
}
