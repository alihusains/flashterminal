# Graph Report - /Users/alihusainsorathiya/Documents/projects/flashterminal  (2026-08-15)

## Corpus Check
- cluster-only mode — file stats not available

## Summary
- 3064 nodes · 8533 edges · 124 communities (102 shown, 22 thin omitted)
- Extraction: 99% EXTRACTED · 1% INFERRED · 0% AMBIGUOUS · INFERRED: 109 edges (avg confidence: 0.82)
- Token cost: 0 input · 0 output

## Graph Freshness
- Built from commit: `a2890392`
- Run `git rev-parse HEAD` and compare to check if the graph is stale.
- Run `graphify update .` after code changes (no API cost).

## Community Hubs (Navigation)
- orchestration.rs
- worktrees.rs
- PaneNode
- validate.rs
- Renderer
- Response
- adaptive.rs
- provider.rs
- Multiplexer
- GlyphCache
- ArtifactStore
- ApplicationEvent
- AgentRuntime
- credential.rs
- TerminalState
- AuditTrail
- phase3f/main.rs
- PlannerState
- desktop/src/main.rs
- Pi CLI Workspace Skill
- App
- collaboration.rs
- PlannerError
- phase2c/main.rs
- phase3b/main.rs
- execution.rs
- policy.rs
- Result
- phase3a/main.rs
- Session
- engine.rs
- AgentLaunchConfig
- .active_workspace
- work.rs
- terminal-core/src/lib.rs
- Row
- Performer
- ApprovalStore
- .new
- phase3d/main.rs
- phase3e/main.rs
- AgentWork
- Command
- phase4/main.rs
- main
- PolicyEngine
- benchmarks/src/main.rs
- redact.rs
- PiAdapter
- AgentDefinition
- .new
- Agent Abstraction
- main
- unicode.rs
- agent.rs
- phase3c/main.rs
- UX Specification
- Cell
- String
- BudgetDimension
- phase051.rs
- planning.rs
- .new
- Rust (Core Language)
- String
- agent_runtime.rs
- terminal-core Crate
- Multiplexer (Phase 1 Engine)
- DirtyTracker
- mod.rs
- Result
- Self
- real_agents.rs
- Pi RPC SDK Skill
- scrollback_bench.rs
- FakeAgentAdapter
- .default
- integration.rs
- ExecutionId
- AgentTimeline
- persistence.rs
- OpenCodeAdapter
- UI Thread · Single State Owner
- terminal_benchmarks.rs
- ClaudeCodeAdapter
- CodexAdapter
- WorkspaceId
- agent_stress.rs
- CountingAlloc
- process_chunk
- EngineMetrics
- soak.rs
- .with_wake
- real_agent_tasks.rs
- pipeline.rs
- .validate_plan
- AgentFilter
- orchestration_bench.rs
- .snapshot
- ShellInterpolationGuard
- paste_bench.rs
- plateau.rs
- raw_throughput.rs
- .fmt
- JSONL LF Framing
- Current Architecture (Phase 1)
- repowise
- repowise
- Event Bus
- run_benchmarks.sh
- AtomicBool
- GlyphRasterConfig
- SessionId
- SessionId
- Electron (Rejected Alternative)
- Cold Start Budget
- Drop
- Item
- Iterator
- Pane
- Receiver
- UnixStream

## God Nodes (most connected - your core abstractions)
1. `Multiplexer` - 300 edges
2. `TerminalState` - 101 edges
3. `App` - 86 edges
4. `ExecutionId` - 83 edges
5. `AgentRuntime` - 67 edges
6. `Renderer` - 52 edges
7. `Response` - 46 edges
8. `AgentLaunchConfig` - 45 edges
9. `TaskGraph` - 44 edges
10. `PlannerState` - 44 edges

## Surprising Connections (you probably didn't know these)
- `Pi RPC SDK Skill` --semantically_similar_to--> `Pi Agent Integration`  [INFERRED] [semantically similar]
  .pi/skills/pi-rpc-sdk/SKILL.md → README.md
- `state_bytes()` --references--> `TerminalState`  [EXTRACTED]
  benchmarks/src/bin/scrollback_bench.rs → crates/terminal-core/src/lib.rs
- `render_prep_10k_rows_ms()` --calls--> `resolve_color()`  [INFERRED]
  benchmarks/src/main.rs → crates/terminal-renderer/src/lib.rs
- `main()` --calls--> `find_fake_agent_bin()`  [INFERRED]
  benchmarks/src/bin/multiplex_bench.rs → crates/terminal-session/src/adapters/mod.rs
- `print_task_line()` --references--> `Task`  [EXTRACTED]
  apps/cli/src/main.rs → crates/terminal-session/src/orchestration.rs

## Import Cycles
- 2-file cycle: `crates/terminal-core/src/lib.rs -> crates/terminal-core/src/scrollback.rs -> crates/terminal-core/src/lib.rs`
- 2-file cycle: `crates/terminal-workspace/src/model.rs -> crates/terminal-workspace/src/pane_tree.rs -> crates/terminal-workspace/src/model.rs`

## Communities (124 total, 22 thin omitted)

### Community 0 - "orchestration.rs"
Cohesion: 0.05
Nodes (47): default_prepare_task(), Artifact, default_max_worktrees(), DependencyFailurePolicy, FailureClass, graph_detects_cycles(), graph_rejects_duplicate_and_missing_dependencies(), graph_validate_reports_structural_issues() (+39 more)

### Community 1 - "worktrees.rs"
Cohesion: 0.07
Nodes (52): branch_for_task(), branch_naming_is_deterministic_and_safe(), canonical(), CleanupPolicy, creates_worktree_with_deterministic_branch(), default_branch(), diff_is_deterministic_and_counts_files(), DiffSummary (+44 more)

### Community 2 - "PaneNode"
Cohesion: 0.06
Nodes (56): horizontal_split_side_by_side(), LayoutEngine, pane(), PaneRect, resize_ratio_changes(), Option, PaneId, Self (+48 more)

### Community 3 - "validate.rs"
Cohesion: 0.06
Nodes (62): alloc_profile(), AllocReport, BackpressureReport, CoalescingReport, collect_latency(), cpu_seconds(), cpu_usage(), drain_all() (+54 more)

### Community 4 - "Renderer"
Cohesion: 0.06
Nodes (48): Attribute, BindGroup, BindGroupLayout, Buffer, Color, atlas_entry_uv_scales(), AtlasEntry, attribute_bits() (+40 more)

### Community 5 - "Response"
Cohesion: 0.08
Nodes (65): agent_cmd(), agent_watch(), artifact_cmd(), control_cmd(), main(), pane_cmd(), parse_filter(), plan_cmd() (+57 more)

### Community 6 - "adaptive.rs"
Cohesion: 0.09
Nodes (40): BTreeMap, ArtifactInvalidation, AutonomyPolicy, BudgetObservation, cooldown_admits_then_rejects(), dedupe_key_coalesces_equivalent_signals(), evaluator_emits_expected_triggers(), HumanEscalation (+32 more)

### Community 7 - "provider.rs"
Cohesion: 0.09
Nodes (39): builtin_models(), builtin_providers(), context_window(), credential_env_vars(), custom_openai_configuration(), default_endpoint(), endpoint_scheme_ok(), HttpProviderConnection (+31 more)

### Community 8 - "Multiplexer"
Cohesion: 0.08
Nodes (6): Multiplexer, HashMap, HashSet, TaskId, Vec, StopAllReport

### Community 9 - "GlyphCache"
Cohesion: 0.09
Nodes (27): cache_evicts_lru_first(), cache_memoizes_and_evicts(), coverage_and_missing(), detect_monospace(), FontLibrary, GlyphCache, GlyphCacheStats, GlyphMetrics (+19 more)

### Community 10 - "ArtifactStore"
Cohesion: 0.10
Nodes (26): artifact(), ArtifactAccessPolicy, ArtifactLineage, ArtifactMaterializer, ArtifactMeta, ArtifactRecord, ArtifactReference, ArtifactRetentionPolicy (+18 more)

### Community 11 - "ApplicationEvent"
Cohesion: 0.09
Nodes (41): ApplicationEvent, Box, Vec, drained_subscriber_is_never_disconnected(), EventBus, EventFilter, filter_gates_channels(), is_critical() (+33 more)

### Community 12 - "AgentRuntime"
Cohesion: 0.10
Nodes (21): adapter_capabilities_are_honest(), AgentRuntime, builtin_registry_lists_agents(), finish(), launch_without_provider_spawns_no_env(), poll_exit_code(), PumpContext, Arc (+13 more)

### Community 13 - "credential.rs"
Cohesion: 0.10
Nodes (26): credential_ref_uri_roundtrip(), CredentialBackend, CredentialRef, CredentialStore, debug_never_reveals_stored_secrets(), KeychainBackend, memory_backend_delete_missing_errors(), MemoryBackend (+18 more)

### Community 14 - "TerminalState"
Cohesion: 0.10
Nodes (4): String, scroll_reuses_buffers(), TerminalState, text_rows()

### Community 15 - "AuditTrail"
Cohesion: 0.11
Nodes (19): action_allowed(), action_approved_by(), action_requires_approval(), AuditEvent, AuditEventKind, AuditResult, AuditTrail, copy_examples_match_phase4_docs() (+11 more)

### Community 16 - "phase3f/main.rs"
Cohesion: 0.13
Nodes (45): adversarial_budget_bypass_rejected(), adversarial_dangerous_command_never_silently_executed(), adversarial_invalid_dependency_rejected(), adversarial_policy_bypass_max_agents_gated(), adversarial_secret_exfiltration_escalated(), agent_done(), approval_center_aggregates_attention_items(), approval_integrity_duplicate_approval_idempotent() (+37 more)

### Community 17 - "PlannerState"
Cohesion: 0.08
Nodes (20): AgentRecommendation, canonical_plan_json(), metrics_count_quality_signals(), PersistedApproval, PersistedPlanState, persistence_round_trip(), plan_hash(), PlannerMetrics (+12 more)

### Community 18 - "desktop/src/main.rs"
Cohesion: 0.12
Nodes (21): agent_running(), agent_state_color(), AgentButton, AgentControl, AgentHit, ApprovalDetail, ApprovalKind, format_duration() (+13 more)

### Community 19 - "Pi CLI Workspace Skill"
Cohesion: 0.05
Nodes (39): Corpus Pin (pi-mono commit 3b7448d), Pi CLI Workspace Skill, available_skills Read Tool Gate, Skill Name Collisions, Auto-Compaction Cut-Point, Credential Resolution Order, Message Queue (Steering / Follow-up), Model Shorthand (provider/id, name:thinking) (+31 more)

### Community 20 - "App"
Cohesion: 0.11
Nodes (11): App, AppEvent, OverlayMode, Instant, TaskId, Window, Clipboard, EventLoopProxy (+3 more)

### Community 21 - "collaboration.rs"
Cohesion: 0.13
Nodes (28): any_critical_is_critical(), consensus_all_pass_approved_candidate(), consensus_pass_warning_fail_means_needs_review(), finding(), high_threshold_is_configurable(), report(), ResultSynthesizer, ReviewAggregation (+20 more)

### Community 22 - "PlannerError"
Cohesion: 0.11
Nodes (26): AgentSummary, first_error(), PlannerApprovalMode, PlannerConfig, PlannerConstraints, PlannerContext, PlannerContextInput, PlannerError (+18 more)

### Community 23 - "phase2c/main.rs"
Cohesion: 0.08
Nodes (15): center_broadcasts(), Notification, NotificationCenter, NotificationKind, Option, PaneId, Self, String (+7 more)

### Community 24 - "phase3b/main.rs"
Cohesion: 0.12
Nodes (33): approved_plan_executes_through_the_scheduler(), budget_violation_is_rejected(), dependency_cycle_is_rejected(), dependency_edits_are_applied_and_compiled(), drain_until_terminal(), execution_requires_approval_first(), fake_available(), injected_shell_fields_never_become_commands() (+25 more)

### Community 25 - "execution.rs"
Cohesion: 0.09
Nodes (23): AgentActivity, AgentEvent, AgentState, CanInput, CanObserve, CanResize, CanStop, ExecutionKind (+15 more)

### Community 26 - "policy.rs"
Cohesion: 0.09
Nodes (24): ApprovalStatus, arg_matches_needle(), ArgMatch, args_match(), ArgSetMatch, base_risk_for(), chmod_chown_gated(), classify_process() (+16 more)

### Community 27 - "Result"
Cohesion: 0.13
Nodes (6): ApprovalError, PaneId, Result, TabId, tab_cycle(), tab_reorder_and_switch()

### Community 28 - "phase3a/main.rs"
Cohesion: 0.19
Nodes (33): auth_failure_is_never_auto_retried(), build_fixture(), cancellation_stops_agent_and_leaves_no_orphan(), commands_are_safe_across_all_task_states(), concurrent_flood_does_not_drop_completions(), create_rejects_unknown_agent_and_workspace(), create_task(), determinism_ten_runs_produce_identical_schedules() (+25 more)

### Community 29 - "Session"
Cohesion: 0.09
Nodes (20): Arc, AtomicBool, AtomicU64, Box, Drop, Fn, Option, Receiver (+12 more)

### Community 30 - "engine.rs"
Cohesion: 0.12
Nodes (18): agent_state_from_str(), AgentDashboard, AgentFileChange, AgentReview, AgentRow, AttentionAgent, AttentionItems, AttentionReplan (+10 more)

### Community 31 - "AgentLaunchConfig"
Cohesion: 0.10
Nodes (19): build_spec(), ChildSpec, Into, Self, String, Vec, AgentLaunchConfig, AgentLaunchContext (+11 more)

### Community 33 - "work.rs"
Cohesion: 0.14
Nodes (20): activity_coalesces_within_window(), AgentSummary, all_fixtures(), all_fixtures_are_secret_free_and_serializable(), collect_git_files(), fixture_approval(), fixture_completion(), fixture_failure() (+12 more)

### Community 34 - "terminal-core/src/lib.rs"
Cohesion: 0.17
Nodes (22): alt_screen_preserves(), ascii_dirty_rows(), basics_ascii(), clear_screen_and_line(), combining_marks(), cursor_wrap_and_margin(), dirty_tracking_incremental(), emoji_zwj_cluster_merges() (+14 more)

### Community 35 - "Row"
Cohesion: 0.16
Nodes (19): Row, Vec, VecDeque, SavedScreen, block_roundtrip(), ColdBlock, ColdStore, compress_block() (+11 more)

### Community 36 - "Performer"
Cohesion: 0.12
Nodes (16): TerminalEvent, csi_cursor_moves(), end_to_end_state_update(), osc_title(), parse_events(), Parser, Performer, plain_text_becomes_write_chars() (+8 more)

### Community 37 - "ApprovalStore"
Cohesion: 0.13
Nodes (16): action_hash(), Approval, approval_action_change_invalidates_old_approval(), approval_expiry_blocks_stale_reuse(), approval_honored_once_and_replay_rejected(), approval_pending_cannot_be_honored_before_grant(), approval_store_bounded_memory(), approval_store_with_granted() (+8 more)

### Community 38 - ".new"
Cohesion: 0.16
Nodes (13): agent_pane_restore_respawns_from_stored_launch(), close_last_pane_closes_tab(), close_last_workspace_rejected(), create_workspace_spawns_session(), drain_empty_is_false(), focus_cycle_works(), pane_move_and_resize_and_write(), persistence_roundtrip_via_engine() (+5 more)

### Community 39 - "phase3d/main.rs"
Cohesion: 0.18
Nodes (25): access_control_denies_unrelated_tasks(), agent_done(), artifact_creation_metadata_and_selection(), artifact_lineage_maps_producers_and_consumers(), artifact_payloads_are_redacted(), artifacts_reviews_and_signals_survive_restart(), create_task(), cross_worktree_consumption() (+17 more)

### Community 40 - "phase3e/main.rs"
Cohesion: 0.20
Nodes (25): adaptive_state_survives_restart(), agent_done(), budget_risk_emits_signal(), create_task(), critical_review_finding_emits_critical_signal(), drain_until_done(), engine_in_repo(), failing_tests_emit_signal_and_proposed_replan() (+17 more)

### Community 41 - "AgentWork"
Cohesion: 0.15
Nodes (14): AgentSummary, BTreeSet, AgentActivityState, AgentWork, confidence_score(), ErrorKind, now_ms(), replay_into() (+6 more)

### Community 42 - "Command"
Cohesion: 0.12
Nodes (14): Command, CommandRegistry, default_bindings(), KeyChord, Item, Iterator, Option, PaneId (+6 more)

### Community 43 - "phase4/main.rs"
Cohesion: 0.16
Nodes (23): PlannerProvider, Send, Sync, DeadPlannerProvider, adversarial_fake_completion_requires_review(), adversarial_invalid_artifact_denied(), adversarial_network_exfiltration_denied(), adversarial_self_approval_ignored() (+15 more)

### Community 44 - "main"
Cohesion: 0.14
Nodes (21): create_ws(), diagnostic_snapshot(), input_sample(), main(), now_ms(), pct_ms(), ptmx_max(), Arc (+13 more)

### Community 45 - "PolicyEngine"
Cohesion: 0.15
Nodes (10): autonomy_decision(), autonomy_description(), AutonomyLevel, PersistedPolicyState, PolicyContext, PolicyDecision, PolicyEngine, PolicyEvaluation (+2 more)

### Community 46 - "benchmarks/src/main.rs"
Cohesion: 0.15
Nodes (24): baseline_path(), budget_table(), generate_output(), glyph_raster_us_per_glyph(), load_baseline(), main(), measure_input_p95(), MetricGetter (+16 more)

### Community 47 - "redact.rs"
Cohesion: 0.15
Nodes (18): does_not_mask_short_tokens(), find_secret(), masks_anthropic_key_shape(), masks_bearer_token(), masks_registered_sentinel(), masks_when_secret_embedded_in_other_text(), multiple_secrets_in_one_line(), Redactor (+10 more)

### Community 48 - "PiAdapter"
Cohesion: 0.09
Nodes (11): Option, Option, Option, ActivityHint, Option, PiAdapter, EnvVarDecision, Option (+3 more)

### Community 49 - "AgentDefinition"
Cohesion: 0.12
Nodes (10): GenericCliAdapter, Default, EnvVarDecision, Self, String, Vec, AgentDefinition, AgentRegistry (+2 more)

### Community 50 - ".new"
Cohesion: 0.13
Nodes (13): budget_increase_requires_authorization(), case_differences_are_distinct_until_canonicalization(), critical_secret_requires_human_granted_allowance(), FilesystemScope, no_filesystem_scope_denies_everything(), PathValidator, PathViolation, Default (+5 more)

### Community 51 - "Agent Abstraction"
Cohesion: 0.11
Nodes (24): Agent Abstraction, Agent-to-Agent Communication, Agent Integration Levels (CLI/Native/ACP), Agent Registry, Universal Approval Layer, BYOK / Provider Abstraction, CLI / Socket API, AI Context Engine (+16 more)

### Community 52 - "main"
Cohesion: 0.13
Nodes (12): DemoPlanner, git(), main(), Arc, FontLibrary, GlyphCache, Mutex, Path (+4 more)

### Community 53 - "unicode.rs"
Cohesion: 0.13
Nodes (14): arabic_renders_but_not_shaped(), ascii_hello(), cell_width_sum_equals_cursor(), cjk_widths_and_cursor(), combining_acute(), emoji_are_wide(), kana_widths_and_cursor(), precomposed_accent() (+6 more)

### Community 54 - "agent.rs"
Cohesion: 0.14
Nodes (12): AgentCapabilities, AgentMetrics, AgentSession, AgentSnapshot, PermissionDecision, AtomicU64, DateTime, Duration (+4 more)

### Community 55 - "phase3c/main.rs"
Cohesion: 0.28
Nodes (22): agent_done(), branch_sanitizes_hostile_slug(), cancellation_preserves_worktree(), create_modify_task(), cross_contamination_same_filename(), dirty_workspace_policy_requires_clean(), drain_until_done(), engine_in_repo() (+14 more)

### Community 56 - "UX Specification"
Cohesion: 0.11
Nodes (23): benchmarks Crate, CI Performance Gates, Input Latency Budget, Performance Plan, Render Latency Budget, Tiered Scrollback (ADR-0004), Warm Start Budget, EventLoopProxy Wakeup (+15 more)

### Community 57 - "Cell"
Cohesion: 0.13
Nodes (6): Cell, Color, parse_compound_color(), RenderSnapshot<'a>, Option, Selection

### Community 58 - "String"
Cohesion: 0.18
Nodes (10): Action, command_spec_is_argv_only_by_default(), CommandSpec, FsOperation, PolicySource, Into, Self, String (+2 more)

### Community 59 - "BudgetDimension"
Cohesion: 0.16
Nodes (7): budget_ledger_enforces_caps(), BudgetCounters, BudgetDimension, BudgetEvent, BudgetLedger, BudgetPolicy, persisted_policy_state_roundtrip_without_secrets()

### Community 60 - "phase051.rs"
Cohesion: 0.18
Nodes (20): burst_paste_without_drain_does_not_deadlock(), child_exits_normally(), child_killed_by_signal(), close_while_streaming(), drain_until(), fzf_roundtrip(), git_diff_pager_roundtrip(), grid_contains() (+12 more)

### Community 61 - "planning.rs"
Cohesion: 0.14
Nodes (14): classify_intent(), duplicate_step_ids_rejected(), intent_normalization_is_deterministic_and_bounded(), IntentDisposition, is_secret_entry(), normalize_intent(), NormalizedIntent, parse_isolation() (+6 more)

### Community 62 - ".new"
Cohesion: 0.25
Nodes (14): compiler_is_deterministic(), constraints(), context_builder_is_bounded_and_secret_free(), detects_cycles_and_missing_deps(), map_graph_error(), PlannerContextBuilder, PlanValidationResult, registry() (+6 more)

### Community 63 - "Rust (Core Language)"
Cohesion: 0.14
Nodes (21): Strict Layered Architecture, macOS Apple Silicon First, portable-pty (PTY Management), Rust (Core Language), vte (VT Parser), wgpu (GPU Rendering), winit (Windowing), JSON-RPC Agent Protocol (+13 more)

### Community 64 - "String"
Cohesion: 0.17
Nodes (13): AgentHealthRow, AgentIntent, AgentUsage, HealthStatus, IntentResolver, observe_file(), parse_count_pattern(), parse_test_counts() (+5 more)

### Community 65 - "agent_runtime.rs"
Cohesion: 0.30
Nodes (19): approval_roundtrip_emits_permission_requested(), completion_reports_completed_with_exit_zero(), crash_is_classified_crashed(), ensure_fake_agent_built(), failure_reports_failed_with_nonzero_exit(), interactive_input_is_echoed_through_the_pty(), launch(), output_events_are_redacted_and_metric_counts() (+11 more)

### Community 66 - "terminal-core Crate"
Cohesion: 0.12
Nodes (19): Bounded crossbeam Channel (cap 1024), fontdue Text Stack, Glyph Atlas + Instanced Quads Renderer, RenderSnapshot Boundary, terminal-session (Ownership Hub), ADR-0004, ColdStore (RLE+flate2 Blocks), Hot Scrollback Tier (+11 more)

### Community 67 - "Multiplexer (Phase 1 Engine)"
Cohesion: 0.13
Nodes (19): CI/CD Pipeline, Crate Structure, FlashTerminal, crates/terminal-core, Manual Desktop Validation Checklist, Multiplexer (Phase 1 Engine), Fairness & State Batching, IPC Unix Socket (+11 more)

### Community 68 - "DirtyTracker"
Cohesion: 0.12
Nodes (5): Attribute, DirtyTracker, Modes, Default, Self

### Community 69 - "mod.rs"
Cohesion: 0.20
Nodes (15): AgentAdapterImpl, exec_found_in_path(), find_executable(), find_fake_agent_bin(), is_executable(), not_found_error(), not_found_error_is_human_readable(), resolve_binary() (+7 more)

### Community 70 - "Result"
Cohesion: 0.18
Nodes (9): audit_records_are_bounded_and_secret_free(), find_step_mut(), now_ms(), plan_hash_is_deterministic_and_order_independent(), PlanEditChange, PlannerAuditRecord, PlannerAuditTrail, Formatter (+1 more)

### Community 71 - "Self"
Cohesion: 0.24
Nodes (5): PlanCompiler, PlannerProfile, PlannerProfileId, profiles_map_parameters_without_provider_assumptions(), Self

### Community 72 - "real_agents.rs"
Cohesion: 0.18
Nodes (16): claude_code_matrix(), codex_matrix(), detection_reports_honestly(), launch(), on_path(), opencode_matrix(), pi_matrix(), Duration (+8 more)

### Community 73 - "Pi RPC SDK Skill"
Cohesion: 0.15
Nodes (16): CI Workflow, Performance Check Job, AgentSessionRuntime, createAgentSession, Pi RPC SDK Skill, pi-mono Repository, rpc.md Framing Doc, Agent-Native Support (+8 more)

### Community 74 - "scrollback_bench.rs"
Cohesion: 0.18
Nodes (9): CountingAlloc, fast_rng(), fill_rows(), main(), measure(), reset_counters(), GlobalAlloc, Layout (+1 more)

### Community 75 - "FakeAgentAdapter"
Cohesion: 0.13
Nodes (8): FakeAgentAdapter, EnvVarDecision, Option, PathBuf, Result, Self, String, Vec

### Community 76 - ".default"
Cohesion: 0.33
Nodes (13): approval_request_redacts_secrets_before_persist(), approval_roundtrip_via_engine(), ctx(), engine_allows_low_risk_at_supervised(), engine_autonomy_escalation_and_budget_increase_never_automatic(), engine_denies_disk_wipe_outright(), engine_denies_secret_without_allowance(), engine_filesystem_scope_enforced() (+5 more)

### Community 77 - "integration.rs"
Cohesion: 0.31
Nodes (14): alt_screen_smoke_via_pty(), drain_until(), grid_text(), massive_output_lands_in_state(), pty_backpressure_no_stall(), pty_to_state_echo(), rapid_input_does_not_lose_bytes(), resize_propagates_to_child() (+6 more)

### Community 78 - "ExecutionId"
Cohesion: 0.10
Nodes (10): ExecutionId, Default, DrainResult, PaneFrame, DirtyTracker, RenderSnapshot, TerminalState, NotificationPrefs (+2 more)

### Community 79 - "AgentTimeline"
Cohesion: 0.19
Nodes (8): AgentTimeline, DateTime, Item, Iterator, Utc, VecDeque, TimelineEntry, TimelineKind

### Community 80 - "persistence.rs"
Cohesion: 0.26
Nodes (13): agent_pane(), agent_pane_persists_config_not_secrets(), crash_recovery_restores_pane_not_process(), fake_agent_available(), launch_redaction_covers_args_and_env(), launch_with_secrets(), restart_preserves_audit_trail(), restart_preserves_pending_approval_and_budget() (+5 more)

### Community 81 - "OpenCodeAdapter"
Cohesion: 0.17
Nodes (5): OpenCodeAdapter, EnvVarDecision, Self, String, Vec

### Community 82 - "UI Thread · Single State Owner"
Cohesion: 0.24
Nodes (13): Bounded Channel, FlashTerminal App, Font Stack, Window / GPU, Input & Selection, IO Thread · Terminal-Session, VT Parser, PTY Manager (+5 more)

### Community 83 - "terminal_benchmarks.rs"
Cohesion: 0.39
Nodes (11): bench_cell_write(), bench_clear(), bench_cursor_move(), bench_dirty_tracking(), bench_insert_delete_lines(), bench_random_access(), bench_resize(), bench_row_scroll() (+3 more)

### Community 84 - "ClaudeCodeAdapter"
Cohesion: 0.18
Nodes (5): ClaudeCodeAdapter, EnvVarDecision, Self, String, Vec

### Community 85 - "CodexAdapter"
Cohesion: 0.18
Nodes (5): CodexAdapter, EnvVarDecision, Self, String, Vec

### Community 87 - "agent_stress.rs"
Cohesion: 0.31
Nodes (10): launch(), main(), percentile_ms(), report_latency(), Duration, String, settle(), spawn_agent() (+2 more)

### Community 88 - "CountingAlloc"
Cohesion: 0.27
Nodes (6): CountingAlloc, main(), read_counters(), reset_counters(), GlobalAlloc, Layout

### Community 89 - "process_chunk"
Cohesion: 0.24
Nodes (5): detect_activity_kind(), process_chunk(), PumpTracker, ActivityKind, observe_command()

### Community 90 - "EngineMetrics"
Cohesion: 0.22
Nodes (4): EngineMetrics, ApprovalId, Instant, VecDeque

### Community 91 - "soak.rs"
Cohesion: 0.32
Nodes (5): main(), Pane, Arc, tree_rss_kb(), Write

### Community 92 - ".with_wake"
Cohesion: 0.29
Nodes (5): Box, Fn, Self, Send, Sync

### Community 93 - "real_agent_tasks.rs"
Cohesion: 0.54
Nodes (7): auth_failure_is_a_typed_task_error(), fake_available(), new_engine(), parallel_multi_agent_workflow_is_isolated(), pump_until_terminal(), Duration, trivial_tasks_run_against_installed_real_agents()

### Community 94 - "pipeline.rs"
Cohesion: 0.57
Nodes (6): bench_input_latency_under_load(), bench_pipeline(), bench_throughput_mbps(), generate_output(), Criterion, Vec

### Community 95 - ".validate_plan"
Cohesion: 0.33
Nodes (4): AgentAvailability, PlanValidator, PlanValidator<'a>, Fn

### Community 96 - "AgentFilter"
Cohesion: 0.22
Nodes (3): AgentFilter, attention_for(), AttentionReason

### Community 97 - "orchestration_bench.rs"
Cohesion: 0.53
Nodes (5): fairness(), main(), String, run_cell(), tree_rss_kb()

### Community 100 - "paste_bench.rs"
Cohesion: 0.83
Nodes (3): grid_has(), main(), shell()

### Community 102 - "raw_throughput.rs"
Cohesion: 0.83
Nodes (3): grid_has(), main(), render_visible()

### Community 104 - "JSONL LF Framing"
Cohesion: 1.00
Nodes (3): JSONL LF Framing, Pi --mode json, Pi --mode rpc

### Community 105 - "Current Architecture (Phase 1)"
Cohesion: 0.67
Nodes (3): ADR-0003, Architecture Proposal, Current Architecture (Phase 1)

## Knowledge Gaps
- **62 isolated node(s):** `repowise`, `repowise`, `run_benchmarks.sh script`, `CanInput`, `CanObserve` (+57 more)
  These have ≤1 connection - possible missing edges or undocumented components.
- **22 thin communities (<3 nodes) omitted from report** — run `graphify query` to explore isolated nodes.

## Suggested Questions
_Questions this graph is uniquely positioned to answer:_

- **Why does `Multiplexer` connect `Multiplexer` to `orchestration.rs`, `worktrees.rs`, `PaneNode`, `validate.rs`, `Response`, `adaptive.rs`, `ArtifactStore`, `ApplicationEvent`, `AgentRuntime`, `AuditTrail`, `phase3f/main.rs`, `PlannerState`, `desktop/src/main.rs`, `App`, `collaboration.rs`, `PlannerError`, `phase2c/main.rs`, `phase3b/main.rs`, `execution.rs`, `Result`, `phase3a/main.rs`, `Session`, `engine.rs`, `.active_workspace`, `ApprovalStore`, `.new`, `phase3d/main.rs`, `phase3e/main.rs`, `phase4/main.rs`, `main`, `PolicyEngine`, `.new`, `main`, `phase3c/main.rs`, `BudgetDimension`, `planning.rs`, `ExecutionId`, `persistence.rs`, `WorkspaceId`, `agent_stress.rs`, `EngineMetrics`, `.with_wake`, `real_agent_tasks.rs`?**
  _High betweenness centrality (0.338) - this node is a cross-community bridge._
- **Why does `TerminalState` connect `TerminalState` to `terminal-core/src/lib.rs`, `validate.rs`, `paste_bench.rs`, `.snapshot`, `raw_throughput.rs`, `DirtyTracker`, `Row`, `scrollback_bench.rs`, `integration.rs`, `terminal_benchmarks.rs`, `unicode.rs`, `Cell`, `soak.rs`, `phase051.rs`?**
  _High betweenness centrality (0.107) - this node is a cross-community bridge._
- **Why does `Session` connect `Session` to `validate.rs`, `.new`, `Multiplexer`, `AgentRuntime`, `integration.rs`, `soak.rs`, `phase051.rs`, `engine.rs`?**
  _High betweenness centrality (0.103) - this node is a cross-community bridge._
- **Are the 7 inferred relationships involving `ExecutionId` (e.g. with `.approve_current()` and `.build_approval_detail()`) actually correct?**
  _`ExecutionId` has 7 INFERRED edges - model-reasoned connections that need verification._
- **What connects `repowise`, `repowise`, `run_benchmarks.sh script` to the rest of the system?**
  _62 weakly-connected nodes found - possible documentation gaps or missing edges._
- **Should `orchestration.rs` be split into smaller, more focused modules?**
  _Cohesion score 0.052394170714781405 - nodes in this community are weakly interconnected._
- **Should `worktrees.rs` be split into smaller, more focused modules?**
  _Cohesion score 0.07049504950495049 - nodes in this community are weakly interconnected._