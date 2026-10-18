#!/usr/bin/env node
// The 40 Edition 5 acceptance scenarios (A01-A40) as executable, shared
// field-level checks. Each host harness produces an observation bundle for
// every scenario from ITS OWN real code paths (VS Code validators/state vs
// JetBrains protocol parsers/Swing models) and this module asserts the spec's
// expected visible outcomes and forbidden fake states.
//
// The checks intentionally name the machine fields from acceptance.md; a
// wrong type, a fabricated zero, a silently dropped field or an accepted
// stale reply is a failure, never a soft warning.

export const SCENARIO_IDS = Array.from({ length: 40 }, (_, index) => `A${String(index + 1).padStart(2, '0')}`);

function assert(condition, message) {
  if (!condition) {
    throw new Error(message);
  }
}

function equal(actual, expected, message) {
  if (actual !== expected) {
    throw new Error(`${message}: expected ${JSON.stringify(expected)}, got ${JSON.stringify(actual)}`);
  }
}

function numbers(actual, expected, message) {
  if (!Array.isArray(actual) || actual.length !== expected.length || actual.some((value, index) => value !== expected[index])) {
    throw new Error(`${message}: expected ${JSON.stringify(expected)}, got ${JSON.stringify(actual)}`);
  }
}

function listHas(actual, expected, message) {
  assert(Array.isArray(actual) && actual.includes(expected), `${message}: missing ${expected} in ${JSON.stringify(actual)}`);
}

export const SCENARIOS = [
  {
    id: 'A01',
    title: 'Provider/model atomicity',
    run(obs) {
      equal(obs.pair.provider, 'p1', 'the rejected pair names its provider');
      equal(obs.pair.model, 'm1', 'the rejected pair names its model');
      equal(obs.availability, 'unavailable', 'availability is explicit');
      equal(obs.applied, false, 'the pair is rejected atomically');
      equal(obs.default_provider, 'p0', 'the default provider did not change');
      equal(obs.default_model, 'm0', 'the default model did not change');
      equal(obs.draft_survives, true, 'the draft survives');
      equal(obs.error_names_pair, true, 'the error names the pair');
    },
  },
  {
    id: 'A02',
    title: 'Capability authority',
    run(obs) {
      equal(obs.unknown.authority, 'unknown', 'unknown authority is surfaced');
      equal(obs.unknown.gated, true, 'gated capabilities are disabled');
      equal(obs.unknown.tools, false, 'tools are disabled for unknown authority');
      equal(obs.unknown.vision, false, 'vision is disabled for unknown authority');
      equal(obs.unknown.structured_output, false, 'structured output is disabled for unknown authority');
      equal(obs.known.authority, 'known', 'known authority is surfaced');
      equal(obs.known.gated, false, 'known authority is not gated');
      equal(obs.known.tools, true, 'known authority exposes supported tools');
      equal(obs.manual_label_certifies, false, 'a manual label never certifies support');
    },
  },
  {
    id: 'A03',
    title: 'Generation compatibility',
    run(obs) {
      equal(obs.descriptor.kind, 'integer', 'the parameter descriptor kind is exact');
      equal(obs.descriptor.min, 0, 'the descriptor min is exact');
      equal(obs.descriptor.max, 2, 'the descriptor max is exact');
      equal(obs.descriptor.requires_exact_authority, true, 'the descriptor requires exact authority');
      equal(obs.local_error, 'unsupported parameter high', 'unsupported tuning fails locally before admission');
      equal(obs.staged, true, 'the incompatible edit remains staged');
      equal(obs.unknown_fields_sent.length, 0, 'no unknown field reaches the strict DTO');
    },
  },
  {
    id: 'A04',
    title: 'Unset versus zero',
    run(obs) {
      equal(obs.zero.request.temperature, 0, 'temperature zero is an explicit override');
      equal(obs.zero.winning_source, 'override', 'zero is attributed to the override');
      assert(!('temperature' in obs.unset.request), 'unset is absent from the effective request');
      equal(obs.unset.winning_source, 'provider_default', 'unset is attributed to the provider default');
      equal(obs.reset_removes_override, true, 'reset removes the override');
      equal(obs.absent_is_not_zero, true, 'absent is never rendered as zero');
    },
  },
  {
    id: 'A05',
    title: 'Active-call capture',
    run(obs) {
      equal(obs.active.model, 'm1', 'the active call keeps its captured model');
      equal(obs.active.parameters.effort, 'low', 'the active call keeps its captured effort');
      equal(obs.active.route_reason, 'exact', 'the route reason is recorded');
      equal(obs.draft.model, 'm2', 'the draft carries the new preference separately');
      equal(obs.active_unchanged, true, 'the active request and historical receipt are unchanged');
      equal(obs.apply_timing, 'Next call', 'the UI states when the edit applies');
    },
  },
  {
    id: 'A06',
    title: 'Secret storage',
    run(obs) {
      equal(obs.shared_config_has_secret, false, 'no credential value is in shared config');
      equal(obs.logs_redacted, true, 'logs are redacted');
      listHas(obs.diagnostics_omitted, 'secret', 'diagnostics omit secret categories');
      listHas(obs.diagnostics_omitted, 'env', 'diagnostics omit environment categories');
      equal(obs.ref, 'vault:provider/p1', 'only an opaque reference is visible');
      equal(obs.classification, 'secret', 'the reference is classified as secret');
      equal(obs.local_delete_revokes_remote, false, 'local deletion is not remote revocation');
    },
  },
  {
    id: 'A07',
    title: 'Loopback versus LAN',
    run(obs) {
      equal(obs.ollama_default.endpoint_class, 'loopback', 'the Ollama default resolves to loopback');
      equal(obs.ollama_default.allowed, true, 'the Ollama default is allowed');
      equal(obs.custom.allow_loopback, true, 'custom opt-in records allow_loopback');
      equal(obs.custom.allowed, true, 'custom loopback is allowed with opt-in');
      equal(obs.lan.endpoint_class, 'private_lan', 'the LAN endpoint is classified as private LAN');
      equal(obs.lan.allowed, false, 'private LAN remains refused');
      assert(obs.lan.reason.length > 0, 'the LAN refusal carries a reason');
      equal(obs.allow_loopback_bypasses_lan, false, 'allow_loopback never authorizes LAN');
    },
  },
  {
    id: 'A08',
    title: 'Remote locality',
    run(obs) {
      equal(obs.editor_host, 'laptop', 'the editor host is explicit');
      equal(obs.daemon_host, 'remote', 'the daemon host is explicit');
      equal(obs.model_endpoint_host, 'daemon', 'the loopback endpoint resolves on the daemon host');
      equal(obs.vault_location, 'remote', 'the vault location is explicit');
      equal(obs.worker_location, 'remote', 'the worker location is explicit');
      equal(obs.old_handle_valid, false, 'a host change invalidates old handles');
      equal(obs.draft_survives, true, 'drafts survive the host change');
    },
  },
  {
    id: 'A09',
    title: 'Ask enforcement',
    run(obs) {
      equal(obs.preset, 'Ask', 'the Ask preset is named');
      equal(obs.write_tools_offered, false, 'no write/effectful tool is offered');
      listHas(obs.offered_tools, 'read_file', 'the read-only tool remains offered');
      equal(obs.dispatch_denied, true, 'an effectful dispatch is denied');
      assert(obs.label.toLowerCase().includes('ask'), 'the prompt-only route is labelled');
      assert(obs.denial_reason.length > 0, 'the denial reason is visible');
    },
  },
  {
    id: 'A10',
    title: 'Preset and policy',
    run(obs) {
      equal(obs.preset_widened_authority, false, 'a preset cannot widen managed authority');
      listHas(obs.managed_denies, 'shell', 'the managed deny remains');
      equal(obs.effective_envelope.denied.includes('shell'), true, 'the effective envelope keeps the deny');
      equal(obs.owner_checkout_edit, false, 'a preset cannot edit the owner checkout');
      equal(obs.mutation_mode, 'shadow', 'the isolated mutation mode is explicit');
    },
  },
  {
    id: 'A11',
    title: 'Context null',
    run(obs) {
      equal(obs.context_usage, null, 'contextUsage stays null');
      equal(obs.label, 'Unavailable', 'null renders the exact Unavailable label');
      equal(obs.basis, 'unavailable', 'the basis is unavailable');
      equal(obs.fabricated_percentage, false, 'no percentage is fabricated');
      equal(obs.measured_label, false, 'a draft estimate is not labelled measured');
    },
  },
  {
    id: 'A12',
    title: 'Context arithmetic',
    run(obs) {
      equal(obs.category_sum, obs.input, 'the category sum equals consumed input');
      equal(obs.input + obs.output_reserve + obs.safety_reserve + obs.headroom, obs.capacity, 'input + reserves + headroom equal capacity');
      equal(obs.input + obs.output_reserve + obs.safety_reserve + obs.headroom, 32768, 'the sample math is consistent');
      equal(obs.input, 18400, 'the sample input is exact');
      equal(obs.output_reserve, 4096, 'the output reserve is exact');
      equal(obs.safety_reserve, 2048, 'the safety reserve is exact');
      equal(obs.headroom, 8224, 'the headroom is exact');
      equal(obs.reserves_counted_as_input, false, 'reserves are not counted as consumed input');
      equal(obs.overflow_rejected, true, 'overflow is rejected before admission');
    },
  },
  {
    id: 'A13',
    title: 'Source identity',
    run(obs) {
      equal(obs.sources.length, 3, 'all three source kinds are attached');
      for (const source of obs.sources) {
        assert(source.uri.startsWith('file:///'), 'each source carries a URI');
        assert(typeof source.version === 'number', 'each source carries a version');
        assert(source.hash.length > 0, 'each source carries a content hash');
        assert(source.worktree.length > 0, 'each source names its worktree');
        assert(source.reason.length > 0, 'each source names its inclusion reason');
      }
      equal(obs.after_switch_same, true, 'sources survive a worktree/session switch');
      equal(obs.stale_version_reused, false, 'a stale version is never silently reused');
      equal(obs.global_cwd_resolution, false, 'a path never resolves against a global current directory');
    },
  },
  {
    id: 'A14',
    title: 'Attachment failure',
    run(obs) {
      equal(obs.attachments.length, 3, 'failed attachments remain visible');
      for (const attachment of obs.attachments) {
        assert(attachment.error.length > 0, 'each failed attachment carries a reason');
      }
      equal(obs.send_blocked, true, 'invalid admission is blocked');
      equal(obs.draft_text, 'keep me', 'the composer text is preserved');
      equal(obs.remove_retry, true, 'Remove/Retry is available');
      equal(obs.truncated_shown_complete, false, 'a truncated attachment is never shown as complete');
    },
  },
  {
    id: 'A15',
    title: 'Compaction continuity',
    run(obs) {
      assert(obs.reduction_ratio < obs.minimum_reduction, 'the minimal reduction is below the configured minimum');
      equal(obs.success, false, 'a minimal reduction does not advance success');
      equal(obs.prior_basis_kept, true, 'the prior valid basis remains');
      equal(obs.pending_approval_kept, true, 'a pending approval survives a failed compaction');
      equal(obs.effect_status, 'unknown', 'the unknown external effect stays unknown');
      listHas(obs.retained_facts, 'criterion A', 'retained facts survive');
      assert(obs.measured_before > obs.measured_after, 'measured before/after are recorded');
      assert(obs.summary_version.length > 0, 'a summary version is recorded');
    },
  },
  {
    id: 'A16',
    title: 'Memory deletion',
    run(obs) {
      equal(obs.record_id, 'mem-1', 'the exact record is named');
      equal(obs.scope, 'project', 'the deletion scope is exact');
      assert(obs.stopped_future_use_at_revision > 0, 'future-use deletion is acknowledged at a revision');
      listHas(obs.prior_artifacts_retained, 'evidence:41', 'prior prompt/artifact copies are disclosed as retained');
      assert(obs.receipt.length > 0, 'a deletion receipt exists');
      equal(obs.claimed_prior_deleted, false, 'the UI never claims prior copies were deleted');
      equal(obs.project_redirected_global_storage, false, 'a project cannot redirect global memory storage');
    },
  },
  {
    id: 'A17',
    title: 'Approval exactly once',
    run(obs) {
      equal(obs.replies_dispatched, 1, 'exactly one reply is dispatched');
      equal(obs.consumed, true, 'the waiter is consumed');
      equal(obs.duplicate_click_refused, true, 'a duplicate click is refused');
      equal(obs.stale_refused, true, 'a stale generation is refused');
      equal(obs.wrong_session_refused, true, 'a wrong-session reply is refused');
      assert(obs.fingerprint.length > 0, 'the action fingerprint is recorded');
    },
  },
  {
    id: 'A18',
    title: 'Persistent rule boundary',
    run(obs) {
      equal(obs.allow_once_persisted, false, 'Allow once cannot silently persist');
      equal(obs.remember_unavailable_without_store, true, 'Remember is unavailable without a policy store');
      equal(obs.rule_scope, 'workspace', 'a saved rule has an explicit scope');
      assert(obs.rule_version > 0, 'a saved rule has a version');
      equal(obs.pending_request_untouched, true, 'a future rule never auto-replies to another pending request');
      equal(obs.auto_replied_other_request, false, 'no cross-request auto-reply');
    },
  },
  {
    id: 'A19',
    title: 'Command normalization',
    run(obs) {
      equal(obs.normalized.executable, '/bin/bash', 'the interpreter executable is normalized');
      listHas(obs.normalized.argv, '-c', 'the interpreter payload is visible');
      equal(obs.normalized.cwd, '/ws', 'the cwd is explicit');
      assert(obs.normalized.symlink_target.startsWith('/usr/'), 'the symlink target is resolved');
      equal(obs.decorative_prefix_safe, false, 'a decorative prefix is not shown as safe');
      equal(obs.changed_args_new_decision, true, 'changed args require a new decision');
      equal(obs.rule_matched, false, 'the prefix/glob rule does not match the real target');
      equal(obs.symlink_hidden, false, 'the symlink target is never hidden');
    },
  },
  {
    id: 'A20',
    title: 'Sandbox truth',
    run(obs) {
      equal(obs.filesystem, 'sandboxed', 'filesystem isolation is reported separately');
      equal(obs.shell, 'sandboxed', 'shell isolation is reported separately');
      equal(obs.network, 'none', 'network isolation is reported separately');
      equal(obs.network_guarantee, 'none', 'the network guarantee is explicit');
      equal(obs.setup_state, 'failed', 'setup failure is visible');
      equal(obs.affected_action_blocked, true, 'failed setup blocks affected actions');
      equal(obs.fallback_fully_isolated, false, 'a network-capable fallback is not labelled fully isolated');
    },
  },
  {
    id: 'A21',
    title: 'MCP bounds',
    run(obs) {
      equal(obs.limits.servers, 8, 'the server bound is 8');
      equal(obs.limits.name_bytes, 128, 'the name bound is 128 UTF-8 bytes');
      equal(obs.limits.command_bytes, 512, 'the command bound is 512');
      equal(obs.limits.args, 32, 'the argument count bound is 32');
      equal(obs.limits.arg_bytes, 512, 'each argument bound is 512');
      equal(obs.persisted, false, 'the over-bound entry fails before persistence');
      equal(obs.prior_revision, 7, 'the prior valid config survives');
      equal(obs.error_names_bound, true, 'the error names the exact bound');
      equal(obs.silently_truncated, false, 'nothing is silently truncated');
    },
  },
  {
    id: 'A22',
    title: 'MCP transport/auth',
    run(obs) {
      equal(obs.stdio_transport, 'stdio', 'the reviewed config is stdio');
      equal(obs.remote_transport, 'unsupported', 'remote/OAuth is labelled unsupported');
      equal(obs.cancelled_auth_state, 'signed_out', 'cancelled auth is not Connected');
      equal(obs.connected_after_cancel, false, 'cancelled auth never becomes Connected');
      equal(obs.disconnect_cancels_remote_effect, false, 'disconnect does not claim a remote effect was cancelled');
      equal(obs.stdio_shown_as_oauth, false, 'stdio is never shown as HTTP/OAuth');
    },
  },
  {
    id: 'A23',
    title: 'Project executable trust',
    run(obs) {
      equal(obs.opening_repo_ran_setup, false, 'opening a repository does not run setup');
      equal(obs.opening_manifest_ran_install, false, 'viewing a manifest does not install');
      equal(obs.trust_state, 'untrusted', 'project executables start untrusted');
      assert(obs.start_action.includes('trust'), 'starting requires deliberate trust');
      equal(obs.prior_valid_server, 'v1', 'the prior valid server/version remains');
      equal(obs.install_implies_trust, false, 'installation is not trust');
      equal(obs.auto_start, false, 'project content never auto-starts executables');
    },
  },
  {
    id: 'A24',
    title: 'Hooks and verdict',
    run(obs) {
      const byOutcome = Object.fromEntries(obs.outcomes.map((entry) => [entry.outcome, entry.verdict]));
      equal(byOutcome.Verdict, 'allow', 'a verdict is carried');
      equal(byOutcome.TimedOut, 'none', 'a timeout is not an allow');
      equal(byOutcome.NonZero, 'none', 'a nonzero exit is not an allow');
      equal(byOutcome.Malformed, 'none', 'malformed output is not a pass');
      equal(obs.timeout_authorizes_tool, false, 'partial stdout after timeout cannot authorize a tool');
      equal(obs.partial_stdout_authorizes, false, 'partial output is never used as a verdict');
      equal(obs.audit_bounded, true, 'the audit artifact is bounded');
      assert(obs.spec_hash.startsWith('sha256:'), 'the audit cites the spec hash');
      equal(obs.fail_open_hidden, false, 'fail-open is never hidden');
    },
  },
  {
    id: 'A25',
    title: 'Child budgets',
    run(obs) {
      const reserved = obs.child_reservations.reduce((sum, value) => sum + value, 0);
      assert(reserved + obs.open + obs.settled + obs.uncertain <= obs.parent_cap_tokens, 'children share the parent envelope');
      equal(obs.ownership, 'orchestrator', 'ownership contention is visible');
      assert(obs.queue_bound > 0, 'the queue is bounded');
      equal(obs.exceeded_parent, false, 'the parent budget is never silently exceeded');
      equal(obs.refusal_reason, null, 'no refusal is needed for a bounded set');
    },
  },
  {
    id: 'A26',
    title: 'Steer bounds',
    run(obs) {
      equal(obs.steer_0, 'rejected', 'empty steer is rejected');
      equal(obs.steer_500, 'accepted', '500 characters are accepted');
      equal(obs.steer_501, 'rejected', '501 characters are rejected');
      equal(obs.truncated, false, 'nothing is silently truncated');
      equal(obs.stale_control_acted, false, 'a stale child control is fenced');
      assert(obs.delivery_point.length > 0, 'the delivery point is stated');
      equal(obs.ack, true, 'delivery is acknowledged');
    },
  },
  {
    id: 'A27',
    title: 'Owned process',
    run(obs) {
      equal(obs.stale_pid_input_authorized, false, 'a stale PID cannot authorize input');
      equal(obs.stale_pid_kill_authorized, false, 'a stale PID cannot authorize kill');
      equal(obs.process_stop_scope, 'process', 'process stop is its own scope');
      equal(obs.task_stop_scope, 'task', 'task stop is its own scope');
      equal(obs.session_abort_scope, 'session', 'session abort is its own scope');
      equal(obs.closed_tab_stops_work, false, 'closing a tab does not stop work');
      assert(obs.live_handle_generation > 0, 'a live owned handle carries a generation');
    },
  },
  {
    id: 'A28',
    title: 'Unknown effect',
    run(obs) {
      equal(obs.effect_status, 'unknown', 'the external effect stays unknown');
      equal(obs.cancelled_on_disconnect, false, 'a disconnect does not mark the effect cancelled');
      equal(obs.blind_retry_offered, false, 'no blind automatic replay is offered');
      assert(obs.reconciliation_action.length > 0, 'a safe reconcile action is offered');
      equal(obs.retry_eligible, false, 'retry eligibility is not assumed');
    },
  },
  {
    id: 'A29',
    title: 'Admission idempotence',
    run(obs) {
      equal(obs.same_retry, 'Existing', 'the same UUID/body recovers the receipt');
      equal(obs.altered_body, 'Conflict', 'an altered body conflicts');
      equal(obs.duplicate_runs, 1, 'no duplicate work is created');
      assert(obs.receipt_run_id.length > 0, 'the receipt names the run');
    },
  },
  {
    id: 'A30',
    title: 'Budget exactness',
    run(obs) {
      equal(obs.usd_5, '5000000', 'USD 5.00 is exactly 5,000,000 micro-USD');
      equal(obs.negative, 'rejected', 'negatives are rejected');
      equal(obs.fractional_micro, 'rejected', 'fractions beyond micro precision are rejected');
      equal(obs.overflow, 'rejected', 'overflow is rejected');
      equal(obs.float_rounding, false, 'no floating-point rounding is used');
      equal(obs.ledger_reservation, '5000000', 'the ledger reservation is exact');
    },
  },
  {
    id: 'A31',
    title: 'Unknown pricing',
    run(obs) {
      equal(obs.list_price, null, 'a missing list price stays unavailable');
      equal(obs.declared_price, null, 'a missing declared price stays unavailable');
      equal(obs.observed_price, null, 'a missing observed price stays unavailable');
      equal(obs.state, 'Unavailable', 'the UI shows Unavailable');
      equal(obs.missing_rate_as_zero, false, 'a missing rate is never zero');
      equal(obs.cancelled_attempted_spend_micro, '100', 'cancelled attempted spend is retained');
      equal(obs.settled, 100, 'the attempted spend remains eligible');
      equal(obs.estimate_labeled_provider_charge, false, 'a local estimate is not labelled a provider charge');
    },
  },
  {
    id: 'A32',
    title: 'Retry policy',
    run(obs) {
      equal(obs.max_attempts, 3, 'max_attempts is the total attempt count');
      equal(obs.attempts, 3, 'three total attempts is not three retries');
      equal(obs.retries, 2, 'the retry count is attempts minus one');
      assert(obs.base_delay_ms > 0 && obs.max_delay_ms >= obs.base_delay_ms, 'backoff bounds are valid');
      assert(obs.jitter >= 0 && obs.jitter <= 1, 'jitter is bounded');
      equal(obs.stop_available, true, 'Stop remains available');
      equal(obs.effect_replayed_after_journal, false, 'an effectful request is not replayed after journaling');
      equal(obs.guessed_reset, false, 'a reset time is never guessed');
    },
  },
  {
    id: 'A33',
    title: 'Checkpoint coverage',
    run(obs) {
      equal(obs.hash_only.target_content_ref, null, 'a hash-only checkpoint has no after-content blob');
      equal(obs.hash_only.redo_enabled, false, 'redo is disabled without blobs');
      equal(obs.hash_only.diff_enabled, false, 'diff is disabled without blobs');
      listHas(obs.hash_only.uncovered, 'shell', 'uncovered shell effects are named');
      listHas(obs.hash_only.uncovered, 'network', 'uncovered network effects are named');
      assert(obs.content_aware.target_content_ref !== null, 'a content-aware checkpoint carries the after blob');
      equal(obs.content_aware.redo_enabled, true, 'redo is enabled with content');
      equal(obs.content_aware.diff_enabled, true, 'diff is enabled with content');
      equal(obs.restore_claims_external, false, 'restore never claims external effects were covered');
    },
  },
  {
    id: 'A34',
    title: 'Manual-edit CAS',
    run(obs) {
      assert(obs.expected_current_hash !== obs.observed_hash, 'the observed hash differs from the expected hash');
      equal(obs.conflict, 'CAS', 'the restore/integration refuses the changed precondition');
      equal(obs.overwrote_manual_edit, false, 'manual edits are preserved');
      equal(obs.rebase_offered, true, 'rebase is offered');
      equal(obs.compare_offered, true, 'compare is offered');
      equal(obs.preserved_edits, true, 'unrelated user edits are preserved');
    },
  },
  {
    id: 'A35',
    title: 'Engineering completion',
    run(obs) {
      equal(obs.chat_completed, true, 'the chat is completed');
      equal(obs.outcome, 'Pending', 'the engineering outcome stays Pending');
      equal(obs.claims_complete, false, 'chat completion is not shown as verified');
      assert(obs.required_passed < obs.required_total, 'missing required evidence remains visible');
      equal(obs.verified_candidate, false, 'no verified candidate yet');
      equal(obs.landed_candidate, false, 'no matching landing yet');
      equal(obs.finish_receipt_match, false, 'no finish receipt match yet');
      equal(obs.turned_green, false, 'the outcome does not turn green');
      assert(obs.blockers.length >= 3, 'each blocker is named');
    },
  },
  {
    id: 'A36',
    title: 'VCSC denominator',
    run(obs) {
      equal(obs.denominator, 0, 'the zero verified denominator is explicit');
      equal(obs.vcsc, null, 'zero verified denominator is Unavailable, not zero');
      equal(obs.state, 'Unavailable', 'the measurement state is Unavailable');
      assert(obs.numerator_micro === '900', 'attempted spend is the numerator');
      assert(obs.cohort.length > 0, 'the cohort is explicit');
      assert(obs.period.length > 0, 'the period is explicit');
      equal(obs.successful_run_cost_labeled_vcsc, false, 'successful-run cost is not mislabelled VCSC');
      equal(obs.denominator_hidden, false, 'the denominator is not hidden');
    },
  },
  {
    id: 'A37',
    title: 'Atomic settings',
    run(obs) {
      equal(obs.invalid_patch.applied, false, 'an invalid patch writes nothing');
      equal(obs.invalid_patch.written, 0, 'no field is partially persisted');
      equal(obs.invalid_patch.draft_retained, true, 'the draft is retained after an invalid patch');
      assert(obs.invalid_patch.error.length > 0, 'the invalid field error is named');
      equal(obs.stale.applied, false, 'a stale revision does not apply');
      equal(obs.stale.draft_retained, true, 'a revision conflict retains the draft');
      equal(obs.stale.compare_offered, true, 'a revision conflict opens compare/rebase');
      assert(obs.reset.includes('applied'), 'Reset restores applied values');
      assert(obs.defaults.includes('staged'), 'Restore defaults stages removal until Apply');
      equal(obs.partial_persist, false, 'no partial persistence');
      equal(obs.toast_as_applied, false, 'a toast is never shown as applied');
    },
  },
  {
    id: 'A38',
    title: 'Session/host fencing',
    run(obs) {
      equal(obs.binding.daemon, 'd1', 'the binding names the daemon generation');
      equal(obs.binding.workspace, 'w1', 'the binding names the workspace');
      equal(obs.binding.worktree, 'wt1', 'the binding names the worktree');
      equal(obs.binding.session, 's1', 'the binding names the session');
      assert(obs.binding.generation > 0, 'the binding carries a generation');
      equal(obs.late_callback_rejected, true, 'a late callback from the old binding is rejected');
      equal(obs.old_handle_controls_new, false, 'an old handle cannot control the new session');
      equal(obs.drafts_preserved, true, 'drafts are preserved');
      equal(obs.stale_marked, true, 'stale state is marked');
      equal(obs.merged_events, false, 'events are never merged across sessions');
    },
  },
  {
    id: 'A39',
    title: 'Accessible compact view',
    run(obs) {
      equal(obs.widths.join(','), '240,320,480', 'the compact widths are exercised');
      equal(obs.text_scale, 2.0, '200% text is exercised');
      equal(obs.keyboard_reachable, true, 'every essential control is keyboard reachable');
      equal(obs.ime_works, true, 'IME input works');
      equal(obs.high_contrast, true, 'high contrast retains the controls');
      equal(obs.accessible_names, true, 'every essential control has an accessible name');
      equal(obs.focus_visible, true, 'focus is visible');
      equal(obs.status_color_only, false, 'status is not color-only');
      equal(obs.send_stop_visible, true, 'Send/Stop and attention remain visible');
      equal(obs.clipped_labels, false, 'labels are not clipped');
      equal(obs.keyboard_trap, false, 'there is no keyboard trap');
      equal(obs.tooltip_only_errors, false, 'errors are not tooltip-only');
    },
  },
  {
    id: 'A40',
    title: 'Long stream and full search',
    run(obs) {
      equal(obs.events_ordered, true, 'event order is preserved');
      equal(obs.seq_monotonic, true, 'durable sequence numbers are monotonic');
      assert(obs.rendered_window > 0, 'rendering is windowed');
      equal(obs.find_reaches_unloaded, true, 'Find reaches unloaded history');
      equal(obs.logs_bounded, true, 'logs stay bounded');
      assert(obs.artifact_ref.length > 0, 'bounded logs link to the full artifact');
      equal(obs.auto_scroll_after_user_scroll, false, 'follow-tail never steals user scroll');
      equal(obs.dropped_events, 0, 'no durable event is dropped');
      equal(obs.find_only_rendered, false, 'Find does not search only rendered rows');
    },
  },
];

export function scenarioById(id) {
  return SCENARIOS.find((scenario) => scenario.id === id) ?? null;
}

/** Run all scenarios against one host's observation bundle. */
export function runScenarios(observations, host) {
  const results = [];
  for (const scenario of SCENARIOS) {
    const observed = observations[scenario.id];
    if (!observed || typeof observed !== 'object') {
      results.push({
        id: scenario.id,
        title: scenario.title,
        host,
        status: 'failed',
        detail: 'no observation bundle for this scenario',
      });
      continue;
    }
    try {
      scenario.run(observed);
      results.push({ id: scenario.id, title: scenario.title, host, status: 'passed', detail: null });
    } catch (error) {
      results.push({
        id: scenario.id,
        title: scenario.title,
        host,
        status: 'failed',
        detail: error && error.message ? error.message : String(error),
      });
    }
  }
  return results;
}

export function loadObservations(path, readFile) {
  return JSON.parse(readFile(path, 'utf8'));
}
