//! The transcript fold, exercised against representative session lines.

use pilus_session::{ItemKind, Transcript};

/// Fold a sequence of JSONL lines.
fn fold(lines: &[&str]) -> Transcript {
    let mut transcript = Transcript::default();
    for line in lines {
        transcript.push_line(line);
    }
    transcript
}

#[test]
fn projects_roles_blocks_and_usage() {
    let transcript = fold(&[
        r#"{"type":"session","version":3,"id":"s1","timestamp":"2024-01-01T00:00:00.000Z","cwd":"/app"}"#,
        r#"{"type":"model_change","id":"m1","parentId":null,"timestamp":"t1","provider":"anthropic","modelId":"claude"}"#,
        r#"{"type":"message","id":"u1","parentId":"m1","timestamp":"t2","message":{"role":"user","content":"do it","timestamp":1}}"#,
        r#"{"type":"message","id":"a1","parentId":"u1","timestamp":"t3","message":{"role":"assistant","provider":"anthropic","model":"claude","content":[{"type":"thinking","thinking":"hmm"},{"type":"text","text":"on it"},{"type":"toolCall","id":"c1","name":"bash","arguments":{"command":"ls"}}],"usage":{"input":10,"output":2,"cacheRead":4,"cacheWrite":0,"totalTokens":16,"cost":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0,"total":0.5}},"stopReason":"toolUse","timestamp":2}}"#,
        r#"{"type":"message","id":"r1","parentId":"a1","timestamp":"t4","message":{"role":"toolResult","toolCallId":"c1","toolName":"bash","content":[{"type":"text","text":"file.txt"}],"isError":false,"timestamp":3}}"#,
    ]);

    assert!(transcript.has_header);
    assert_eq!(transcript.header.id, "s1");
    assert_eq!(transcript.header.cwd, "/app");
    assert_eq!(transcript.header.version, 3);
    assert_eq!(transcript.model.as_deref(), Some("anthropic/claude"));

    // model note, user, thinking, assistant, tool call (with its result).
    assert_eq!(transcript.items.len(), 5);
    match &transcript.items[0].kind {
        ItemKind::Note { title, text } => {
            assert_eq!(title, "model");
            assert_eq!(text, "anthropic/claude");
        }
        other => panic!("expected model note, got {other:?}"),
    }
    match &transcript.items[1].kind {
        ItemKind::User { text, .. } => assert_eq!(text, "do it"),
        other => panic!("expected user, got {other:?}"),
    }
    match &transcript.items[2].kind {
        ItemKind::Thinking { text } => assert_eq!(text, "hmm"),
        other => panic!("expected thinking, got {other:?}"),
    }
    match &transcript.items[3].kind {
        ItemKind::Assistant { text, model } => {
            assert_eq!(text, "on it");
            assert_eq!(model.as_deref(), Some("claude"));
        }
        other => panic!("expected assistant, got {other:?}"),
    }
    match &transcript.items[4].kind {
        ItemKind::ToolCall {
            call_id,
            name,
            outcome,
            ..
        } => {
            assert_eq!(call_id, "c1");
            assert_eq!(name, "bash");
            let outcome = outcome.as_ref().expect("result attached to its call");
            assert_eq!(outcome.text, "file.txt");
            assert!(!outcome.is_error);
        }
        other => panic!("expected tool call, got {other:?}"),
    }

    assert_eq!(transcript.usage.total, 16);
    assert_eq!(transcript.usage.input, 10);
    assert_eq!(transcript.usage.cache_read, 4);
    assert!((transcript.usage.cost - 0.5).abs() < f64::EPSILON);
}

#[test]
fn models_are_tracked_and_thinking_level_syncs() {
    let transcript = fold(&[
        r#"{"type":"model_change","id":"m1","parentId":null,"timestamp":"t1","provider":"anthropic","modelId":"claude"}"#,
        r#"{"type":"message","id":"a1","parentId":"m1","timestamp":"t2","message":{"role":"assistant","provider":"openai","model":"gpt","thinkingLevel":"high","content":[{"type":"text","text":"hi"}],"timestamp":2}}"#,
        r#"{"type":"thinking_level_change","id":"t1","parentId":"a1","timestamp":"t3","thinkingLevel":"low"}"#,
    ]);
    assert_eq!(
        transcript.models,
        vec!["anthropic/claude".to_string(), "openai/gpt".to_string()]
    );
    assert_eq!(transcript.model.as_deref(), Some("openai/gpt"));
    // The latest explicit change wins over the message-recorded level.
    assert_eq!(transcript.thinking_level.as_deref(), Some("low"));
}

#[test]
fn a_result_without_its_call_stays_its_own_item() {
    let transcript = fold(&[
        r#"{"type":"message","id":"r1","parentId":null,"timestamp":"t","message":{"role":"toolResult","toolCallId":"gone","toolName":"bash","content":[{"type":"text","text":"late"}],"isError":true,"timestamp":3}}"#,
    ]);
    match &transcript.items[0].kind {
        ItemKind::ToolResult {
            name,
            text,
            is_error,
            ..
        } => {
            assert_eq!(name, "bash");
            assert_eq!(text, "late");
            assert!(*is_error);
        }
        other => panic!("expected tool result, got {other:?}"),
    }
}

#[test]
fn system_prompt_is_plumbing_and_kept_out() {
    let transcript = fold(&[
        r#"{"type":"session","version":3,"id":"s","cwd":"/app","timestamp":"t"}"#,
        r#"{"type":"message","id":"sys","parentId":null,"timestamp":"t","message":{"role":"system","content":"","sections":{"preamble":"You are…"},"timestamp":0}}"#,
    ]);
    assert!(transcript.items.is_empty());
    assert_eq!(transcript.unknown, 0);
}

#[test]
fn unknown_entries_are_counted_not_fatal() {
    let transcript = fold(&[
        r#"{"type":"session","version":3,"id":"s","cwd":"/app","timestamp":"t"}"#,
        r#"{"type":"future_thing","id":"x","timestamp":"t"}"#,
        "not json at all",
        "",
    ]);
    assert_eq!(transcript.unknown, 1);
    assert_eq!(transcript.items.len(), 0);
}

#[test]
fn bash_and_notes_render_as_items() {
    let transcript = fold(&[
        r#"{"type":"session","version":3,"id":"s","cwd":"/app","timestamp":"t"}"#,
        r#"{"type":"thinking_level_change","id":"t1","parentId":null,"timestamp":"t1","thinkingLevel":"high"}"#,
        r#"{"type":"message","id":"b1","parentId":"t1","timestamp":"t2","message":{"role":"bashExecution","command":"ls","output":"a\nb","exitCode":0,"cancelled":false,"truncated":false,"timestamp":1}}"#,
        r#"{"type":"compaction","id":"c1","parentId":"b1","timestamp":"t3","summary":"summed up","firstKeptEntryId":"x","tokensBefore":100}"#,
        r#"{"type":"session_info","id":"n1","parentId":"c1","timestamp":"t4","name":"Refactor auth"}"#,
    ]);
    assert_eq!(transcript.thinking_level.as_deref(), Some("high"));
    assert_eq!(transcript.header.name.as_deref(), Some("Refactor auth"));
    let kinds: Vec<&ItemKind> = transcript.items.iter().map(|item| &item.kind).collect();
    assert!(matches!(kinds[0], ItemKind::Note { title, .. } if title == "thinking"));
    assert!(matches!(kinds[1], ItemKind::Bash { command, .. } if command == "ls"));
    assert!(matches!(kinds[2], ItemKind::Note { title, .. } if title == "compaction"));
    assert!(matches!(kinds[3], ItemKind::Note { title, .. } if title == "name"));
}

#[test]
fn image_only_user_turn_still_appears() {
    let transcript = fold(&[
        r#"{"type":"message","id":"u","parentId":null,"timestamp":"t","message":{"role":"user","content":[{"type":"image","data":"…","mimeType":"image/png"}],"timestamp":0}}"#,
    ]);
    match &transcript.items[0].kind {
        ItemKind::User { text, images } => {
            assert!(text.is_empty());
            assert_eq!(*images, 1);
        }
        other => panic!("expected user, got {other:?}"),
    }
}
