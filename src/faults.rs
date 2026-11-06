//! Deliberate misbehaviour: latency, error statuses, dropped and truncated responses.
//!
//! The engine is generic: a fake's own [`Target`] and fault types decide what a request is and
//! what a fault does to it, and the rules, the seeded dice and the log of applied faults are
//! kept here.

use std::time::Duration;

use serde::{Deserialize, Serialize};

/// What a fault does to a request a rule hit.
pub trait Fault: Clone {
    /// The delay the fault adds before the request is served, or `None` when it ends the
    /// request instead of delaying it.
    fn latency(&self) -> Option<Duration>;

    /// True of a request with this method that the fault applies to at all, for a fault that
    /// only touches some of them, such as one that refuses a write. Every method, and so
    /// every request, by default.
    fn claims(&self, method: &str) -> bool {
        let _ = method;
        true
    }

    /// How the fault reaches the client when it ends the request, for the two faults every
    /// fake shares. [`Transport::Answer`] is the default: the fake answers the request itself.
    fn transport(&self) -> Transport {
        Transport::Answer
    }
}

/// How a terminal fault reaches the client.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Transport {
    /// The fake answers the request itself.
    Answer,
    /// The connection closes without a response.
    Drop,
    /// The request is served, but the response body ends halfway, after its full
    /// `Content-Length` was sent.
    Truncate,
}

/// A fault applied to matching requests with some probability, optionally a limited number
/// of times. `target` is the fake's own [`Target`]-like type and `fault` its [`Fault`]-like
/// one.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Rule<T, F> {
    pub target: T,
    pub fault: F,
    /// Chance, from 0 to 1, that a matching request is hit. Defaults to 1.
    #[serde(default = "always")]
    pub probability: f64,
    /// How many more requests this rule may hit; unlimited when absent.
    #[serde(default)]
    pub times: Option<u32>,
}

fn always() -> f64 {
    1.0
}

impl<T, F> Rule<T, F> {
    /// A rule that always hits requests matching `target`.
    pub fn new(target: T, fault: F) -> Self {
        Self {
            target,
            fault,
            probability: 1.0,
            times: None,
        }
    }

    pub fn with_probability(mut self, probability: f64) -> Self {
        self.probability = probability;
        self
    }

    pub fn times(mut self, times: u32) -> Self {
        self.times = Some(times);
        self
    }
}

/// A fault that was applied, recorded for inspection.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Applied<F> {
    pub method: String,
    pub path: String,
    /// The matched target, as its `Debug` form.
    pub target: String,
    pub fault: F,
}

/// The effect of the rules on one request.
#[derive(Debug, PartialEq)]
pub struct Decision<F> {
    pub latency: Duration,
    /// The first terminal fault a matching rule hit, when one did.
    pub terminal: Option<F>,
}

impl<F> Default for Decision<F> {
    fn default() -> Self {
        Self {
            latency: Duration::ZERO,
            terminal: None,
        }
    }
}

/// The installed rules, a seeded random generator and the log of applied faults.
pub struct Faults<T, F> {
    rules: Vec<Rule<T, F>>,
    rng: fastrand::Rng,
    pub applied: Vec<Applied<F>>,
}

impl<T, F> Default for Faults<T, F> {
    fn default() -> Self {
        Self {
            rules: Vec::new(),
            rng: fastrand::Rng::with_seed(0x6e69_6e6f_7421),
            applied: Vec::new(),
        }
    }
}

impl<T, F> Faults<T, F> {
    /// Installs `rule`, evaluated after the rules already installed.
    pub fn add(&mut self, rule: Rule<T, F>) {
        self.rules.push(rule);
    }

    /// Installs `rule`, evaluated before the rules already installed.
    pub fn add_first(&mut self, rule: Rule<T, F>) {
        self.rules.insert(0, rule);
    }

    pub fn rules(&self) -> &[Rule<T, F>] {
        &self.rules
    }

    /// Removes every rule and the log.
    pub fn clear(&mut self) {
        self.rules.clear();
        self.applied.clear();
    }

    /// Restarts the random sequence, so runs with the same seed hit the same requests.
    pub fn seed(&mut self, seed: u64) {
        self.rng = fastrand::Rng::with_seed(seed);
    }
}

impl<T: std::fmt::Debug, F: Fault> Faults<T, F> {
    /// Rolls every matching rule in order: latencies add up, and the first terminal fault hit
    /// ends the evaluation. A rule whose fault does not claim `method` is passed over, so a
    /// fault that only touches some methods never reaches the others.
    pub fn decide(
        &mut self,
        method: &str,
        path: &str,
        matches: impl Fn(&T) -> bool,
    ) -> Decision<F> {
        let mut decision = Decision::default();
        let mut index = 0;
        while index < self.rules.len() {
            let rule = &self.rules[index];
            if !rule.fault.claims(method) || !matches(&rule.target) || rule.times == Some(0) {
                index += 1;
                continue;
            }
            let probability = rule.probability;
            if self.rng.f64() >= probability {
                index += 1;
                continue;
            }
            let rule = &mut self.rules[index];
            if let Some(times) = &mut rule.times {
                *times -= 1;
            }
            let fault = rule.fault.clone();
            self.applied.push(Applied {
                method: method.to_owned(),
                path: path.to_owned(),
                target: format!("{:?}", rule.target),
                fault: fault.clone(),
            });
            match fault.latency() {
                Some(latency) => decision.latency += latency,
                None => {
                    decision.terminal = Some(fault);
                    break;
                }
            }
            index += 1;
        }
        decision
    }
}

/// Applies `fault` to `response`: closes the connection or truncates the body, and returns
/// the response unchanged for a fault the fake answers itself.
#[cfg(feature = "server")]
pub async fn apply<F: Fault>(
    fault: &F,
    response: axum::response::Response,
) -> axum::response::Response {
    match fault.transport() {
        Transport::Answer => response,
        Transport::Drop => drop_connection(),
        Transport::Truncate => truncate(response).await,
    }
}

/// Ends the connection's task, which closes the socket without a response. `resume_unwind`
/// skips the panic hook, so nothing is printed.
#[cfg(feature = "server")]
pub fn drop_connection() -> ! {
    std::panic::resume_unwind(Box::new("injected fault: drop"))
}

/// Sends the status, headers and full `Content-Length`, then only half of the body.
#[cfg(feature = "server")]
pub async fn truncate(response: axum::response::Response) -> axum::response::Response {
    use axum::body::Body;
    let (mut parts, body) = response.into_parts();
    let bytes = axum::body::to_bytes(body, usize::MAX)
        .await
        .unwrap_or_default();
    parts.headers.insert(
        http::header::CONTENT_LENGTH,
        http::HeaderValue::from(bytes.len()),
    );
    let half = bytes.slice(..bytes.len() / 2);
    // The pause lets hyper flush the first half before the error aborts the connection.
    use futures_util::StreamExt as _;
    let stream = futures_util::stream::iter([Ok(half)]).chain(futures_util::stream::once(async {
        tokio::time::sleep(Duration::from_millis(50)).await;
        Err(std::io::Error::other("injected fault: truncated response"))
    }));
    axum::response::Response::from_parts(parts, Body::from_stream(stream))
}

#[cfg(test)]
mod tests {
    use super::*;
    const GET: &str = "GET";
    const REPOS_GET: &str = "repos/get";

    /// The targets and faults a fake declares, in miniature.
    #[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
    #[serde(rename_all = "snake_case")]
    enum Target {
        Any,
        Operation(String),
        PathPrefix(String),
    }

    #[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
    #[serde(rename_all = "snake_case")]
    enum FaultKind {
        LatencyMs(u64),
        Status(u16),
        Drop,
        /// A fault that only ever hits a request that writes.
        RefuseWrite,
    }

    impl Fault for FaultKind {
        fn latency(&self) -> Option<Duration> {
            match self {
                Self::LatencyMs(ms) => Some(Duration::from_millis(*ms)),
                _ => None,
            }
        }

        fn claims(&self, method: &str) -> bool {
            match self {
                Self::RefuseWrite => method != GET,
                _ => true,
            }
        }
    }

    fn any(_target: &Target) -> bool {
        true
    }

    #[test]
    fn latencies_add_up_until_a_terminal_fault() {
        let mut faults = Faults::default();
        faults.add(Rule::new(Target::Any, FaultKind::LatencyMs(10)));
        faults.add(Rule::new(
            Target::PathPrefix("/repos/".into()),
            FaultKind::LatencyMs(5),
        ));
        faults.add(Rule::new(
            Target::Operation(REPOS_GET.into()),
            FaultKind::Status(502),
        ));
        faults.add(Rule::new(Target::Any, FaultKind::Drop));
        let decision = faults.decide(GET, "/repos/o/r", |target| match target {
            Target::Any => true,
            Target::PathPrefix(prefix) => "/repos/o/r".starts_with(prefix),
            Target::Operation(id) => id == REPOS_GET,
        });
        assert_eq!(decision.latency, Duration::from_millis(15));
        assert_eq!(decision.terminal, Some(FaultKind::Status(502)));
        assert_eq!(faults.applied.len(), 3);
    }

    #[test]
    fn a_rule_installed_first_is_tried_first() {
        let mut faults = Faults::default();
        faults.add(Rule::new(Target::Any, FaultKind::LatencyMs(5)));
        faults.add_first(Rule::new(Target::Any, FaultKind::Status(503)));
        assert_eq!(faults.rules()[0].fault, FaultKind::Status(503));
        let decision = faults.decide(GET, "/repos/o/r", any);
        assert_eq!(decision.latency, Duration::ZERO);
        assert_eq!(decision.terminal, Some(FaultKind::Status(503)));
    }

    #[test]
    fn a_fault_that_claims_some_methods_spares_the_rest() {
        let mut faults = Faults::default();
        faults.add(Rule::new(Target::Any, FaultKind::RefuseWrite));
        assert!(faults.decide(GET, "/repos/o/r", any).terminal.is_none());
        assert!(
            faults.applied.is_empty(),
            "a fault that did not hit is not logged"
        );
        assert_eq!(
            faults.decide("PUT", "/repos/o/r", any).terminal,
            Some(FaultKind::RefuseWrite)
        );
        assert_eq!(faults.applied.len(), 1);
    }

    #[test]
    fn limited_rules_expire() {
        let mut faults = Faults::default();
        faults.add(Rule::new(Target::Any, FaultKind::Status(500)).times(2));
        let hits = (0..5)
            .filter(|_| faults.decide(GET, "/repos/o/r", any).terminal.is_some())
            .count();
        assert_eq!(hits, 2);
    }

    #[test]
    fn probability_is_reproducible_for_a_seed() {
        let run = |seed| {
            let mut faults = Faults::default();
            faults.seed(seed);
            faults.add(Rule::new(Target::Any, FaultKind::Drop).with_probability(0.3));
            (0..1000)
                .map(|_| faults.decide(GET, "/repos/o/r", any).terminal.is_some())
                .collect::<Vec<_>>()
        };
        assert_eq!(run(7), run(7));
        assert_ne!(run(7), run(8));
        let hits = run(7).into_iter().filter(|hit| *hit).count();
        assert!((250..350).contains(&hits), "{hits}");
    }

    #[test]
    fn rules_round_trip_through_json() {
        let rule: Rule<Target, FaultKind> = serde_json::from_str(
            r#"{"target":{"operation":"repos/get"},"fault":"drop","times":1}"#,
        )
        .unwrap();
        assert_eq!(
            rule,
            Rule::new(Target::Operation(REPOS_GET.into()), FaultKind::Drop).times(1)
        );
        let rule: Rule<Target, FaultKind> =
            serde_json::from_str(r#"{"target":"any","fault":{"latency_ms":20},"probability":0.5}"#)
                .unwrap();
        assert_eq!(rule.fault, FaultKind::LatencyMs(20));
    }

    #[test]
    fn applied_faults_record_the_request_and_the_target() {
        let mut faults = Faults::default();
        faults.add(Rule::new(
            Target::Operation(REPOS_GET.into()),
            FaultKind::Drop,
        ));
        faults.decide(
            GET,
            "/repos/o/r",
            |target| matches!(target, Target::Operation(id) if id == REPOS_GET),
        );
        assert_eq!(
            faults.applied,
            [Applied {
                method: GET.to_owned(),
                path: "/repos/o/r".to_owned(),
                target: r#"Operation("repos/get")"#.to_owned(),
                fault: FaultKind::Drop,
            }]
        );
    }
}
