// Consolidated schema captured from boss-v1.0.707, schema 32.
// The independent released-chain golden lives in schema_init tests.
pub(super) const SQL: &str = r###"-- Captured from boss-v1.0.707 (cc72dac8), schema 32, by running the old chain.
CREATE TABLE answer_agent_runs (
             id                 TEXT PRIMARY KEY,
             comment_id         TEXT NOT NULL REFERENCES work_comments(id),
             artifact_kind      TEXT NOT NULL,
             artifact_id        TEXT NOT NULL,
             doc_version        TEXT NOT NULL,
             thread_turn        INTEGER NOT NULL DEFAULT 0,
             status             TEXT NOT NULL,
             workspace_lease_id TEXT,
             reply_body         TEXT,
             error_kind         TEXT,
             created_at         TEXT NOT NULL,
             completed_at       TEXT,
             execution_id       TEXT REFERENCES work_executions(id) ON DELETE SET NULL
         , workspace_positioned INTEGER);

CREATE TABLE attention_group_short_id_sequences (
             product_id  TEXT PRIMARY KEY REFERENCES products(id),
             next_value  INTEGER NOT NULL DEFAULT 1
         );

CREATE TABLE attention_groups (
             id                         TEXT PRIMARY KEY,
             product_id                 TEXT NOT NULL REFERENCES products(id),
             short_id                   INTEGER,
             kind                       TEXT NOT NULL,
             association_project_id     TEXT REFERENCES projects(id),
             association_task_id        TEXT REFERENCES tasks(id),
             source_kind                TEXT NOT NULL,
             source_task_id             TEXT,
             source_run_id              TEXT,
             source_doc_path            TEXT,
             source_doc_repo_remote_url TEXT,
             source_doc_branch          TEXT,
             grouping_key               TEXT NOT NULL,
             generation                 INTEGER NOT NULL DEFAULT 0,
             state                      TEXT NOT NULL DEFAULT 'open',
             produced_artifact_kind     TEXT,
             produced_artifact_ref      TEXT,
             created_at                 TEXT NOT NULL,
             actioned_at                TEXT,
             dismissed_at               TEXT,
             CHECK (
                 (association_project_id IS NOT NULL AND association_task_id IS NULL)
                 OR (association_project_id IS NULL  AND association_task_id IS NOT NULL)
             )
         );

CREATE TABLE attention_merges (
             id                      TEXT PRIMARY KEY,
             canonical_attention_id  TEXT REFERENCES attentions(id),
             canonical_work_item_id  TEXT,
             product_id              TEXT NOT NULL,
             trigger                 TEXT NOT NULL,
             duplicate_attention_id  TEXT,
             candidate_summary       TEXT NOT NULL,
             candidate_source        TEXT,
             model                   TEXT NOT NULL,
             decision_rationale      TEXT,
             edits_applied           TEXT,
             created_at              TEXT NOT NULL
         );

CREATE TABLE attentions (
             id                  TEXT PRIMARY KEY,
             group_id            TEXT NOT NULL
                                     REFERENCES attention_groups(id) ON DELETE CASCADE,
             ordinal             INTEGER NOT NULL,
             source_anchor       TEXT,
             answer_state        TEXT NOT NULL DEFAULT 'open',
             created_at          TEXT NOT NULL,
             answered_at         TEXT,
             question_type       TEXT,
             prompt_text         TEXT,
             choice_options      TEXT,
             answer              TEXT,
             proposed_name       TEXT,
             proposed_description TEXT,
             proposed_effort     TEXT,
             proposed_work_kind  TEXT,
             rationale           TEXT,
             confidence_source   TEXT NOT NULL DEFAULT 'structured'
         , score INTEGER NOT NULL DEFAULT 1, merged_into_attention_id TEXT, linked_work_item_id TEXT, source_proposal_id TEXT);

CREATE TABLE automation_dedup_suppressions (
             id                 TEXT PRIMARY KEY,
             automation_id      TEXT NOT NULL REFERENCES automations(id),
             surviving_task_id  TEXT NOT NULL REFERENCES tasks(id),
             attempted_name     TEXT NOT NULL,
             matched_on         TEXT NOT NULL,
             match_key          TEXT NOT NULL,
             created_at         TEXT NOT NULL
         );

CREATE TABLE automation_runs (
             id                   TEXT PRIMARY KEY,
             automation_id        TEXT NOT NULL REFERENCES automations(id),
             scheduled_for        TEXT NOT NULL,
             started_at           TEXT NOT NULL,
             finished_at          TEXT,
             triage_execution_id  TEXT,
             outcome              TEXT NOT NULL,
             produced_task_id     TEXT REFERENCES tasks(id),
             detail               TEXT
         , first_attempted_at TEXT);

CREATE TABLE automation_short_id_sequences (
             product_id  TEXT PRIMARY KEY REFERENCES products(id),
             next_value  INTEGER NOT NULL DEFAULT 1
         );

CREATE TABLE automations (
             id                    TEXT PRIMARY KEY,
             short_id              INTEGER,
             product_id            TEXT NOT NULL REFERENCES products(id),
             name                  TEXT NOT NULL,
             repo_remote_url       TEXT,
             trigger_kind          TEXT NOT NULL,
             trigger_config        TEXT NOT NULL,
             standing_instruction  TEXT NOT NULL,
             open_task_limit       INTEGER NOT NULL DEFAULT 1,
             catch_up_window_secs  INTEGER,
             enabled               INTEGER NOT NULL DEFAULT 1,
             created_via           TEXT NOT NULL DEFAULT 'unknown',
             created_at            TEXT NOT NULL,
             updated_at            TEXT NOT NULL,
             last_fired_at         TEXT,
             last_outcome          TEXT,
             next_due_at           TEXT
         );

CREATE TABLE boothby_actions (
             id            TEXT PRIMARY KEY,
             -- NOT NULL per the design: an action is always part of a pass.
             -- ON DELETE CASCADE so the retention prune of old passes takes
             -- their journal detail with them (design §Retention).
             pass_id       TEXT NOT NULL REFERENCES boothby_passes(id) ON DELETE CASCADE,
             -- Ordinal within the pass; `(pass_id, seq)` is the read order.
             seq           INTEGER NOT NULL,
             -- Catalogue slug, e.g. 'close_stale_task'. Supplied by the
             -- executor's verb catalogue (task 2), not inferred here: the
             -- mutation layer sees a column delta, never the intent behind
             -- it, and a guessed verb in an audit trail is worse than none.
             verb          TEXT NOT NULL,
             -- task | project | attention | attention_item | execution |
             -- lease | workspace | file | issue. Unconstrained: the
             -- operational verbs (task 9) target kinds that are not WorkDb
             -- rows at all, and the catalogue is the authority on the set.
             target_kind   TEXT NOT NULL,
             target_id     TEXT NOT NULL,
             -- JSON: the verb's inputs.
             params        TEXT,
             -- Agent-supplied one-liner, required by the design — an
             -- unexplained autonomous mutation is exactly what the journal
             -- exists to prevent.
             rationale     TEXT NOT NULL,
             -- JSON of the mutated fields before / after. Restricted to the
             -- columns the mutation actually touched, so replaying
             -- `pre_image` reverts exactly what Boothby changed and cannot
             -- clobber a column another writer has moved since. `pre_image`
             -- is NULL for I-class (irreversible) actions, which journal
             -- `params` + evidence instead.
             pre_image     TEXT,
             -- Also the undo conflict check: undo compares the row's
             -- current state against this before restoring `pre_image`.
             post_image    TEXT,
             reversibility TEXT NOT NULL
                               CHECK (reversibility IN ('reversible', 'semi', 'irreversible')),
             undo_state    TEXT NOT NULL DEFAULT 'none'
                               CHECK (undo_state IN ('none', 'undoable', 'undone', 'expired', 'conflicted')),
             undone_at     TEXT,
             -- Undo is human-only; the Boothby session has no undo verb, so
             -- it cannot launder its own mistakes.
             undone_by     TEXT,
             created_at    TEXT NOT NULL
         );

CREATE TABLE boothby_cursors (
             -- e.g. 'engine-trace', 'dispatch-events', 'transcript:<session>'.
             source     TEXT PRIMARY KEY,
             -- JSON: segment/offset or timestamp high-water mark.
             position   TEXT NOT NULL,
             updated_at TEXT NOT NULL
         );

CREATE TABLE boothby_findings (
             id                TEXT PRIMARY KEY,
             -- Content-derived dedup key, and the memory that makes 'this
             -- has happened 40 times' legible without a GROUP BY over
             -- history. Also what a human veto suppresses.
             fingerprint       TEXT NOT NULL UNIQUE,
             kind              TEXT NOT NULL
                                   CHECK (kind IN ('error', 'anomaly', 'perf', 'friction', 'taxonomy')),
             -- JSON refs: log span / transcript span / row ids.
             subject           TEXT NOT NULL,
             first_seen        TEXT NOT NULL,
             last_seen         TEXT NOT NULL,
             occurrences       INTEGER NOT NULL DEFAULT 1 CHECK (occurrences >= 1),
             status            TEXT NOT NULL
                                   CHECK (status IN ('open', 'filed', 'resolved', 'suppressed')),
             filed_kind        TEXT CHECK (filed_kind IS NULL OR filed_kind IN ('chore', 'github_issue')),
             -- Task id or issue URL, per `filed_kind`.
             filed_ref         TEXT,
             suppressed_reason TEXT
         );

CREATE TABLE boothby_passes (
             id              TEXT PRIMARY KEY,
             -- 'schedule' | 'event:<name>' | 'manual'. Left unconstrained
             -- past the documented shapes: the event name is open-ended, so
             -- a CHECK here would reject triggers the design allows.
             trigger         TEXT NOT NULL,
             started_at      TEXT NOT NULL,
             -- NULL while the pass is in flight; set with `outcome`.
             finished_at     TEXT,
             outcome         TEXT
                                 CHECK (outcome IS NULL OR outcome IN
                                     ('completed', 'nothing_to_do', 'timed_out', 'failed', 'capped')),
             actions_count   INTEGER NOT NULL DEFAULT 0,
             proposals_count INTEGER NOT NULL DEFAULT 0,
             findings_count  INTEGER NOT NULL DEFAULT 0,
             -- Agent-authored, written by the `pass-summary` verb.
             summary         TEXT,
             session_id      TEXT,
             transcript_path TEXT,
             -- A pass is finished exactly when it has an outcome. Without
             -- this a crashed pass could sit in flight forever holding an
             -- outcome, or report `completed` with no end time.
             CHECK ((outcome IS NULL) = (finished_at IS NULL))
         );

CREATE TABLE ci_failure_suppressions (
             work_item_id  TEXT NOT NULL,
             head_sha      TEXT NOT NULL,
             created_at    TEXT NOT NULL,
             PRIMARY KEY (work_item_id, head_sha)
         );

CREATE TABLE ci_inflight_observations (
             work_item_id        TEXT NOT NULL,
             head_sha            TEXT NOT NULL,
             first_observed_at   TEXT NOT NULL,
             alert_level_emitted TEXT NOT NULL DEFAULT 'none',
             PRIMARY KEY (work_item_id, head_sha)
         );

CREATE TABLE ci_remediations (
             id                  TEXT PRIMARY KEY,
             product_id          TEXT NOT NULL,
             work_item_id        TEXT NOT NULL,
             pr_url              TEXT NOT NULL,
             pr_number           INTEGER NOT NULL,
             head_branch         TEXT NOT NULL,
             head_sha_at_trigger TEXT NOT NULL,
             head_sha_after      TEXT,
             attempt_kind        TEXT NOT NULL,
             consumes_budget     INTEGER NOT NULL,
             failed_checks       TEXT NOT NULL,
             triage_class        TEXT,
             log_excerpt         TEXT,
             status              TEXT NOT NULL,
             failure_reason      TEXT,
             cube_lease_id       TEXT,
             cube_workspace_id   TEXT,
             worker_id           TEXT,
             created_at          TEXT NOT NULL,
             started_at          TEXT,
             finished_at         TEXT, failure_kind TEXT NOT NULL DEFAULT 'pr_branch_ci', before_commit_sha TEXT, revision_task_id TEXT,
             UNIQUE (work_item_id, head_sha_at_trigger, attempt_kind)
         );

CREATE TABLE comment_thread_entries (
             id                   TEXT PRIMARY KEY,
             comment_id           TEXT NOT NULL REFERENCES work_comments(id),
             entry_kind           TEXT NOT NULL,
             author               TEXT NOT NULL,
             body                 TEXT NOT NULL,
             revise_task_id       TEXT,
             answer_agent_run_id  TEXT REFERENCES answer_agent_runs(id),
             created_at           TEXT NOT NULL
         );

CREATE TABLE "conflict_resolutions" (
             id                  TEXT PRIMARY KEY,
             product_id          TEXT NOT NULL,
             work_item_id        TEXT NOT NULL,
             pr_url              TEXT NOT NULL,
             pr_number           INTEGER NOT NULL,
             head_branch         TEXT NOT NULL,
             base_branch         TEXT NOT NULL,
             base_sha_at_trigger TEXT,
             head_sha_before     TEXT,
             head_sha_after      TEXT,
             status              TEXT NOT NULL,
             failure_reason      TEXT,
             cube_lease_id       TEXT,
             cube_workspace_id   TEXT,
             worker_id           TEXT,
             conflict_diagnosis  TEXT,
             created_at          TEXT NOT NULL,
             started_at          TEXT,
             finished_at         TEXT,
             revision_task_id    TEXT, event_source TEXT NOT NULL DEFAULT 'review_watch', conflict_class TEXT, resolved_by_rung INTEGER, mechanical_rung_in_flight INTEGER,
             UNIQUE (work_item_id, base_sha_at_trigger, head_sha_before)
         );

CREATE TABLE decision_short_id_sequences (
             product_id TEXT PRIMARY KEY,
             next_value INTEGER NOT NULL
         );

CREATE TABLE editorial_actions (
             id           INTEGER PRIMARY KEY,
             product_id   TEXT NOT NULL REFERENCES products(id),
             execution_id TEXT,
             pr_url       TEXT,
             tool_command TEXT NOT NULL,
             action       TEXT NOT NULL CHECK (action IN ('allow', 'rewrite', 'deny')),
             reason       TEXT,
             created_at   TEXT NOT NULL
         );

CREATE TABLE effort_escalations (
             id             TEXT PRIMARY KEY,
             product_id     TEXT NOT NULL,
             work_item_id   TEXT NOT NULL,
             original_level TEXT NOT NULL,
             new_level      TEXT NOT NULL,
             markers        TEXT NOT NULL,
             rule_id        TEXT,
             created_at     TEXT NOT NULL
         );

CREATE TABLE execution_bookmarks (
        execution_id TEXT PRIMARY KEY REFERENCES work_executions(id),
        repo_path TEXT NOT NULL,
        host_id TEXT NOT NULL,
        recovered_from TEXT,
        recovered_work INTEGER
    );

CREATE TABLE execution_driver_decisions (
             execution_id      TEXT PRIMARY KEY REFERENCES work_executions(id) ON DELETE CASCADE,
             work_item_id      TEXT NOT NULL,
             driver            TEXT,
             reason            TEXT NOT NULL,
             split_at_decision TEXT,
             created_at        TEXT NOT NULL
         );

CREATE TABLE github_api_calls (
             id               INTEGER PRIMARY KEY,
             -- Epoch MILLISECONDS (integer), not a string. See the doc
             -- comment on migrate_github_api_calls_table.
             started_at_ms    INTEGER NOT NULL,
             -- Subsystem that made the call ('merge_poller.sweep',
             -- 'ci_watch', …), or 'unattributed' when no scope was active.
             caller           TEXT NOT NULL,
             -- 'graphql' | 'rest' | 'cli'. GraphQL and REST are metered
             -- against separate hourly buckets and must not be summed.
             api              TEXT NOT NULL,
             verb             TEXT NOT NULL,
             endpoint         TEXT NOT NULL,
             -- 'ok' | 'error' | 'rate_limited'.
             outcome          TEXT NOT NULL,
             duration_ms      INTEGER NOT NULL,
             -- GraphQL points this call cost (REST: 1 request). NULL when
             -- the response carried no reading.
             points_cost      INTEGER,
             points_remaining INTEGER,
             points_limit     INTEGER,
             -- Epoch MILLISECONDS (integer) of the quota-window reset.
             reset_at_ms      INTEGER
         );

CREATE TABLE github_merge_intents (
             id           TEXT PRIMARY KEY,
             work_item_id TEXT NOT NULL,
             pr_url       TEXT NOT NULL,
             head_sha     TEXT NOT NULL,
             status       TEXT NOT NULL,
             created_at   TEXT NOT NULL
         );

CREATE TABLE guide_comment_outcomes (
             comment_id TEXT PRIMARY KEY,
             revise_task_id TEXT NOT NULL,
             disposition TEXT NOT NULL,
             response TEXT NOT NULL,
             request_regeneration INTEGER NOT NULL DEFAULT 0,
             created_at TEXT NOT NULL
         );

CREATE TABLE host_capabilities (
             host_id    TEXT NOT NULL REFERENCES hosts(id) ON DELETE CASCADE,
             capability TEXT NOT NULL,
             source     TEXT NOT NULL,
             PRIMARY KEY (host_id, capability)
         );

CREATE TABLE hosts (
             id             TEXT PRIMARY KEY,
             ssh_target     TEXT,
             pool_size      INTEGER NOT NULL DEFAULT 1,
             enabled        INTEGER NOT NULL DEFAULT 1,
             last_seen_at   TEXT,
             last_error_text TEXT,
             created_at     TEXT NOT NULL
         , consecutive_failures INTEGER NOT NULL DEFAULT 0);

CREATE TABLE idea_short_id_sequences (
             product_id TEXT PRIMARY KEY,
             next_value INTEGER NOT NULL
         );

CREATE TABLE ideas (
             id                TEXT PRIMARY KEY,
             short_id          INTEGER,
             product_id        TEXT NOT NULL,
             name              TEXT NOT NULL,
             body              TEXT NOT NULL DEFAULT '',
             status            TEXT NOT NULL DEFAULT 'draft',
             graduated_to_id   TEXT,
             created_via       TEXT NOT NULL DEFAULT 'unknown',
             created_at        TEXT NOT NULL,
             updated_at        TEXT NOT NULL
         );

CREATE TABLE magic_wand_dispatches (
             id            TEXT PRIMARY KEY,
             comment_id    TEXT NOT NULL REFERENCES work_comments(id),
             artifact_kind TEXT NOT NULL,
             artifact_id   TEXT NOT NULL,
             doc_version   TEXT NOT NULL,
             status        TEXT NOT NULL,
             input_tokens  INTEGER,
             output_tokens INTEGER,
             result_md     TEXT,
             error_kind    TEXT,
             anchor_warning INTEGER NOT NULL DEFAULT 0,
             created_at    TEXT NOT NULL,
             resolved_at   TEXT
         , chore_id TEXT);

CREATE TABLE metadata (
                key TEXT PRIMARY KEY,
                value TEXT NOT NULL
            );

CREATE TABLE metrics_counter (
             name           TEXT PRIMARY KEY,
             value          INTEGER NOT NULL,
             updated_at_ms  INTEGER NOT NULL,
             description    TEXT NOT NULL
         );

CREATE TABLE metrics_gauge (
             name             TEXT PRIMARY KEY,
             value            INTEGER NOT NULL,
             observed_at_ms   INTEGER NOT NULL,
             description      TEXT NOT NULL
         );

CREATE TABLE pane_summaries (
                work_item_id TEXT PRIMARY KEY,
                summary TEXT NOT NULL,
                basis_hash TEXT NOT NULL,
                created_at TEXT NOT NULL
            );

CREATE TABLE planner_runs (
             id             TEXT PRIMARY KEY,
             project_id     TEXT NOT NULL,
             product_id     TEXT NOT NULL,
             design_task_id TEXT,
             caller         TEXT NOT NULL,
             doc_ref        TEXT,
             model          TEXT,
             input_summary  TEXT,
             raw_output     TEXT,
             effort_audit   TEXT,
             notes          TEXT,
             outcome        TEXT NOT NULL,
             result_summary TEXT,
             created_at     TEXT NOT NULL,
             updated_at     TEXT NOT NULL
         );

CREATE TABLE pr_review_batch_members (
             id                 TEXT PRIMARY KEY,
             batch_id           TEXT NOT NULL REFERENCES pr_review_batches(id) ON DELETE CASCADE,
             attempt            INTEGER NOT NULL CHECK (attempt >= 1),
             created_at         TEXT NOT NULL,
             provider_effort    TEXT NOT NULL,
             requested_driver   TEXT NOT NULL,
             resolved_model     TEXT NOT NULL,
             role               TEXT NOT NULL CHECK (role IN ('claude_reviewer', 'codex_reviewer', 'grok_reviewer', 'supervisor', 'post_merge_reviewer')),
             status             TEXT NOT NULL CHECK (status IN ('pending', 'running', 'reported', 'failed')),
             updated_at         TEXT NOT NULL,
             execution_id       TEXT REFERENCES work_executions(id) ON DELETE SET NULL,
             report_proposal_id TEXT,
             terminal_at        TEXT,
             UNIQUE (batch_id, role, attempt),
             UNIQUE (execution_id)
         );

CREATE TABLE "pr_review_batches" (
                 id TEXT PRIMARY KEY,
                 cycle_root_id TEXT NOT NULL,
                 base_sha TEXT NOT NULL,
                 classification_json TEXT NOT NULL,
                 created_at TEXT NOT NULL,
                 phase TEXT NOT NULL CHECK (phase IN ('pre_merge', 'post_merge')),
                 pr_number INTEGER NOT NULL,
                 pr_url TEXT NOT NULL,
                 status TEXT NOT NULL CHECK (status IN ('collecting', 'supervising', 'applying', 'completed', 'failed')),
                 target_sha TEXT NOT NULL,
                 updated_at TEXT NOT NULL,
                 completed_at TEXT,
                 final_verdict_proposal_id TEXT,
                 merge_sha TEXT,
                 generation INTEGER NOT NULL DEFAULT 1 CHECK (generation >= 1), explicit INTEGER NOT NULL DEFAULT 0, producing_work_item_id TEXT,
                 CHECK (phase = 'pre_merge' OR generation = 1),
                 UNIQUE (cycle_root_id, phase, target_sha, generation)
             );

CREATE TABLE pr_review_guide_attempts (
            id TEXT PRIMARY KEY,
            series_id TEXT NOT NULL REFERENCES pr_review_guide_source_series(id),
            comparison_id TEXT NOT NULL REFERENCES pr_review_guide_source_comparisons(id),
            request_epoch INTEGER NOT NULL,
            ordinal INTEGER NOT NULL,
            execution_id TEXT,
            status TEXT NOT NULL,
            prompt_version TEXT NOT NULL,
            driver TEXT,
            model TEXT,
            effort_value TEXT,
            error TEXT,
            retries INTEGER NOT NULL DEFAULT 0,
            idempotency_token TEXT,
            created_at TEXT NOT NULL,
            started_at TEXT,
            finished_at TEXT
        , provider_usage_json TEXT, failed_pre_start INTEGER NOT NULL DEFAULT 0, failed_by_build TEXT);

CREATE TABLE pr_review_guide_request_tokens (
            series_id TEXT NOT NULL REFERENCES pr_review_guide_source_series(id) ON DELETE CASCADE,
            token TEXT NOT NULL,
            attempt_id TEXT NOT NULL REFERENCES pr_review_guide_attempts(id) ON DELETE CASCADE,
            PRIMARY KEY (series_id, token)
        );

CREATE TABLE pr_review_guide_source_comparisons (
            id TEXT PRIMARY KEY,
            series_id TEXT NOT NULL REFERENCES pr_review_guide_source_series(id),
            observation_sequence INTEGER NOT NULL,
            observed_base_sha TEXT NOT NULL,
            merge_base_sha TEXT NOT NULL,
            head_sha TEXT NOT NULL,
            trigger TEXT NOT NULL,
            packet_hash TEXT NOT NULL,
            complete INTEGER NOT NULL CHECK (complete IN (0, 1)),
            omission_count INTEGER NOT NULL DEFAULT 0,
            packet_path TEXT,
            omission_summary_json TEXT,
            captured_at TEXT NOT NULL, probe_base_sha TEXT, attempt_count INTEGER NOT NULL DEFAULT 1,
            UNIQUE(series_id, observed_base_sha, head_sha)
        );

CREATE TABLE pr_review_guide_source_observation_sequence (
            id INTEGER PRIMARY KEY CHECK (id = 1),
            last_sequence INTEGER NOT NULL
        );

CREATE TABLE pr_review_guide_source_series (
            id TEXT PRIMARY KEY,
            root_task_id TEXT NOT NULL,
            canonical_pr_url TEXT NOT NULL UNIQUE,
            latest_observation_sequence INTEGER NOT NULL DEFAULT 0,
            selected_comparison_id TEXT,
            last_capture_error TEXT,
            created_at TEXT NOT NULL,
            updated_at TEXT NOT NULL
        , guide_lifecycle TEXT NOT NULL DEFAULT 'idle', request_epoch INTEGER NOT NULL DEFAULT 0, readable_version_id TEXT);

CREATE TABLE pr_review_guide_versions (
            id TEXT PRIMARY KEY,
            series_id TEXT NOT NULL REFERENCES pr_review_guide_source_series(id),
            comparison_id TEXT NOT NULL REFERENCES pr_review_guide_source_comparisons(id),
            attempt_id TEXT NOT NULL REFERENCES pr_review_guide_attempts(id),
            markdown TEXT NOT NULL,
            raw_output TEXT NOT NULL,
            content_hash TEXT NOT NULL,
            prompt_version TEXT NOT NULL,
            generated_at TEXT NOT NULL
        );

CREATE TABLE pr_review_verdicts (
             id                  TEXT PRIMARY KEY,
             execution_id        TEXT NOT NULL REFERENCES work_executions(id) ON DELETE CASCADE,
             work_item_id        TEXT NOT NULL,
             head_sha            TEXT,
             findings_count      INTEGER NOT NULL DEFAULT 0,
             revision_warranted  INTEGER NOT NULL DEFAULT 0,
             gate_outcome        TEXT NOT NULL,
             revision_task_id    TEXT,
             created_at          TEXT NOT NULL
         , batch_id TEXT, proposal_id TEXT);

CREATE TABLE product_decisions (
             id                    TEXT PRIMARY KEY,
             short_id              INTEGER,
             product_id            TEXT NOT NULL,
             kind                  TEXT NOT NULL,
             status                TEXT NOT NULL DEFAULT 'active',
             title                 TEXT NOT NULL,
             body                  TEXT NOT NULL,
             keywords              TEXT,
             related_work_item_id  TEXT,
             superseded_by         TEXT,
             created_by            TEXT NOT NULL,
             created_via           TEXT NOT NULL DEFAULT 'unknown',
             created_at            TEXT NOT NULL,
             updated_at            TEXT NOT NULL
         );

CREATE TABLE products (
                id TEXT PRIMARY KEY,
                name TEXT NOT NULL,
                slug TEXT NOT NULL UNIQUE,
                description TEXT NOT NULL DEFAULT '',
                repo_remote_url TEXT,
                status TEXT NOT NULL,
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL,
                last_status_actor TEXT,
                status_basis TEXT,
                default_model TEXT,
                default_driver TEXT,
                ci_attempt_budget INTEGER NOT NULL DEFAULT 3,
                dispatch_preamble TEXT,
                design_guidance TEXT,
                external_tracker_kind TEXT,
                external_tracker_config TEXT,
                design_repo TEXT,
                worker_branch_prefix TEXT,
                merge_mechanism TEXT
            , auto_pr_maintenance_enabled INTEGER NOT NULL DEFAULT 1, docs_repo TEXT, editorial_rules TEXT);

CREATE TABLE project_property_audit (
                id          TEXT PRIMARY KEY,
                project_id  TEXT NOT NULL,
                property    TEXT NOT NULL,
                old_value   TEXT,
                new_value   TEXT,
                actor       TEXT NOT NULL,
                changed_at  TEXT NOT NULL
            , basis TEXT);

CREATE TABLE "projects" (
    id TEXT PRIMARY KEY,
    product_id TEXT NOT NULL REFERENCES products(id),
    name TEXT NOT NULL,
    slug TEXT NOT NULL,
    description TEXT NOT NULL DEFAULT '',
    goal TEXT NOT NULL DEFAULT '',
    status TEXT NOT NULL CHECK (status IN ('planned', 'active', 'blocked', 'done', 'archived')),
    priority TEXT NOT NULL,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    design_doc_repo_remote_url TEXT,
    design_doc_branch TEXT,
    design_doc_path TEXT,
    last_status_actor TEXT NOT NULL DEFAULT 'human',
    short_id INTEGER
, status_basis TEXT);

CREATE TABLE short_id_sequences (
             product_id  TEXT PRIMARY KEY REFERENCES products(id),
             next_value  INTEGER NOT NULL DEFAULT 1
         );

CREATE TABLE task_blocked_signals (
             work_item_id  TEXT NOT NULL,
             reason        TEXT NOT NULL,
             attempt_id    TEXT,
             created_at    TEXT NOT NULL,
             cleared_at    TEXT,
             PRIMARY KEY (work_item_id, reason)
         );

CREATE TABLE task_targets (
             id         TEXT PRIMARY KEY,
             task_id    TEXT NOT NULL REFERENCES tasks(id),
             kind       TEXT NOT NULL CHECK (kind IN ('file', 'symbol')),
             value      TEXT NOT NULL,
             created_at TEXT NOT NULL
         );

CREATE TABLE "tasks" (
    id TEXT PRIMARY KEY,
    product_id TEXT NOT NULL REFERENCES products(id),
    project_id TEXT REFERENCES projects(id),
    kind TEXT NOT NULL,
    name TEXT NOT NULL,
    description TEXT NOT NULL DEFAULT '',
    status TEXT NOT NULL CHECK (status IN ('todo', 'active', 'blocked', 'in_review', 'done', 'archived')),
    ordinal INTEGER,
    pr_url TEXT,
    deleted_at TEXT,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    autostart INTEGER NOT NULL DEFAULT 1,
    deferred INTEGER NOT NULL DEFAULT 0,
    human_driven INTEGER NOT NULL DEFAULT 0,
    design_reasoning_effort_xhigh INTEGER NOT NULL DEFAULT 0,
    completion_summary TEXT,
    priority TEXT NOT NULL DEFAULT 'medium',
    repo_remote_url TEXT,
    created_via TEXT NOT NULL DEFAULT 'unknown',
    effort_level TEXT,
    model_override TEXT,
    reasoning TEXT,
    driver TEXT,
    ci_attempt_budget INTEGER,
    ci_attempts_used INTEGER NOT NULL DEFAULT 0,
    external_ref_kind TEXT,
    external_ref_canonical_id TEXT,
    external_ref_raw TEXT,
    external_ref_synced_at TEXT,
    external_ref_unbound_at TEXT,
    last_status_actor TEXT NOT NULL DEFAULT 'human',
    blocked_reason TEXT,
    blocked_attempt_id TEXT,
    doc_repo_remote_url TEXT,
    doc_branch TEXT,
    doc_path TEXT,
    short_id INTEGER,
    ci_required_state TEXT,
    review_required_state TEXT,
    ci_required_detail TEXT,
    review_required_detail TEXT,
    pr_state_polled_at TEXT,
    merge_queue_state TEXT,
    pr_mergeable_state TEXT,
    parent_task_id TEXT,
    source_automation_id TEXT REFERENCES automations(id),
    external_ref_upstream_title TEXT,
    external_ref_upstream_body TEXT,
    external_ref_upstream_checksum TEXT,
    external_ref_boss_checksum TEXT,
    review_cycle INTEGER NOT NULL DEFAULT 0,
    last_reviewed_sha TEXT,
    origin_task_short_id INTEGER,
    origin_pr_number INTEGER,
    completed_at TEXT,
    planner_run_id TEXT,
    archived_by TEXT,
    archived_at TEXT,
    archived_reason TEXT,
    dispatch_failed_reason TEXT,
    dispatch_failed_error TEXT,
    dispatch_failed_at TEXT,
    merge_queue_detail TEXT,
    blocked_detail TEXT,
    effort_matched_rule TEXT,
    effort_reasons TEXT,
    pr_merge_state_status TEXT,
    pr_head_sha TEXT,
    pr_status_observed_at TEXT,
    tags TEXT NOT NULL DEFAULT '[]'
);

CREATE TABLE trunk_merge_intents (
             id                   TEXT PRIMARY KEY,
             work_item_id         TEXT NOT NULL,
             pr_url               TEXT NOT NULL,
             pr_number            INTEGER NOT NULL,
             repo                 TEXT NOT NULL,
             target_branch        TEXT NOT NULL,
             status               TEXT NOT NULL,
             last_trunk_state     TEXT,
             last_trunk_state_at  TEXT,
             submit_count         INTEGER NOT NULL DEFAULT 1,
             created_at           TEXT NOT NULL
         , adopted_at_head_sha TEXT, adopted_at_check_completed_at TEXT);

CREATE TABLE work_attachments (
             id             TEXT PRIMARY KEY,
             execution_id   TEXT NOT NULL,
             work_item_id   TEXT NOT NULL,
             caption        TEXT NOT NULL DEFAULT '',
             content_digest TEXT NOT NULL,
             media_type     TEXT NOT NULL,
             pixel_width    INTEGER NOT NULL,
             pixel_height   INTEGER NOT NULL,
             size_bytes     INTEGER NOT NULL,
             source_name    TEXT NOT NULL,
             created_at     TEXT NOT NULL,
             reclaimed_at   TEXT,
             UNIQUE (execution_id, content_digest)
         );

CREATE TABLE work_attention_items (
                id TEXT PRIMARY KEY,
                execution_id TEXT REFERENCES work_executions(id) ON DELETE CASCADE,
                work_item_id TEXT,
                kind TEXT NOT NULL,
                status TEXT NOT NULL,
                title TEXT NOT NULL,
                body_markdown TEXT NOT NULL,
                created_at TEXT NOT NULL,
                resolved_at TEXT,
                converted_task_id TEXT,
                last_raised_at TEXT,
                CHECK (
                    (execution_id IS NOT NULL AND work_item_id IS NULL)
                    OR (execution_id IS NULL AND work_item_id IS NOT NULL)
                )
            );

CREATE TABLE work_capability_requirements (
             subject_kind TEXT NOT NULL,
             subject_id   TEXT NOT NULL,
             capability   TEXT NOT NULL,
             PRIMARY KEY (subject_kind, subject_id, capability)
         );

CREATE TABLE work_comments (
             id                            TEXT PRIMARY KEY,
             artifact_kind                 TEXT NOT NULL,
             artifact_id                   TEXT NOT NULL,
             doc_version                   TEXT NOT NULL,
             anchor_json                   TEXT NOT NULL,
             body                          TEXT NOT NULL,
             author                        TEXT NOT NULL,
             status                        TEXT NOT NULL,
             status_actor                  TEXT,
             last_resolved_with            TEXT,
             plain_text_projection_version INTEGER NOT NULL DEFAULT 0,
             created_at                    TEXT NOT NULL,
             updated_at                    TEXT NOT NULL,
             dismissed_at                  TEXT
         , intent TEXT, intent_confidence REAL, intent_classified_at TEXT, intent_overridden_by TEXT, revise_task_id TEXT, intent_classification_failed_at TEXT, intent_classification_error TEXT, reopened_at TEXT, guide_version_id TEXT REFERENCES pr_review_guide_versions(id), guide_context_json TEXT);

CREATE TABLE work_executions (
                id TEXT PRIMARY KEY,
                work_item_id TEXT NOT NULL,
                kind TEXT NOT NULL,
                status TEXT NOT NULL,
                repo_remote_url TEXT NOT NULL,
                cube_repo_id TEXT,
                cube_lease_id TEXT,
                cube_workspace_id TEXT,
                workspace_path TEXT,
                priority INTEGER NOT NULL DEFAULT 0,
                preferred_workspace_id TEXT,
                created_at TEXT NOT NULL,
                started_at TEXT,
                finished_at TEXT
            , worker_branch_prefix TEXT, pre_start_failure_count INTEGER NOT NULL DEFAULT 0, dispatch_not_before TEXT, pr_url TEXT, pr_head_before TEXT, pr_head_after TEXT, pr_body_before TEXT, metadata_fix_confirmed_at TEXT, pinned_host_id TEXT, host_id TEXT, prefer_is_soft INTEGER NOT NULL DEFAULT 0, transient_failure_count INTEGER NOT NULL DEFAULT 0, allow_dirty INTEGER NOT NULL DEFAULT 0, branch_naming TEXT, dispatch_wait_reason TEXT, dispatch_wait_since TEXT, stop_seen INTEGER NOT NULL DEFAULT 0, revision_stop_contributed_head TEXT, pr_title_before TEXT, driver_runtime_state TEXT, driver TEXT, model TEXT, effort_level TEXT, pr_head_baseline_absorbed INTEGER NOT NULL DEFAULT 0, run_done_declared_at TEXT, run_done_outcome TEXT, run_undeclared_at TEXT, last_error TEXT);

CREATE TABLE work_item_dependencies (
                dependent_id     TEXT NOT NULL,
                prerequisite_id  TEXT NOT NULL,
                relation         TEXT NOT NULL DEFAULT 'blocks',
                created_at       TEXT NOT NULL,
                PRIMARY KEY (dependent_id, prerequisite_id, relation),
                CHECK (dependent_id <> prerequisite_id)
            );

CREATE TABLE work_runs (
                id TEXT PRIMARY KEY,
                execution_id TEXT NOT NULL REFERENCES work_executions(id) ON DELETE CASCADE,
                agent_id TEXT NOT NULL,
                status TEXT NOT NULL,
                error_text TEXT,
                result_summary TEXT,
                transcript_path TEXT,
                artifacts_path TEXT,
                created_at TEXT NOT NULL,
                started_at TEXT,
                liveness_anchor_at TEXT,
                finished_at TEXT,
                host_id TEXT NOT NULL DEFAULT 'local',
                cube_workspace_id TEXT,
                remote_pid INTEGER,
                shell_pid INTEGER,
                tmux_server_label TEXT,
                tmux_session_name TEXT,
                tmux_spawn_token TEXT,
                tmux_spawn_state TEXT,
                tmux_pane_pid INTEGER,
                tmux_hosted INTEGER NOT NULL DEFAULT 0,
                tmux_observed_pane_dead INTEGER,
                tmux_observed_pane_dead_status TEXT,
                tmux_observed_session_name TEXT,
                tmux_pane_observation TEXT
            , progress_session_id TEXT, model TEXT, output_tokens INTEGER, input_tokens INTEGER, cache_creation_tokens INTEGER, cache_read_tokens INTEGER, cache_creation_5m_tokens INTEGER, cache_creation_1h_tokens INTEGER, rounds INTEGER, agent_active_ms INTEGER, turn_boundary_at TEXT, progress_ingress_checkpoint TEXT, semantic_progress_at TEXT, semantic_tool_condition TEXT, tmux_pane_observation_at TEXT);

CREATE TABLE worker_proposals (
             id              TEXT PRIMARY KEY,
             execution_id    TEXT NOT NULL REFERENCES work_executions(id) ON DELETE CASCADE,
             work_item_id    TEXT,
             kind            TEXT NOT NULL,
             payload_json    TEXT NOT NULL,
             idempotency_key TEXT NOT NULL,
             state           TEXT NOT NULL DEFAULT 'proposed',
             decided_by      TEXT,
             decision_reason TEXT,
             applied_ref     TEXT,
             created_at      TEXT NOT NULL,
             decided_at      TEXT,
             UNIQUE (execution_id, idempotency_key)
         );

CREATE INDEX answer_agent_runs_by_comment
             ON answer_agent_runs(comment_id, created_at);

CREATE INDEX answer_agent_runs_by_execution ON answer_agent_runs(execution_id);

CREATE UNIQUE INDEX attention_groups_grouping_key_idx
             ON attention_groups(grouping_key, generation);

CREATE UNIQUE INDEX attention_groups_product_short_id_idx
             ON attention_groups(product_id, short_id)
             WHERE short_id IS NOT NULL;

CREATE INDEX attention_groups_product_state_idx
             ON attention_groups(product_id, state, created_at);

CREATE INDEX attention_merges_canonical_idx
             ON attention_merges(canonical_attention_id, created_at)
             WHERE canonical_attention_id IS NOT NULL;

CREATE UNIQUE INDEX attention_merges_pair_uq
             ON attention_merges(canonical_attention_id, duplicate_attention_id)
             WHERE duplicate_attention_id IS NOT NULL;

CREATE INDEX attention_merges_work_item_idx
             ON attention_merges(canonical_work_item_id, created_at)
             WHERE canonical_work_item_id IS NOT NULL;

CREATE INDEX attentions_group_idx
             ON attentions(group_id, ordinal);

CREATE INDEX automation_dedup_suppressions_by_automation_idx
             ON automation_dedup_suppressions(automation_id, created_at);

CREATE INDEX automation_runs_by_automation_idx
             ON automation_runs(automation_id, scheduled_for);

CREATE INDEX automations_due_idx
             ON automations(enabled, next_due_at);

CREATE UNIQUE INDEX automations_product_short_id_idx
             ON automations(product_id, short_id) WHERE short_id IS NOT NULL;

CREATE UNIQUE INDEX boothby_actions_by_pass
             ON boothby_actions(pass_id, seq);

CREATE INDEX boothby_actions_by_target
             ON boothby_actions(target_kind, target_id);

CREATE INDEX boothby_findings_status_idx
             ON boothby_findings(status, last_seen DESC);

CREATE UNIQUE INDEX boothby_passes_single_open_idx
             ON boothby_passes((1))
             WHERE finished_at IS NULL;

CREATE INDEX boothby_passes_started_idx
             ON boothby_passes(started_at DESC);

CREATE INDEX ci_remediations_product_idx
             ON ci_remediations(product_id);

CREATE INDEX ci_remediations_status_idx
             ON ci_remediations(status);

CREATE INDEX ci_remediations_work_item_idx
             ON ci_remediations(work_item_id);

CREATE INDEX comment_thread_entries_by_comment
             ON comment_thread_entries(comment_id, created_at);

CREATE INDEX conflict_resolutions_product_idx
             ON conflict_resolutions(product_id);

CREATE INDEX conflict_resolutions_status_idx
             ON conflict_resolutions(status);

CREATE INDEX conflict_resolutions_work_item_idx
             ON conflict_resolutions(work_item_id);

CREATE INDEX effort_escalations_product_idx
             ON effort_escalations(product_id, created_at);

CREATE INDEX effort_escalations_work_item_idx
             ON effort_escalations(work_item_id);

CREATE INDEX execution_driver_decisions_work_item_idx
             ON execution_driver_decisions(work_item_id);

CREATE INDEX github_api_calls_caller_idx
             ON github_api_calls(caller, started_at_ms);

CREATE INDEX github_api_calls_started_idx
             ON github_api_calls(started_at_ms);

CREATE UNIQUE INDEX github_merge_intents_active_work_item_idx
             ON github_merge_intents(work_item_id)
             WHERE status = 'active';

CREATE INDEX github_merge_intents_pr_head_idx
             ON github_merge_intents(pr_url, head_sha)
             WHERE status = 'active';

CREATE INDEX guide_comment_outcomes_by_task
             ON guide_comment_outcomes(revise_task_id);

CREATE UNIQUE INDEX ideas_product_short_id_idx
             ON ideas(product_id, short_id) WHERE short_id IS NOT NULL;

CREATE INDEX ideas_product_status_idx
             ON ideas(product_id, status, created_at);

CREATE INDEX idx_editorial_actions_product
             ON editorial_actions(product_id, created_at DESC);

CREATE INDEX idx_tasks_parent_task_id
        ON tasks(parent_task_id);

CREATE TRIGGER immutable_guide_comment_context
         BEFORE UPDATE OF artifact_kind, artifact_id, guide_version_id, guide_context_json,
                          anchor_json, doc_version, plain_text_projection_version ON work_comments
         WHEN OLD.guide_version_id IS NOT NULL
         BEGIN SELECT RAISE(ABORT, 'guide comment authored context is immutable'); END;

CREATE INDEX magic_wand_dispatches_by_comment
             ON magic_wand_dispatches(comment_id, created_at);

CREATE UNIQUE INDEX planner_runs_one_per_project
             ON planner_runs(project_id)
             WHERE outcome IN ('running','staged','applied');

CREATE INDEX planner_runs_project_idx
             ON planner_runs(project_id, created_at);

CREATE INDEX pr_review_batch_members_batch_idx
             ON pr_review_batch_members(batch_id, role, attempt);

CREATE INDEX pr_review_batches_cycle_root_idx
                 ON pr_review_batches(cycle_root_id, created_at);

CREATE INDEX pr_review_guide_attempts_comparison_idx
            ON pr_review_guide_attempts(comparison_id, created_at DESC);

CREATE INDEX pr_review_guide_attempts_execution_idx
            ON pr_review_guide_attempts(execution_id) WHERE execution_id IS NOT NULL;

CREATE UNIQUE INDEX pr_review_guide_attempts_idempotency_idx
            ON pr_review_guide_attempts(series_id, idempotency_token)
            WHERE idempotency_token IS NOT NULL;

CREATE UNIQUE INDEX pr_review_guide_attempts_one_live_series
           ON pr_review_guide_attempts(series_id) WHERE status IN ('queued', 'running');

CREATE INDEX pr_review_guide_attempts_series_idx
            ON pr_review_guide_attempts(series_id, request_epoch DESC);

CREATE INDEX pr_review_guide_source_comparisons_series_sequence_idx
            ON pr_review_guide_source_comparisons(series_id, observation_sequence DESC, captured_at DESC);

CREATE INDEX pr_review_guide_source_series_observation_idx
            ON pr_review_guide_source_series(root_task_id, latest_observation_sequence DESC, id DESC);

CREATE INDEX pr_review_guide_versions_series_idx
            ON pr_review_guide_versions(series_id, generated_at DESC);

CREATE UNIQUE INDEX pr_review_verdicts_batch_id_uidx
             ON pr_review_verdicts(batch_id) WHERE batch_id IS NOT NULL;

CREATE INDEX pr_review_verdicts_execution_idx
             ON pr_review_verdicts(execution_id);

CREATE UNIQUE INDEX pr_review_verdicts_proposal_id_uidx
             ON pr_review_verdicts(proposal_id) WHERE proposal_id IS NOT NULL;

CREATE INDEX pr_review_verdicts_work_item_idx
             ON pr_review_verdicts(work_item_id, created_at);

CREATE UNIQUE INDEX product_decisions_product_short_id_idx
             ON product_decisions(product_id, short_id) WHERE short_id IS NOT NULL;

CREATE INDEX product_decisions_product_status_idx
             ON product_decisions(product_id, status, created_at);

CREATE INDEX project_property_audit_project_idx
                ON project_property_audit(project_id, changed_at);

CREATE UNIQUE INDEX projects_product_short_id_idx
        ON projects(product_id, short_id) WHERE short_id IS NOT NULL;

CREATE UNIQUE INDEX projects_product_slug_idx
        ON projects(product_id, slug);

CREATE INDEX task_blocked_signals_active_idx
             ON task_blocked_signals(work_item_id, reason)
             WHERE cleared_at IS NULL;

CREATE INDEX task_targets_kind_value_idx
             ON task_targets(kind, value);

CREATE INDEX task_targets_task_id_idx
             ON task_targets(task_id);

CREATE UNIQUE INDEX tasks_external_ref_bound_uniq
        ON tasks (external_ref_kind, external_ref_canonical_id)
        WHERE external_ref_canonical_id IS NOT NULL
          AND external_ref_unbound_at  IS NULL
          AND deleted_at               IS NULL;

CREATE INDEX tasks_external_ref_idx
        ON tasks (external_ref_kind, external_ref_canonical_id)
        WHERE external_ref_canonical_id IS NOT NULL;

CREATE INDEX tasks_product_idx
        ON tasks(product_id, kind, deleted_at);

CREATE UNIQUE INDEX tasks_product_short_id_idx
        ON tasks(product_id, short_id) WHERE short_id IS NOT NULL;

CREATE INDEX tasks_project_idx
        ON tasks(project_id, deleted_at, ordinal);

CREATE INDEX tasks_repo_idx
        ON tasks(repo_remote_url, deleted_at) WHERE repo_remote_url IS NOT NULL;

CREATE INDEX tasks_source_automation_idx
        ON tasks(source_automation_id, status) WHERE source_automation_id IS NOT NULL;

CREATE UNIQUE INDEX trunk_merge_intents_active_work_item_idx
             ON trunk_merge_intents(work_item_id)
             WHERE status = 'active';

CREATE INDEX trunk_merge_intents_adopted_episode_idx
             ON trunk_merge_intents(work_item_id, adopted_at_head_sha, adopted_at_check_completed_at);

CREATE INDEX trunk_merge_intents_status_idx
             ON trunk_merge_intents(status);

CREATE INDEX trunk_merge_intents_work_item_idx
             ON trunk_merge_intents(work_item_id);

CREATE INDEX work_attachments_digest_idx
             ON work_attachments(content_digest);

CREATE INDEX work_attachments_work_item_idx
             ON work_attachments(work_item_id, created_at);

CREATE INDEX work_attention_items_execution_idx
                ON work_attention_items(execution_id, created_at);

CREATE INDEX work_attention_items_work_item_idx
            ON work_attention_items(work_item_id, created_at);

CREATE INDEX work_comments_by_artifact
             ON work_comments(artifact_kind, artifact_id, status);

CREATE INDEX work_comments_by_revise_task ON work_comments(revise_task_id);

CREATE INDEX work_comments_guide_version_idx ON work_comments(guide_version_id);

CREATE INDEX work_executions_ready_idx
                ON work_executions(status, priority, created_at);

CREATE INDEX work_executions_work_item_idx
                ON work_executions(work_item_id, created_at);

CREATE INDEX work_item_dependencies_dependent_idx
                ON work_item_dependencies(dependent_id, relation);

CREATE INDEX work_item_dependencies_prereq_idx
                ON work_item_dependencies(prerequisite_id, relation);

CREATE INDEX work_runs_execution_idx
                ON work_runs(execution_id, created_at);

CREATE UNIQUE INDEX work_runs_tmux_spawn_token_idx
                ON work_runs(tmux_spawn_token)
                WHERE tmux_spawn_token IS NOT NULL;

CREATE INDEX worker_proposals_work_item_idx
             ON worker_proposals(work_item_id, created_at);
"###;
