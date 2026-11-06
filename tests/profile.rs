//! The request profiler: what a client asked of a fake, and what it asked more than once.
#![cfg(feature = "profile")]

use std::time::Duration;

use axum::Router;
use axum::routing::{get, post};
use ninot_common::profile::{Label, Profiler};
use ninot_common::server::Server;
use reqwest::blocking::Client;

/// A fake with two routes, profiled, labelling requests by their route template.
fn serve(profiler: &Profiler) -> Server {
    let router = Router::new()
        .route("/repos/{owner}/{repo}", get(|| async { "repo" }))
        .route("/repos/{owner}/{repo}/issues", post(|| async { "created" }));
    let router = profiler.layer(router);
    Server::serve(Server::bind(([127, 0, 0, 1], 0).into()).unwrap(), router).unwrap()
}

#[test]
fn nothing_is_recorded_until_the_profiler_starts() {
    let profiler = Profiler::new();
    let server = serve(&profiler);
    let client = Client::new();
    client
        .get(format!("{}/repos/a/b", server.url()))
        .send()
        .unwrap();
    assert!(!profiler.is_recording());
    profiler.start();
    client
        .get(format!("{}/repos/a/b", server.url()))
        .send()
        .unwrap();
    let profile = profiler.stop();
    assert_eq!(profile.hits().len(), 1);
    assert!(!profiler.is_recording());
}

#[test]
fn hits_carry_the_route_template_status_and_sizes() {
    let profiler = Profiler::new();
    let server = serve(&profiler);
    profiler.start();
    let client = Client::new();
    client
        .get(format!("{}/repos/a/b?x=1", server.url()))
        .send()
        .unwrap();
    client
        .post(format!("{}/repos/a/b/issues", server.url()))
        .body("{\"title\":\"t\"}")
        .send()
        .unwrap();
    client
        .get(format!("{}/nowhere", server.url()))
        .send()
        .unwrap();
    let profile = profiler.stop();
    let hits = profile.hits();
    assert_eq!(hits.len(), 3);
    assert_eq!(hits[0].method, "GET");
    assert_eq!(hits[0].path, "/repos/a/b");
    assert_eq!(hits[0].query.as_deref(), Some("x=1"));
    assert_eq!(hits[0].label, "GET /repos/{owner}/{repo}");
    assert_eq!(hits[0].status, 200);
    assert_eq!(hits[0].response_bytes, Some(4));
    assert_eq!(hits[1].label, "POST /repos/{owner}/{repo}/issues");
    assert_eq!(hits[1].request_bytes, Some(13));
    assert_eq!(hits[2].label, "GET (unmatched)");
    assert_eq!(hits[2].status, 404);
    assert!(hits.iter().all(|hit| hit.duration < Duration::from_secs(5)));
}

#[test]
fn the_profile_counts_requests_per_label_and_finds_repeats() {
    let profiler = Profiler::new();
    let server = serve(&profiler);
    profiler.start();
    let client = Client::new();
    for repo in ["a", "b", "a", "a"] {
        client
            .get(format!("{}/repos/o/{repo}", server.url()))
            .send()
            .unwrap();
    }
    client
        .post(format!("{}/repos/o/a/issues", server.url()))
        .send()
        .unwrap();
    let profile = profiler.stop();

    let labels = profile.by_label();
    assert_eq!(labels[0].label, "GET /repos/{owner}/{repo}");
    assert_eq!(labels[0].count, 4);
    assert_eq!(labels[1].count, 1);

    let repeats = profile.repeats();
    assert_eq!(repeats.len(), 1);
    assert_eq!(repeats[0].request, "GET /repos/o/a");
    assert_eq!(repeats[0].count, 3);

    let report = profile.report();
    assert!(report.contains("GET /repos/{owner}/{repo}"), "{report}");
    assert!(report.contains("GET /repos/o/a"), "{report}");
    assert!(report.contains("5 requests"), "{report}");
}

#[test]
fn starting_again_clears_the_previous_profile() {
    let profiler = Profiler::new();
    let server = serve(&profiler);
    let client = Client::new();
    profiler.start();
    client
        .get(format!("{}/repos/a/b", server.url()))
        .send()
        .unwrap();
    profiler.start();
    client
        .get(format!("{}/repos/a/c", server.url()))
        .send()
        .unwrap();
    assert_eq!(profiler.stop().hits().len(), 1);
}

#[test]
fn a_label_the_fake_gives_a_response_names_the_hit() {
    let profiler = Profiler::new();
    let router = profiler.layer(Router::new().fallback(|| async {
        let mut response = axum::response::IntoResponse::into_response("any");
        response.extensions_mut().insert(Label("repos/get".into()));
        response
    }));
    let server = Server::serve(Server::bind(([127, 0, 0, 1], 0).into()).unwrap(), router).unwrap();
    profiler.start();
    Client::new()
        .get(format!("{}/repos/a/b", server.url()))
        .send()
        .unwrap();
    assert_eq!(profiler.stop().hits()[0].label, "GET repos/get");
}
