use super::*;
use crate::jsonl::{parse_record, ScalarKind};
use std::path::PathBuf;

struct Fixture {
    root: PathBuf,
    corpus: PathBuf,
}

impl Fixture {
    fn new(name: &str) -> Self {
        let root =
            std::env::temp_dir().join(format!("ebira-actions-{name}-{}", std::process::id()));
        let corpus = root.join("corpus with spaces 日本語");
        std::fs::create_dir_all(&root).unwrap();
        let source = root.join("session.jsonl");
        let records = [
            "2026-08-16T10:00:00Z",
            "2026-08-16T11:00:00Z",
            "2026-08-17T10:00:00Z",
            "2026-08-18T10:00:00Z",
        ]
        .iter()
        .enumerate()
        .map(|(index, timestamp)| {
            format!(
                "{{\"type\":\"user\",\"message\":{{\"role\":\"user\",\"content\":\"--needle {index}\"}},\"sessionId\":\"s1\",\"uuid\":\"u{index}\",\"timestamp\":\"{timestamp}\"}}\n"
            )
        })
        .collect::<String>();
        std::fs::write(&source, records).unwrap();
        corpus::build(
            &[source.to_string_lossy().into_owned()],
            &corpus,
            false,
            300,
            &[],
        )
        .unwrap();
        Self { root, corpus }
    }

    fn request(&self, operation: &str) -> SearchRequest {
        let offset_minutes = corpus::corpus_offset_minutes(&self.corpus).unwrap();
        SearchRequest {
            query: "--Needle".into(),
            operation: operation.into(),
            source_id: corpus::source_catalog(&self.corpus)
                .unwrap()
                .keys()
                .next()
                .cloned(),
            session: Some("s1".into()),
            role: Some("user".into()),
            kind: Some("user".into()),
            sender: Some("human".into()),
            from: Some("2026-08-16".into()),
            to: Some("2026-08-18".into()),
            range: Range::parse(Some("2026-08-16"), Some("2026-08-18"), offset_minutes).unwrap(),
            offset_minutes,
            limit: 1,
            date_limit: 1,
            fold_ascii_case: true,
            newest_first: true,
            ..SearchRequest::default()
        }
    }

    fn result(&self, request: &SearchRequest) -> String {
        let catalog = corpus::source_catalog(&self.corpus).unwrap();
        let scope = corpus::scope(
            &catalog,
            request.source_id.as_deref(),
            request.session.as_deref(),
        );
        let files = scope.files(&self.corpus).unwrap();
        let stats = scan_parallel(&files, &catalog, request).unwrap();
        if is_history_request(request) {
            history_json(&self.corpus, request, &scope, &files, &stats, 0).unwrap()
        } else {
            search_json(&self.corpus, request, &scope, &files, &stats, 0).unwrap()
        }
    }

    /// Interpret the public action schema without supplying missing arguments for it.
    fn follow(&self, result: &str) -> Vec<Vec<String>> {
        let fields = parse_record(result.as_bytes(), 0).unwrap();
        let mut commands = Vec::new();
        for field in &fields {
            if !field.path.starts_with("/next_actions/") || !field.path.ends_with("/action") {
                continue;
            }
            let prefix = format!("{}/request/", field.path.strip_suffix("/action").unwrap());
            let request = fields
                .iter()
                .filter(|f| f.path.starts_with(&prefix))
                .collect::<Vec<_>>();
            let corpus = request.iter().find(|f| f.path == format!("{prefix}corpus"));
            assert_eq!(
                corpus.map(|f| f.value.as_str()),
                Some(self.corpus.to_str().unwrap()),
                "{result}"
            );
            let mut args = vec![field.value.clone()];
            for option in request {
                let name = option.path.strip_prefix(&prefix).unwrap().replace('_', "-");
                match option.kind {
                    ScalarKind::Null => {}
                    ScalarKind::Bool if option.value == "false" => {}
                    ScalarKind::Bool => args.push(format!("--{name}")),
                    _ => args.push(format!("--{name}={}", option.value)),
                }
            }
            crate::run(&args)
                .unwrap_or_else(|error| panic!("emitted action {args:?} failed: {error}"));
            commands.push(args);
        }
        commands
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

#[test]
fn search_page_actions_execute_with_the_original_filters() {
    let fixture = Fixture::new("search-page");
    for raw in [false, true] {
        let mut request = fixture.request("");
        request.raw = raw;
        let actions = fixture.follow(&fixture.result(&request));
        assert_eq!(actions.len(), 1);
        let args = &actions[0];
        assert_eq!(args[0], "search");
        for expected in [
            "--query=--Needle",
            "--offset=1",
            "--limit=1",
            "--order=desc",
            "--ignore-case",
            "--session=s1",
            "--sender=human",
            "--role=user",
            "--kind=user",
            "--from=2026-08-16",
            "--to=2026-08-18",
        ] {
            assert!(args.iter().any(|arg| arg == expected), "{args:?}");
        }
        assert_eq!(args.iter().any(|arg| arg == "--raw"), raw);
        let next = crate::search_request(args, "", request.offset_minutes).unwrap();
        assert!(fixture.result(&next).contains("\"returned\":1"));
    }
}

#[test]
fn history_actions_execute_for_matches_dates_and_time_buckets() {
    let fixture = Fixture::new("history-pages");
    for raw in [false, true] {
        let mut request = fixture.request("history");
        request.raw = raw;
        let actions = fixture.follow(&fixture.result(&request));
        assert_eq!(
            actions.len(),
            5,
            "three dates, the next match, and the next date page"
        );
        assert!(actions
            .iter()
            .any(|args| args.contains(&"--offset=1".into())));
        assert!(actions
            .iter()
            .any(|args| args.contains(&"--date-offset=1".into())));
        for (index, args) in actions.into_iter().enumerate() {
            assert_eq!(args[0], if index < 3 { "search" } else { "history" });
            assert!(args.contains(&"--query=--Needle".into()));
            assert!(args.contains(&"--session=s1".into()));
            assert!(args.contains(&"--sender=human".into()));
            assert_eq!(args.iter().any(|arg| arg == "--raw"), raw);
            let next = crate::search_request(&args, &args[0], request.offset_minutes).unwrap();
            let result = fixture.result(&next);
            assert!(result.contains("\"returned\":1"));
            if args[0] == "search" {
                for following in fixture.follow(&result) {
                    assert!(
                        following.contains(&"--offset=1".into()),
                        "a bucket must advance into its records: {following:?}"
                    );
                }
            }
        }
    }
}

#[test]
fn empty_search_and_history_offer_executable_raw_requests() {
    let fixture = Fixture::new("raw-recovery");
    for operation in ["", "history"] {
        let mut request = fixture.request(operation);
        request.query = "absent \"quoted\" 日本語".into();
        request.offset = 50;
        request.date_offset = 50;
        let actions = fixture.follow(&fixture.result(&request));
        assert_eq!(actions.len(), 1);
        let args = &actions[0];
        assert_eq!(
            args[0],
            if operation.is_empty() {
                "search"
            } else {
                "history"
            }
        );
        assert!(args.contains(&"--raw".into()));
        assert!(args.contains(&"--offset=0".into()));
        assert!(args.contains(&"--date-offset=0".into()));
        assert!(args.contains(&format!("--query={}", request.query)));
        let next = crate::search_request(args, operation, request.offset_minutes).unwrap();
        let result = fixture.result(&next);
        assert!(result.contains("\"search_scope\":\"source_records\""));
        assert!(fixture.follow(&result).is_empty());
    }
}

#[test]
fn history_time_bucket_actions_keep_tighter_timestamp_bounds() {
    let fixture = Fixture::new("history-time-bounds");
    let mut request = fixture.request("history");
    request.from = Some("2026-08-16T10:30:00Z".into());
    request.to = Some("2026-08-18T09:30:00Z".into());
    request.range = Range::parse(
        request.from.as_deref(),
        request.to.as_deref(),
        request.offset_minutes,
    )
    .unwrap();
    let actions = fixture.follow(&fixture.result(&request));
    assert_eq!(actions.len(), 4, "two dates, a match page, and a date page");
    for args in actions {
        let next = crate::search_request(&args, &args[0], request.offset_minutes).unwrap();
        for timestamp in ["2026-08-16T10:00:00Z", "2026-08-18T10:00:00Z"] {
            assert!(
                !next.range.contains(timestamp, next.offset_minutes),
                "{args:?} widened the original range"
            );
        }
        assert!(fixture.result(&next).contains("\"returned\":1"));
    }
}

#[test]
fn timeline_actions_execute_in_the_selected_corpus() {
    let fixture = Fixture::new("timeline");
    let search = fixture.request("");
    let request = TimelineRequest {
        offset_minutes: search.offset_minutes,
        source_id: search.source_id,
        from: search.from,
        to: search.to,
        range: search.range,
        limit: 1,
        offset: 5,
        order: TimelineOrder::Asc,
        ..TimelineRequest::default()
    };
    let result = json::Object::new()
        .raw(
            "next_actions",
            &json::array([timeline_next_action(
                &fixture.corpus,
                &request,
                "2026-08-16",
            )]),
        )
        .finish();
    let actions = fixture.follow(&result);
    assert_eq!(actions.len(), 1);
    let args = &actions[0];
    assert_eq!(args[0], "timeline");
    for expected in [
        "--date=2026-08-16",
        "--offset=0",
        "--limit=1",
        "--order=asc",
    ] {
        assert!(args.iter().any(|arg| arg == expected), "{args:?}");
    }
}

#[test]
fn stale_source_actions_sync_the_selected_corpus() {
    use std::io::Write;

    let fixture = Fixture::new("stale-source");
    std::fs::OpenOptions::new().append(true).open(fixture.root.join("session.jsonl")).unwrap()
        .write_all(b"{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":\"appended\"},\"sessionId\":\"s1\"}\n").unwrap();
    let mut request = fixture.request("");
    request.query = "absent".into();
    let actions = fixture.follow(&fixture.result(&request));
    assert_eq!(actions.len(), 2);
    assert_eq!(actions[0][0], "sync");
    assert_eq!(actions[1][0], "search");
}
