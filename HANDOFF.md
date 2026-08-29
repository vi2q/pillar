# 引き継ぎ指示書 (2026-08-29, 3rd update)

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
| 今回2 | **feat(ai): プロバイダ層の共有インフラ移植** (transport.rs [FetchFn トレイト + reqwest 既定実装], provider_retry.rs, error_body.rs, provider_env.rs, constrained_sampling.rs, transform_messages.rs, headers.rs, text.rs に sanitize_surrogates)。`tests/api_infra_parity.rs` 27ケース |

**pillar-ai 124テスト (core 35 + faux 22 + models-runtime 39 + api-infra 27 + 1) と pillar-agent 11テスト全パス。`cargo fmt --check` / `cargo clippy --workspace --all-targets -- -D warnings` クリーン。**

上流チェックアウトは `/tmp/upstream/pi`, `/tmp/upstream/luaur` (再作成手順は docs/rules/06)。

## 未移植 (優先順)

1. **pillar-ai のプロバイダ本体**: 共有インフラ (transport / provider-retry / error-body / constrained-sampling / transform-messages / provider-env / headers) は移植済み。次は pi の `packages/ai/src/api/*` 各プロバイダ: `openai-completions.ts` (1707行) が第一候補。`FetchFn` を注入し、SSE は手前でパース、WebSocket transport は後回し。`openai-responses-shared.ts` (792行) が openai-responses/codex/azure と共通。`models.generated.ts` はジェネレータで再生成、手移植禁止 (docs/rules/01、生成器は pillar-ai/src/bin/generate-models.rs に作る)。
2. **pillar-agent の残り**: `agent.ts` (592行, Agent クラス/状態管理) 未移植。ループは完成。
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

### 16. プロバイダ層の意図的な divergence (今回確定)

- `error_body.rs`: 上流は SDK エラーオブジェクトのフィールドを掘る (Mistral/openai/genai/Bedrock 形, pipe スニッフィング, class instance 判定)。Rust に SDK オブジェクトは無いので `normalize_provider_error(message, status, body)` が transport から受け取る形。`format_provider_error` / truncation は厳密互換。`provider-error-body-passthrough/regression.test.ts` は SDK レベルなので非移植。
- `provider_retry.rs`: `retry-after` は数値のみパース (HTTP-date は指数バックオフにフォールバック)。jitter は fastrand。
- `provider_env.rs`: Bun サンドボックスの /proc フォールバックは非移植。
- `sanitize_surrogates` (text.rs): Rust の String は UTF-8 で unpaired surrogate を保持できないため恒等関数。上流と同じ呼び出し点を維持するための grep-parity 用。
- `transport.rs`: 上流 `FetchFunction` は WHATWG Response を返すが、Rust は `FetchFn` トレイト + ストリーミング `FetchResponse`。既定実装は `ReqwestFetch` (rustls + gzip + stream)。

## 次のセッションの最初の一歩

**最初のプロバイダ移植: `openai-completions.ts` (1707行)**。`transport::FetchFn` を介して HTTP、`provider_retry::retry_provider_request` でリト包装、`error_body` でエラー組立、`constrained_sampling` / `transform_messages` を流用。上流テストは `openai-completions*.test.ts` 系 (stream 形状, SSE パース, tool calls, reasoning) をモック FetchFn で再現する。その次は `openai-responses-shared.ts` と `anthropic-messages.ts`。

## セッション運用の反省 (継続)

- **「修正した」は必ずテスト実行で確認してから言うこと**。デッドロック調査では「修正→別の箇所でハング」が連鎖した (credential_store の select 修正だけでは直らず、abort の watch/send 問題とテストの lazy-future 問題が重なっていた)。
- フレークするテストは連続10回回して固定すること。今回 `rejects_late_publication` は 1/5 でしか落ちなかった (phase 1 が sender を消費する競合)。
- ユーザーが「止めて」と言ったら、同じ推論を繰り返さず手を止めること。
