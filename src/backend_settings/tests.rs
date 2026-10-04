//! Tests for the backend settings engine: the python-dotenv semantic probe
//! matrix (analysis §1.1), lossless round-trips, edit operations, quoting
//! rules, the key registry, validation, template generation, the drift guard
//! and the atomic write pipeline.

use super::*;
use std::path::PathBuf;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Unique temp directory (no `tempfile` dependency in this crate).
fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "chibi-tui-settings-{}-{}-{}",
        tag,
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn active_value(doc: &SettingsDoc, key: &str) -> String {
    match doc.get(key) {
        ValueState::Active { value } => value,
        other => panic!("expected active value for {}, got {:?}", key, other),
    }
}

// ---------------------------------------------------------------------------
// Parse semantics — every row of the analysis §1.1 probe matrix
// ---------------------------------------------------------------------------

#[test]
fn plain_value_parses_as_is() {
    let doc = SettingsDoc::parse("KEY1=plain\n");
    assert_eq!(active_value(&doc, "KEY1"), "plain");
}

#[test]
fn double_quoted_value_has_quotes_stripped() {
    let doc = SettingsDoc::parse("KEY2=\"quoted value\"\n");
    assert_eq!(active_value(&doc, "KEY2"), "quoted value");
}

#[test]
fn single_quoted_value_is_literal() {
    let doc = SettingsDoc::parse("KEY3='single'\n");
    assert_eq!(active_value(&doc, "KEY3"), "single");
}

#[test]
fn inline_comment_stripped_only_after_whitespace() {
    let doc = SettingsDoc::parse("KEY4=has # inline comment\n");
    assert_eq!(active_value(&doc, "KEY4"), "has");
}

#[test]
fn bare_hash_glued_to_value_survives() {
    let doc = SettingsDoc::parse("KEY9=hash#nospace\n");
    assert_eq!(active_value(&doc, "KEY9"), "hash#nospace");
}

#[test]
fn hash_after_quoted_value_starts_inline_comment() {
    let doc = SettingsDoc::parse("KEY=\"a value\" # trailing comment\n");
    assert_eq!(active_value(&doc, "KEY"), "a value");
}

#[test]
fn double_quoted_value_may_span_physical_lines() {
    let doc = SettingsDoc::parse("KEY5=\"multi\nline\"\nNEXT=1\n");
    assert_eq!(active_value(&doc, "KEY5"), "multi\nline");
    assert_eq!(active_value(&doc, "NEXT"), "1");
}

#[test]
fn export_prefix_is_accepted() {
    let doc = SettingsDoc::parse("export KEY6=exported\n");
    assert_eq!(active_value(&doc, "KEY6"), "exported");
}

#[test]
fn whitespace_around_key_and_value_is_trimmed() {
    let doc = SettingsDoc::parse("  KEY7  =  spaced  \n");
    assert_eq!(active_value(&doc, "KEY7"), "spaced");
}

#[test]
fn variable_references_pass_through_without_interpolation() {
    let doc = SettingsDoc::parse("KEY1=plain\nKEY8=${KEY1}-suffix\n");
    assert_eq!(active_value(&doc, "KEY8"), "${KEY1}-suffix");
}

#[test]
fn empty_value_is_set_but_empty() {
    let doc = SettingsDoc::parse("KEY10=\n");
    assert_eq!(
        doc.get("KEY10"),
        ValueState::Active {
            value: String::new()
        }
    );
}

#[test]
fn duplicate_keys_resolve_last_wins() {
    let doc = SettingsDoc::parse("DUP=first\nDUP=second\n");
    assert_eq!(active_value(&doc, "DUP"), "second");
}

#[test]
fn commented_keys_are_ignored_by_the_loader() {
    let doc = SettingsDoc::parse("#KEY12=commented\n# KEY13=also\n");
    assert_eq!(
        doc.get("KEY12"),
        ValueState::Commented {
            value: Some("commented".into())
        }
    );
    assert_eq!(
        doc.get("KEY13"),
        ValueState::Commented {
            value: Some("also".into())
        }
    );
}

#[test]
fn escaped_quotes_inside_double_quoted_value_decode() {
    let doc = SettingsDoc::parse("K=\"say \\\"hi\\\" now\"\n");
    assert_eq!(active_value(&doc, "K"), "say \"hi\" now");
}

#[test]
fn multiline_with_escaped_quote_on_opening_line() {
    let doc = SettingsDoc::parse("K=\"a \\\"\nstill inside\"\nNEXT=1\n");
    assert_eq!(active_value(&doc, "K"), "a \"\nstill inside");
    assert_eq!(active_value(&doc, "NEXT"), "1");
}

#[test]
fn unterminated_quote_swallows_the_rest_without_panic() {
    let doc = SettingsDoc::parse("K=\"never closed\nNEXT=1\n");
    assert_eq!(active_value(&doc, "K"), "never closed\nNEXT=1");
    assert_eq!(doc.get("NEXT"), ValueState::Absent);
}

#[test]
fn non_key_junk_lines_are_preserved_as_other() {
    let text = "this is not a key line\nKEY=1\n";
    let doc = SettingsDoc::parse(text);
    assert_eq!(doc.render(), text);
    assert_eq!(active_value(&doc, "KEY"), "1");
}

#[test]
fn comment_prose_lines_are_not_mistaken_for_keys() {
    let doc = SettingsDoc::parse("# Comma-separated list of user IDs\n# Example: \"1,2\"\nK=1\n");
    assert_eq!(doc.get("Example"), ValueState::Absent);
    assert_eq!(doc.get("Comma"), ValueState::Absent);
    assert_eq!(active_value(&doc, "K"), "1");
}

// ---------------------------------------------------------------------------
// Preservation: EOL kind, BOM, trailing newline, CRLF
// ---------------------------------------------------------------------------

#[test]
fn crlf_line_endings_are_preserved() {
    let text = "A=1\r\nB=2\r\n";
    let doc = SettingsDoc::parse(text);
    assert_eq!(doc.render(), text);
    assert_eq!(active_value(&doc, "A"), "1");
}

#[test]
fn byte_order_mark_is_preserved() {
    let text = "\u{feff}A=1\n";
    let doc = SettingsDoc::parse(text);
    assert_eq!(doc.render(), text);
    assert_eq!(active_value(&doc, "A"), "1");
}

#[test]
fn missing_trailing_newline_is_preserved() {
    let text = "A=1\nB=2";
    let doc = SettingsDoc::parse(text);
    assert_eq!(doc.render(), text);
}

#[test]
fn mixed_eol_kinds_are_preserved() {
    let text = "A=1\nB=2\r\nC=3\n";
    let doc = SettingsDoc::parse(text);
    assert_eq!(doc.render(), text);
}

#[test]
fn non_utf8_file_is_reported_not_parsed_silently() {
    let dir = temp_dir("non-utf8");
    let path = dir.join("settings");
    std::fs::write(&path, [0xff, 0xfe, b'K', b'=', b'1', b'\n']).unwrap();
    match load_settings(&path) {
        Err(SettingsError::NotUtf8) => {}
        other => panic!("expected NotUtf8, got {:?}", other),
    }
    std::fs::remove_dir_all(&dir).unwrap();
}

// ---------------------------------------------------------------------------
// Round-trip byte-exactness (required)
// ---------------------------------------------------------------------------

#[test]
fn round_trip_is_byte_exact_on_a_complex_document() {
    let text = "\u{feff}# header comment\n\nexport A=\"quoted value\"\nB='single'\n\
                C=has # inline comment\nD=hash#nospace\nE=\nF=\"multi\nline\" span\n\r\n\
                G=${A}-ref\n#H=hidden\n# I=hidden too\n# ==================\n# SECTION TITLE\n\
                # ==================\nJ  =  spaced  \nlast=no-newline";
    let doc = SettingsDoc::parse(text);
    assert_eq!(doc.render(), text);
}

#[test]
fn vendored_template_round_trips_byte_exactly() {
    let template = strip_provenance_header(VENDORED_TEMPLATE);
    let doc = SettingsDoc::parse(template);
    assert_eq!(doc.render(), template);
}

// ---------------------------------------------------------------------------
// Edit operations
// ---------------------------------------------------------------------------

#[test]
fn set_patches_only_the_target_line() {
    let text = "before=1\nTARGET=old\nafter=2\n";
    let mut doc = SettingsDoc::parse(text);
    doc.set_key("TARGET", "new", None, None);
    assert_eq!(doc.render(), "before=1\nTARGET=new\nafter=2\n");
    assert_eq!(active_value(&doc, "TARGET"), "new");
}

#[test]
fn set_preserves_indent_export_and_inline_comment() {
    let text = "  export TARGET=old # keep me\n";
    let mut doc = SettingsDoc::parse(text);
    doc.set_key("TARGET", "new", None, None);
    assert_eq!(doc.render(), "  export TARGET=new # keep me\n");
}

#[test]
fn set_uncomments_a_commented_key_in_place() {
    let mut doc = SettingsDoc::parse("# STT_PROVIDER=\nOTHER=1\n");
    doc.set_key("STT_PROVIDER", "OpenAI", None, None);
    assert_eq!(doc.render(), "STT_PROVIDER=OpenAI\nOTHER=1\n");
    assert_eq!(
        doc.get("STT_PROVIDER"),
        ValueState::Active {
            value: "OpenAI".into()
        }
    );
}

#[test]
fn set_uncomments_glued_hash_style_in_place() {
    let mut doc = SettingsDoc::parse("#DELEGATE_TASK_TIMEOUT=600\n");
    doc.set_key("DELEGATE_TASK_TIMEOUT", "300", None, None);
    assert_eq!(doc.render(), "DELEGATE_TASK_TIMEOUT=300\n");
}

#[test]
fn set_prefers_active_line_over_commented_duplicate() {
    let mut doc = SettingsDoc::parse("# K=old\nK=active\n");
    doc.set_key("K", "new", None, None);
    assert_eq!(doc.render(), "# K=old\nK=new\n");
}

#[test]
fn clear_comments_the_line_out_preserving_the_value() {
    let mut doc = SettingsDoc::parse("SECRET_KEY=abc123\n");
    doc.clear("SECRET_KEY");
    assert_eq!(doc.render(), "# SECRET_KEY=abc123\n");
    assert_eq!(
        doc.get("SECRET_KEY"),
        ValueState::Commented {
            value: Some("abc123".into())
        }
    );
}

#[test]
fn clear_is_a_noop_for_absent_or_already_commented_keys() {
    let text = "# K=old\n";
    let mut doc = SettingsDoc::parse(text);
    doc.clear("K");
    doc.clear("MISSING");
    assert_eq!(doc.render(), text);
}

#[test]
fn append_places_key_under_matching_section_banner() {
    let text = "# ==================\n# 6. MCP CONFIGURATION\n# ==================\n\n\
                ENABLE_MCP_SSE=true\n\n# ==================\n# 7. NEXT SECTION\n# ==================\n\
                MODELS_WHITELIST=\n";
    let mut doc = SettingsDoc::parse(text);
    doc.set_key(
        "NEW_KEY",
        "v",
        Some("6. MCP CONFIGURATION"),
        Some("A new setting"),
    );
    let out = doc.render();
    assert!(
        out.contains("ENABLE_MCP_SSE=true\n\n# A new setting\nNEW_KEY=v\n"),
        "append misplaced:\n{}",
        out
    );
    assert!(out.contains("# 7. NEXT SECTION"));
}

#[test]
fn append_without_matching_section_goes_to_eof() {
    let mut doc = SettingsDoc::parse("A=1\nB=2\n");
    doc.set_key(
        "NEW_KEY",
        "v",
        Some("NO SUCH SECTION"),
        Some("Description here"),
    );
    let out = doc.render();
    assert!(
        out.ends_with("\n\n# Description here\nNEW_KEY=v\n"),
        "got:\n{}",
        out
    );
}

#[test]
fn append_into_empty_document_has_no_leading_blank() {
    let mut doc = SettingsDoc::parse("");
    doc.set_key("NEW_KEY", "v", None, Some("Description"));
    assert_eq!(doc.render(), "# Description\nNEW_KEY=v\n");
}

#[test]
fn set_multiline_value_serializes_with_escapes() {
    let mut doc = SettingsDoc::parse("K=plain\n");
    doc.set_key("K", "line1\nline2", None, None);
    assert_eq!(doc.render(), "K=\"line1\\nline2\"\n");
    assert_eq!(active_value(&doc, "K"), "line1\nline2");
}

#[test]
fn setting_empty_value_writes_bare_key_equals() {
    let mut doc = SettingsDoc::parse("K=old\n");
    doc.set_key("K", "", None, None);
    assert_eq!(doc.render(), "K=\n");
    assert_eq!(
        doc.get("K"),
        ValueState::Active {
            value: String::new()
        }
    );
}

#[test]
fn debug_impls_never_leak_values() {
    let doc = SettingsDoc::parse("SECRET_KEY=super-secret-value-12345\n");
    let debugged = format!("{:?}\n{:?}", doc, doc.get("SECRET_KEY"));
    assert!(
        !debugged.contains("super-secret-value-12345"),
        "leaked via Debug: {}",
        debugged
    );
}

// ---------------------------------------------------------------------------
// Quoting / serialization rules (analysis §2.1)
// ---------------------------------------------------------------------------

#[test]
fn serialization_quotes_dangerous_values_and_round_trips() {
    let cases = [
        "has # comment",
        "hash#nospace",
        "with \"quotes\"",
        " padded ",
        "${VAR}-ref",
        "a=b",
        "back\\slash",
        "multi\nline",
    ];
    for value in cases {
        let mut doc = SettingsDoc::parse("K=x\n");
        doc.set_key("K", value, None, None);
        let rendered = doc.render();
        let reparsed = SettingsDoc::parse(&rendered);
        assert_eq!(
            active_value(&reparsed, "K"),
            value,
            "round-trip failed for {:?}",
            value
        );
        let _ = rendered;
    }
}

#[test]
fn serialization_leaves_safe_values_unquoted() {
    assert_eq!(serialize_value("plain"), "plain");
    assert_eq!(serialize_value("with spaces"), "with spaces");
    assert_eq!(serialize_value(""), "");
    assert_eq!(
        serialize_value("http://localhost:1234/v1"),
        "http://localhost:1234/v1"
    );
}

// ---------------------------------------------------------------------------
// Key registry
// ---------------------------------------------------------------------------

#[test]
fn registry_row_counts_match_the_approved_slice() {
    assert_eq!(Group::ProviderKeys.keys().count(), 31);
    assert_eq!(Group::MediaProviders.keys().count(), 2);
    assert_eq!(Group::ModelRouting.keys().count(), 12);
    assert_eq!(Group::CoreBehaviour.keys().count(), 12);
    assert_eq!(Group::Restrictions.keys().count(), 4);
    assert_eq!(Group::AgentPaths.keys().count(), 3);
    assert_eq!(REGISTRY.len(), 64);
}

#[test]
fn registry_group1_is_30_secrets_plus_customopenai_url() {
    let secrets = Group::ProviderKeys
        .keys()
        .filter(|d| d.kind == Kind::Secret)
        .count();
    assert_eq!(secrets, 30);
    let url = registry_get("CUSTOMOPENAI_URL").unwrap();
    assert_eq!(url.kind, Kind::PlainStr);
    assert_eq!(url.default, "http://localhost:1234/v1");
    assert!(!url.secret);
}

#[test]
fn registry_secrets_and_commented_flags_are_correct() {
    for def in Group::ProviderKeys.keys() {
        // 30 Secret rows + CUSTOMOPENAI_URL (PlainStr) in group 1.
        assert_eq!(
            def.secret,
            def.key != "CUSTOMOPENAI_URL",
            "{} secret flag",
            def.key
        );
    }
    for def in Group::ModelRouting.keys() {
        assert_eq!(
            def.commented,
            !def.key.starts_with("DEFAULT_"),
            "{} commented flag wrong",
            def.key
        );
        assert_eq!(def.kind, Kind::PlainStr);
    }
    for def in Group::Restrictions.keys() {
        assert_eq!(def.kind, Kind::Csv);
    }
    for def in Group::AgentPaths.keys() {
        assert_eq!(def.kind, Kind::Path);
    }
    let proxy = registry_get("PROXY").unwrap();
    assert!(proxy.commented);
}

#[test]
fn registry_defaults_match_the_template() {
    let expected = [
        ("FILESYSTEM_ACCESS", "false"),
        ("ENABLE_MCP_STDIO", "false"),
        ("ENABLE_MCP_SSE", "true"),
        ("ALLOW_DELEGATION", "true"),
        ("TEMPERATURE", "1.0"),
        ("MAX_TOKENS", "32000"),
        ("RETRIES", "3"),
        ("TIMEOUT", "180"),
        ("SHOW_LLM_THOUGHTS", "false"),
        ("LOG_PROMPT_DATA", "false"),
        ("PUBLIC_MODE", "false"),
    ];
    for (key, default) in expected {
        assert_eq!(
            registry_get(key).unwrap().default,
            default,
            "{} default",
            key
        );
    }
    assert!(
        registry_get("TELEGRAM_BOT_TOKEN").is_none(),
        "telegram keys are out of scope"
    );
    assert!(
        registry_get("CLIENT").is_none(),
        "CLIENT must not be exposed"
    );
    assert!(
        registry_get("REDIS").is_none(),
        "storage group is out of scope"
    );
}

#[test]
fn registry_environment_names_match_the_backend_configs() {
    // Spot-check the owner-verified env names (analysis §2.2).
    for key in [
        "OPENAI_API_KEY",
        "MISTRALAI_API_KEY",
        "MOONSHOTAI_API_KEY",
        "CHEAPERINFERENCE_API_KEY",
        "CLOUDFLARE_ACCOUNT_ID",
        "SUNO_API_ORG_API_KEY",
        "ELEVEN_LABS_API_KEY",
        "IMAGE_GENERATIONS_WHITELIST",
    ] {
        assert!(registry_get(key).is_some(), "{} missing from registry", key);
    }
}

// ---------------------------------------------------------------------------
// Validation
// ---------------------------------------------------------------------------

#[test]
fn bool_validation_matches_pydantic_parsing() {
    for ok in ["true", "false", "TRUE", "False", "1", "0", "yes", "No"] {
        assert!(
            validate_value(&Kind::Bool, ok).is_ok(),
            "{} should be valid",
            ok
        );
    }
    for bad in ["", "2", "on", "maybe", "true false"] {
        assert!(
            validate_value(&Kind::Bool, bad).is_err(),
            "{} should be invalid",
            bad
        );
    }
}

#[test]
fn int_validation_enforces_range_and_parse() {
    let kind = Kind::Int { min: 1, max: 100 };
    assert!(validate_value(&kind, "50").is_ok());
    assert!(validate_value(&kind, " 7 ").is_ok());
    assert!(validate_value(&kind, "0").is_err());
    assert!(validate_value(&kind, "101").is_err());
    assert!(validate_value(&kind, "abc").is_err());
}

#[test]
fn float_validation_enforces_range() {
    let kind = Kind::Float { min: 0.0, max: 2.0 };
    assert!(validate_value(&kind, "1.0").is_ok());
    assert!(validate_value(&kind, "0").is_ok());
    assert!(validate_value(&kind, "2.5").is_err());
    assert!(validate_value(&kind, "hot").is_err());
    assert!(validate_value(&kind, "NaN").is_err());
}

#[test]
fn enum_and_csv_and_path_validation() {
    let kind = Kind::Enum(&["local", "telegram"]);
    assert!(validate_value(&kind, "telegram").is_ok());
    assert!(validate_value(&kind, "s3").is_err());

    let csv = Kind::Csv;
    assert!(validate_value(&csv, "").is_ok());
    assert!(validate_value(&csv, "a, b,c").is_ok());
    assert!(validate_value(&csv, "a,,c").is_err());
    assert!(validate_value(&csv, "a,").is_err());

    let path = Kind::Path;
    assert!(validate_value(&path, "~/chibi-bot/home").is_ok());
    assert!(validate_value(&path, "/abs/path").is_ok());
    assert!(validate_value(&path, "").is_err());
}

#[test]
fn validate_doc_returns_structured_per_row_errors() {
    let text = "TEMPERATURE=9.9\nRETRIES=abc\nOPENAI_API_KEY=sk-x\n";
    let doc = SettingsDoc::parse(text);
    let errors = validate_doc(&doc);
    let keys: Vec<&str> = errors.iter().map(|e| e.key).collect();
    assert_eq!(keys, vec!["TEMPERATURE", "RETRIES"]);
    assert!(errors.iter().all(|e| !e.message.is_empty()));
}

// ---------------------------------------------------------------------------
// Template generation + drift guard
// ---------------------------------------------------------------------------

#[test]
fn generated_output_interpolates_paths_and_forces_defaults() {
    let base = PathBuf::from("/tmp/fake-home/chibi-bot");
    let text = generate_settings_text(&base);
    assert!(text.contains(&format!("LOCAL_DATA_PATH={}", base.join("data").display())));
    assert!(text.contains(&format!("HOME_DIR={}", base.join("home").display())));
    // The template uses the HOME_DIR placeholder for WORKING_DIR (mirrored).
    assert!(text.contains(&format!("WORKING_DIR={}", base.join("home").display())));
    assert!(text.contains(&format!("SKILLS_DIR={}", base.join("skills").display())));
    assert!(text.contains("FILESYSTEM_ACCESS=true"));
    assert!(text.contains("ENABLE_MCP_STDIO=true"));
    assert!(text.contains("ENABLE_INFINITE_CONTEXT=true"));
    assert!(!text.contains("FILESYSTEM_ACCESS=false"));
    assert!(!text.contains("ENABLE_INFINITE_CONTEXT=false"));
    // Provenance header is stripped; the template body starts at its own header.
    assert!(text.starts_with("# Chibi Agent Configuration File"));
}

#[test]
fn write_generated_settings_creates_dirs_and_file_only_once() {
    let dir = temp_dir("generate");
    let base = dir.join("chibi-bot");
    let path = base.join("settings");
    assert!(write_generated_settings(&path).unwrap());
    for subdir in ["", "data", "skills", "home"] {
        assert!(base.join(subdir).is_dir(), "missing dir {}", subdir);
    }
    let written = std::fs::read_to_string(&path).unwrap();
    assert!(written.contains("FILESYSTEM_ACCESS=true"));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "generated file must be 0600");
    }
    // Second call is a no-op.
    assert!(!write_generated_settings(&path).unwrap());
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn vendored_template_matches_backend_checkout_drift_guard() {
    // Skipped when the backend checkout is not found (e.g. CI without it).
    let backend_src = Path::new("../chibi/chibi/config_generator.py");
    if !backend_src.exists() {
        eprintln!("backend checkout not found at ../chibi — drift guard skipped");
        return;
    }
    let src = std::fs::read_to_string(backend_src).unwrap();
    let marker = "DEFAULT_CONFIG = f\"\"\"";
    let start = src
        .find(marker)
        .expect("DEFAULT_CONFIG not found in backend source")
        + marker.len();
    let end = src[start..].find("\n\"\"\"").expect("closing triple quote") + start + 1;
    let template = &src[start..end];
    assert_eq!(
        strip_provenance_header(VENDORED_TEMPLATE),
        template,
        "vendored template drifted from backend main — re-copy the fixture"
    );
}

#[test]
fn settings_path_is_literal_home_chibi_bot() {
    // The backend hardcodes `~/chibi-bot/settings` (no XDG); mirror it.
    if let Some(home) = dirs::home_dir() {
        assert_eq!(
            settings_path().unwrap(),
            home.join("chibi-bot").join("settings")
        );
        assert_eq!(settings_base_dir().unwrap(), home.join("chibi-bot"));
    }
}

#[cfg(windows)]
#[test]
fn windows_path_shape_uses_platform_separators() {
    let base = PathBuf::from("C:\\Users\\someone\\chibi-bot");
    let text = generate_settings_text(&base);
    assert!(text.contains("HOME_DIR=C:\\Users\\someone\\chibi-bot\\home"));
}

// ---------------------------------------------------------------------------
// Atomic write pipeline
// ---------------------------------------------------------------------------

#[test]
fn save_writes_atomically_with_backup_and_chmod() {
    let dir = temp_dir("save");
    let path = dir.join("settings");
    std::fs::write(&path, "A=1\n").unwrap();
    let (doc, meta) = load_settings(&path).unwrap();

    let mut edited = doc;
    edited.set_key("A", "2", None, None);
    let new_meta = save_settings(&edited, &path, Some(meta)).unwrap();

    let content = std::fs::read_to_string(&path).unwrap();
    assert_eq!(content, "A=2\n");
    assert!(
        !dir.join("settings.tmp").exists(),
        "temp file must be renamed away"
    );
    assert_eq!(
        std::fs::read_to_string(dir.join("settings.bak")).unwrap(),
        "A=1\n"
    );
    assert_eq!(new_meta.len, content.len() as u64);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn save_refuses_when_file_changed_on_disk() {
    let dir = temp_dir("guard");
    let path = dir.join("settings");
    std::fs::write(&path, "A=1\n").unwrap();
    let (doc, meta) = load_settings(&path).unwrap();

    // External edit after load.
    std::fs::write(&path, "A=external\n").unwrap();
    let mut edited = doc;
    edited.set_key("A", "2", None, None);
    match save_settings(&edited, &path, Some(meta)) {
        Err(SettingsError::ChangedOnDisk) => {}
        other => panic!("expected ChangedOnDisk, got {:?}", other),
    }
    // The external edit is not clobbered.
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "A=external\n");

    // Force (overwrite policy) succeeds and replaces the content.
    save_settings(&edited, &path, None).unwrap();
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "A=2\n");
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn save_creates_file_when_missing_without_backup() {
    let dir = temp_dir("create");
    let path = dir.join("settings");
    let doc = SettingsDoc::parse("A=1\n");
    let meta = save_settings(&doc, &path, None).unwrap();
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "A=1\n");
    assert!(!dir.join("settings.bak").exists());
    assert_eq!(meta.len, 4);
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn load_captures_external_change_guard_metadata() {
    let dir = temp_dir("meta");
    let path = dir.join("settings");
    std::fs::write(&path, "A=1\n").unwrap();
    let (_, meta) = load_settings(&path).unwrap();
    let stat = stat_settings(&path).unwrap().unwrap();
    assert_eq!(meta, stat);
    assert_eq!(meta.len, 4);
    std::fs::remove_dir_all(&dir).unwrap();
}

// ---------------------------------------------------------------------------
// Realistic end-to-end flow over the real template
// ---------------------------------------------------------------------------

#[test]
fn template_edit_flow_patches_registry_keys_in_place() {
    let base = PathBuf::from("/tmp/fake-home/chibi-bot");
    let mut doc = SettingsDoc::parse(&generate_settings_text(&base));

    // Active key: patch.
    doc.set(registry_get("TEMPERATURE").unwrap(), "0.5");
    // Commented key: uncomment-in-place.
    doc.set(registry_get("STT_PROVIDER").unwrap(), "OpenAI");
    // Secret: set.
    doc.set(registry_get("MELIOUS_API_KEY").unwrap(), "sk-test");

    assert_eq!(
        doc.get("TEMPERATURE"),
        ValueState::Active {
            value: "0.5".into()
        }
    );
    assert_eq!(
        doc.get("STT_PROVIDER"),
        ValueState::Active {
            value: "OpenAI".into()
        }
    );
    assert_eq!(
        doc.get("MELIOUS_API_KEY"),
        ValueState::Active {
            value: "sk-test".into()
        }
    );
    // Untouched keys keep their generated values.
    assert_eq!(
        doc.get("MAX_TOKENS"),
        ValueState::Active {
            value: "32000".into()
        }
    );
    assert_eq!(
        doc.get("FILESYSTEM_ACCESS"),
        ValueState::Active {
            value: "true".into()
        }
    );

    // Validation of the whole edited document passes.
    assert!(
        validate_doc(&doc).is_empty(),
        "unexpected validation errors: {:?}",
        validate_doc(&doc)
    );

    // The edited file still parses and preserves untouched lines verbatim.
    let regenerated = generate_settings_text(&base);
    for untouched in [
        "CUSTOMOPENAI_URL=http://localhost:1234/v1",
        "RETRIES=3",
        "TIMEOUT=180",
    ] {
        assert!(doc.render().contains(untouched), "lost line: {}", untouched);
        assert!(regenerated.contains(untouched));
    }
}
