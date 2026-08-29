# 引き継ぎ指示書 (2026-08-29)

次のセッションで作業を続ける人/エージェント向けの引き継ぎ。規範は `AGENTS.md` と `docs/rules/` の方。ここには「現状」と「今回判明した落とし穴」を書く。

## プロジェクトの目的

pi v0.84.3 (TypeScript, commit `56700d4`) を Rust に移植する。拡張機能ランタイムだけは luaur v0.1.8 (Luau on Rust) に置き換える。ピンは `UPSTREAM.toml`。移植方針は `docs/rules/02-porting-policy.md` (外部境界は厳密互換、内部は慣用的Rust)。

## 現在の状態 (main, ワーキングツリークリーン)

| コミット | 内容 |
| --- | --- |
| `82e4333` | docs: ルールドキュメント一式 |
| `38406ec` | pillar-protocol: CBOR codec + framing + schemas (テスト49本) |
| `1c624`.. | pillar-telemetry: in-memory adapter + conformance (テスト15本) |
| `adaf0e5` | pillar-ai: types, event-stream, retry, estimate, overflow, faux provider, uuid (テスト58本) |
| `f50a1dd`/`b04c4ce`/`b1b92fa` | pillar-agent: agent loop + types + abort (テスト11本) |

**合計133テスト全パス。`cargo fmt --check` / `cargo clippy --workspace --all-targets -- -D warnings` クリーン。**

上流チェックアウトは `/tmp/upstream/pi`, `/tmp/upstream/luaur` (再作成手順は docs/rules/06)。

## 未移植 (優先順)

1. **pillar-ai のプロバイダ層**: HTTP transport なし。pi の `packages/ai/src/api/*` (openai-completions 1707行, anthropic-messages 1391行など) と `models.ts` (944行, Models/Provider/auth解決), `auth/*` (616行) が未移植。faux provider はあるので agent 側テストは賄える。
2. **pillar-agent の残り**: `agent.ts` (592行, Agent クラス/状態管理) 未移植。ループは完成。
3. **pillar-coding-agent**: 未着手 (最大、61k行)。
4. **pillar-tui / client / server / session-store**: 未着手。
5. **pillar-extensions**: 未着手 (luaur VM 統合)。設計は docs/rules/04 に確定済み。

## 今回のセッションで判明した落とし穴 (重要)

### 1. MutexGuard を await またぎで保持しない

`std::sync::Mutex` のガードを async ブロック内で await またぎに保持すると:

- コンパイルは通るが future が Send でなくなり `tokio::spawn` が失敗するか
- 同一ステートメント内の複数 `.lock()` で即デッドロック (一時ガードが文末まで生存するため)

パターン: データは clone してから await。ガードは明示スコープで落とす。

```rust
let value = { let g = m.lock().unwrap(); g.something.clone() };  // ガードはここで死ぬ
async_fn(value).await;                                            // ガードなしでawait
```

実例: pillar-ai/faux.rs の `resolve_response` (修正済み), pillar-agent/agent_loop.rs の `AgentEventSink::emit` (修正済み)。

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

## 次のセッションの最初の一歩

`pillar-ai` の `models.ts` ポート (Models/Provider/auth解決) を推奨。faux provider があるので `pillar-agent` のテストは既に賄えており、認証解決 (`auth/resolve.ts` 205行) が coding-agent の土台になる。テストは `models-runtime.test.ts` (1159行, 39ケース) を移植する。

## 追記: セッション運用の反省

デッドロック調査で長時間ループに陥った。原因は (1) 複数 `.lock()` の同一ステートメント呼び出し、(2) テスト自体の2回 `result()` 呼び出し。**「修正した」は必ずテスト実行で確認してから言うこと**。また、ユーザーが「止めて」と言ったら、同じ推論を繰り返さず手を止めること。
