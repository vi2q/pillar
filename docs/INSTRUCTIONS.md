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

## 現在の状態 (2026-09-01)

全ワークスペース 837テストがパス (protocol 49 / telemetry 15 / ai 345 / agent 301 / coding-agent 118 / tui 9)。**cache-stats 完了 (6cc1d02)**: core/cache_stats.rs (cache-stats.ts 164行) — プロンプトキャッシュ浪費の集計。missedTokens = min(prev.promptTokens, promptTokens) - cacheRead、ノイズフロア 1024 以下は無視、zero-cache ターンは prior に cache 報告があった時のみカウント (reportedCache sticky — cache-read-only provider の total miss と非対応 provider の区別)、compaction/branch_summary で scan リセット (model 切替はリセットされない)、missedCost = missedTokens x (このメッセージの paid rate − read rate、read 0 なら pricing source フォールバック)。モデル価格は ModelPriceSource トレイトに抽象 (upstream は AssistantMessage 直接参照 — ポートは使用フィールドに絞った CacheStatsMessage を定義し SessionEntry に依存しない)。+ parity 16テスト。**resolve-config-value 完了 (未コミット)**: core/resolve_config_value.rs (resolve-config-value.ts 287行) — config 値解決 (`!command` シェル実行 + プロセス生存キャッシュ、`$VAR`/`${VAR}` 補間、`$$`/`$!` エスケープ、リテラル)、env var 名ヘルパ (単一参照/一覧/欠損検出)、resolve_config_value_or_throw (コマンド/単一 env/複数 env のエラーメッセージ)、resolve_headers (+ or_throw、解決不能ヘッダは脱落)。+ parity 24テスト。divergence: コマンド実行は `sh -c` 単経路 (upstream は Windows で設定シェル + fallback、他は execSync — ポートは POSIX 想定); env 値は空文字を未設定扱い (upstream `||` セマンティクス)。**skills ディレクトリ走査完了 (未コミット)**: core/skills.rs (skills.ts 507行の走査部) — load_skills_from_dir (SKILL.md で停止する再帰、ルート直下 .md 読み込み、node_modules/dotfile スキップ)、load_skills (user agentDir/skills → project cwd/pi/skills → 明示パス、~ 展開、実パス重複排除、名前衝突は先勝ち + collision diagnostic)、SKILL.md YAML パーサを block scalar 対応に拡張 (| 改行結合 / > 空白結合 — 複数行 description)、未閉鎖 bracket/brace は "at line N" エラー、ignore パターン近似 (gitignore/ignore/fdignore、suffix/segment マッチ — `ignore` クレート不使用)、name 検証メッセージに文字数埋め込み。+ parity 22テスト。divergence: ignore マッチは近似 (upstream は ignore クレートの完全 gitignore セマンティクス); sourceInfo は host 側で管理。**model-config 完了 (未コミット)**: core/model_config.rs (model-config.ts 232行) — models.json の Immutable スナップショット。serde 構造バリデーション (typebox スキーマ相当: Model/Override/Provider/Compat 3系統/cost tiers)、JSONC 対応 (手書き stripJsonComments — 行コメント除去 + 末尾カンマ除去、文字列内は保護、lookahead 方式)、欠損ファイルは空 config、parse/schema エラーは error 文字列にパス付きで記録。+ parity 12テスト。divergence: typebox の union は宣言順で先勝ち & unknown field を無視するため、responses/anthropic 形の compat も completions variant に解決される (実害なし — compat は API 型経由で消費)。**pillar-tui クレート新設 + keybindings 完了 (未コミット)**: crates/pillar-tui — keys.rs (keys.ts 1300行の一致判定コア: legacy シーケンス / Kitty CSI u / xterm modifyOtherKeys / raw ctrl、 Kitty プロトコル active フラグはグローバル状態、raw backspace の WT 判定は env 参照 — テストでは process env 制御が必要) + keybindings.rs (TUI_KEYBINDINGS 45定義、user override は該当 binding のみ置換し他の default を退避しない、直接 user binding の key 衝突のみ検出)。coding-agent core/keybindings.rs — App 定義 42個 (Windows/WSL/darwin 分岐、win32 native は ctrl+z が windowsKeybindings の alt+z より優先 — spread 順)、legacy 名 migration (60名、新旧同名時は namespaced 側を残し migrated=true)、keybindings.json 読み込み (非文字列エントリは破棄)、reload/create。+ parity 13テスト (tui 9 + coding-agent keybindings 9 / migration 3 / windows 5)。divergence: 上流は宣言マージで Keybindings 型を拡張するがポートは BTreeMap 定義; editor-history 優先は registry ではなく editor コンポーネント側の責務。**prompt-templates 完了 (d8eb159)**: core/prompt_templates.rs (上流 prompt-templates.ts 285行) — parse_command_args (bash 方式クォート対応)、substitute_args ($1/$2, $@/$ARGUMENTS, ${N:-default}, ${@:N}, ${@:N:L} bash 式スライス、再帰置換なし)、load_prompt_templates (agentDir/prompts + cwd/pi/prompts + 明示パス、include_defaults 制御)、expand_prompt_template (/name args 展開、未一致は原文)。frontmatter は最小行パーサ (description/argument-hint、description 未設定時は最初の非空行を60文字で切り詰め「...」)。+ parity 19テスト。divergence: yaml パッケージ不使用 (消費キーはスカラーのみ); 上流テストは vitest fixture、ポートは temp_dir。**pillar-coding-agent クレート新設 + system-prompt 完了 (未コミット)**: `crates/pillar-coding-agent` を workspace に追加。core/system_prompt.rs (system-prompt.ts 移植, ~350行) — build_system_prompt (デフォルト/カスタムプロンプト、ツールスニペット、guideline 重複排除、project_context 追加、skills XML セクション)、format_skills_for_prompt (agentskills.io XML、disable_model_invocation 除外)、SKILL.md frontmatter パーサ + name/description 検証 (Agent Skills spec)。+ parity 16テスト全パス (system-prompt.test.ts 相当)。divergence: pi docs パスは PromptPaths として host 注入 (上流は package dir 解決); skills のディレクトリ走査 (ignore パターン付き) は未移植 — frontmatter 検証とフォーマットのみ。**models catalog ジェネレータ + builtin providers 完了 (未コミット)**: `src/bin/generate-models.rs` (上流 generate-models.ts の移植) — models.dev api.json + OpenRouter /v1/models + Vercel AI Gateway /models を取得し、compat 検出 / thinking-level map / 価格 override / static catalog (codex, deepseek-v4, ant-ling, azure clone 等) を適用して `src/models_generated.rs` (39 providers / 1371 モデル) を生成。`src/models_catalog.rs` が JSON を Model に変換。`src/providers_all.rs` が builtin provider 登録 (auth 定義: envApiKeyAuth / Cloudflare account+gateway / Bedrock cred chain / Vertex ADC / Radius placeholder) と `builtin_models()` を提供。+ parity 4テスト。divergence: 上流は data/*.json (gitignored) + .models.ts shard を出すが、ポートは単一 generated rs に集約; stream dispatch も `src/api_dispatch.rs` で結線済み (StreamRequestOptions → 各アダプタ Options の From 変換を 10 API に実装、google_vertex.rs は未登録だったので api/mod.rs に登録して修復)。catalog/auth/stream の 3 層がすべて生きた。**images-models 完了 (未コミット)**: images_models.rs — ImagesModels / ImagesProvider / create_images_provider (in-flight refresh dedupe via oneshot slot)。auth 解決は共有 resolve_provider_auth、options との per-field マージ (apiKey は explicit 優先 / headers・env は key マージ)、未設定 auth でもディスパッチ継続、不明 provider は error 結果。+ parity 7テスト全パス。**images 完了 (未コミット)**: openrouter_images.rs — images 型群 (ImagesModel/Context/AssistantImages/StopReason/Options + ImagesOnPayloadFn/ImagesOnResponseFn) と openrouter-images アダプタ (唯一の images API)。chat/completions 非ストリーミング POST、modalities (text 出力対応で [image,text])、data: URL パース (prefix チェックのみ, MIME allow-list なし — 上流同様)、object 形 image_url 対応、usage の cached/cache_write 差引計算、onPayload/onResponse フック。+ parity 9テスト全パス (live E2E images.test.ts は OPENROUTER_API_KEY 依存で非移植)。**pi-messages 完了 (4a9b015)**: pi_messages.rs (~900行) — pi 独自ワイヤプロトコル (Radius ゲートウェイが話すもの)。POST {model, context, options} を `<baseUrl>/messages` へ送り、SSE でシリアライズ済み assistant-message イベントを受信、terminal done/error で終了。イベントコンバータ (text/thinking/toolcall start/delta/end、スパース slot 補間)、toolcall の streaming-JSON パース、rewrite impact 診断、debug=1 クエリ、PI_CACHE_RETENTION レガシー env、非2xx は error-body パース + pi_messages_response_failure 診断。+ parity 6テスト。ターミナル done の reason は文字列 (stop/length/toolUse) で来るので StopReason 変換が必須 (初期バグ)。`cargo fmt --check` / `cargo clippy` (クレート毎に `-D warnings`) クリーン。**azure-openai-responses 完了 (未コミット)**: azure_openai_responses.rs (~700行) + azure_openai_responses_parity.rs 24テスト全パス。Azure URL 正規化 (/openai/v1 への書き換え, 非Azure プロキシの保持, クエリ除去)、AZURE_OPENAI_BASE_URL / RESOURCE_NAME / API_VERSION / DEPLOYMENT_NAME_MAP 解決 (scoped env 注入)、deployment name フォールバック、store:false、prompt_cache_key クランプ、strict-mode compat、pi User-Agent + ヘッダ上書き、tool_choice 転送、reasoning encrypted_content バックフィル (terminal response から output_item.done 欠落分を補完)。divergence: `openai` npm SDK (AzureOpenAI クライアント) を使わず生リクエスト + `?api-version=` クエリ; `url` クレート不使用の手動パース (bedrock と同方針)。**openai-codex-responses 完了 (60759ac)**: openai_codex_responses.rs (2.5k行) + openai_codex_stream_parity.rs 20テスト全パス (SSE 15 / WebSocket 5)。SSE トランスポート (zstd 圧縮 #zstd crate, Codex ヘッダ, リトライ/指数バックオフ, friendly usage-limit メッセージ, service tier 料金 flex 0.5x/priority 2x/2.5x, accountId JWT 抽出, cache key/session-id 64文字クランプ) + WebSocket トランスポート (WsConn/WsConnFactory トレイト注入 — 上流は global WebSocket をモック, 接続プール per (sessionId, accountId), previous_response_id 継続, SSE フォールバック, 接続タイムアウト, idle タイムアウト)。divergence: strict: null は共有ヘルパーの Option<bool> で表現できないため Codex アダプタで false→null リマップ; Bun プロキシ WebSocket 非該当; Date.parse(retry-after) フォールバック省略。**bedrock-converse 完了 (cc1eac9)**: bedrock_converse_stream.rs (2.3k行, AWS SDK不使用 — FetchFn + 生HTTP + AWS eventstream バイナリフレーミングデコーダ #54) + bedrock_converse_stream_parity.rs 40テスト全パス。テスト修正過程で判明した落とし穴: テストヘルパーの `assistant_message()` が model ID を base_model にハードコードすると `transform_messages` の `isSameModel` 判定が true になり tool call ID 正規化・redacted thinking 保持が発火しない (#58)、struct update 構文のデフォルト上書き (#57)、ambient AWS_PROFILE は process env 制御が必要 (#55)。**google 系 API 完了 (9717334)**: google-shared.rs + google-generative-ai.rs + google-vertex.rs + 27テスト。**mistral-conversations.rs 済み (ab4e80a + 300d7f9)**: SSE パーサ (境界はバイト検索 #53)。harness session の jsonl バックエンド済み (91a03b0 + f20fe8e) + jsonl パリティテスト 3スイート (#45/#47 バグ修正, #46 destination 予約, #47 spawn_blocking+inline runtime)。proxy.ts 済み (6ed4f6c)。search/ 済み (187bcf2)。e2e 済み (e1c55c1)。telemetry.ts スパン開始部完了 (fa2fe02)。

移植メモ (jsonl): `FileSystem` は RPITIT で dyn 非対応のため `JsonlSessionStorage<FT: FileSystem + ?Sized>` / `JsonlSessionRepo<F: FileSystem + 'static>` はジェネリクスで受ける (docs/INSTRUCTIONS.md #41)。書き込みは `SessionStorage` トレイトの sync メソッド内で `spawn_blocking` + インライン current_thread ランタイムにより append を駆動し、state mutex で直列化 (#47)。torn-tail 修復は `publishFileAtomically` (tmp + rename) を再現。

| クレート | テスト | 状況 |
| --- | --- | --- |
| pillar-protocol | 49 | ✅ 完了 |
| pillar-telemetry | 15 | ✅ 完了 |
| pillar-ai | 345 | 🔶 コア + 全プロバイダ + カタログ + builtin registry (core 35 / faux 22 / models-runtime 39 / api-infra 27 / openai-completions 23 / openai-responses 14 / anthropic-messages 27 / uuid 1 / google-shared 27 / mistral 20 / bedrock 40 / openai-codex 20 / azure 24 / openrouter-images 9 / pi-messages 6 / images-models 7 / providers-all 4) |
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
| ai | 27.7k | 35.1k | 🔶 約9割 (src 24.5k / tests 13.2k。残り: provider stream 結線の仕上げ) |
| agent | 12.9k | 8.6k | ✅ ほぼ完了 (src 16.2k / tests 10.6k。残り: live-API 依存の e2e・models_generated.rs のみ。search 208行 / e2e 415行 / telemetry.ts 615行 + docs renderer 117行 / reducer / result / agent-harness / tools 1203 / session jsonl 848 / proxy 370 / branch-summarization 済み) |
| coding-agent | 78.9k | 50.3k | 🔶 着手 (system-prompt 16 + prompt-templates 19 + keybindings 9 + model-config 12 + skills 22 + resolve-config-value 24 + cache-stats 16 + model-resolver 48 + provider-composer 32 + 小規模 core 群 20 + auth-storage/models-store 11 + model-runtime 15 + messages 18 + compaction 23 + session-entries/branch-summarization 11 + session-manager 22 + settings-manager 22 + trust-manager/pi-manifest 16 + export-html 10 + truncate 17 + bash-executor/exec 10 + tools (output-accumulator/path-utils) 12 + tools (render-utils/file-mutation-queue) 10 + tools/ls 11 + tools/search (FFF 型ネイティブ find/grep) 21 テスト) |
| tui | 17.9k | 16.4k | 🔶 着手 (keys + keybindings 9テスト) |
| server / client / session-backends | 6.3k | 4.2k | ❌ 未着手 |
| evals | 1.3k | 0.5k | ❌ 対象外の可能性 |

**体感 2割強。** 土台層 (protocol / telemetry / ai コア / agent コア) は最難関部 (SSE パーサ・非同期セマンティクス・イベント順序の厳密互換) を含めて固まっており、537テストで保護済み。残り約7割は coding-agent (79k) と tui (18k) で、両者とも土台の上に載せる形なので行数比よりは速く進む見込み。

## 作業指示チェックリスト

ユーザー指示を記録し、進捗に合わせて更新する。

- [x] luaur PR #40 のレビュー反映 (2026-09-02)
  - [x] PR 本文を現行実装・テスト結果に合わせて更新する (tuple fast path / serde 回帰テスト / 253 tests)
  - [x] serde 回帰テストの `map_err(|e| e)?` を単純な `?` に整理し、commit `2ae9ea74` を push (2026-09-02)
  - [x] 検証: 対象 serde 回帰テスト・`cargo fmt -p luaur-rt --check` パス。push 後 CI 6ジョブ全パス
  - [x] Confirm (user): PR 上の最終内容の承認 — user-confirmed (2026-09-02)

- [x] luaur follow-up PR #40 の修正後レビュー (2026-09-02)
  - [x] commit `37636ee5` 以降の実装・テスト・PR 本文を再確認する (2026-09-02)
    - 実装: tuple の stack fast path は正しく伝播。3-tuple・nested tuple・戻り値不足を追加プローブして全パス
    - テスト: Rust→Lua→Rust、mixed tuple、serde/JSON 回帰ケースが追加され、初回指摘は解消
    - CI: Ubuntu / Windows / macOS / fmt / wasm / clean-check の全ジョブ成功
    - 残件: PR 本文が更新前のまま。`252 passed`→`253 passed`、serde の「verified manually」→コミット済み回帰テスト、tuple fast path の説明追加が必要。`map_err(|e| e)?` は単純な `?` にできる軽微な nit
  - [x] Confirm (user): 修正後レビュー結果の承認 — user-confirmed (2026-09-02)

- [x] luaur follow-up PR #40 のレビュー (2026-09-02) — **指摘4点すべて修正済み・push済み (37636ee5)**
  - [x] PR 本文・実装・PR #37 コメントを確認し、異議と修正文案を提示する (2026-09-02)
    - 要修正: tuple の `FromLuaMulti` が `from_stack_multi` を override しておらず、`eval::<(f64,)>()` 等では引き続き `-0.0` の符号を失う (mlua の tuple 実装は stack fast path を伝播する)。再現テストで確認 → **修正**: `impl_tuple_from` の各アリティに `from_stack_multi` を override 追加 (各 `FromLua` スロットはスタック直読み、1-tuple は全域転送)。`(f64,)` / `(f64,f64)` / `(i32,f64)` プローブで符号保持を確認
    - テスト記述: Rust→Lua→Rust の素通しケースが実装されておらず、同一 assertion が重複。PR 本文の説明とも不一致 → **修正**: 重複 assertion 削除し、`call::<(f64,f64)>(-0.0)` + mixed `(i32,f64)` tuple の Rust→Lua→Rust 素通しに書き換え
    - 添削: f32 の value-level conversion error は従来の `to: f64` から `to: f32` に変わるため「error messages unchanged」は不正確。serde 回帰ケースはコミット済みテスト化を推奨 → **修正**: f32 は `f64::from_lua` 委託に戻しエラーメッセージを従来通り維持、serde 回帰ケースを `mlua_serde.rs` に `test_from_value_whole_number_floats_stay_integers` として追加 (JSON 形状検査込み)
    - PR #37 コメント自体は適切。#40 が後継であるため #37 は close 推奨 → maintainer の判断待ち (自分では close しない)
  - 検証: luaur-rt 253 tests / mlua_serde 21 tests (serde feature) / workspace 5725 tests 全パス、fmt クリーン、clippy luaur-rt 内エラーなし
  - [x] Confirm (user): 修正内容の承認 — user-confirmed (2026-09-02)

- [x] 考察: 今後の独自性としての「Rust製エージェントランタイム + id_effect」の相性と方向性 (2026-09-02) — user-confirmed (2026-09-02)
  - 結論: 外部境界は厳密互換を維持し、実行モデル (プロバイダ層/ツール実行層/拡張ランタイム) への Effect セマンティクス導入を独自性の中核にする。id_effect / id_effect_ai は pillar から独立した leaf として育成
  - 移行は 3段階: (1) 移植完了後、id_effect でプロバイダ層を対照実験的に再実装 (#53/#49/#3/#4 のバグクラスが消えるか既存 parity テストで検証) → (2) 新規コンポーネント (pillar-extensions / server / session-store / agent-harness 操作本体) を Effect ベースで → (3) コア (agent-loop) は外部 API 互換のまま内部 Runtime を吸収
  - **ユーザー確認済み: 完全移植完了後の道筋であり、移植完了前には実装に着手しない** (対照実験含む)。移植を最優先する
- [ ] 「docs/INSTRUCTIONS.md から引き継いで続きを頼む」— 次の作業キューに従って移植を継続する (2026-08-31)
  - [x] pillar-ai: google 系 (google-shared / google-generative-ai / google-vertex / providers/google*) — 9717334, 27テスト (9717334)
  - [x] pillar-ai: mistral-conversations — ab4e80a + 870fca8 (adapter + 20 parity tests)
  - [x] pillar-ai: bedrock-converse — アダプタ完成 + パリティテスト 40ケース全パス (2026-09-01)。AWS SDK 未使用, FetchFn + 生 HTTP + AWS eventstream (vnd.amazon.eventstream) バイナリフレーミングデコーダで構築 (google-vertex #52 同型 divergence: SigV4 サインクリック対策なし, bearer token / skip-auth 経路のみ自己完結)。初期6失敗はすべてテスト側のバグだった: ヘルパーの model ハードコード (#58), struct update のデフォルト上書き (#57), ambient profile の process env 制御 (#55), capture_payload の cache_retention 強制 — 実装の修正は不要だった。clippy: ptr_arg/type_complexity/未使用ヘルパー整理済み
  - [x] pillar-ai: openai-codex — アダプタ (2.5k行) + パリティテスト 20ケース全パス (2026-09-01, 未コミット)。SSE (zstd 圧縮, Codex ヘッダ, リトライ, usage-limit friendly メッセージ, service tier 料金, JWT accountId 抽出, 64文字クランプ) + WebSocket (WsConn/WsConnFactory トレイト注入 — 上流は global WebSocket モック; 接続プール per (sessionId, accountId), previous_response_id 継続, SSE フォールバック, connect/idle タイムアウト)。divergence: strict:null はアダプタ側で false→null リマップ; Date.parse(retry-after) 省略; Bun プロキシ非該当。live-API (oauth 486行, cache-affinity e2e) は非移植
  - [x] pillar-ai: azure — アダプタ (~700行) + パリティテスト 24ケース全パス (2026-09-01, 未コミット)。URL 正規化 / env 解決 / deployment 名 / strict-mode / tool_choice / reasoning replay。divergence: npm SDK を使わず生リクエスト + api-version クエリ; 手動 URL パース
  - [x] pillar-ai: images — openrouter-images アダプタ + images 型群 + parity 9テスト全パス (2026-09-01, 未コミット)。data: URL / modalities / usage 計算 / abort・error 結路。live E2E は非移植
  - [x] pillar-ai: pi-messages — アダプタ (~900行) + parity 6テスト全パス (4a9b015)。pi 独自ワイヤプロトコル (Radius ゲートウェイ)。ターミナル reason は文字列で来るので StopReason 変換が必須
  - [x] pillar-ai: images-models — ImagesModels コレクション + create_images_provider + parity 7テスト全パス (2026-09-01, 未コミット)。in-flight refresh dedupe は oneshot slot 方式 (上流は共有 Promise)
  - [x] pillar-ai: providers/all + generate-models — カタログジェネレータ (39 providers / 1371 モデル) + builtin provider 登録 + parity 4テスト (2026-09-01, 未コミット)。stream dispatch の per-API option 変換は未結線 (pending stub)
  - [ ] pillar-ai: providers/*
- [ ] luaur 上流 PR への返答対応 (2026-09-01)
  - [x] 状況確認: PR #38 (__index meta-method on fieldless userdata) と #39 (Table raw operations の stack slot 予約) はマージ済み、#37 (-0.0 の符号ビット保持) は CHANGES_REQUESTED で保留
  - [x] maintainer レビューの検証: (1) mlua 0.10.5 の `stack_value` は -0.0 を `Value::Integer(0)` に折り畳むため Value 層では 当PRの DEVIATION 記述は誤り、(2) 差分は `is_exact_integer(n) && n != 0.0` のため **plain 0 も Number(0.0) になり serde (deserialize_any → visit_f64) と JSON 形状を壊す** — どちらも指摘通り。真の修正層は f64/f32 の from_stack fast path (luaur の呼び出し経路は exec_raw が一旦 `Vec<Value>` に materialize してから `FromLuaMulti` するので、値を Value 経由で復元する限り -0.0 の符号は失われている)
  - [ ] 返答ドラフト提示・送信 — 送信済み (2026-09-01)。serde リグレッションの承認 + follow-up PR (#40) の案内をコメントした。<https://github.com/pjankiewicz/luaur/pull/37#issuecomment-5502370281>
  - Confirm (user): 返答文の承認 — 承認不要で follow-up 対応の一環としてユーザー指示で送信、内容は PR 作業で実施 (2026-09-01)
- [x] luaur PR #37 への follow-up PR: f32/f64 の from_stack fast path (2026-09-01) — **PR #40 作成済み** <https://github.com/pjankiewicz/luaur/pull/40>
  - 実装: `FromLua` に `#[doc(hidden)] unsafe fn from_stack(idx, lua)` (default は value_from_stack → from_lua)、`FromLuaMulti` に `from_stack_multi(base, nvals, lua)` (単一値 blanket impl は from_stack に転送)、`f64/f32` が `lua_tonumberx` 直読みを override、`Function::call` / `Lua::exec_raw` の結果回収を `R::from_stack_multi` 経由に変更 (ここで符号が失われていた)。`value_from_stack` の整数正規化は変更せず serde 無傷
  - 検証: luaur-rt 252 tests / workspace 5725 tests 全パス (以前の 22 失敗はバイナリ未ビルドが原因で `cargo build --workspace` 後はパス)、serde regression ケース (Cfg{count:0,idx:0,ratio:0}) --features serde でパス、`cargo fmt --check` クリーン、clippy は luaur-rt 内エラーなし (luaur-common の既存エラーは CI ゲート外)
  - 落とし穴メモ: luaur VM 自体は `-0.0` を保持しており、符号喪失の実体は exec_raw/function.rs が `Value` を経由して戻り値を materialize する箇所にあった。mlua 同様の from_stack フックが正しい修正層。fork (vi2q/luaur) の main は上流に3コミット遅れていたため branch は上流 main ベースで作成
  - Confirm (user): PR 本文と送信の承認 — 承認付きで送信済み (2026-09-01, ユーザー指示「修正を加えて欲しい」に基づき実施)

## 次の作業キュー

1. **agent の残り**: search/ (187bcf2) / e2e のモック可能部 (e1c55c1) は**完了**。残りは live-API 依存の e2e (responseid, xhigh, tool-call-without-result, tool-call-id-normalization — 非移植) と models_generated.rs (generate-models ジェネレータ, live カタログ依存) のみで、**agent クレートは実質完了**。agent-harness の操作本体 (prompt/compact/resume/watch 等) の依存先は揃ったので次は着手可能。→ 次: pillar-ai のプロバイダ残り または pillar-coding-agent 着手。
2. **pillar-ai**: **完了**。カタログジェネレータ (39 providers / 1371 モデル) + builtin provider 登録 + stream dispatch 結線済み。`cargo run -p pillar-ai --bin generate-models` で `models_generated.rs` を再生成できる。残るは live-API 依存の検証のみ (非移植方針)。→ 次: pillar-coding-agent 着手。
3. **pillar-coding-agent**: 着手。小規模 core 群 + auth-storage/models-store + model-runtime/runtime-credentials + messages + compaction + session-entries/branch-summarization + session-manager + settings-manager + trust-manager/pi-manifest + export-html + usage-totals 群 + truncate/strip-ansi/sanitize + bash-executor/exec + tools (output-accumulator/path-utils/render-utils/file-mutation-queue/ls/search) 完了。find/grep は FFF (dmtrKovalenko/fff) 設計参考のネイティブ実装に置換済み。残り: package-manager (2.7k行) / agent-session (3.5k行) / extensions ランナー系 / tools 本体 (bash/read/edit/write)。
4. **pillar-tui / client / server / session-store**: pillar-tui の keys + keybindings 完了。次の候補: エディタ/セレクタ コンポーネント、VirtualTerminal テスト基盤。client / server / session-store は未着手。
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
- **Mistral SSE: `finish_reason: null` není stop reason a boundary hledej v bajtech** (#53) — mistral-conversations 移植で判明。上流 `if (choice.finish_reason)` は null/欠落を falsy としてスキップする (ストリームは継続)。null を「stop」として即時確定させると、途中チャンクで stop に落ちて後続データが無視される。また SSE 境界の検索はバイト列で行うこと — `String::from_utf8_lossy` はマルチバイト UTF-8 がチャンク境界で分断されると置換文字 (3バイト) に展開され、元のバイト列とインデックスがずれて `buffer.drain` が壊れる。区切り文字はすべて ASCII なのでバイト検索が安全。abort/timeout は fetch だけでなく body 読み取りにも掛ける (upstream `AbortSignal.any([signal, timeout])` 相当)。
- **Google 系の ADC パスはトークン発行まで移植しない** (#52) — google-vertex 移植で判明。上流は google-auth-library が ADC ファイルから OAuth2 JWT→access token 交換を行うが、Rust ポートはトークン発行まで実装しない。ADC パス (`~/.config/gcloud/application_default_credentials.json`) の存在確認 (`adc_credentials_available`) までを実装し、実際の Bearer トークンは呼び出し側が headers で渡す前提。API-key パス (Vertex Express) だけが完全に自己完結。

- **Bedrock eventstream はバイナリフレーム、CRC は長さフレーミングで代用** (#54) — bedrock-converse 移植で判明。ConverseStream のレスポンス体は SSE ではなく AWS eventstream 二進プロトコル ([4B total][4B headers-len][4B payload-len][headers][4B headers-CRC][payload][4B msg-CRC])。ヘッダ値型 7 (string) は 2 バイト長プレフィックス付き、他の型 (bool/byte/short/int/long/bytearray/timestamp/guid) は正しいワイアサイズでスキップすること (誤ったサイズは後続フレーム全体を壊す)。exception フレームは `:error-code` ヘッダにモデル化エラーコード。CRC 検証はせず長さフレーミングで信頼 (divergence)。
- **テストヘルパーの AssistantMessage は model をターゲットモデルに合わせる** (#58) — bedrock-converse パリティテストで判明。`assistant_message()` ヘルパーが `model` フィールドを base_model の ID にハードコードすると、`transform_messages` の `isSameModel` 判定が true になり tool call ID 正規化と redacted thinking 保持が発火しない (両者は cross-model のみ有効)。cross-model 挙動を検証するテストでは `assistant_message_from(&foreign_model, ...)` を使う。逆に same-model 挙動 (redacted reasoning 再生) を検証する場合はターゲットモデルと同じ ID で構築する。
- **ヘルパーにオプションを押し込むと別テストの前提を壊す** (#59) — bedrock-converse パリティテストで判明。`capture_payload` ヘルパーに `cache_retention: None` を焼き込むと、cache-point 注入を検証するテストが必ず失敗する (ヘルパー経由の全テストに設定が波及する)。ヘルパーは上流の共通前提 (abort + onPayload) だけを固定し、テスト固有の設定 (cache_retention など) は各テスト側で上書きする。ヘルパー変更後は全呼び出しテストを再実行すること。
- **global WebSocket をモックするテストは WsConn/WsConnFactory トレイト注入で置換** (#60) — openai-codex 移植で判明。上流は `vi.stubGlobal("WebSocket", MockWebSocket)` でイベントリスナー型の双方向ストリームを模倣する。Rust では `WsConnFactory::connect(url, headers, signal, timeout)` / `WsConn::{send, close, recv, is_reusable}` に抽象化し options 経由で注入する。モックの注意点: (a) スクリプト済みメッセージは `send` 内で同期 push し `terminal_sent` フラグを立てる — spawn+sleep 方式だと recv が終端を検出できずハングする、(b) `recv` はキュー空 & terminal_sent で `None` を返して clean close を模倣する、(c) `busy` な接続は上流同様 one-shot (プールに入れない) で扱う。tokio-tungstenite はデフォルト実装 (NativeWsConnFactory) とし、テストからのみモックに差し替える。
- **Bedrock ambient profile 判定は process env のみ。テストは process env を制御する** (#55) — 上流 `Boolean(getProviderEnvValue("AWS_PROFILE"))` は scoped options.env を含まない。options.env の AWS_PROFILE を ambient 判定に混ぜると「scoped profile が endpoint pinning を無効化する」バグになる。テストは `unsafe { std::env::remove_var/set_var }` で process env を制御し、終了時に復元する (#30 同様)。
- **上流の onPayload キャプチャパターンはコールバックで受ける** (#56) — 上流テストの `capturePayload` は「onPayload コールバック内で payload を取り、abort された request は送信されない」が前提。Rust テストで mock fetch の送信リクエストから payload を読むと「abort 済みなので request が飛ばない」で必ず失敗する。必ずコールバック内で `Arc<Mutex<Option<Value>>>` に格納する。
- **struct デフォルト値の上書きに注意** (#57) — テストヘルパーで `reasoning: High` を `..options` の**後**に置くと呼び出し側の Xhigh を上書きし「常に high」になる。デフォルト付けは struct 定義の**前**に置き、呼び出し側の値を `..options` で優先させる (Rust struct update 構文は後のフィールドが勝つ)。

## セッション運用の反省 (継継) (継続)

- **「修正した」は必ずテスト実行で確認してから言うこと**。デッドロック調査では「修正→別の箇所でハング」が連鎖した。
- フレークするテストは連続10回回して固定すること。実例: `rejects_late_publication` は 1/5 でしか落ちなかった (phase 1 が sender を消費する競合)。
- ユーザーが「止めて」と言ったら、同じ推論を繰り返さず手を止めること。
- この環境の command-guard 拡張 (`.pi/extensions/command-guard.ts`) の制約: 検証コマンド (`cargo test/check/clippy/fmt --check`) には bash ツールの `timeout` 秒指定が必須。`cargo test` は `-p <crate>` 付きで実行し、`--workspace` は `/guard allow-workspace` か確認ダイアログで明示許可。同じ検証コマンドの無変更再実行・同一ターン内の並列検証・1呼び出し内の複数 Git 操作はブロックされる。
