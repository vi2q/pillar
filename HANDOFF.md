# 引き継ぎ指示書 (2026-08-29, 8th update)

次のセッションで作業を続ける人/エージェント向けの引き継ぎ。規範は `AGENTS.md` と `docs/rules/` の方。ここには「現状」と「今回判明した落とし穴」を書く。

## プロジェクトの目的

pi v0.84.3 (TypeScript, commit `56700d4`) を Rust に移植する。拡張機能ランタイムだけは luaur v0.1.8 (Luau on Rust) に置き換える。ピンは `UPSTREAM.toml`。移植方針は `docs/rules/02-porting-policy.md` (外部境界は厳密互換、内部は慣用的Rust)。

## 現在の状態

| コミット | 内容 |
| --- | --- |
| `38406ec` 以前 | pillar-protocol / pillar-telemetry / pillar-ai 基盤 / pillar-agent ループ |
| `7217ebf` | pillar-ai: auth基盤 (abort, auth_types, credential_store, models_store, auth_resolve, error) |
| `83955cf` | chore: workspace を edition 2024 / resolver 3 に更新 (fmt 影響を protocol/telemetry/agent に適用) |
| 今回 | **feat(ai): Models 本体 + createProvider + models-runtime.test.ts 39ケースの移植完了** (`models.rs`, `auth_context.rs`, `tests/models_runtime_parity.rs` 新規、auth_resolve/types/credential_store/abort の調整を含む) |
| 今回2 | **feat(ai): プロバイダ層の共有インフラ移植** (transport.rs [FetchFn トレイト + reqwest 既定実装], provider_retry.rs, error_body.rs, provider_env.rs, constrained_sampling.rs, transform_messages.rs, headers.rs, json_parse.rs, text.rs に sanitize_surrogates)。`tests/api_infra_parity.rs` 27ケース |
| 今回3 | **feat(ai): openai-completions プロバイダ移植** (src/api/{mod,openai_completions,github_copilot_headers,openai_prompt_cache}.rs: stream/stream_simple/convert_messages/convert_tools/build_params/compat 自動検出/SSE パーサ/reasoning_details リプレイ)。`tests/openai_completions_parity.rs` 23ケース |
| 今回4 | **feat(ai): openai-responses プロバイダ移植** (src/api/{openai_responses,openai_responses_shared}.rs + deferred_tools.rs: processResponsesStream / convert_responses_messages / convert_responses_tools / grammar custom_tool_call ストリーミング / service-tier pricing)。`tests/openai_responses_parity.rs` 14ケース。**ModelCompat union 化** (Model.compat を per-API untagged enum に、Box 包装; AnthropicMessagesCompat 追加) |
| 今回5 | **feat(agent): Agent クラス移植 (agent.ts)** (`agent.rs` 新規: 状態スナップショット/ミューテータ, steering/follow-up キュー [既定 one-at-a-time], subscribe [リスナーは登録順に await、activity signal 付き], abort/waitForIdle/reset, prompt [text\|message\|batch] と continue_run — 上流エラー文言を Result エラーで再現)。`stream_fn.rs` 新規 (setDefaultStreamFn グローバル フォールバック)。`agent_loop.rs` 改修: 並列ツールバッチで `tool_execution_end` を完了順に emit (結果は source 順で永続化), onUpdate をバッファリング型 `tool_execution_update` に配線 (settle 後の呼び出しは無視), stream-fn panic を error AssistantMessage に変換 (spawn タスク越しの unwind を防止)。`types.rs`: `ShouldStopAfterTurnContext.context` 追加, AgentLoopConfig に reasoning/thinking_budgets/max_retry_delay_ms, StreamCallOptions に onPayload/onResponse/transport 等を追加, prepare_next_turn の thinking_level を後続リクエストへ反映。`tests/agent_parity.rs` 新規: agent.test.ts 22ケース。 |
| 今回6 | **feat(agent): agent-loop.test.ts パリティ網羅完了 (23ケース)** (`93f5b9c`)。残り13ケースを移植: shouldStopAfterTurn 完全断言版 (steeringPolls=1/followUpPolls=0/12イベント完全一致), カスタムメッセージ convertToLlm 2件, prepareNextTurn スナップショット (2ターン目の systemPrompt 置換検証), prepareArguments (editツール強制変換), executionMode sequential/parallel 強制3件, tool_execution_end 完了順 vs source順永続化, terminate=true 4件 (全結果/blocked call/mixed batch/afterToolCall)。**実装変更**: `agent_loop`/`agent_loop_continue` が `Option<StreamFn>` を受け `streamFn ?? getDefaultStreamFn()` フォールバックを解決 (agent_loop_continue は文脈検証後に解決 = 上流順), `Agent.stream_function` も Option 化。 |
| 今回7 | **feat(agent): harness 基盤 + messages + compaction 共通部 + session 移植** (`712560b`, `a1f5d7d`, `95f65e4`, `d5087a8`, `f569a7d`, `9d70fa8`, `a96b18e`)。`harness/types.rs` (Skill/PromptTemplate/FileInfo/FileError+ExecutionError [安定コード]/FileSystem/Shell/ExecutionEnv トレイト — 上流 Result<T,E> は std Result に map), `harness/utils/truncate.rs` (truncateHead/Tail [UTF-8バイト制限・部分行対応]/formatSize/truncateLine), `harness/system_prompt.rs` (formatSkillsForSystemPrompt XML+エスケープ), `harness/events.rs` (HarnessEventBus: 型フィルタ直接リスナー + バッファリングwatch), `harness/prompt_templates.rs` (frontmatter/first-line description/parseCommandArgs/substituteArgs [$1/${@:N}/${@:N:L}/$ARGUMENTS/$@]), `harness/env/mod.rs` (**StdFsExecutionEnv** = NodeExecutionEnv 移植: symlink非追従 fs 操作/remove force/timeout+abort でプロセスグループ kill/env レイヤリング [shellEnv → overrides、inherit_env=Some(false) は env_clear]/stdout+stderr ストリーミングコールバック), `harness/utils/shell_output.rs` (executeShellWithCapture: sanitize/バイト会計/tail切り詰め/spillファイル — divergence: 完全出力後の切り詰め計算、チャンク進捗コールバックは将来のストリーミング改修時に), `harness/skills.rs` (**skills.ts 完全移植**), `harness/messages.rs` (**messages.ts 完全移植**: summary prefix/suffix 定数/bashExecutionToText/create* ファクトリ/ハーネス convertToLlm)。**`AgentMessage` を閉じた union から拡張**: BashExecution/Custom/BranchSummary/CompactionSummary 変種追加 (Box 包装, serde camelCase)、`as_message()` は Option 返しに変更し `as_base_message()` が後方互換。`harness/compaction/{shared,utils,branch_summarization}.rs` (**compaction.ts 共通部 + branch-summarization.ts の session 非依存部**: estimateTokens [UTF-8 バイト版]/SUMMARIZATION_SYSTEM_PROMPT/completeSimpleWithRetries/FileOperations 会計/serializeConversation/BRANCH_SUMMARY プロンプト類/prepareBranchEntries 予算トリム)。**`harness/session/` (v4 セッションツリー完全移植)**: types.rs (Entry エンベロープ + EntryPayload enum/LaneRecord + RecordPayload/ProvisionedEntry/queries/LogItem/ForkOptions/SessionError), state.rs (SessionState: 連続 seq 不変量/id 一意性/lane leaf チェーン/open operation 追跡/統計台帳/log/walkToRoot サイクル検出/createForkMutations), context.rs (buildSessionContext: compaction 境界/thinking·model·active-tools 導出/deferred assistant 除外/custom projector), memory.rs (Session facade/SessionStorage トレイト/InMemorySessionStorage + Repo [create/open/list/delete/fork]/注入可能 id ジェネレータ)。テスト: system-prompt 3 + events 2 + truncate 10 (fuzz含む) + nodejs-env 25 + utils 10 + skills 8 + messages 13 + lib 20 + session 20。**pillar-agent 143テスト全パス。** |

**pillar-ai 188テスト (core 35 + faux 22 + models-runtime 39 + api-infra 27 + openai-completions 23 + openai-responses 14 + anthropic-messages 27 + uuid 1) と pillar-agent 103テスト (lib 13 + loop 25 + agent 22 + nodejs-env 25 + utils 10 + skills 8) 全パス。`cargo fmt --check` / `cargo clippy --workspace --all-targets -- -D warnings` クリーン。**

上流チェックアウトは `/tmp/upstream/pi`, `/tmp/upstream/luaur` (再作成手順は docs/rules/06)。

## 全体進捗の目安 (2026-08-29 時点の行数集計)

上流パッケージ規模 (src = テスト以外の .ts, tests = *.test.ts):

| パッケージ | 上流 src | 上流 tests | pillar 状況 |
| --- | --- | --- | --- |
| protocol | 1.2k | 0.7k | ✅ 完了 (src 2.5k / tests 1.0k) |
| telemetry | 0.9k | 0.2k | ✅ 完了 (src 0.7k / tests 0.5k) |
| ai | 27.7k | 35.1k | 🔶 約4〜5割 (src 14.6k / tests 8.2k。openai-completions / openai-responses / anthropic-messages / 共有インフラ / auth / models 済み。残り: google 系 1.4k、mistral-conversations 0.9k、bedrock-converse 1.3k、openai-codex 1.7k、azure 0.3k、images、providers/* 等) |
| agent | 12.9k | 8.6k | 🔶 約15% (src 2.9k / tests 3.2k。ループ + Agent クラス + ループテスト23ケース完璧。残り: harness/ 10.1k、proxy 0.4k、search/、e2e) |
| coding-agent | 78.9k | 50.3k | ❌ 未着手 (最大) |
| tui | 17.9k | 16.4k | ❌ 未着手 |
| server / client / session-backends | 6.3k | 4.2k | ❌ 未着手 |
| evals | 1.3k | 0.5k | ❌ 対象外の可能性 |

**体感 1.5割前後。** ただし偏りがある: 土台層 (protocol / telemetry / ai コア / agent コア) は最難関部 (SSE パーサ・非同期セマンティクス・イベント順序の厳密互換) を含めてほぼ固まっており、235テストで保護済み。残りの約7割は coding-agent (79k) と tui (18k) で、両者とも土台の上に載せる形なので行数比よりは速く進む見込み。**harness/ (agent の 10k行) が coding-agent 着手前の最後の大きな関門。**

## 未移植 (優先順)

1. **pillar-ai のプロバイダ残り**: `openai-completions` は移植済み。次は `openai-responses-shared.ts` (792行) と `openai-responses.ts` (376行)、そして `anthropic-messages.ts` (1391行)。`models.generated.ts` はジェネレータで再生成、手移植禁止 (docs/rules/01、生成器は pillar-ai/src/bin/generate-models.rs に作る)。live-API テスト (responseid, xhigh, tool-call-without-result, tool-call-id-normalization e2e) はモック不能なので非移植。
2. **pillar-agent の残り**: Agent クラス、agent-loop.test.ts 23ケース、harness 基盤 (types/env/messages/skills/compaction共通部/branch-summarization準備部) は移植済み (〜`9d70fa8`)。残りは `proxy.ts` (370行), `harness/` の中核 (compaction 本体 848行/reducer 667行/telemetry 615行/agent-harness 508行/session 738行 [session があると collectEntriesForBranchSummary/generateBranchSummary も移植可]/tools 935行), `search/`, `e2e.test.ts` のうちモック可能なもの。
3. **pillar-coding-agent**: 未着手 (最大、61k行)。
4. **pillar-tui / client / server / session-store**: 未着手。
5. **pillar-extensions**: 未着手 (luaur VM 統合)。設計は docs/rules/04 に確定済み。

## 前セッションまでに判明した落とし穴 (重要)

### 1. MutexGuard を await またぎで保持しない

`std::sync::Mutex` のガードを async ブロック内で await またぎに保持するとコンパイルは通るが future が Send でなくなるか、同一ステートメント内の複数 `.lock()` で即デッドロックする。パターン: データは clone してから await、ガードは明示スコープで落とす。**sync 関数でも同じ**: `Models::clear_providers` で `providers`/`refresh_signals` のガードを保持したまま `supersede_provider_refresh` (同じ mutex を再ロック) を呼んでデッドロックした。ガードを保持したまま他のロックを取る関数を呼ばないこと。テストが panic で途中止まりすると、後段のデッドロックが隠れる (直したら別のバグが出る) ので注意。

### 2. EventStream::result() は複数回呼べること (修正済み: b04c4ce)

pi の Promise セマンティクスでは同じ promise を何度でも await できる。`Option::take()` で消費しないこと。

### 3. ハングのデバッグ手順 (macOS)

- `cargo test` が止まったら **ビルドかテスト実行かを切り分ける**: テストバイナリのタイムスタンプとソースのタイムスタンプを比べる。
- テストバイナリを直接実行 (`--test-threads=1 --nocapture`) して特定のテストを絞る。
- `eprintln!` を「どの await まで進んだか」のマーカーとして入れるのが `sample` より速いことも多い (今回の切り分けはこれが決め手)。
- panic が握り潰されている可能性: `tokio::spawn` の JoinHandle を無視するとタスク内 panic が観測できない。

### 4. bash ツールの安全な使い方 (ハング防止)

- **バックグラウンドジョブ (`cmd &`) + `wait` は使わない**。タイムアウトは `perl -e 'alarm N; exec @ARGV' cmd...` でラップする (macOSにtimeoutはない)。出力はファイルにリダイレクトし、alarm で殺した後 `tail` する (ツールが stdout を返さないことがある)。

## 今回判明した追加の落とし穴

### 10. tokio watch の `send` は受信者がいないと値を捨てる

`watch::Sender::send` は全 Receiver が drop されていると `Err` を返し**値を保存しない**。abort フラグのように「後から subscribe する人も値を見る」べきものは `send_replace` を使う。実例: `AbortSignal::abort` — abort が待機タスクの subscribe より先に起きると `send` では何も起きず、後から来たタスクが永遠に待つ (models-runtime パリティテストのデッドロックの直接原因)。

### 11. `AbortSignal::any` は転送タスクを介さず同期的に合成する

上流 `AbortSignal.any` はリスナーを同期的に attach する。tokio watch の受信を spawn した転送タスクで中継すると (a) subscribe 前の abort を取りこぼし、(b) `send` なら受信者不在で失敗し、(c) 呼び出し側が転送タスクのスケジューリングを待たずに `is_aborted()` を見るためレースする。現実装は composite が入力の `Receiver` を保持し、`is_aborted`/`reason`/`aborted_or_pending` が同期的に inputs を見る。

### 12. JS の promise は即座に走り、Rust の future は lazy

上流テストの `const p = models.refresh(...); await started;` は promise が即実行される前提。Rust では `pending` を poll しないと provider コールバックが走らないので、`Box::pin` + `tokio::select!` で「started シグナルまで並行ドライブ」する必要がある (models_runtime_parity の3テストで修正済み)。

### 13. refresh の2フェーズ呼び出しで one-shot スロットを消費しない

`run_provider_operation` は refresh fn を2回呼ぶ (phase 1: allowNetwork=false, phase 2: true)。クロージャの呼び出し時に `Mutex<Option<Sender>>::take()` すると phase 1 が消費して phase 2 で `None` になる。テスト側は `Arc<Mutex<Option<_>>>` にして**使用時点** (allowNetwork チェック後) に take する。上流のクロージャ変数は永続するのが正。

### 14. 上流の Map は挿入順。Rust では HashMap を使わない

`InMemoryCredentialStore` (credentials) と provider registry は上流では `Map` で `list`/`getProviders` が挿入順を返す。Rust 側は `Vec<(String, T)>` で保持している。既存キーの再 set は位置を維持 (Vec では in-place 更新)。新規 keyed コレクションの移植時も同様に。

### 15. serde_json の Map は順序付き (既定は BTreeMap)

`serde_json::Value` のオブジェクトは既定で BTreeMap (key ソート)。上流の Map 挿入順に依存する出力 (constrained-sampling の strict 変換が生成する `required` 配列など) は順序が変わる。 providers は配列順に意味がないので控えめに寄せる方針。workspace で `preserve_order` feature を有効化するのは CBOR パリティへの影響が読めないため今回は見送り。

### 17. Responses SSE → AssistantMessageEventStream の契約

`processResponsesStream` は **Done/Error イベントを push しない** (上流呼び出し側が push する)。Rust テストで直接呼ぶときは、drive 完了後に `stream.end(None)` を呼ばないと EventIter が `done=false` のまま Pending で永遠に待つ。assert は `output.stop_reason` (finalize_response が書き込む) で行う。

### 19. repair_json の制御文字分岐で index が進まない (修正済み: 576c6fe)

repair_json の in_string 内で `repaired.push(if ... { continue } else { c })` と書くと continue が `index += 1` を飛ばして無限ループする。if 文に分けて index を進めてから continue。テストは生の不正JSONを SSE data 行に流す形でしか露出しない (serde_json::from_str で先に弾かないこと)。

### 18. ModelCompat union 化 (Box 包装)

`Model.compat` を per-API untagged enum `ModelCompat` にした。Box 包装 (clippy large-enum-difference)。使用側は `match model.compat.as_ref() { Some(ModelCompat::OpenaiCompletions(c)) => c, _ => return detected }` パターン。serde untagged なので JSON からは各 API 形がそのまま読める。テスト側は `ModelCompat::OpenaiCompletions(Box::new(...))` か `.into()`。

### 16. プロバイダ層の意図的な divergence (今回確定)

- `error_body.rs`: 上流は SDK エラーオブジェクトのフィールドを掘る (Mistral/openai/genai/Bedrock 形, pipe スニッフィング, class instance 判定)。Rust に SDK オブジェクトは無いので `normalize_provider_error(message, status, body)` が transport から受け取る形。`format_provider_error` / truncation は厳密互換。`provider-error-body-passthrough/regression.test.ts` は SDK レベルなので非移植。
- `provider_retry.rs`: `retry-after` は数値のみパース (HTTP-date は指数バックオフにフォールバック)。jitter は fastrand。
- `provider_env.rs`: Bun サンドボックスの /proc フォールバックは非移植。
- `sanitize_surrogates` (text.rs): Rust の String は UTF-8 で unpaired surrogate を保持できないため恒等関数。上流と同じ呼び出し点を維持するための grep-parity 用。
- `transport.rs`: 上流 `FetchFunction` は WHATWG Response を返すが、Rust は `FetchFn` トレイト + ストリーミング `FetchResponse`。既定実装は `ReqwestFetch` (rustls + gzip + stream)。

### 20. Agent 移植で判明した落とし穴 (今回)

- **spawn タスク内の panic は catch_unwind しないとプロセスを落とす**: `agent_loop` の `tokio::spawn` 内で stream fn が panic すると、JoinHandle を無視しているので panic が外に伝播してテストプロセスごと死ぬ (HANDOFF #3 の「panic が握り潰されている」の逆 — 観測される前に死ぬ)。`stream_assistant_response` で `futures::FutureExt::catch_unwind` + `AssertUnwindSafe` で包み、panic を stopReason "error" + errorMessage の AssistantMessage に変換した (上流 handleRunFailure 相当)。
- **Agent の待ち合わせ構造**: 上流は `emit()` をループ内で await するが、Rust ループは spawn で stream に push するだけ。Agent は `drive_agent_stream` で EventIter を消費しながら `process_event` (状態 reduce + リスナー await) を同期的に回す。リスナーが完了するまで prompt が返らない保証はこの構造で出している。
- **ListenerEntry の take/push パターン**: リスナー dispatch 中に再 lock しないよう、`std::mem::take` でリストを抜いてから順に await し、各リスナーを呼び終えたら push し戻す。簡易だが「リスナー内部で subscribe/unsubscribe」も壊れない。
- **session_id 等の可変フィールドは `Arc<Mutex<Option<String>>>`**: `Agent` は `&self` で使う (tokio::spawn に置くため Arc<Agent>)。setter は内部可変性で。
- **並列ツールの onUpdate はバッファ → settle 後 drain**: 上流は updateEvents 配列に Promise を積み、`execute` 返却後に `Promise.all` で flush する。tokio::spawn で即配信すると実行順序とイベント順がずれ、late-update テスト (settle 後は無視) も壊れる。sync バッファ + `AtomicBool accepting` で再現。
- **ツールテストの oneshot::Sender は Fn クロージャに直接 move できない**: `ToolExecuteFn` は `Fn` なので `Arc<Mutex<Option<Sender>>>` に包んで使用時に take する (HANDOFF #13 と同型)。
- **テストの `let () = tokio::join!(...)` は型注釈エラーになる**: join! はタプルを返すので `tokio::join!(...)` 単独で。

### 21. ループテスト移植で判明した落とし穴 (今回)

- **shouldStopAfterTurn 等のフッククロージャは `&参照 -> Future` で借用が future に食い込む**: `Box::pin(async move { ... context.field ... })` と参照を直接持ち込むと lifetime エラー。クロージャ本体で必要なデータを clone してから `Box::pin(async move ...)` に渡す (今回の snapshot パターン)。
- **`Arc::new(クロージャ)` を `Arc<ConvertToLlmFn>` 等のトレイトオブジェクト型に `.into()` できない**: フィールド型が既に `Option<Arc<dyn Fn...>>` なら `Some(Arc::new(明示型注釈付きクロージャ))` を直接代入する。`|messages| {...}` だと推論が効かないので `|messages: &[AgentMessage]| -> Vec<Message>` と注釈する。
- **並列実行の可視化テストは `tokio::sync::Notify` がゲートに便利**: 上流の `new Promise + setTimeout(release, 20)` は `Notify::notify_one` + `release_gate_after(gate, 20)` スポーンで置換。既に await 中の notified() を起こすのは notify_one で十分 (値をためる必要があるなら notify_waiters/Notified::enable の挙動に注意)。
- **上流テストの llmCalls カウントは message_end(assistant) の個数で代用**: Rust の stream fn はループ内で直接呼ばれないため呼び出し回数を直接観測できない。AtomicU32 をカウンタにして stream fn クロージャ内で fetch_add するのが素直 (今回の prepareNextTurn テスト)。
- **閉じた `AgentMessage` union でのカスタムメッセージテスト**: `CustomAgentMessages` は移植対象外 (divergence 済)。toolResult をスタンドインにし、converter が toolName=="notification" でフィルタ/マップする形で上流の挙動 (convertToLlm でのフィルタ/変換) を検証できる。

### 22. harness/env 移植で判明した落とし穴 (今回)

- **tokio Command は親 env を継承する**: 上流 `getShellEnv` の `{...process.env, ...baseEnv, ...extraEnv}` は「マージ結果」を返すが、Rust で `cmd.env(k,v)` を重ねるだけだと `inherit_env: false` でも親 env が残る。`inherit_env: Some(false)` のときは `cmd.env_clear()` を呼んでからマージ結果を適用する (デバッグに時間を食った: `ShellExecOptions::default()` の `inherit_env: bool` が false 既定だったのが原因。最終的に `Option<bool>` + `unwrap_or(true)` にして上流 `?? true` と揃えた)。
- **同一トレイトメソッド名の曖昧呼び出し**: `ExecutionEnv = FileSystem + Shell` で両者が `cleanup()` を持つと `env.cleanup()` が曖昧になる。呼び出し側は `FileSystem::cleanup(&env)` と明示する。
- **`select!` で future を再利用する今後のdrain**: pipe読み取りを `&mut` 借用で future に組み込むと select! の落としていない branch 側から借用が残り E0499。読み取りは `tokio::spawn` の独立タスク + `Arc<Mutex<Vec<u8>>>` バッファにして、select! は child.wait のみを対象にするのが素直。
- **pi agent ハーネス自体が `PI_SESSION_FILE` 等を注入する**: このリポジトリの開発環境 (pi の bash ツール) は `PI_SESSION_FILE`/`PI_CODING_AGENT`/`PI_SESSION_ID` を環境にセットする。env レイヤリングのテストはこの値が混入する前提で書く (上流テストも同一の変数名を使うので、期待値は上流テストのリテラル通りで正しい — テスト側の期待値を環境に合わせて変えないこと)。
- **edition 2024 で `std::env::set_var/remove_var` は unsafe**: テストでも `unsafe { }` で囲む。
- **`.err().expect()` は clippy err_expect で落ちる**: `.expect_err()` を使う。
- **async fn の再帰はサイズ計算が循環してコンパイルできない**: 再帰関数を薄いラッパーにして、実体を `Box::pin(inner(...))` で呼ぶ (skills.rs の `load_skills_from_dir_internal`)。
- **session 移植の構造判断**: 上流 `Entry` 判別ユニオンは「storage 割当エンベロープ (type/id/seq/parentId/timestamp) + `#[serde(flatten)]` payload enum」に分離。`findOpenOperations` の最新順は lane ごとの挿入順 Vec + id マップの二重管理で再現。Usage が unsigned のため上流の負の adjustment 記録はゼロ差分になり統計断言を調整 (divergence 記録)。repo の `share()` は `Arc` 内部共有ハンドルで同一 state を返す。clippy module_inception 回避のため session.rs 空スタブは削除し memory.rs に統合。
- **上流の `yaml.parse` が throw する入力を再現する**: `[invalid` のような未終了フロー構文はフラットパーサでは有効なスカラーになってしまう。`[`/`{` で始まり対応閉じがない値を `malformed` フラグにして、declared skill (SKILL.md) のみ `parse_failed` 診断、root .md は無視 — という上流挙動を再現した。python yaml で期待挙動を事前確認すると速い。
- **globset の gitignore 相似処理**: スラッシュを含まないパターンは basename マッチ (`literal_separator(false)`)、`dir/` 付きは候補の前方一致も見る。upstream `ignore` クレートの完全互換ではないので、複雑な ignore パターンが出てきたら `ignore` クレートへの置換を検討。

## 次のセッションの最初の一歩

**harness 基盤 + skills 完了** (`712560b`, `a1f5d7d`, `95f65e4`, `d5087a8`)。pillar-agent は 103テスト。

次の大きい塊は **harness の中核**: `compaction/compaction.ts` 本体 (848行 — session/state が揃ったので移植可。collectEntriesForBranchSummary/generateBranchSummary 完結形も) → `reducer.ts` (667行) → `tools/` (935行) → `agent-harness.ts` (508行) → `telemetry.ts` (615行) → `jsonl.ts` (800行, JsonlSessionRepo) → `proxy.ts` (370行) の順が依存順。上流 `test/harness/` の reducer 1127行・compaction 697行・tools 622行がパリティテスト源。models_generated.rs (generate-models ジェネレータ, docs/rules/01) は未作成 — live カタログ依存のため別タスク。

## セッション運用の反省 (継続)

- **「修正した」は必ずテスト実行で確認してから言うこと**。デッドロック調査では「修正→別の箇所でハング」が連鎖した (credential_store の select 修正だけでは直らず、abort の watch/send 問題とテストの lazy-future 問題が重なっていた)。
- フレークするテストは連続10回回して固定すること。今回 `rejects_late_publication` は 1/5 でしか落ちなかった (phase 1 が sender を消費する競合)。
- ユーザーが「止めて」と言ったら、同じ推論を繰り返さず手を止めること。
