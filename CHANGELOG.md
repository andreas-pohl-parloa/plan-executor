# Changelog

## [0.39.0] - 2026-04-28

### Bug Fixes

- Address Phase C+D+F2 code-review findings ([ed19911](https://github.com/andreas-pohl-parloa/plan-executor/commit/ed199112ed2f41dc1ecd3fa4284c8ecc143aa924))

### Features

- JobKind::PrFinalize step structs (Phase C1.1) ([4350f79](https://github.com/andreas-pohl-parloa/plan-executor/commit/4350f79250126f357ea28cdaeea44a092c38e9b9))
- JobMetrics struct + per-job metrics writer (Phase F2.1) ([4ff234f](https://github.com/andreas-pohl-parloa/plan-executor/commit/4ff234fde0df3860ac2f79770ee9d74de8b10b43))
- Plan-executor run pr-finalize CLI subcommand (Phase C1.2) ([026cf7a](https://github.com/andreas-pohl-parloa/plan-executor/commit/026cf7a2e916e4469bcb931e5d8ca8cf46943dc4))
- Invoke_helper Rust subroutine for structured helper-skill calls (Phase D2.1) ([c04a0b1](https://github.com/andreas-pohl-parloa/plan-executor/commit/c04a0b1c654e49297bc5e29d1c583e6c512bd0dc))
- Plan-executor jobs metrics CLI subcommand (Phase F2.2) ([49952db](https://github.com/andreas-pohl-parloa/plan-executor/commit/49952db1efc0bf70605491e0598821c316c15aeb))
- GHA workflow dispatches by job-spec kind (Phase C2.1) ([6f6dc5f](https://github.com/andreas-pohl-parloa/plan-executor/commit/6f6dc5fc7cf76e2a1de51b644aac244aa0b4dbae))
- Typed wrappers for invoke_helper (Phase D2.2) ([15b63c2](https://github.com/andreas-pohl-parloa/plan-executor/commit/15b63c202432b6df5b86fc1374f95623c3ceb59e))
- Rust wave-execution scheduler replaces orchestrator skill (Phase D3.1) ([05767ee](https://github.com/andreas-pohl-parloa/plan-executor/commit/05767ee68da106b7ec388e12774fa8a566acf816))
- Replace 5 plan-job step shells with real Rust impls (Phase D3.2+D3.3) ([19097cb](https://github.com/andreas-pohl-parloa/plan-executor/commit/19097cbf271e67516c16ca583a52ada4bbd53623))

### Testing

- Integration tests for JobKind::PrFinalize 5-step pipeline (Phase C1.3) ([e839aca](https://github.com/andreas-pohl-parloa/plan-executor/commit/e839aca91181a64863182e2fa2c9bacb8a602de4))
- Per-job metrics writer + jobs metrics CLI aggregation (Phase F2.3) ([1a7b798](https://github.com/andreas-pohl-parloa/plan-executor/commit/1a7b7983850bb1fd3c469298e6d9448e250de3d0))
- GHA workflow dispatcher logic + metadata validation (Phase C2.2) ([2e13111](https://github.com/andreas-pohl-parloa/plan-executor/commit/2e13111dd600f5dedd185a3c5ca233b0e791b473))
- Invoke_helper + typed wrappers (Phase D2.3) ([fba4dfc](https://github.com/andreas-pohl-parloa/plan-executor/commit/fba4dfc0cb4b1350d7c6b0c9cc24c9de2d4dff7d))
- End-to-end Rust scheduler runs without orchestrator skill (Phase D3.4) ([1baee75](https://github.com/andreas-pohl-parloa/plan-executor/commit/1baee75d44d0b67cc05fe21ee083599e17509ad2))
## [0.38.0] - 2026-04-28

### Bug Fixes

- Address Phase A+B code-review findings (H1-H4, M1-M3) ([331cb26](https://github.com/andreas-pohl-parloa/plan-executor/commit/331cb2611704efb1baa785ebb63b899f65e67eb0))

### Documentation

- Design doc for Job Framework + Protocol-Recovery Supervisor (Task 0) ([0a17fdf](https://github.com/andreas-pohl-parloa/plan-executor/commit/0a17fdfe32ed119d58169730ee9eb2415f263ffb))

### Features

- Introduce Job/Step/JobState core types (Phase A1.1) ([aa46fbe](https://github.com/andreas-pohl-parloa/plan-executor/commit/aa46fbe72e021c904a33065446ed6419732f5854))
- Introduce RecoveryPolicy and Backoff types (Phase A1.2) ([45fe822](https://github.com/andreas-pohl-parloa/plan-executor/commit/45fe82217cb7baa8a0e944011e84287ee0dfd4f3))
- Step trait + JobKind→steps registry with stub plan-step shells (Phase A2.1) ([40f597c](https://github.com/andreas-pohl-parloa/plan-executor/commit/40f597c8204cd07ce97f3ddd71364c306032e109))
- Per-step append-only job storage (Phase A3.1) ([be0f256](https://github.com/andreas-pohl-parloa/plan-executor/commit/be0f256e9930048072634ae0018bfbffff6cf67b))
- JobDir sub_agent_dir + JobStore::list_all back-compat (Phase A3.2 storage-side) ([1a5b73e](https://github.com/andreas-pohl-parloa/plan-executor/commit/1a5b73ef7df12f2e36c0a4fd139e76e1e793181d))
- Plan-executor jobs subcommand group (Phase A4.1) ([9af4a82](https://github.com/andreas-pohl-parloa/plan-executor/commit/9af4a827c82aec36d724345cb16d731b02741c9e))
- ProtocolViolation taxonomy, detector, and corrective-prompt catalog (Phase B1.1+B1.2) ([5cef6bb](https://github.com/andreas-pohl-parloa/plan-executor/commit/5cef6bbceb2cca348009c304de688c1af8f7678e))
- SupervisorState + observe_turn wiring layer (Phase B2.1 wiring-only) ([8c396b4](https://github.com/andreas-pohl-parloa/plan-executor/commit/8c396b413a14733d75793e3ca43bb591ce528d51))
- Rollback layer + ExhaustedNext routing in SupervisorAction (Phase B2.2) ([ba0bea3](https://github.com/andreas-pohl-parloa/plan-executor/commit/ba0bea3c3db63df6bb5d624574c0c7f09bf893cf))
## [0.37.0] - 2026-04-28

### Bug Fixes

- **cli:** Hard-fail compile-fix-waves on --plan vs manifest.plan.path mismatch (F5) ([42109fc](https://github.com/andreas-pohl-parloa/plan-executor/commit/42109fc684fb47b04fe62e4558a8231b0d342c2b))
- **compile:** Address review findings F1-F4,F6-F10,F12 in append_fix_waves ([28b84c8](https://github.com/andreas-pohl-parloa/plan-executor/commit/28b84c8c7a934bffab671dc7ed5b26377214fc9c))
- **cli:** Cap manifest preflight read at 16 MiB to match append_fix_waves (N2) ([b2d9285](https://github.com/andreas-pohl-parloa/plan-executor/commit/b2d92859526b69efdd0f8e77373e92fdd2bafe89))
- **compile:** Address attempt-2 review findings N1,N3,N4,SEC-9..SEC-11 ([adf0661](https://github.com/andreas-pohl-parloa/plan-executor/commit/adf066149cbc33e308df15f24c8145d34169d503))
- **compile:** Kill child on wait_timeout Err + correct SEC-11 cleanup doc ([f38bd2f](https://github.com/andreas-pohl-parloa/plan-executor/commit/f38bd2fe01d2850d91ffc8cc07af59d68834d068))

### Features

- Add Finding type mirroring findings.schema.json ([f2d44c1](https://github.com/andreas-pohl-parloa/plan-executor/commit/f2d44c1603c85ea66bf67466598635bf99641d93))
- Add append_fix_waves for code-review fix-loop ([29a2d92](https://github.com/andreas-pohl-parloa/plan-executor/commit/29a2d92348db250a9585f8edbc43d64b9b9bbcfe))
- Add plan-executor compile-fix-waves CLI subcommand ([ddd2033](https://github.com/andreas-pohl-parloa/plan-executor/commit/ddd20336c847e0827b59ef682d562430ab7e4825))
## [0.36.1] - 2026-04-27

### Refactoring

- Consume tasks.json directly via execute, drop inline compile + sha2 dep (#6) ([9d9fa4c](https://github.com/andreas-pohl-parloa/plan-executor/commit/9d9fa4cf9e0f3ba7d58267b8dbf990f309e8a490))
## [0.36.0] - 2026-04-27

### Features

- Add plan-executor validate CLI + wire compile-plan into execute (#5) ([5bfc0d1](https://github.com/andreas-pohl-parloa/plan-executor/commit/5bfc0d172861364e318d58f46cdabcc0ec0d87a0))
## [0.35.1] - 2026-04-24

### Miscellaneous

- Raise remote-execution job timeout from 2h to 4h ([f842fe0](https://github.com/andreas-pohl-parloa/plan-executor/commit/f842fe00c64d8e73adf13dc51c91dabe68b6b8f2))
## [0.35.0] - 2026-04-24

### Features

- Validate agent-type and kill on post-handoff tool_use ([e172e33](https://github.com/andreas-pohl-parloa/plan-executor/commit/e172e33b15bc9e38e263b86166a126f22a7faea1))
## [0.34.0] - 2026-04-24

### Features

- Frame sub-agent prompts with identity to prevent orchestrator recursion ([854c84c](https://github.com/andreas-pohl-parloa/plan-executor/commit/854c84c8401b712526fff231d47fda4d2627d1f6))
## [0.33.2] - 2026-04-19

### Bug Fixes

- Handle missing gpg_key gh scope with manual-paste fallback ([1adf195](https://github.com/andreas-pohl-parloa/plan-executor/commit/1adf195e5848dc7c1e656bc2791e0dade6571c45))
## [0.33.1] - 2026-04-19

### Bug Fixes

- Use real name in commit signing, not CI marker ([ed9dd2e](https://github.com/andreas-pohl-parloa/plan-executor/commit/ed9dd2eb94e21be56cfb932b3c09d805ae6a0de8))
## [0.33.0] - 2026-04-19

### Features

- Orchestrate commit-signing setup from remote-setup ([ea4f4b1](https://github.com/andreas-pohl-parloa/plan-executor/commit/ea4f4b14273e95443bb773d9f7ecf1c891980c11))
## [0.32.2] - 2026-04-19

### Bug Fixes

- Pipe resume continuation via stdin to avoid ARG_MAX ([7e090b2](https://github.com/andreas-pohl-parloa/plan-executor/commit/7e090b275f06d9787f1d83c77112ffbfa64128ba))
## [0.32.1] - 2026-04-19

### Bug Fixes

- Switch protocol-violation detection to 60s post-handoff timeout ([29ee140](https://github.com/andreas-pohl-parloa/plan-executor/commit/29ee14077995a960334ef408d5ad105329500109))
## [0.32.0] - 2026-04-19

### Features

- Stream sub-agent output live in foreground execute ([9de9b40](https://github.com/andreas-pohl-parloa/plan-executor/commit/9de9b40585d3d34d87d8878e582dff0ddef57c2c))
## [0.31.2] - 2026-04-19

### Bug Fixes

- Detect handoff-protocol violation and kill session ([6cc6025](https://github.com/andreas-pohl-parloa/plan-executor/commit/6cc602587006a17e18aab0e806cc1e41c3f79eca))
## [0.31.1] - 2026-04-19

### Bug Fixes

- Fall back to PR+auto-merge when main is branch-protected ([9965162](https://github.com/andreas-pohl-parloa/plan-executor/commit/996516211efbcc584f1641372ec69cf2682bea71))
## [0.31.0] - 2026-04-17

### Features

- Include sub-agent index in indent prefix ([77b8299](https://github.com/andreas-pohl-parloa/plan-executor/commit/77b82994de9f3ba71ab901d9103052cf233d1f43))
## [0.30.1] - 2026-04-17

### Bug Fixes

- Prepend hygiene block as a top-of-prompt blockquote ([b9e33d8](https://github.com/andreas-pohl-parloa/plan-executor/commit/b9e33d8edcc9e046784a9380d906ff13dd7cd649))
## [0.30.0] - 2026-04-17

### Features

- Daemon-inject subprocess-hygiene block into every sub-agent prompt ([1a9203f](https://github.com/andreas-pohl-parloa/plan-executor/commit/1a9203fb7e86e29ba9b85337b7e5e8b4adeb3359))
## [0.29.1] - 2026-04-17

### Bug Fixes

- Unblock main agent when sub-agent emits result but doesn't exit ([1145061](https://github.com/andreas-pohl-parloa/plan-executor/commit/1145061ef0dc4dda0b921f0aa7efadb64a4f70ab))
## [0.29.0] - 2026-04-17

### Features

- Stream sub-agent output live in `output -f` ([e5edb5c](https://github.com/andreas-pohl-parloa/plan-executor/commit/e5edb5c6d7d2814ddaeded6dba8a2e9bf7fca973))
## [0.28.0] - 2026-04-17

### Features

- Persist and render sub-agent JSONL inline in output command ([b1edadc](https://github.com/andreas-pohl-parloa/plan-executor/commit/b1edadc494e8f79787db6fb0e6f1f65b0234135d))
## [0.27.0] - 2026-04-17

### Features

- Show last activity column for running jobs ([178e327](https://github.com/andreas-pohl-parloa/plan-executor/commit/178e327b0117c3c7bc92826e9aac6a75885f65b7))
## [0.26.1] - 2026-04-17

### Bug Fixes

- Tree-walking kill reaches grand-children in foreign pgroups ([96b23b9](https://github.com/andreas-pohl-parloa/plan-executor/commit/96b23b947b3a3ea14e1186e6ffa65f714be88bc3))
## [0.26.0] - 2026-04-17

### Features

- Stream sub-agent stdout for real liveness, drop heartbeat ([4dc60a4](https://github.com/andreas-pohl-parloa/plan-executor/commit/4dc60a46de8ec986923060d270e2a60444781dfd))
## [0.25.0] - 2026-04-17

### Features

- Show dimmed process sub-tree under running jobs ([a7c81e8](https://github.com/andreas-pohl-parloa/plan-executor/commit/a7c81e8e0016f95ef001a2a66858ba4f1bafa36b))
## [0.24.0] - 2026-04-17

### Features

- Color running jobs yellow in jobs table ([4843248](https://github.com/andreas-pohl-parloa/plan-executor/commit/4843248e89c46255b1b293c83b3b9e0ec5f6af8b))
## [0.23.0] - 2026-04-17

### Features

- Format job durations with minutes/hours/days instead of raw seconds ([191e694](https://github.com/andreas-pohl-parloa/plan-executor/commit/191e694392b92741211207e700be6dfbed3e4773))
## [0.22.0] - 2026-04-17

### Features

- Show duration column for remote jobs ([cb82bfc](https://github.com/andreas-pohl-parloa/plan-executor/commit/cb82bfcea19866fd3c8645b6e5ac075b2a38fc47))
## [0.21.2] - 2026-04-17

### Bug Fixes

- Size remote jobs status column to widest value ([da6e81c](https://github.com/andreas-pohl-parloa/plan-executor/commit/da6e81ce5ce19c433ac0d1fc9631438f2c247bee))
## [0.21.1] - 2026-04-17

### Bug Fixes

- Track sub-agent pgids in real time so kill reaches in-flight dispatches ([cf6c69d](https://github.com/andreas-pohl-parloa/plan-executor/commit/cf6c69d12df03b71a62bdf986577567abe252298))
## [0.21.0] - 2026-04-17

### Features

- Add watchdog for hung local jobs ([13a699f](https://github.com/andreas-pohl-parloa/plan-executor/commit/13a699fe6d244558a3128c8ea65100aaa166c2b2))
## [0.20.2] - 2026-04-16

### Bug Fixes

- Add READY status check to foreground execution path ([e276650](https://github.com/andreas-pohl-parloa/plan-executor/commit/e27665010bebd0230ee59450ee05706950436447))
## [0.20.1] - 2026-04-16

### Bug Fixes

- Add READY status check in daemon trigger_execution path ([0bb948d](https://github.com/andreas-pohl-parloa/plan-executor/commit/0bb948de0491d8a59611690356d9b30307a408e8))
## [0.20.0] - 2026-04-16

### Features

- Fail fast on execute if plan status is not READY ([57f41c9](https://github.com/andreas-pohl-parloa/plan-executor/commit/57f41c99dc342a51a8194548598295d2623b5cd6))
## [0.19.20] - 2026-04-15

### Miscellaneous

- Remove leftover tmp execution files ([1f2b33f](https://github.com/andreas-pohl-parloa/plan-executor/commit/1f2b33fecd22bffafa973a05fe9c065e33d15f6c))
## [0.19.19] - 2026-04-15

### Bug Fixes

- Show PR URL instead of useless watch command for remote executions ([300e792](https://github.com/andreas-pohl-parloa/plan-executor/commit/300e7921864e3f70f686d128a682b34fcdcad98a))
## [0.19.18] - 2026-04-15

### Bug Fixes

- Support SSH config aliases in git remote URL parsing ([26f4182](https://github.com/andreas-pohl-parloa/plan-executor/commit/26f4182c3d92d9e274a77585e8d2bea59e15cd8a))
## [0.19.17] - 2026-04-12

### Bug Fixes

- Search entire working directory for execution summary file ([9afafef](https://github.com/andreas-pohl-parloa/plan-executor/commit/9afafef4766bd4e1eacd40a4f4007c32b067bdb4))
## [0.19.16] - 2026-04-12

### Bug Fixes

- Send error events to CLI when remote execution fails ([fb01224](https://github.com/andreas-pohl-parloa/plan-executor/commit/fb012244eab89ae8659065d3702032a1da2043d0))
## [0.19.15] - 2026-04-12

### Bug Fixes

- Use alerter for macOS notifications, remove Notifier.app/osascript ([be28b72](https://github.com/andreas-pohl-parloa/plan-executor/commit/be28b72b0934cff3c26cac661c930d0ed1bdf05d))
## [0.19.14] - 2026-04-12

### Bug Fixes

- Use display notification instead of NSUserNotification ([68b2fc3](https://github.com/andreas-pohl-parloa/plan-executor/commit/68b2fc3395bcc7cd88d020d339e4d9b8b5b07e02))
## [0.19.13] - 2026-04-12

### Bug Fixes

- Correct remote table gap count and status width ([29f1c9a](https://github.com/andreas-pohl-parloa/plan-executor/commit/29f1c9a04b19543e7fafce1aa3635e6c955ff36a))
## [0.19.12] - 2026-04-12

### Bug Fixes

- Pad remote job rows to full terminal width for consistency ([e2b6b33](https://github.com/andreas-pohl-parloa/plan-executor/commit/e2b6b330643a9d0a33ac7f36b3b261955aac53c7))
## [0.19.11] - 2026-04-12

### Bug Fixes

- Log notification attempts and capture osascript errors ([2e15b25](https://github.com/andreas-pohl-parloa/plan-executor/commit/2e15b2504aafbdeb5e5a582af3a66bd2034170c2))
## [0.19.10] - 2026-04-12

### Bug Fixes

- Default RUST_LOG to plan_executor=info for daemon logging ([cc6b3b2](https://github.com/andreas-pohl-parloa/plan-executor/commit/cc6b3b289e21e521759703e4aa5badd93d603806))
## [0.19.9] - 2026-04-12

### Bug Fixes

- Create labels before adding, search worktrees for execution summary ([53ee717](https://github.com/andreas-pohl-parloa/plan-executor/commit/53ee717c451217b2c4239172a5282165a42683f6))
## [0.19.8] - 2026-04-12

### Bug Fixes

- Size remote jobs columns from data, not terminal width ([cd29da1](https://github.com/andreas-pohl-parloa/plan-executor/commit/cd29da10ca06b436fe01ba20ccff37dffbbabc71))
## [0.19.7] - 2026-04-12

### Bug Fixes

- Prune old completed jobs on daemon startup (keep 50) ([0db94a4](https://github.com/andreas-pohl-parloa/plan-executor/commit/0db94a491edde4f72d967587129beed539bc7e95))
## [0.19.6] - 2026-04-12

### Bug Fixes

- Route remote executions through daemon for jobs/notifications ([dc28cc3](https://github.com/andreas-pohl-parloa/plan-executor/commit/dc28cc34c50430035c22ac0fc00665c8ecd5c8a2))
## [0.19.5] - 2026-04-12

### Bug Fixes

- Use current gh account as default owner in remote-setup ([f2ef78f](https://github.com/andreas-pohl-parloa/plan-executor/commit/f2ef78f604b0610f283361d063857009ba60607d))
## [0.19.4] - 2026-04-12

### Bug Fixes

- Avoid dirname warning when BASH_SOURCE is empty ([de460ac](https://github.com/andreas-pohl-parloa/plan-executor/commit/de460ac0fdf6c55fa0735cae7a0fd4df4450f9f9))
## [0.19.3] - 2026-04-12

### Bug Fixes

- Fall back to default config on parse errors instead of panicking ([21327b3](https://github.com/andreas-pohl-parloa/plan-executor/commit/21327b3dd4eb0841b623087e07c1aa1fde14383a))
## [0.19.2] - 2026-04-12

### Bug Fixes

- Silence dirname warning when $0 is empty under bash -c ([396d018](https://github.com/andreas-pohl-parloa/plan-executor/commit/396d01888ca711509ce691f7891d70ab7b4099e5))
## [0.19.1] - 2026-04-12

### Bug Fixes

- Support bash -c invocation and symlinked rc files in install.sh ([4ba1bc6](https://github.com/andreas-pohl-parloa/plan-executor/commit/4ba1bc602a7aa4549c4cc607d37f7c9c18fd827f))
## [0.19.0] - 2026-04-11

### Features

- Stream bash sub-agent output live instead of capturing silently ([14918f9](https://github.com/andreas-pohl-parloa/plan-executor/commit/14918f9a386a772a7848f6b42fd500baf9225478))
## [0.18.0] - 2026-04-11

### Features

- Embed notification icon in binary, use JXA for macOS notifications ([9dab646](https://github.com/andreas-pohl-parloa/plan-executor/commit/9dab646c96e10420136a65dc250b45a6a7e119d3))
## [0.17.2] - 2026-04-11

### Bug Fixes

- Use osascript/notify-send instead of notify-rust for notifications ([b24d117](https://github.com/andreas-pohl-parloa/plan-executor/commit/b24d117ba64e718d609adc16efb83441d14b99c2))
## [0.17.1] - 2026-04-11

### Documentation

- Update README to reflect current CLI and architecture ([36a52cc](https://github.com/andreas-pohl-parloa/plan-executor/commit/36a52cc894f88e5f84b70d65d994efba306369eb))
## [0.17.0] - 2026-04-11

### Features

- Restore desktop notifications for job start/finish ([a2041a9](https://github.com/andreas-pohl-parloa/plan-executor/commit/a2041a9bb28f4f87a32c70b78da853f932e85ff7))
## [0.16.0] - 2026-04-11

### Features

- Add bash agent type for shell script handoffs ([8b998f8](https://github.com/andreas-pohl-parloa/plan-executor/commit/8b998f83e40941d30c34211421aa54055f14cd24))
## [0.15.4] - 2026-04-11

### Bug Fixes

- Reset handoff_calls when new batch starts (index 1 reappears) ([9ca0441](https://github.com/andreas-pohl-parloa/plan-executor/commit/9ca0441f660e274d90befcbdc8580b3f1c430b82))
## [0.15.3] - 2026-04-11

### Refactoring

- Make output lines the primary handoff transport ([dfc112e](https://github.com/andreas-pohl-parloa/plan-executor/commit/dfc112e66146bd9c19cea13db47af2791f37315d))
## [0.15.2] - 2026-04-11

### Bug Fixes

- Inject handoffs from output lines instead of auto-detecting all prompt files ([f50f872](https://github.com/andreas-pohl-parloa/plan-executor/commit/f50f872e8974ffa5e40bd64b39d1a5515707bcea))
## [0.15.1] - 2026-04-11

### Bug Fixes

- Fall back to prompt-file auto-detection when state file lacks handoffs array ([0e6cbc1](https://github.com/andreas-pohl-parloa/plan-executor/commit/0e6cbc1d98830f7ea6be782eddb64f7ef1ce0bb8))
## [0.15.0] - 2026-04-11

### Features

- Retry execution when plan is still EXECUTING after agent returns success ([24ac40f](https://github.com/andreas-pohl-parloa/plan-executor/commit/24ac40fb8ded7d30902aa8f91cec05bc6bfaf972))
## [0.14.1] - 2026-04-11

### Bug Fixes

- Search git worktrees and sibling dirs for handoff state file ([dc02831](https://github.com/andreas-pohl-parloa/plan-executor/commit/dc028318ea3af8280f7cf8c0d99847e8ceea56bd))
## [0.14.0] - 2026-04-11

### Features

- Prompt for remote-setup during install if not configured ([7c45c8c](https://github.com/andreas-pohl-parloa/plan-executor/commit/7c45c8cac5ddb51adc7851f3d246f8682153dc82))
## [0.13.0] - 2026-04-11

### Features

- Generate execution repo README during remote-setup ([ab4e9b9](https://github.com/andreas-pohl-parloa/plan-executor/commit/ab4e9b94ab5eea1467ea3037cc432cb688f20896))
## [0.12.0] - 2026-04-11

### Features

- Use plan-executor-plugin marketplace instead of my-coding ([10da706](https://github.com/andreas-pohl-parloa/plan-executor/commit/10da706747e33fec33f3fe688e419a07442169ba))

### Miscellaneous

- Cleanup install output ([176dab4](https://github.com/andreas-pohl-parloa/plan-executor/commit/176dab43c970e3ca779d9c86a52fa01089e578b9))
## [0.11.1] - 2026-04-10

### Bug Fixes

- Compute TARGET width from data to avoid cutoff ([07ef6ca](https://github.com/andreas-pohl-parloa/plan-executor/commit/07ef6ca8432118f8031926fff42c90906d328b7f))
## [0.11.0] - 2026-04-10

### Features

- Scale jobs output to terminal width ([d60e6c0](https://github.com/andreas-pohl-parloa/plan-executor/commit/d60e6c00bbe79e8feb9fd6c22e4a96ddd18c44dd))
## [0.10.0] - 2026-04-10

### Features

- Post execution summary from .tmp-execution-summary.md to PR ([ac4d3a7](https://github.com/andreas-pohl-parloa/plan-executor/commit/ac4d3a73133e705b328847efe4a93f0ee6909506))
## [0.9.0] - 2026-04-10

### Features

- Include execution log in PR result comment ([da748f2](https://github.com/andreas-pohl-parloa/plan-executor/commit/da748f209da4c64a35a5bc5c2001b111888ea829))
## [0.8.1] - 2026-04-10

### Bug Fixes

- Parse current_phase as string or integer for phase display ([e9971ee](https://github.com/andreas-pohl-parloa/plan-executor/commit/e9971eeb7a740158ee20b3e972857fca151f0265))
## [0.8.0] - 2026-04-10

### Features

- Remove auto-execute, file watcher, TUI, and notifications ([05677d6](https://github.com/andreas-pohl-parloa/plan-executor/commit/05677d627619e1bb19715f2073102bc5c67b4f8a))
## [0.7.1] - 2026-04-10

### Bug Fixes

- Use stable default watch_dirs in config instead of SCRIPT_DIR ([524785d](https://github.com/andreas-pohl-parloa/plan-executor/commit/524785df79e24e431dfd51170f287a7784853ae1))
## [0.7.0] - 2026-04-10

### Features

- Track remote executions as proper jobs ([2b288c4](https://github.com/andreas-pohl-parloa/plan-executor/commit/2b288c41f1af0718c34c69bf837b042a54310f52))
## [0.6.9] - 2026-04-10

### Documentation

- Add note about my-coding plugin as primary install method ([b6d4952](https://github.com/andreas-pohl-parloa/plan-executor/commit/b6d49524266e56d93e403c6675e5a17eb022a077))
## [0.6.8] - 2026-04-10

### Documentation

- Add README with remote setup instructions ([4135ee1](https://github.com/andreas-pohl-parloa/plan-executor/commit/4135ee15f5ec99e321afd06a41a90435c44dcb06))
## [0.6.7] - 2026-04-10

### Bug Fixes

- Read plan headers from plan.md in execution repo root ([f246d57](https://github.com/andreas-pohl-parloa/plan-executor/commit/f246d577491b1e31466a8af635bddc9106245aff))
## [0.6.6] - 2026-04-10

### Bug Fixes

- Always invoke execute-plan-non-interactive skill ([df3744e](https://github.com/andreas-pohl-parloa/plan-executor/commit/df3744e655802832b157edcba0a7aba295afc8ca))
## [0.6.5] - 2026-04-10

### Bug Fixes

- Tell Claude to implement directly on CI, not invoke skills ([bd4109b](https://github.com/andreas-pohl-parloa/plan-executor/commit/bd4109b289e2c4ffaa245d6e210e5d9346a77d1e))
## [0.6.4] - 2026-04-10

### Bug Fixes

- Pass plan content directly to Claude on remote runner ([bda3e60](https://github.com/andreas-pohl-parloa/plan-executor/commit/bda3e60b8c21914c3f74dbbc39b5b4a0a21825ca))
## [0.6.3] - 2026-04-10

### Bug Fixes

- Remove local keyword from case block in install.sh ([5b5b620](https://github.com/andreas-pohl-parloa/plan-executor/commit/5b5b620f77a766ab51458f01ded2fe1f795835c7))
## [0.6.2] - 2026-04-10

### Bug Fixes

- Copy cargo-built binary to target install dir ([939396b](https://github.com/andreas-pohl-parloa/plan-executor/commit/939396bbcd1b590e30c0fb59df35921e3a2631c4))
## [0.6.1] - 2026-04-10

### Bug Fixes

- Always install my-coding marketplace and plan-executor plugin ([86705ff](https://github.com/andreas-pohl-parloa/plan-executor/commit/86705ff3af38655532cfff3177cddf1eeb792cd8))
## [0.6.0] - 2026-04-10

### Features

- Read marketplaces and plugins from plan headers ([5da1b58](https://github.com/andreas-pohl-parloa/plan-executor/commit/5da1b581bc7d7474b17fc71f6087657a6d470081))
## [0.5.3] - 2026-04-10

### Bug Fixes

- Install binary to ~/bin or ~/.local/bin instead of ~/.cargo/bin ([85be81a](https://github.com/andreas-pohl-parloa/plan-executor/commit/85be81a4d3e69f5958451a0babcdafedfa6a5f62))
## [0.5.2] - 2026-04-10

### Bug Fixes

- Use TARGET_REPO_TOKEN everywhere, drop GH_PAT dependency ([f2e6c0f](https://github.com/andreas-pohl-parloa/plan-executor/commit/f2e6c0fd5799202c19134cb247ba5cda6e1af245))
## [0.5.1] - 2026-04-10

### Bug Fixes

- Pass TARGET_REPO_TOKEN as GH_TOKEN to execute step ([459d5d1](https://github.com/andreas-pohl-parloa/plan-executor/commit/459d5d13274a392d353c0aeb5237b6f6866e179b))
## [0.5.0] - 2026-04-10

### Features

- Install binaries directly and add my-coding as Claude plugin ([9bbd05f](https://github.com/andreas-pohl-parloa/plan-executor/commit/9bbd05f51e710174ca3dd8fd292f4ad065e1b8df))
## [0.4.12] - 2026-04-10

### Bug Fixes

- Find latest release with Linux asset before downloading ([616ce60](https://github.com/andreas-pohl-parloa/plan-executor/commit/616ce60dddeed00fa1c808369c881bb06ffd9843))
## [0.4.11] - 2026-04-10

### Bug Fixes

- Use GH_PAT for plan-executor binary download ([7faf8ac](https://github.com/andreas-pohl-parloa/plan-executor/commit/7faf8acc0175b745f25cee15236e7050fc78d220))
## [0.4.10] - 2026-04-10

### Bug Fixes

- Download plan-executor binary directly in workflow ([4e36cbf](https://github.com/andreas-pohl-parloa/plan-executor/commit/4e36cbfa23fe7c8d6306160c6d8137e94024f851))
## [0.4.9] - 2026-04-10

### Bug Fixes

- Remove environment protection (requires paid plan) ([8d55bb2](https://github.com/andreas-pohl-parloa/plan-executor/commit/8d55bb2391ad4f9ec653187e26d1db979d6a4cfd))
## [0.4.8] - 2026-04-10

### Bug Fixes

- Download binary from any release with matching asset ([ddf490c](https://github.com/andreas-pohl-parloa/plan-executor/commit/ddf490cd675e95f3b50fcab2b27e4ef786c19bd3))
## [0.4.7] - 2026-04-10

### Bug Fixes

- Correct Gemini CLI package name to @google/gemini-cli ([7dab7ef](https://github.com/andreas-pohl-parloa/plan-executor/commit/7dab7efc748b2960078f129a126f2819c9dd5115))
## [0.4.6] - 2026-04-09

### Bug Fixes

- Prevent recursive remote execution on GitHub Actions runner ([dfc1272](https://github.com/andreas-pohl-parloa/plan-executor/commit/dfc1272679b11e074ae984457951158fcc8e9bfc))
## [0.4.5] - 2026-04-09

### Bug Fixes

- Add libdbus-1-dev to workflow for source compilation fallback ([798955c](https://github.com/andreas-pohl-parloa/plan-executor/commit/798955c6e974a79ecb66cec6e90a609b79eaf014))
## [0.4.4] - 2026-04-09

### Bug Fixes

- Configure git credentials for private repo clones in workflow ([e4bee36](https://github.com/andreas-pohl-parloa/plan-executor/commit/e4bee3694cceb9ebc99d45c60710ecd8c5e2a403))
## [0.4.3] - 2026-04-09

### Bug Fixes

- Use gh api for private repo install script and add PATH setup ([aa9ec02](https://github.com/andreas-pohl-parloa/plan-executor/commit/aa9ec027bc385c54af3bdb3bf4be28c2c1a8a51a))
- Move Node.js and agent CLI installs before my-coding plugin ([814a88b](https://github.com/andreas-pohl-parloa/plan-executor/commit/814a88b5473ce75ad4fb929be4283a7b5630a608))
## [0.4.2] - 2026-04-09

### Bug Fixes

- Correct plan filename validation regex in workflow ([336119f](https://github.com/andreas-pohl-parloa/plan-executor/commit/336119f65ed8f63515510d37eed211aff4b87740))
- Update plan status and PR number from CLI trigger_remote path ([eaa2b7d](https://github.com/andreas-pohl-parloa/plan-executor/commit/eaa2b7d067f478466881bfb89a2c064758354757))
## [0.4.1] - 2026-04-09

### Miscellaneous

- Update Cargo.lock and stream-json-view submodule ([8ae5653](https://github.com/andreas-pohl-parloa/plan-executor/commit/8ae56536bfb249f25747739f0cd022c82f528d06))
## [0.4.0] - 2026-04-09

### Features

- Track remote execution lifecycle via plan headers ([fc958c9](https://github.com/andreas-pohl-parloa/plan-executor/commit/fc958c9434dd4cb69af813118baa47abc2de49f8))
## [0.3.4] - 2026-04-09

### Bug Fixes

- Make plan header parsing case-insensitive ([ca532ec](https://github.com/andreas-pohl-parloa/plan-executor/commit/ca532ecb230a531009b4b65ec5b292ad694356ba))
## [0.3.3] - 2026-04-09

### Bug Fixes

- Add 'enter to skip' hint to all remote-setup prompts ([7c93507](https://github.com/andreas-pohl-parloa/plan-executor/commit/7c935070a1495e684015a3857a988b86a553d8dc))
## [0.3.2] - 2026-04-09

### Bug Fixes

- Push workflow via git clone+push instead of Contents API ([094b607](https://github.com/andreas-pohl-parloa/plan-executor/commit/094b6073bbf5c68965d4a8993b8a9c399f628654))
## [0.3.1] - 2026-04-09

### Bug Fixes

- Push workflow via JSON stdin to avoid arg length limits ([e495b2f](https://github.com/andreas-pohl-parloa/plan-executor/commit/e495b2f6540807be85081adc39b1a61209f66cd4))
## [0.3.0] - 2026-04-09

### Features

- Auto-create execution repo and environment during remote-setup ([a4844b4](https://github.com/andreas-pohl-parloa/plan-executor/commit/a4844b4b1b5c0cb77760e41dd348bdfa763bbbcd))
## [0.2.1] - 2026-04-09

### Bug Fixes

- Use my-coding plugin installer for plan-executor + sjv in workflow ([d8ed2c2](https://github.com/andreas-pohl-parloa/plan-executor/commit/d8ed2c269f997bab272eff60f94d6665e60fc778))
## [0.2.0] - 2026-04-09

### Features

- Push workflow to execution repo during remote-setup ([bcc3653](https://github.com/andreas-pohl-parloa/plan-executor/commit/bcc3653ee3ca5ede147838790880ee6ac54de03a))
## [0.1.19] - 2026-04-08

### Bug Fixes

- **ci:** Remove platform-specific deps section to fix Linux builds ([7c911e8](https://github.com/andreas-pohl-parloa/plan-executor/commit/7c911e87dd8ebef217cea7c5f6d280c112e13280))
## [0.1.18] - 2026-04-08

### Bug Fixes

- **ci:** Use ubuntu-22.04 for Linux build, revert sed lock patching ([f69802c](https://github.com/andreas-pohl-parloa/plan-executor/commit/f69802c7233bb7b287b0ddc21470946216b9220a))
- Regenerate Cargo.lock after sed corruption ([c7892a2](https://github.com/andreas-pohl-parloa/plan-executor/commit/c7892a2abdb31cb0f85718c97f95d7964735e230))
## [0.1.17] - 2026-04-08

### Bug Fixes

- **ci:** Use sed to patch Cargo.lock version instead of cargo update ([c48bac1](https://github.com/andreas-pohl-parloa/plan-executor/commit/c48bac18da108110a973dae89ebe14081ce61e35))
## [0.1.16] - 2026-04-08

### Bug Fixes

- **ci:** Add verbose build output for Linux debugging ([2108fb1](https://github.com/andreas-pohl-parloa/plan-executor/commit/2108fb1d946208dd220cfae1e89bfe683fa9780e))
## [0.1.15] - 2026-04-08

### Bug Fixes

- **ci:** Simplify Linux build — native cargo + diagnostics ([1a68725](https://github.com/andreas-pohl-parloa/plan-executor/commit/1a68725178a03597c9dbf8993439888121e6a01e))
## [0.1.14] - 2026-04-08

### Miscellaneous

- Update stream-json-view submodule (edition 2021) ([7ec38da](https://github.com/andreas-pohl-parloa/plan-executor/commit/7ec38da013655f12bcc9f561759bb9632c200fc1))
## [0.1.13] - 2026-04-08

### Bug Fixes

- **ci:** Downgrade to edition 2021 to fix Linux builds ([9d9ff3b](https://github.com/andreas-pohl-parloa/plan-executor/commit/9d9ff3b88c34c8567d1e209483c704f30c028f34))
## [0.1.12] - 2026-04-08

### Bug Fixes

- **ci:** Remove lib.rs to fix edition 2024 Linux build failure ([2315931](https://github.com/andreas-pohl-parloa/plan-executor/commit/2315931dc60215260ca46ede95c30fb1927d65f0))
## [0.1.11] - 2026-04-08

### Bug Fixes

- **ci:** Add libdbus-1-dev to Cross.toml for x86_64 target ([4ccc29a](https://github.com/andreas-pohl-parloa/plan-executor/commit/4ccc29a5057f6486630f1217c3921113ae1f338b))
## [0.1.10] - 2026-04-08

### Bug Fixes

- **ci:** Use cross for Linux x86_64 build to bypass cargo bug ([7966e3c](https://github.com/andreas-pohl-parloa/plan-executor/commit/7966e3cc589b029a778f313a0ca1a6684eee8e46))
## [0.1.9] - 2026-04-08

### Bug Fixes

- **ci:** Use explicit --target on Linux x86_64 build ([0370920](https://github.com/andreas-pohl-parloa/plan-executor/commit/03709206d22a48ce57b6f53a60e6fd773dbf7212))
## [0.1.8] - 2026-04-08

### Bug Fixes

- **ci:** Add explicit [lib] section to fix Linux dep resolution ([3c07f7b](https://github.com/andreas-pohl-parloa/plan-executor/commit/3c07f7bd9b58bbedf0d6a5c1aec5fcef4bb42d72))
## [0.1.7] - 2026-04-08

### Bug Fixes

- **ci:** Build --bin target explicitly to work around resolver v3 bug ([b10e9fe](https://github.com/andreas-pohl-parloa/plan-executor/commit/b10e9fe237113a69ca95ab8f3f4b5ba19899efd5))
## [0.1.6] - 2026-04-08

### Bug Fixes

- **ci:** Force resolver v2 to fix Linux dependency resolution ([288be1d](https://github.com/andreas-pohl-parloa/plan-executor/commit/288be1d675e35affed6bfa598ca253f9951ae6d9))
## [0.1.5] - 2026-04-08

### Bug Fixes

- **ci:** Pin notify-rust to 4.12.0 for Linux build compatibility ([a55176e](https://github.com/andreas-pohl-parloa/plan-executor/commit/a55176e2950a121cb44115a6b37c3b1090ccea6c))
## [0.1.4] - 2026-04-08

### Bug Fixes

- **ci:** Use targeted cargo update to preserve dep versions ([1aefff2](https://github.com/andreas-pohl-parloa/plan-executor/commit/1aefff2e28440b7c936d2532ef61ba89f1841966))
## [0.1.3] - 2026-04-08

### Bug Fixes

- **ci:** Use generate-lockfile and --locked for reproducible builds ([cf04434](https://github.com/andreas-pohl-parloa/plan-executor/commit/cf044348882f31619efc606d49d161d92f2c0f0e))
## [0.1.2] - 2026-04-08

### Bug Fixes

- Avoid anyhow::Context ambiguity in cross-compilation ([91efe12](https://github.com/andreas-pohl-parloa/plan-executor/commit/91efe12680a149b99c5e124f289b9ec322867506))
## [0.1.1] - 2026-04-08

### Bug Fixes

- Accept absolute prompt_file paths in handoff state ([5f613ed](https://github.com/andreas-pohl-parloa/plan-executor/commit/5f613ed237342566b71320e54f56339ec6e7d352))
## [0.1.0] - 2026-04-08

### Bug Fixes

- Write PID file on every daemon start ([5c58104](https://github.com/andreas-pohl-parloa/plan-executor/commit/5c5810445ad3b79efc661320f78cd233bc3a12bf))
- Handle actual state file schema in load_state ([ecc6e2d](https://github.com/andreas-pohl-parloa/plan-executor/commit/ecc6e2d282b7372afa0db6e930ec585182de8433))
- Use @file syntax and correct flags for claude sub-agent dispatch ([fa08cc6](https://github.com/andreas-pohl-parloa/plan-executor/commit/fa08cc697f7bfaf4303051028d4a3e0e465e55ec))
- Remove --verbose --output-format stream-json from sub-agent dispatch ([b1a839d](https://github.com/andreas-pohl-parloa/plan-executor/commit/b1a839d2913836a8c13d8d711e8acff0685dd6e3))
- Remove @ prefix from sub-agent prompt file path ([9f3c031](https://github.com/andreas-pohl-parloa/plan-executor/commit/9f3c03147ea7d2cbb223aeb2bac94ee9404ff2df))
- Mark incomplete execute jobs as Failed; seed config on install ([4f14d1a](https://github.com/andreas-pohl-parloa/plan-executor/commit/4f14d1a1848b531812474450d48c6b133fcbaef3))
- Replace launchctl unload/load with kickstart to avoid killing terminal ([093ff95](https://github.com/andreas-pohl-parloa/plan-executor/commit/093ff95569d11957bb9f8205d46341c2e1c69dd3))
- Stop daemon via PID kill only during install, skip launchctl entirely ([8594623](https://github.com/andreas-pohl-parloa/plan-executor/commit/859462336697b4f60d5a6c5601aca41870f6671d))
- Verify PID belongs to plan-executor before killing during install ([c037e67](https://github.com/andreas-pohl-parloa/plan-executor/commit/c037e6728163e780019f12d2fa0a1795f1ddc9bb))
- Replace launchctl unload/load with kickstart to avoid killing terminal ([1410b22](https://github.com/andreas-pohl-parloa/plan-executor/commit/1410b2234cc2adf5364139f8ef7109e5345b39dc))
- Drop launchd entirely, use binary-managed daemon like claude-code-proxy ([f067175](https://github.com/andreas-pohl-parloa/plan-executor/commit/f067175e73a810e74f6e0d3b6b99b8dd749669b3))
- Remove plan from pending when status changes away from READY ([6122e5c](https://github.com/andreas-pohl-parloa/plan-executor/commit/6122e5ca30c8a62e26e74380f3784bd5922b1df0))
- Recursive plan scan with walkdir, background startup, default pattern ([427f9a2](https://github.com/andreas-pohl-parloa/plan-executor/commit/427f9a237f157f40b757b39dc979edbdc3238a9c))
- Install.sh exits silently after uninstall due to set -e + && chain ([3c9c19f](https://github.com/andreas-pohl-parloa/plan-executor/commit/3c9c19f08500629716631ac19c35c6f94104a4a1))
- Use render_stateful_widget so selection highlight actually works ([83c2385](https://github.com/andreas-pohl-parloa/plan-executor/commit/83c23851a8c05b501b59cc657fef83c6170d99ba))
- Remove bg colors; gray for unselected rows, bold white for selected ([4158c28](https://github.com/andreas-pohl-parloa/plan-executor/commit/4158c288c934b2631c0ff5868c030dde316dd1e3))
- Yellow bold for selected plan title ([e0f9b26](https://github.com/andreas-pohl-parloa/plan-executor/commit/e0f9b26c4b65ecf5bf8471b83adf39c646ebf202))
- Path line always dark gray; yellow bold applied per-span on title only ([55ccc25](https://github.com/andreas-pohl-parloa/plan-executor/commit/55ccc25d5191267dbf53c1a28d502704faebaa7d))
- Show notification icon on left only (app_icon); remove content_image ([21a37f0](https://github.com/andreas-pohl-parloa/plan-executor/commit/21a37f08324c560acada783c524f2d0135637a53))
- Use key:action format in help bar ([b6c69e3](https://github.com/andreas-pohl-parloa/plan-executor/commit/b6c69e35856cde9ec541b5e32ad07016a7b6c83f))
- Add space after colon in help bar ([55ec791](https://github.com/andreas-pohl-parloa/plan-executor/commit/55ec79186b8970cabda5b9b939f05eb3ea562da4))
- Move selection after cancel; clamp selected on state update ([92905de](https://github.com/andreas-pohl-parloa/plan-executor/commit/92905de7eac87a2adfd2d77f3d9c23ea6f99e2f3))
- Clamp Down/j to list length ([6a53155](https://github.com/andreas-pohl-parloa/plan-executor/commit/6a53155df411e7326801aa70026806e64e049523))
- Kill/pause/resume offset by n_pending; autoscroll border fix; colors ([0f3cc62](https://github.com/andreas-pohl-parloa/plan-executor/commit/0f3cc628220a034c24b7dbc1fe1e7e3c2dcaff82))
- Split sjv multi-line output into individual lines for correct colorization ([5afb0c2](https://github.com/andreas-pohl-parloa/plan-executor/commit/5afb0c24c95799bd216d63039cbd93df437f5db9))
- Clamp PageUp scroll to line count ([c0ebb52](https://github.com/andreas-pohl-parloa/plan-executor/commit/c0ebb52f5110b05da0dd242bcc007090956ddfce))
- Canonicalize --config path before daemonize() changes CWD to / ([44f8baa](https://github.com/andreas-pohl-parloa/plan-executor/commit/44f8baa810b2539d272edeb8cedba7c8699f999e))
- Kill existing daemon before daemonize to avoid PID file lock conflict ([58a5860](https://github.com/andreas-pohl-parloa/plan-executor/commit/58a586056b16d3d69ab15afa05d495c589cb9b85))
- Remove pid_file() from daemonize to avoid lock conflict on restart ([35dea1e](https://github.com/andreas-pohl-parloa/plan-executor/commit/35dea1e270f8e0f8315adb3b1e45aa83c0b2efce))
- Kill all plan-executor daemon processes on startup, not just PID file entry ([672661f](https://github.com/andreas-pohl-parloa/plan-executor/commit/672661fbd21bc829552d63612a9aab53bd059dc3))
- Config-handoff.json should watch mock dir, not workspace/code ([3f0c5e5](https://github.com/andreas-pohl-parloa/plan-executor/commit/3f0c5e5b4040db508dc654e044192ec75a17d99d))
- Resolve relative agent command paths against config file directory ([98b66d5](https://github.com/andreas-pohl-parloa/plan-executor/commit/98b66d554778c94f5b1d87a10bdd3c3245d8dc21))
- Delete state file after sub-agent dispatch to prevent HandoffRequired loop ([c10680d](https://github.com/andreas-pohl-parloa/plan-executor/commit/c10680d156717bb243974a3729afaba6350653c0))
- Replace heredocs with printf in mock to avoid stdout pipe holding ([5e8a51f](https://github.com/andreas-pohl-parloa/plan-executor/commit/5e8a51f8d4ab0f3427dd0a3912b28748efeca97a))
- Canonicalize plan path; show actual agent command in execute header ([4a0454c](https://github.com/andreas-pohl-parloa/plan-executor/commit/4a0454c286da973097a8427fcf0f81c16044310f))
- Colour only the prefix green, not the whole line ([dad0310](https://github.com/andreas-pohl-parloa/plan-executor/commit/dad03101eb859f47db44d5f51bfff33033b0b416))
- Save finished job to disk in daemon Finished handler ([c9247ad](https://github.com/andreas-pohl-parloa/plan-executor/commit/c9247ad726399993d31202c3b4e969558e3aa04d))
- Remove unused mut warning ([27de15e](https://github.com/andreas-pohl-parloa/plan-executor/commit/27de15e66445ae4a4bb7949ce4f4b0139195a366))
- Resume_execution writes output and display lines like initial turn ([f75cf5c](https://github.com/andreas-pohl-parloa/plan-executor/commit/f75cf5c904d020d8b00f0694263bc3195b68ccf7))
- Suppress unused variable warnings in execute_via_daemon ([c49e48a](https://github.com/andreas-pohl-parloa/plan-executor/commit/c49e48a0f219fa4c7e7cbcc66195a6288df5b1ec))
- Restore variable names broken by sed replacement ([1d7c818](https://github.com/andreas-pohl-parloa/plan-executor/commit/1d7c818e6a20b02ffbe7bb66e62419d29ed35b15))
- Remove unused vars in execute_via_daemon ([752cf3e](https://github.com/andreas-pohl-parloa/plan-executor/commit/752cf3e668def24cf222fb3ceaecb5b8a164ceea))
- Remove unused Path import ([ee45a10](https://github.com/andreas-pohl-parloa/plan-executor/commit/ee45a10ab95ea5d0a102a0b30d7243b228f62cc2))
- No CLI command falls back to disk; all require daemon ([e1b627d](https://github.com/andreas-pohl-parloa/plan-executor/commit/e1b627d8aa2e0e9ae79a26a06f0505b4d0a388a2))
- Jobs command shows pending (READY) plans from daemon state ([3e0bfd2](https://github.com/andreas-pohl-parloa/plan-executor/commit/3e0bfd25f76c46a4c909c1832d14d53dafca0a26))
- Capture duration_ms and token counts from resume result event ([a23e603](https://github.com/andreas-pohl-parloa/plan-executor/commit/a23e603f93918997864e4e7eb529fe455f1e9615))
- Resume job failure detection ([c1dc9fe](https://github.com/andreas-pohl-parloa/plan-executor/commit/c1dc9fe8deebe32df8ad3a4a01b7a958b9f11f19))
- Fail job when any sub-agent fails rather than continuing to resume ([ce20d03](https://github.com/andreas-pohl-parloa/plan-executor/commit/ce20d03055d126ddc38fcef9a692e5ebd5517335))
- Right-align cost to same column as duration; fix padding off-by-one ([af5e9f3](https://github.com/andreas-pohl-parloa/plan-executor/commit/af5e9f39db49187716012f4e97a71ad56cf21b7b))
- Address review findings F1-F8 ([b70ef7e](https://github.com/andreas-pohl-parloa/plan-executor/commit/b70ef7ee2fc0363bdceaf9c06af7363dd500773d))
- Correct workflow trigger and add plan-executor install step ([cf3e9ac](https://github.com/andreas-pohl-parloa/plan-executor/commit/cf3e9acb856e6fc56625131b2e2bafbfdd5c102c))
- Allow remote plan execution without running daemon ([34c015e](https://github.com/andreas-pohl-parloa/plan-executor/commit/34c015e62a2cca289b79f3eb0e2910ee040d77b2))
- Gather git context from plan repo root, not CWD ([42c7b93](https://github.com/andreas-pohl-parloa/plan-executor/commit/42c7b9308197823bba6b4cbff0d808ac6b4c25aa))
- Persist session_id on running job before handoff dispatch ([a900f24](https://github.com/andreas-pohl-parloa/plan-executor/commit/a900f24fe71cec89d1764fb6a185f99b642825f2))
- Make retry detach immediately like execute ([b169c64](https://github.com/andreas-pohl-parloa/plan-executor/commit/b169c64617160cfe289622874e66c3d141c1775c))
- Preserve orchestrator state file across handoff dispatch ([0d9f7e1](https://github.com/andreas-pohl-parloa/plan-executor/commit/0d9f7e14df8eb3323357892b2d445ac952fd6a82))
- **security:** Address 10 findings from security review ([fcc54fa](https://github.com/andreas-pohl-parloa/plan-executor/commit/fcc54fab30d3cce418e86174a01e24f3db3bfc80))
- **ci:** Use GH_PAT for private submodule checkout in release workflow ([22d006e](https://github.com/andreas-pohl-parloa/plan-executor/commit/22d006e35c635a30c278df295c785250fd85a48e))

### CI/CD

- Auto-release on push to main via git-cliff version detection ([a15793c](https://github.com/andreas-pohl-parloa/plan-executor/commit/a15793c681254a994a1621ef17cf642b154c8e36))

### Features

- **VC-0:** Initial implementation of plan-executor daemon and TUI ([352d5aa](https://github.com/andreas-pohl-parloa/plan-executor/commit/352d5aac8bfedb62ecd8136f68d8b5cafb5dd193))
- Daemonize the daemon command on startup ([bce7af1](https://github.com/andreas-pohl-parloa/plan-executor/commit/bce7af14a99706859ddee4b901e5ebccb809a817))
- Add install.sh for launchd auto-start on login ([414c80b](https://github.com/andreas-pohl-parloa/plan-executor/commit/414c80b926ad3bd59be89bc91dfc7fa6212610c6))
- Add uninstall action to install.sh ([1460dd4](https://github.com/andreas-pohl-parloa/plan-executor/commit/1460dd48b54705621c0c5764708bab70ed7db9ba))
- Add stop and start actions to install.sh ([561c345](https://github.com/andreas-pohl-parloa/plan-executor/commit/561c345ed5af140719f1cae0e1c15e7e37399120))
- Add stop subcommand to binary ([bb727dd](https://github.com/andreas-pohl-parloa/plan-executor/commit/bb727dd430e0f355d535dd1d8afef48a001d4252))
- Add execute subcommand for direct plan execution ([fcdcc5b](https://github.com/andreas-pohl-parloa/plan-executor/commit/fcdcc5b1b4d6d150547599d8c6807146f42cee27))
- Pipe execute output through sjv for rendering ([61d180a](https://github.com/andreas-pohl-parloa/plan-executor/commit/61d180ab2e29dd0d3b4721c33f891ccf47d42c86))
- Show plan path in gray second row; add r key to reload state ([1d75549](https://github.com/andreas-pohl-parloa/plan-executor/commit/1d75549a4434392b2ebd8939c9e1889db7055c0f))
- Configurable agent commands, --config flag, mock scripts ([e177ee9](https://github.com/andreas-pohl-parloa/plan-executor/commit/e177ee90cc6d06cb1ba084328520a5980aa71d56))
- Add restart action to install.sh ([dcf0ee9](https://github.com/andreas-pohl-parloa/plan-executor/commit/dcf0ee918896361bde66f0ccf660471586c8a69c))
- Restart action now rebuilds before restarting ([9c6cfb1](https://github.com/andreas-pohl-parloa/plan-executor/commit/9c6cfb1fc96867fea546bbbf49a01745319e29dd))
- Show repo-relative path in TUI instead of full absolute path ([1818b9c](https://github.com/andreas-pohl-parloa/plan-executor/commit/1818b9c3a0163d611f142378c51417671eca1183))
- Use custom icon for macOS notifications ([86dd0fc](https://github.com/andreas-pohl-parloa/plan-executor/commit/86dd0fc1d116eb82d13f6c03100dd110bcd83060))
- Show repo name in plan path; flag worktrees with [wt] ([8363a40](https://github.com/andreas-pohl-parloa/plan-executor/commit/8363a403dc4630c8ff35958cf658de0b1c296593))
- Pause/resume jobs and kill key bindings in TUI ([6fbe9b0](https://github.com/andreas-pohl-parloa/plan-executor/commit/6fbe9b0cbd93c1c3f87a3aa065e3436bc528ca46))
- Remove execute hint from list; add key bindings bar at bottom ([3037cce](https://github.com/andreas-pohl-parloa/plan-executor/commit/3037cce52d30fdd4cd92caa9b544f2a649f1284c))
- Cancel writes CANCELLED status to plan file to prevent re-detection ([48ecfd9](https://github.com/andreas-pohl-parloa/plan-executor/commit/48ecfd9ed977facb1f979fa68a7668a2a400404a))
- Add status column to running/pending and history lists ([adcb240](https://github.com/andreas-pohl-parloa/plan-executor/commit/adcb240a73c6d174c4ed7dfe791c3d515e9a57c4))
- Enter executes selected pending plan ([28f355b](https://github.com/andreas-pohl-parloa/plan-executor/commit/28f355b662cf01be17cee9bfe6218838a8c3d11d))
- Load history job output from disk; elapsed time as separate span ([f3c62c2](https://github.com/andreas-pohl-parloa/plan-executor/commit/f3c62c220d78f1d5acadea2265a6c827c038c646))
- Sjv colors in output pane via minimal ANSI parser ([5563dc3](https://github.com/andreas-pohl-parloa/plan-executor/commit/5563dc3b8f3b553b2cda791d40b525f853b39bec))
- Update stream-json-view submodule before building ([be13178](https://github.com/andreas-pohl-parloa/plan-executor/commit/be1317800ad69cb3fd50fad000fc74ff3baa2a40))
- Right-align elapsed time in running jobs list ([3708d35](https://github.com/andreas-pohl-parloa/plan-executor/commit/3708d3553b010f670891af13a37e9b3c544f7d0c))
- Kill process group; detect dead processes; restart from history; mm:ss elapsed ([c575a56](https://github.com/andreas-pohl-parloa/plan-executor/commit/c575a569efd0d771e655a17556b3dabd16d427f3))
- Right-align duration on line 1, cost on line 2 in history ([6a09dac](https://github.com/andreas-pohl-parloa/plan-executor/commit/6a09dac6b6faf3b2f84afabd5bf559510f99178a))
- Show handoff dispatch/resume status in TUI output pane ([f1df153](https://github.com/andreas-pohl-parloa/plan-executor/commit/f1df153a93c71c5cdf90ace8ec25118a0ad53f1b))
- TUI auto-reconnects when daemon restarts ([7f1214d](https://github.com/andreas-pohl-parloa/plan-executor/commit/7f1214d48b1fb87b1dcdb9bdd77d0f54f80e6504))
- Colour ⏺ bullet prefix green in output pane ([fd12d1f](https://github.com/andreas-pohl-parloa/plan-executor/commit/fd12d1f3141cdf5c8b074d5805c731426bc92c15))
- Execute delegates to daemon when running, same code path as TUI ([ec930cf](https://github.com/andreas-pohl-parloa/plan-executor/commit/ec930cf180de4063dd9ab2cbef201b5b3bc0757d))
- Remove standalone execute; add kill/pause/unpause CLI commands ([c6e8126](https://github.com/andreas-pohl-parloa/plan-executor/commit/c6e8126b33ffa439d8b98c30ebbbcd0e7c162a19))
- Jobs command queries daemon for live state including running jobs ([9a07718](https://github.com/andreas-pohl-parloa/plan-executor/commit/9a077188d686a868b9b42022526431e003056f98))
- Execute accepts job ID prefix to re-run a historical job ([7b7288c](https://github.com/andreas-pohl-parloa/plan-executor/commit/7b7288cfe58d595c640ec56942ef0e1776872892))
- Execute resolves pending plans by filename prefix, like Enter in TUI ([05e6410](https://github.com/andreas-pohl-parloa/plan-executor/commit/05e6410784883598f2dfec276831fab56f947dc1))
- Add output command with -f follow mode ([9d4c5e8](https://github.com/andreas-pohl-parloa/plan-executor/commit/9d4c5e887dc632155652d28f29b1f2b2be2013a7))
- ⏺ [plan-executor] prefix (yellow); display.log for output CLI ([7ca567a](https://github.com/andreas-pohl-parloa/plan-executor/commit/7ca567adc1c7533a8318b71d85f12f8fff6f6e46))
- Colour [plan-executor] failed lines red ([34c4754](https://github.com/andreas-pohl-parloa/plan-executor/commit/34c47544947c40644e390800abb5ae3f2f828936))
- Major plan-executor improvements ([a5abd40](https://github.com/andreas-pohl-parloa/plan-executor/commit/a5abd40d2c3ce37f5bac0930666953e721c02a34))
- Add remote_repo config field for remote execution ([80b5bed](https://github.com/andreas-pohl-parloa/plan-executor/commit/80b5bed29db66e5d67302085de4753145a9647b9))
- Add ExecutionMode enum and parse_execution_mode() to plan.rs ([47c0ee6](https://github.com/andreas-pohl-parloa/plan-executor/commit/47c0ee612b72ee3499c5f83d0539cc0825259a3f))
- Add GitHub Actions workflow template for remote execution ([e178ae2](https://github.com/andreas-pohl-parloa/plan-executor/commit/e178ae2549b6e8fa246c766516edddb5cd1a0d29))
- Add foreground execution mode (execute -f) ([0515b5a](https://github.com/andreas-pohl-parloa/plan-executor/commit/0515b5a08d83b2b1ab98ef997758e868cc20f544))
- Add remote execution module with metadata, PR creation, and status query ([4e4c90a](https://github.com/andreas-pohl-parloa/plan-executor/commit/4e4c90a8c4b370100ca82f5eada1af1ee5fd8335))
- Route remote plans to GitHub PR trigger from execute command ([3e7fe09](https://github.com/andreas-pohl-parloa/plan-executor/commit/3e7fe0973918703576d7767cdacc66b9dab31772))
- Route remote plans in daemon auto-execute to GitHub PR trigger ([a646d77](https://github.com/andreas-pohl-parloa/plan-executor/commit/a646d77ef5a8ae5a1bd884d206f7660021e00230))
- Show remote execution status in jobs command ([ed6376f](https://github.com/andreas-pohl-parloa/plan-executor/commit/ed6376f2082f40dbf60bbdb29faa3b5d1c503eb3))
- Add remote-setup wizard for configuring execution repo secrets ([92d3b5d](https://github.com/andreas-pohl-parloa/plan-executor/commit/92d3b5dde3255e99f9a666c1d6052a1ab528c261))
- Add GitHub Actions release workflow for binary builds ([1365b05](https://github.com/andreas-pohl-parloa/plan-executor/commit/1365b052a6b2fbb519cfeb77bd1374bb48ecf6c3))

### Miscellaneous

- Fix all compiler warnings ([5f3f240](https://github.com/andreas-pohl-parloa/plan-executor/commit/5f3f240ef22ad87b634a1ef56f605286a8bed7c1))
- Update mock configs and add test plan ([82fb6ae](https://github.com/andreas-pohl-parloa/plan-executor/commit/82fb6ae77175c98859c9a44cdd216a2a1f16961f))
- Remove unused mark_job_failed_if_running and find_repo_root ([8f34c43](https://github.com/andreas-pohl-parloa/plan-executor/commit/8f34c43f99f340282124cf00a6f7febd248710a4))
- Update stream-json-view submodule (error result rendering) ([8387bc3](https://github.com/andreas-pohl-parloa/plan-executor/commit/8387bc3ac40a58a60fedfb8f75aafe68556ebd80))
- Update stream-json-view submodule (content preview; clean result block) ([5ed624f](https://github.com/andreas-pohl-parloa/plan-executor/commit/5ed624ff6da303b22fb565b7461030240a1e0d08))
- Fix clippy warnings and finalize remote execution integration ([aac0d14](https://github.com/andreas-pohl-parloa/plan-executor/commit/aac0d14994dc1642095e97a0bf211697c58c2ffb))
- Remove accidentally committed temp files ([6021fd9](https://github.com/andreas-pohl-parloa/plan-executor/commit/6021fd93f35719ccca4d22577c6cb8a025717c15))
- Update stream-json-view submodule to v0.3.0 ([4b776ce](https://github.com/andreas-pohl-parloa/plan-executor/commit/4b776ce076144b474f5bf03e6b22be88ecd16f16))

### Performance

- Only redraw TUI when dirty, reducing CPU from ~10% to near-zero ([4f24a49](https://github.com/andreas-pohl-parloa/plan-executor/commit/4f24a496d96ec101f304ec1267d19d14c47009c0))

### Refactoring

- Replace formatter + sjv subprocess with sjv library ([9094363](https://github.com/andreas-pohl-parloa/plan-executor/commit/9094363e5ac1e4c5332ed1469c4afa6f2f682a0f))

### Testing

- **VC-0:** Add smoke test for jobs subcommand ([7f7e4e3](https://github.com/andreas-pohl-parloa/plan-executor/commit/7f7e4e3f3c7e2e4019506a584800852e40741be2))

### Debug

- Log every install.sh command with timestamps to /tmp/plan-executor-install.log ([d67b19f](https://github.com/andreas-pohl-parloa/plan-executor/commit/d67b19f1067b11c52d188c84623ad1feeded6318))
- Add tracing to daemon startup + 10s countdown in install.sh ([233a94b](https://github.com/andreas-pohl-parloa/plan-executor/commit/233a94b5ac4add62eb0c51349456c773ae243911))
- Trace session_id capture and EOF state in executor ([889f4ce](https://github.com/andreas-pohl-parloa/plan-executor/commit/889f4ce37dffc3a38a33ffa871196737e1b83dc7))

### Merge

- Resolve conflicts with main ([142a058](https://github.com/andreas-pohl-parloa/plan-executor/commit/142a0586d92c85b41149d1c8fe94167248c1336a))
