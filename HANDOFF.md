# 引き継ぎ指示書 (2026-08-29, 2nd update)

次のセッションで作業を続ける人/エージェント向けの引き継ぎ。規範は `AGENTS.md` と `docs/rules/` の方。ここには「現状」と「今回判明した落とし穴」を書く。

## プロジェクトの目的

pi v0.84.3 (TypeScript, commit `56700d4`) を Rust に移植する。拡張機能ランタイムだけは luaur v0.1.8 (Luau on Rust) に置き換える。ピンは `UPSTREAM.toml`。移植方針は `docs/rules/02-porting-policy.md` (外部境界は厳密互換、内部は慣用的Rust)。

## 現在の状態 (main, ワーキングツリークリーン)

| コミット | 内容 |
| --- | --- |
| `82e4333` | docs: ルールドキュメント一式 |
| `38406ec` | pillar-protocol: CBOR codec + framing + schemas (テスト49本) |
| `1c624..` | pillar-telemetry: in-memory adapter + conformance (テスト15本) |
| `adaf0e5` | pillar-ai: types, event-stream, retry, estimate, overflow, faux provider, uuid (テスト58本) |
| `f50a1dd`/`b04c4ce`/`b1b92fa` | pillar-agent: agent loop + types + abort (テスト11本) |
| `7217ebf` | **pillar-ai: auth基盤** — abort.rs (AbortSignal/AbortReason, race/timeout/any/operation_signal), auth_types.rs (Credential/ModelAuth/AuthResult, CredentialStore/ApiKeyAuth/OAuthAuth/AuthContext/AuthInteraction traits), credential_store.rs (InMemoryCredentialStore, プロバイダ単位で直列化), models_store.rs (ModelsStore trait + InMemory), auth_resolve.rs (resolve_provider_auth 完全移植: stored credential優先, OAuth二重チェックロック + refresh timeout 15s, minOAuthValidityMs, 無言のenvフォールバックなし), error.rs (AiError) |

**合計133テスト全パス。`cargo fmt --check` / `cargo clippy --workspace --all-targets -- -D warnings` クリーン。**

上流チェックアウトは `/tmp/upstream/pi`, `/tmp/upstream/luaur` (再作成手順は docs/rules/06)。

## 未移植 (優先順)

1. **pillar-ai の `Models` コレクション本体** (models.ts 残り ~700行): `ModelsImpl` (provider registry, generation-checked refresh, publication chains, applyAuth + mergeHeaders), `createProvider` factory (baseline + dynamic overlay, api map dispatch), `calculateCost` (tiered pricing, Anthropic 1h cache 2x), `getSupportedThinkingLevels`/`clampThinkingLevel`, `hasApi`, `modelsAreEqual`。**models.ts は前セッションで全文読了済み** (944行)。`models-runtime.test.ts` (1159行, 39ケース) を移植する。
2. **pillar-ai のプロバイダ層**: HTTP transport なし。pi の `packages/ai/src/api/*` (openai-completions 1707行, anthropic-messages 1391行など) と `models.generated.ts` (ジェネレータで再生成、手移植禁止 — docs/rules/01) が未移植。faux provider はあるので agent 側テストは賄える。
3. **pillar-agent の残り**: `agent.ts` (592行, Agent クラス/状態管理) 未移植。ループは完成。
4. **pillar-coding-agent**: 未着手 (最大、61k行)。
5. **pillar-tui / client / server / session-store**: 未着手。
6. **pillar-extensions**: 未着手 (luaur VM 統合)。設計は docs/rules/04 に確定済み。

## 前セッションまでに判明した落とし穴 (重要)

### 1. MutexGuard を await またぎで保持しない

`std::sync::Mutex` のガードを async ブロック内で await またぎに保持すると:

- コンパイルは通るが future が Send でなくなり `tokio::spawn` が失敗するか
- 同一ステートメント内の複数 `.lock()` で即デッドロック (一時ガードが文末まで生存するため)

パターン: データは clone してから await。ガードは明示スコープで落とす。

```rust
let value = { let g = m.lock().unwrap(); g.something.clone() };  // ガードはここで死ぬ
async_fn(value).await;                                            // ガードなしでawait
```

実例: pillar-ai/faux.rs の `resolve_response` (修正済み), pillar-agent/agent_loop.rs の `AgentEventSink::emit` (修正済み)。今回の credential_store.rs も同じパターン (ガードはブロック内で落とし、modify は per-provider の tokio::sync::Mutex で直列化)。

### 2. EventStream::result() は複数回呼べること (修正済み: b04c4ce)

元実装が `Option::take()` で結果を消費していたため、同じストリームに対して2回 `result()` を呼ぶと2回目が永久待機した。pi の Promise セマンティクスでは同じ promise を何度でも await できる。`Clone` して返す形に修正済み。**agent-loop パリティテスト3本がこのバグでハングしていた**。

### 3. ハングのデバッグ手順 (macOS)

- `cargo test` が止まったら **ビルドかテスト実行かを切り分ける**: `target/debug/deps/<test>-<hash>` バイナリのタイムスタンプとソースのタイムスタンプを比べる。バイナリが新しければ実行時ハング。
- テストバイナリを直接実行 (`--test-threads=1 --nocapture`) して特定のテストを絞る。
- **`sample <pid> <秒>` でスタックダンプを取る** — これで「ランタイムがparkしていて起動可能タスクが無い」= spawn済みタスクが消えた/全員待機、と即断定できる。今回の原因特定はこれが決め手。
- panic が握り潰されている可能性: `tokio::spawn` の JoinHandle を無視するとタスク内 panic が観測できない。

### 4. bash ツールの安全な使い方 (ハング防止)

- **バックグラウンドジョブ (`cmd &`) + `wait` は使わない** — ツールが制御を返さなくなる。
- タイムアウトは `perl -e 'alarm N; exec @ARGV' cmd...` でラップする (macOSにtimeoutはない)。
- ビルドが長引くだけの場合もある。alarmで殺す前にタイムスタンプ確認。

### 5. cargo test のタイムアウト殺しでも状態は壊れない

alarm で cargo を殺しても target/ は壊れない (rcgu オブジェクトが散在するだけ)。再実行で再開する。

## 前セッションで判明した追加の落とし穴

### 6. async_trait とライフタイムの食い違い

`#[async_trait]` を付けた trait は、impl 側にも必ず `#[async_trait]` が必要。付け忘れると E0195 (lifetime parameters do not match) が出るが、原因はライフタイムではなく async_trait の付け忘れ。impl 側の `CredentialModifier<'_>` など late-bound/early-bound の食い違いも同じエラーになる。まず async_trait の付け忘れを疑うこと。

### 7. watch チャネルの borrow は返り値で越えられない

`tokio::sync::watch::Receiver::borrow()` のガードは関数の返り値で越えられない (E0515)。`Option<&T>` を返す代わりに `Option<T>` を clone して返す (abort.rs の `reason()` が該当)。

### 8. ディスク逼迫 (228GB のうち ~51GB 空き、頻繁に 100% 付近まで行く)

- `cargo check` の incremental で target/ が膨らむ。**ユーザーが「cleanした」と言ったら target/ は消えている** — 再ビルド時間を覚悟。
- ディスク 100% で write が ENOSPC で失敗することがある。**書き込み失敗したらまず `df -h` で確認**、それからファイル状態を検証 (ENOSPC で半端なファイルが残る)。
- 上流チェックアウト `/tmp/upstream/{pi,luaur}` 合計 ~112MB。クリーンされても再クローン (docs/rules/06)。

### 9. コンパイラエラーの原因は1つとは限らない (今回の実例)

`async_trait` 付け忘れ + `Arc` の import 忘れ + `CredentialModifier` のライフタイム + watch の borrow が**同時に**起きていた。エラーを1つずつ潰すより、まず新規ファイルの import セクションと derive を一括点検する方が速い。

## 次のセッションの最初の一歩

`pillar-ai` の **`Models` コレクション本体** (models.ts 残り) を移植。`models.ts` は全文読了済みなので構造は把握済み:

- `ModelsImpl`: provider registry (`Map<id, Provider>`), `refreshGenerations`/`refreshControllers` (generation-checked refresh + supersede), `publicationChains` (provider単位の直列化された publish chain), `applyAuth` (getAuth → mergeHeaders → transformHeaders → baseUrl差し替え)
- `createProvider`: baseline + dynamic overlay の merge (`currentModels`), api map dispatch (`apiFor`), fetchModels → publish({persist, update}) の transactional flow
- `calculateCost`: tiered pricing (最高 matching threshold が全体に適用) + Anthropic 1h cache write 2x
- `getSupportedThinkingLevels`/`clampThinkingLevel`: EXTENDED_THINKING_LEVELS 順に fallback
- `hasApi`/`modelsAreEqual`: 型ガード/等価チェック
- テスト: `models-runtime.test.ts` (1159行, 39ケース) を移植。abort/credential-store/models-store/auth_resolve は既に移植済みなので、Models 本体と createProvider が主作業。

## 追記: セッション運用の反省

デッドロック調査で長時間ループに陥った。原因は (1) 複数 `.lock()` の同一ステートメント呼び出し、(2) テスト自体の2回 `result()` 呼び出し。**「修正した」は必ずテスト実行で確認してから言うこと**。また、ユーザーが「止めて」と言ったら、同じ推論を繰り返さず手を止めること。
