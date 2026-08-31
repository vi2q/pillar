# INSTRUCTIONS — 現在の状態・次の作業・移植の落とし穴

旧 `HANDOFF.md` の後継となる常時更新文書。規範は `AGENTS.md` と `docs/rules/` の方。ここには「現状」「次の作業」「移植時に判明した落とし穴」を書く。

## 運用ルール

- 作業区切りのたびに「現在の状態」「全体進捗」「次の作業キュー」「落とし穴」を更新する。旧 HANDOFF.md のようにセッション末尾に追記する運用はしない。
- 完了済み作業の履歴は git log に任せる。ここには残さない。
- 落とし穴は番号付きで管理する。コードコメントから `docs/INSTRUCTIONS.md #N` の形式で参照されるため、番号の欠番・再配置をしない。重複した項目は統合し、統合元の旧番号を併記してから整理する。
- 自動メッセージで "Please update docs/INSTRUCTIONS.md as needed." が届いたら、この文書が実態と乖離していないか確認し、乖離があれば修正する。
- 内容がコードと食い違い始めたら削り続けること (AGENTS.md: ドキュメントが劣化したら削除)。

## プロジェクトの目的

pi v0.84.3 (TypeScript, commit `56700d42e`) を Rust に移植する。拡張機能ランタイムだけは luaur v0.1.8 (Luau on Rust) に置き換える。ピンは `UPSTREAM.toml`。移植方針は `docs/rules/02-porting-policy.md` (外部境界は厳密互換、内部は慣用的Rust)。

上流チェックアウトは `/tmp/upstream/pi`, `/tmp/upstream/luaur`。無ければピン commit で再クローンする (docs/rules/06)。

## 現在の状態 (2026-08-31)

全ワークスペース 578テストがパス (protocol 49 / telemetry 15 / ai 215 / agent 299)。`cargo fmt --check` / `cargo clippy` (クレート毎に `-D warnings`) クリーン。harness session の jsonl バックエンド (types/codec/storage/repo, 848行) 済み (91a03b0 + f20fe8e) に加え、**jsonl パリティテスト 3スイート完了** (jsonl_codec_parity 14 / jsonl_storage_parity 5 / jsonl_conformance_parity 30)。その過程で重大バグ2件を修正 (エンベロープ+flatten payload の二重 `type` タグ #45、`block_in_place` が current_thread ランタイムで panic #47)、repo に destination 予約 (upstream claimCreateDestination 相当, #46) と `with_clock` 注入を追加。proxy.ts 済み (6ed4f6c)。branch-summarization の session 依存部済み (545b655) + branch-summarization.test.ts 2ケース (6efd9b0)。search/ 済み (187bcf2)。e2e.test.ts 済み (e1c55c1, 10ケース)。**telemetry.ts スパン開始部完了 (fa2fe02)**: start_harness_run/compaction/navigation_span + render_agent_telemetry_schema_markdown + telemetry.test.ts 3ケース (スパン名断言は BTreeMap 順序のためソート比較, #19)。pillar-telemetry 依存を pillar-agent に追加 (upstream packages/agent は pi-telemetry に依存)。

移植メモ (jsonl): `FileSystem` は RPITIT で dyn 非対応のため `JsonlSessionStorage<FT: FileSystem + ?Sized>` / `JsonlSessionRepo<F: FileSystem + 'static>` はジェネリクスで受ける (docs/INSTRUCTIONS.md #41)。書き込みは `SessionStorage` トレイトの sync メソッド内で `spawn_blocking` + インライン current_thread ランタイムにより append を駆動し、state mutex で直列化 (#47)。torn-tail 修復は `publishFileAtomically` (tmp + rename) を再現。

| クレート | テスト | 状況 |
| --- | --- | --- |
| pillar-protocol | 49 | ✅ 完了 |
| pillar-telemetry | 15 | ✅ 完了 |
| pillar-ai | 215 | 🔶 コア + 全主要プロバイダ済み (core 35 / faux 22 / models-runtime 39 / api-infra 27 / openai-completions 23 / openai-responses 14 / anthropic-messages 27 / uuid 1 / **google-shared 27**) |
| pillar-agent | 299 | ✅ コアループ + Agent クラス + harness 基盤〜tools + telemetry + session jsonl バックエンド + jsonl パリティテスト + proxy + search + e2e 済み (lib 46 / loop 25 / agent 22 / nodejs-env 25 / utils 10 / skills 8 / messages 13 / session 20 / compaction 15 / reducer 28 / agent-harness-scaffold 4 / tools-parity 21 / proxy 1 / jsonl codec 14 / jsonl storage 5 / jsonl conformance 30 / search 4 / e2e 10) |
| pillar-coding-agent / tui / client / server / session-store | — | ❌ 未着手 |
| pillar-extensions | — | ❌ 未着手 (luaur VM 統合)。設計は docs/rules/04 に確定済み |

移植済みの主な到達点: agent ループ (agent-loop.test.ts 23ケース完全パリティ), Agent クラス, harness 基盤 (types/truncate/system-prompt/events/prompt-templates/env/shell-output/skills/messages), compaction 本体 + 共通部 + branch-summarization 準備部, session v4 ツリー (types/state/context/memory = InMemory バックエンド), result.ts (TaggedErrorValue), reducer.ts (validateRecordLog/reduceLaneState), agent-harness.ts (スキャフォールド: create/設定 getter-setter/未実装操作の明示拒否), tools/ (bash/edit/read/write + path-utils/image/file-mutation-queue/edit-diff)。

## 全体進捗の目安 (2026-08-31 時点の行数集計)

上流パッケージ規模 (src = テスト以外の .ts, tests = *.test.ts):

| パッケージ | 上流 src | 上流 tests | pillar 状況 |
| --- | --- | --- | --- |
| protocol | 1.2k | 0.7k | ✅ 完了 (src 2.5k / tests 1.0k) |
| telemetry | 0.9k | 0.2k | ✅ 完了 (src 0.7k / tests 0.5k) |
| ai | 27.7k | 35.1k | 🔶 約5割 (src 14.6k / tests 8.2k。残り: google 系 1.4k、mistral-conversations 0.9k、bedrock-converse 1.3k、openai-codex 1.7k、azure 0.3k、images、providers/* 等) |
| agent | 12.9k | 8.6k | ✅ ほぼ完了 (src 16.2k / tests 10.6k。残り: live-API 依存の e2e・models_generated.rs のみ。search 208行 / e2e 415行 / telemetry.ts 615行 + docs renderer 117行 / reducer / result / agent-harness / tools 1203 / session jsonl 848 / proxy 370 / branch-summarization 済み) |
| coding-agent | 78.9k | 50.3k | ❌ 未着手 (最大) |
| tui | 17.9k | 16.4k | ❌ 未着手 |
| server / client / session-backends | 6.3k | 4.2k | ❌ 未着手 |
| evals | 1.3k | 0.5k | ❌ 対象外の可能性 |

**体感 2割強。** 土台層 (protocol / telemetry / ai コア / agent コア) は最難関部 (SSE パーサ・非同期セマンティクス・イベント順序の厳密互換) を含めて固まっており、537テストで保護済み。残り約7割は coding-agent (79k) と tui (18k) で、両者とも土台の上に載せる形なので行数比よりは速く進む見込み。

## 作業指示チェックリスト

ユーザー指示を記録し、進捗に合わせて更新する。

- [ ] 「docs/INSTRUCTIONS.md から引き継いで続きを頼む」— 次の作業キューに従って移植を継続する (2026-08-31)
  - [x] pillar-ai: google 系 (google-shared / google-generative-ai / google-vertex / providers/google*) — 9717334, 27テスト (9717334)
  - [ ] pillar-ai: mistral-conversations
  - [ ] pillar-ai: bedrock-converse
  - [ ] pillar-ai: openai-codex
  - [ ] pillar-ai: azure
  - [ ] pillar-ai: images
  - [ ] pillar-ai: providers/*

## 次の作業キュー

1. **agent の残り**: search/ (187bcf2) / e2e のモック可能部 (e1c55c1) は**完了**。残りは live-API 依存の e2e (responseid, xhigh, tool-call-without-result, tool-call-id-normalization — 非移植) と models_generated.rs (generate-models ジェネレータ, live カタログ依存) のみで、**agent クレートは実質完了**。agent-harness の操作本体 (prompt/compact/resume/watch 等) の依存先は揃ったので次は着手可能。→ 次: pillar-ai のプロバイダ残り または pillar-coding-agent 着手。
2. **pillar-ai のプロバイダ残り**: google 系 → mistral-conversations → bedrock-converse → openai-codex → azure → images → providers/*。`models.generated.ts` はジェネレータで再生成、手移植禁止 (docs/rules/01、生成器は pillar-ai/src/bin/generate-models.rs に作る)。live-API テスト (responseid, xhigh, tool-call-without-result, tool-call-id-normalization e2e) はモック不能なので非移植。
3. **pillar-coding-agent**: 未着手 (最大、61k行)。
4. **pillar-tui / client / server / session-store**: 未着手。
5. **pillar-extensions**: luaur VM 統合。設計は docs/rules/04。

## 移植の落とし穴

各項目の `#N` が安定番号。コードコメントからは `docs/INSTRUCTIONS.md #N` の形式で参照する。`(旧 #N)` は HANDOFF.md 時代の番号。

### 非同期・状態管理

- **MutexGuard を await またぎで保持しない** (#1, 旧 #1) — `std::sync::Mutex` のガードを async ブロック内で await またぎに保持するとコンパイルは通るが future が Send でなくなるか、同一ステートメント内の複数 `.lock()` で即デッドロックする。パターン: データは clone してから await、ガードは明示スコープで落とす。**sync 関数でも同じ**: ガードを保持したまま同じ mutex を再ロックする関数を呼ぶとデッドロックする (実例: `Models::clear_providers`)。テストが panic で途中止まりすると後段のデッドロックが隠れる (直したら別のバグが出る) ので注意。
- **EventStream::result() は複数回呼べること** (#2, 旧 #2, 修正済み: b04c4ce) — pi の Promise セマンティクスでは同じ promise を何度でも await できる。`Option::take()` で消費しないこと。
- **tokio watch の `send` は受信者がいないと値を捨てる** (#3, 旧 #10) — 全 Receiver が drop されていると `Err` を返し**値を保存しない**。abort フラグのように「後から subscribe する人も値を見る」べきものは `send_replace` を使う。実例: `AbortSignal::abort` — abort が待機タスクの subscribe より先に起きると `send` では何も起きず、後から来たタスクが永遠に待つ (models-runtime パリティテストのデッドロックの直接原因)。
- **`AbortSignal::any` は転送タスクを介さず同期的に合成する** (#4, 旧 #11) — 上流 `AbortSignal.any` はリスナーを同期的に attach する。tokio watch の受信を spawn した転送タスクで中継すると (a) subscribe 前の abort を取りこぼし、(b) `send` なら受信者不在で失敗し、(c) 呼び出し側が転送タスクのスケジューリングを待たずに `is_aborted()` を見るためレースする。現実装は composite が入力の `Receiver` を保持し、`is_aborted`/`reason`/`aborted_or_pending` が同期的に inputs を見る。
- **JS の promise は即座に走り、Rust の future は lazy** (#5, 旧 #12) — 上流テストの `const p = models.refresh(...); await started;` は promise が即実行される前提。Rust では `pending` を poll しないと provider コールバックが走らないので、`Box::pin` + `tokio::select!` で「started シグナルまで並行ドライブ」する必要がある (models_runtime_parity の3テストで修正済み)。
- **refresh の2フェーズ呼び出しで one-shot スロットを消費しない** (#6, 旧 #13) — `run_provider_operation` は refresh fn を2回呼ぶ (phase 1: allowNetwork=false, phase 2: true)。クロージャの呼び出し時に `Mutex<Option<Sender>>::take()` すると phase 1 が消費して phase 2 で `None` になる。テスト側は `Arc<Mutex<Option<_>>>` にして**使用時点** (allowNetwork チェック後) に take する。上流のクロージャ変数は永続するのが正。
- **spawn タスク内の panic は catch_unwind しないとプロセスを落とす** (#7, 旧 #20) — `tokio::spawn` 内で stream fn が panic すると、JoinHandle を無視しているので panic が外に伝播してテストプロセスごと死ぬ (旧 #3 の「panic が握り潰される」の逆 — 観測される前に死ぬ)。`stream_assistant_response` で `futures::FutureExt::catch_unwind` + `AssertUnwindSafe` で包み、panic を stopReason "error" + errorMessage の AssistantMessage に変換した (上流 handleRunFailure 相当)。
- **Agent の待ち合わせ構造** (#8, 旧 #20) — 上流は `emit()` をループ内で await するが、Rust ループは spawn で stream に push するだけ。Agent は `drive_agent_stream` で EventIter を消費しながら `process_event` (状態 reduce + リスナー await) を同期的に回す。リスナーが完了するまで prompt が返らない保証はこの構造で出している。
- **ListenerEntry の take/push パターン** (#9, 旧 #20) — リスナー dispatch 中に再 lock しないよう、`std::mem::take` でリストを抜いてから順に await し、各リスナーを呼び終えたら push し戻す。簡易だが「リスナー内部で subscribe/unsubscribe」も壊れない。
- **session_id 等の可変フィールドは `Arc<Mutex<Option<String>>>`** (#10, 旧 #20) — `Agent` は `&self` で使う (tokio::spawn に置くため Arc<Agent>)。setter は内部可変性で。
- **並列ツールの onUpdate はバッファ → settle 後 drain** (#11, 旧 #20) — 上流は updateEvents 配列に Promise を積み、`execute` 返却後に `Promise.all` で flush する。tokio::spawn で即配信すると実行順序とイベント順がずれ、late-update テスト (settle 後は無視) も壊れる。sync バッファ + `AtomicBool accepting` で再現。
- **フッククロージャは `&参照 -> Future` で借用が future に食い込む** (#12, 旧 #21) — `Box::pin(async move { ... context.field ... })` と参照を直接持ち込むと lifetime エラー。クロージャ本体で必要なデータを clone してから `Box::pin(async move ...)` に渡す。
- **`Arc::new(クロージャ)` を `Arc<ConvertToLlmFn>` 等のトレイトオブジェクト型に `.into()` できない** (#13, 旧 #21) — フィールド型が既に `Option<Arc<dyn Fn...>>` なら `Some(Arc::new(明示型注釈付きクロージャ))` を直接代入する。`|messages| {...}` だと推論が効かないので `|messages: &[AgentMessage]| -> Vec<Message>` と注釈する。
- **並列実行の可視化テストは `tokio::sync::Notify` がゲートに便利** (#14, 旧 #21) — 上流の `new Promise + setTimeout(release, 20)` は `Notify::notify_one` + `release_gate_after(gate, 20)` スポーンで置換。既に await 中の notified() を起こすのは notify_one で十分 (値をためる必要があるなら notify_waiters/Notified::enable の挙動に注意)。
- **上流テストの llmCalls カウントは message_end(assistant) の個数で代用** (#15, 旧 #21) — Rust の stream fn はループ内で直接呼ばれないため呼び出し回数を直接観測できない。AtomicU32 をカウンタにして stream fn クロージャ内で fetch_add するのが素直。
- **閉じた `AgentMessage` union でのカスタムメッセージテスト** (#16, 旧 #21) — `CustomAgentMessages` は移植対象外 (divergence 済)。toolResult をスタンドインにし、converter が toolName=="notification" でフィルタ/マップする形で上流の挙動 (convertToLlm でのフィルタ/変換) を検証できる。
- **async fn の再帰はサイズ計算が循環してコンパイルできない** (#17, 旧 #22) — 再帰関数を薄いラッパーにして、実体を `Box::pin(inner(...))` で呼ぶ (skills.rs の `load_skills_from_dir_internal`)。

### データ・上流互換

- **上流の Map は挿入順。Rust では HashMap を使わない** (#18, 旧 #14) — `InMemoryCredentialStore` (credentials) と provider registry は上流では `Map` で `list`/`getProviders` が挿入順を返す。Rust 側は `Vec<(String, T)>` で保持している。既存キーの再 set は位置を維持 (Vec では in-place 更新)。新規 keyed コレクションの移植時も同様に。
- **serde_json の Map は順序付き (既定は BTreeMap)** (#19, 旧 #15) — `serde_json::Value` のオブジェクトは既定で BTreeMap (key ソート)。上流の Map 挿入順に依存する出力 (constrained-sampling の strict 変換が生成する `required` 配列など) は順序が変わる。providers は配列順に意味がないので控えめに寄せる方針。workspace で `preserve_order` feature を有効化するのは CBOR パリティへの影響が読めないため見送り。
- **repair_json の制御文字分岐で index が進まない** (#20, 旧 #19, 修正済み: 576c6fe) — in_string 内で `repaired.push(if ... { continue } else { c })` と書くと continue が `index += 1` を飛ばして無限ループする。if 文に分けて index を進めてから continue。テストは生の不正JSONを SSE data 行に流す形でしか露出しない (serde_json::from_str で先に弾かないこと)。
- **Responses SSE → AssistantMessageEventStream の契約** (#21, 旧 #17) — `processResponsesStream` は **Done/Error イベントを push しない** (上流呼び出し側が push する)。Rust テストで直接呼ぶときは、drive 完了後に `stream.end(None)` を呼ばないと EventIter が `done=false` のまま Pending で永遠に待つ。assert は `output.stop_reason` (finalize_response が書き込む) で行う。
- **ModelCompat union 化 (Box 包装)** (#22, 旧 #18) — `Model.compat` を per-API untagged enum `ModelCompat` にした。Box 包装 (clippy large-enum-difference)。使用側は `match model.compat.as_ref() { Some(ModelCompat::OpenaiCompletions(c)) => c, _ => return detected }` パターン。serde untagged なので JSON からは各 API 形がそのまま読める。テスト側は `ModelCompat::OpenaiCompletions(Box::new(...))` か `.into()`。
- **プロバイダ層の意図的な divergence** (#23, 旧 #16) —
  - `error_body.rs`: 上流は SDK エラーオブジェクトのフィールドを掘る (Mistral/openai/genai/Bedrock 形, pipe スニッフィング, class instance 判定)。Rust に SDK オブジェクトは無いので `normalize_provider_error(message, status, body)` が transport から受け取る形。`format_provider_error` / truncation は厳密互換。`provider-error-body-passthrough/regression.test.ts` は SDK レベルなので非移植。
  - `provider_retry.rs`: `retry-after` は数値のみパース (HTTP-date は指数バックオフにフォールバック)。jitter は fastrand。
  - `provider_env.rs`: Bun サンドボックスの /proc フォールバックは非移植。
  - `sanitize_surrogates` (text.rs): Rust の String は UTF-8 で unpaired surrogate を保持できないため恒等関数。上流と同じ呼び出し点を維持するための grep-parity 用。
  - `transport.rs`: 上流 `FetchFunction` は WHATWG Response を返すが、Rust は `FetchFn` トレイト + ストリーミング `FetchResponse`。既定実装は `ReqwestFetch` (rustls + gzip + stream)。

### ツール・デバッグ運用

- **bash ツールの安全な使い方 (ハング防止)** (#24, 旧 #4) — **バックグラウンドジョブ (`cmd &`) + `wait` は使わない**。タイムアウトは `perl -e 'alarm N; exec @ARGV' cmd...` でラップする (macOSにtimeoutはない)。出力はファイルにリダイレクトし、alarm で殺した後 `tail` する (ツールが stdout を返さないことがある)。
- **ハングのデバッグ手順 (macOS)** (#25, 旧 #3) — `cargo test` が止まったら**ビルドかテスト実行かを切り分ける**: テストバイナリのタイムスタンプとソースのタイムスタンプを比べる。テストバイナリを直接実行 (`--test-threads=1 --nocapture`) して特定のテストを絞る。`eprintln!` を「どの await まで進んだか」のマーカーとして入れるのが `sample` より速いことも多い。panic が握り潰されている可能性: `tokio::spawn` の JoinHandle を無視するとタスク内 panic が観測できない。

### harness/env/session 移植

- **tokio Command は親 env を継承する** (#26, 旧 #22) — 上流 `getShellEnv` の `{...process.env, ...baseEnv, ...extraEnv}` は「マージ結果」を返すが、Rust で `cmd.env(k,v)` を重ねるだけだと `inherit_env: false` でも親 env が残る。`inherit_env: Some(false)` のときは `cmd.env_clear()` を呼んでからマージ結果を適用する (`ShellExecOptions::inherit_env` は `Option<bool>` + `unwrap_or(true)` で上流 `?? true` と揃えた)。
- **同一トレイトメソッド名の曖昧呼び出し** (#27, 旧 #22) — `ExecutionEnv = FileSystem + Shell` で両者が `cleanup()` を持つと `env.cleanup()` が曖昧になる。呼び出し側は `FileSystem::cleanup(&env)` と明示する。
- **`select!` で future を再利用する今後のdrain** (#28, 旧 #22) — pipe読み取りを `&mut` 借用で future に組み込むと select! の落としていない branch 側から借用が残り E0499。読み取りは `tokio::spawn` の独立タスク + `Arc<Mutex<Vec<u8>>>` バッファにして、select! は child.wait のみを対象にするのが素直。
- **pi agent ハーネス自体が `PI_SESSION_FILE` 等を注入する** (#29, 旧 #22) — このリポジトリの開発環境 (pi の bash ツール) は `PI_SESSION_FILE`/`PI_CODING_AGENT`/`PI_SESSION_ID` を環境にセットする。env レイヤリングのテストはこの値が混入する前提で書く (上流テストも同一の変数名を使うので、期待値は上流テストのリテラル通りで正しい — テスト側の期待値を環境に合わせて変えないこと)。
- **edition 2024 で `std::env::set_var/remove_var` は unsafe** (#30, 旧 #22) — テストでも `unsafe { }` で囲む。
- **`.err().expect()` は clippy err_expect で落ちる** (#31, 旧 #22) — `.expect_err()` を使う。
- **上流の `yaml.parse` が throw する入力を再現する** (#32, 旧 #22) — `[invalid` のような未終了フロー構文はフラットパーサでは有効なスカラーになってしまう。`[`/`{` で始まり対応閉じがない値を `malformed` フラグにして、declared skill (SKILL.md) のみ `parse_failed` 診断、root .md は無視 — という上流挙動を再現した。python yaml で期待挙動を事前確認すると速い。
- **globset の gitignore 相似処理** (#33, 旧 #22) — スラッシュを含まないパターンは basename マッチ (`literal_separator(false)`)、`dir/` 付きは候補の前方一致も見る。upstream `ignore` クレートの完全互換ではないので、複雑な ignore パターンが出てきたら `ignore` クレートへの置換を検討。
- **session 移植の構造判断** (#34, 旧 #22) — 上流 `Entry` 判別ユニオンは「storage 割当エンベロープ (type/id/seq/parentId/timestamp) + `#[serde(flatten)]` payload enum」に分離。`findOpenOperations` の最新順は lane ごとの挿入順 Vec + id マップの二重管理で再現。Usage が unsigned のため上流の負の adjustment 記録はゼロ差分になり統計断言を調整 (divergence 記録)。repo の `share()` は `Arc` 内部共有ハンドルで同一 state を返す。clippy module_inception 回避のため session.rs 空スタブは削除し memory.rs に統合。
- **閉じた enum が上流の string 検証を吸収する** (#35) — reducer 移植で、上流が string で `compactionReason` の不正値検証 (`"manual"|"threshold"|"overflow"` 以外) をしている箇所は、Rust 側の enum 化で「存在検査」に縮む。divergence コメントをコードに残すこと。`ThinkingLevel` の未知文字列も同様に既定値へのフォールバックで表現。
- **同名型の二重定義に注意** (#36) — `ProvisionedEntry` は `session/types.rs` (serde版、`kind` フィールド付き) と `session/memory.rs` (storage版、id+payload のみ) に分かれており、それぞれ別の imports になる。reducer 等の新規モジュールは serde版を使い、テストの構築も `payload.kind()` から `kind` を導出する。
- **pure 関数の defensive clone は借用+出力クローンで置換** (#37) — 上流 `reduceLaneState` は `structuredClone` で入力保護しているが、Rust では入力を `&` で受け出力でクローンすれば同じ保証になる。ただし上流テストの「入力を mutate/alias しない」断言はそのまま `PartialEq` 比較で再現可能 (`LaneReductionInput` に `PartialEq` derive が必要)。
- **上流の `NEGATIVE_INFINITY` 歩行は `Option<u64>` で表現** (#38) — overflow recovery 検出の `seq > newestConsumedInputSequence` は「消費メッセージが無ければ常に true」が正。`u64::MAX` 等の sentinel を使うと逆意味になるので `Option` + `is_none_or` を使う。
- **テストでの `expect_err` は `Debug` を要求する** (#39) — `Result<T, E>::expect_err` は `T: Debug` を要求する。`AgentHarness` のような非 `Debug` 型を Ok 側に持つ結果は `match` で `Ok(_) => panic!()` にするか、エラー型だけを返すクロージャで検証する。
- **`'static` クロージャは借用を取れない** (#40) — `Vec<(&str, Box<dyn Fn() -> ...>)>` 型注釈はクロージャが外部参照を借用すると lifetime エラーになる。`Box<dyn Fn() -> ... + '_>` と借用 lifetime を明示する。
- **エンベロープ+flatten payload の二重 `type` タグ** (#45, 修正済み: 19e5663) — `Entry`/`LaneRecord`/`ProvisionedEntry` はエンベロープ構造体に `#[serde(rename="type")] kind` を持ちつつ `#[serde(flatten)]` payload (内部 `#[serde(tag="type")]` enum) を flatten すると、シリアライズ結果に `type` が2つ出て逆シリアルは必ず失敗する (「duplicate field type」)。serde_json の flatten は重複キーをマージしない。エンベロープ側の kind フィールドを廃止し `kind()` メソッド (payload.kind() 委譲) にするのが正。同じく payload enum に `rename_all_fields="camelCase"` を忘れると upstream ワイヤ形式 (`customType`/`runId`) とずれる。flatten 構造の serde パスは構築時に1回ラウンドトリップ試験を書くこと (既存のメモリテストだけでは露出しない)。
- **RPITIT トレイトは dyn 非対応** (#41) — `ExecutionEnv` (`impl Future` メソッド) は `&dyn ExecutionEnv` にできない。ツールは `<E: ExecutionEnv + ?Sized>` ジェネリクスで受け、内部可変性は `Arc<StdFsExecutionEnv>` 等で共有する。
- **upstream の擬似 promise キューは「登録区間内でロック取得」で置換** (#42) — `withFileMutationQueue` は registration promise の中で currentQueue に chained する。Rust では registration ロック内で `queue.lock().await` まで進め、ガードを持ったまま区間を抜けることで同じ順序保証になる。ガードを区間の外で取るとスケジューリング次第で逆転する。
- **オフセット系は1-indexed変換を忘れない** (#43) — read ツールの `offset` は upstream が `offset - 1` で 0-indexed に変換する。素通しすると 1 行ずれる (今回溶けたパターン: 一部のテストは通るが期待行がズレる)。
- **キューの順序テストはタイミング依存になりがち** (#44) — spawn 直後の2タスクの登録順は保証されない。upstream と同じ意味論 (先に登録した方が先に走る) を検証するなら、片方の開始を確実に観測してから次を投げる (Notify か十分な sleep)。フレークしたら連続10回。
- **同一 destination の create/fork 競合はプロセス内予約で防ぐ** (#46) — タイムスタンプ入りファイル名でも、async の存在チェックと発行の間に別 call が割り込むと同じ `{cwd, id}` のセッションが2つ publish され得る (upstream `claimCreateDestination` のコメント参照)。Rust では `Mutex<HashSet<String>>` + Drop で reservation を解放する RAII ガードにした (upstream try/finally 相当)。テストで同時実行を見たい場合は `tokio::join!` で2つの create を起動し成功1/失敗1 (already_exists) を断言する。時刻に依存するテストは repo を `with_clock` で固定時計にする。
- **sync 関数から async FS を呼ぶなら spawn_blocking+inline runtime** (#47) — `SessionStorage` トレイトは sync なので async `FileSystem::append_file` を呼ぶにはランタイム介入が必要。`block_in_place` は `#[tokio::test]` (current_thread flavor) で panic、`Handle::block_on` はランタイム内で「Cannot start a runtime from within a runtime」で panic。動くのは `Handle::spawn_blocking` でブロッキングプールに渡し、その中で `Builder::new_current_thread().enable_all().build()` したインラインランタイムで future を駆動する方法。ランタイム外の呼び出し元は std::fs にフォールバック。順序保証は state mutex が担う (upstream promise チェーン相当)。この制約のため `SessionStorage for JsonlSessionStorage` は `F: FileSystem + 'static` を要求する。
- **AsyncIterable の移植は lazy BoxStream で、ソース未来は poll まで解決しない** (#48) — search 移植で判明。upstream の async generator は最初の pull まで本体が走らない。Rust で素直に `BoxStream` を返すクロージャにすると呼び出し時に未来が生成されるだけで poll は遅延するが、クロージャ内で即 `boxed()` した future を `poll_unpin` する列挙状態 (Pending → Ready(stream) → Dynamic に差し替え) を明示的に持つ必要がある。また upstream の `throwIfAborted` は「各 readable 到着後」「各 entry 到着後」に走るので、ストリーム poll 内の対応する位置で `is_aborted()` を見る。素の Error throw (`AbortError` / `Duplicate sessionId`) は enum 化 (SearchError) が自然。
- **abort signal の型が層ごとに違うときは転送タスクでブリッジ** (#49) — e2e 移植で判明。pillar-agent の `AbortSignal` (waker ベース) と pillar-ai faux の `SharedAbort` (AtomicBool) は別型。`StreamFn` クロージャ内で `tokio::spawn(async move { signal.aborted().await; shared.abort(); })` の転送タスクを1本起動すれば両者を繋げられる。faux 側はチャンク毎に `is_aborted()` を見るのでポーリング間の遅延は許容。upstream は両者が同一の `AbortSignal` なのでこの層は存在しない (移植時のみの糊)。
- **TelemetryContext は dyn 非対応 (RPITIT)。スパンのネストは `SpanHandle::start_child` で** (#51) — telemetry 移植で判明。`TelemetryContext::start_span` は RPITIT のため `&dyn` にできず、typed starter 関数は `C: TelemetryContext` ジェネリクスで受ける (docs/INSTRUCTIONS.md #41 同型)。コールバック内でネストスパンを張るには `|span, starter|` の2引数クロージャを受け、`span.start_child(options, ...)` を使う (upstream `startChildSpan(span, ...)` 相当)。コンテキスト直呼びの `start_span` はルートスパンになり parent が切れないので注意。
- **Google 系の ADC パスはトークン発行まで移植しない** (#52) — google-vertex 移植で判明。上流は google-auth-library が ADC ファイルから OAuth2 JWT→access token 交換を行うが、Rust ポートはトークン発行まで実装しない。ADC パス (`~/.config/gcloud/application_default_credentials.json`) の存在確認 (`adc_credentials_available`) までを実装し、実際の Bearer トークンは呼び出し側が headers で渡す前提。API-key パス (Vertex Express) だけが完全に自己完結。

## セッション運用の反省 (継続)

- **「修正した」は必ずテスト実行で確認してから言うこと**。デッドロック調査では「修正→別の箇所でハング」が連鎖した。
- フレークするテストは連続10回回して固定すること。実例: `rejects_late_publication` は 1/5 でしか落ちなかった (phase 1 が sender を消費する競合)。
- ユーザーが「止めて」と言ったら、同じ推論を繰り返さず手を止めること。
- この環境の command-guard 拡張 (`.pi/extensions/command-guard.ts`) の制約: 検証コマンド (`cargo test/check/clippy/fmt --check`) には bash ツールの `timeout` 秒指定が必須。`cargo test` は `-p <crate>` 付きで実行し、`--workspace` は `/guard allow-workspace` か確認ダイアログで明示許可。同じ検証コマンドの無変更再実行・同一ターン内の並列検証・1呼び出し内の複数 Git 操作はブロックされる。
