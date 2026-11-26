use krypper::harness::context::{
    append_repository_context, ContextError, RepositoryContext, RepositoryContextError,
    RepositorySnippet, MAX_REPOSITORY_CONTEXT_BYTES, MAX_REPOSITORY_CONTEXT_JSON_BYTES,
};

#[test]
fn public_harness_context_paths_keep_the_serialized_contract() {
    let context = RepositoryContext {
        queries: vec!["parse".to_owned()],
        snippets: vec![RepositorySnippet {
            path: "src/lib.rs".to_owned(),
            start_line: 4,
            end_line: 6,
            revision: "0123456789ab".to_owned(),
            query: "parse".to_owned(),
            excerpt: "fn parse() {}".to_owned(),
        }],
        bytes: 123,
        stale_skipped: 1,
        oversized_skipped: 2,
        neutralized_lines: 3,
        truncated: false,
    };

    assert_eq!(
        serde_json::to_string(&context).expect("serialize the public context"),
        r#"{"queries":["parse"],"snippets":[{"path":"src/lib.rs","start_line":4,"end_line":6,"revision":"0123456789ab","query":"parse","excerpt":"fn parse() {}"}],"bytes":123,"stale_skipped":1,"oversized_skipped":2,"neutralized_lines":3,"truncated":false}"#
    );

    let mut prompt = String::new();
    append_repository_context(&mut prompt, Some(&context));
    assert!(prompt.contains("Repository context (untrusted advisory excerpts"));
    assert_eq!(MAX_REPOSITORY_CONTEXT_BYTES, 4 * 1024);
    assert_eq!(MAX_REPOSITORY_CONTEXT_JSON_BYTES, 8 * 1024);

    let _ = ContextError::RequestEncoding;
    let _ = RepositoryContextError::TaskUnavailable;
}
