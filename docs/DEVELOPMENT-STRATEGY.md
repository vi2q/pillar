# pillar 開発方針 — ゲーム開発支援の充実・高性能化・ランタイム再利用

状態: **開発方針としてユーザー承認済み（2026-09-17）**。今回は設計までの承認であり、実装着手の指示ではない。本書の目標構造・検証ゲートは、実装済みという主張ではない。

## 1. 目的と優先順位

ユーザーが指定した目的:

1. **ゲーム開発サポートエージェントとして必要な機能の上限に到達する。Luau拡張も製品機能に含む。**
2. **その能力・正確性を維持したまま、性能を継続的に高める。**
3. **内部を疎結合に保ち、必要部分を選んでランタイムエージェントとして再利用できるようにする。**

Rust / Luau（luaur）採用の背景には、統合クリエイティブソフト **azparam への Wasm 埋め込み**がある。将来はハーネス自体を Wasm 内に隔離し、ゲーム内 NPC（LMPC）のランタイムとしてトリミング・カスタムする。

この順序は「まず低機能な NPC ランタイムを完成させる」でも「全機能完成まで性能や境界を放置する」でもない。**機能開発を主線とし、性能計測・依存制約・切り出し検証を並走させる。** 機能上限へ到達後は、性能改善を主線へ移す。

pi は実績ある挙動・契約の参照元であり、全パッケージ・全UI・全APIの漏れない再現を目的にしない。grep 周りなどの独自強化を積極的に行う。既存の有用な機能を、Wasm対応や最適化の都合で開発支援版から削らない。

### 文書の役割

- 本書: 何を完成とし、どこへ投資し、どの境界を守るか。
- [01-architecture.md](rules/01-architecture.md): **現在の** crate 依存表・保存契約。本書の目標図に合わせて先に依存表を書き換えない。
- [02-porting-policy.md](rules/02-porting-policy.md) / [06-upstream-sync.md](rules/06-upstream-sync.md): 採用した互換面の扱いと上流参照。
- [04-luau-extensions.md](rules/04-luau-extensions.md): Luau API の参照資料。完了判定は実ホスト経路の検証で行う。
- [TASKS.md](TASKS.md): 着手・検証・確認の記録。本書に完了チェックリストを複製しない。

過去のレビュー・引き継ぎは調査時点の記録であり、最新の未実装一覧や製品目標として扱わない。以下の提案は既存の外部契約を無断変更する許可ではない。

## 2. 「必要な機能上限」の定義

API数や移植率ではなく、**対象のゲーム制作工程を、ホスト本体の改造や手作業の迂回なしに完結できる能力**を上限とする。MVPだけを満たして終了しない。一方、対象外の全エンジン・全provider・全上流UIまで無限に広げない。

機能の採用条件は次のいずれか:

- 対象制作工程に必要で、欠けると完結しない。
- 頻出工程の精度・応答性・作業量を、再現可能な比較で改善する。
- Luau拡張やazparam接続を成立させる基盤能力である。
- 保存・権限・中断・再開の正しさを支える。

### 能力と完了条件

| 能力 | 必要範囲 | 完了を証明する制作工程 |
| --- | --- | --- |
| 調査・検索 | ファイル探索、literal/regex grep、範囲指定、ignore、前後文脈、大規模出力の制限と追加取得。構造・シンボル検索、診断も利用可能にする（実装はnativeサービス／拡張でよい） | コード・設定・シェーダ・スクリプトを跨いで原因と参照箇所を特定し、根拠の位置と版を提示できる |
| 編集・検証 | 読取、差分編集、生成、競合検出、ビルド・テスト・lint等の起動、ログ取得、中断、再検証 | 複数ファイルの不具合を修正し、対象検証の結果と変更差分を提示。失敗や競合を成功扱いしない |
| azparam制作操作 | capability発見、シーン／entity／component／assetの照会と更新、実行・プレビュー、ログ・画像等の観測。具体ツールはazparam側の契約に従う | 対象選択→変更計画→承認が必要なら承認→適用→プレビュー／観測→結果照合。未対応操作は能力不足を明示 |
| 長時間作業 | streaming、steering、中断、モデル切替、使用量、圧縮、履歴、分岐・再開、保存失敗からの回復 | ビルドやモデル通信の最中にも操作可能。中断・再開後に変更済み／未実行／結果不明を区別できる |
| Luauによる拡張 | ツール・コマンド・イベント、context／session操作、対話・custom UI・描画、host能力呼出、load/reload/dispose、型契約。制作に必要なprovider拡張も対象 | ビルド補助・資産点検・対話パネルなどをRust側の案件別改造なしに実装できる |
| 配布・運用 | 設定・認証、明示install/update、信頼確認、拡張エラー診断、CLIとazparam組込みの入口 | 新規の隔離環境から設定→起動→作業→終了まで再現でき、既存環境への無言変更がない |

資産・画像・音声・大量ログを全て会話のJSONへ詰め込まない。ホスト管理の参照と範囲読取／サムネイル／要約を用意し、必要な内容だけをモデルへ渡す。資産変換器やゲームエンジンをpillar内に再実装することは目的ではない。

### 上限到達の判定方法

「必要範囲」は上表を初期境界とし、次の成果物で確定する。

- **制作シナリオ集合**: コード修正、ビルド失敗診断、シーン編集、資産検査、プレビュー観測、長時間中断・再開、拡張追加の各工程を収録。匿名化した実プロジェクトを含める。
- **代表Luau拡張集合**: ①作業記録、②検索／資産検査、③中断・進捗付きビルド、④承認・custom UI、⑤session/context変更、⑥必要ならprovider接続。これは検証の最低集合であり、提供APIの上限ではない。
- **capability台帳**: 能力ID、対象profile、提供API、所有adapter、必要権限、状態（未実装／配線済み／検証済み）、型チェック例、成功・失敗・中断のテスト、性能corpusへの参照。

上限到達は、対象集合に未解決の機能欠落がなく、正常系・失敗系・中断・再開・拡張経路が実ホストで成立すること。fakeモデルによる決定的検証と、実制作での品質評価を分け、前者だけでゲーム開発支援の品質を保証したことにしない。

新しい必須機能は「どの制作工程が完結しないか」を示して集合へ追加する。upstreamに追加された、あるいは実装しやすいという理由だけでは必須にしない。

## 3. Luauを二級の入口にしない

Luauから登録されたツールにも、built-inと同じ引数検証・認可・進捗・中断・deadline・エラー分類・結果制限・監査を適用する。型定義に存在するだけ、登録名が見えるだけ、native単体で成功するだけでは完了しない。

必須の契約:

- 一登録一呼出。登録順・変換連鎖・遮断規則をイベント種別ごとに固定する。
- extension / handler / session / generation / request の所有者を保持する。後続の拡張が他拡張の登録を奪わない。
- session変更は「要求を受け付けた」と「適用された」を区別する。UI回答やhost待ちの間も中断・終了を処理できる。
- reloadはcommand/tool/renderer等を世代単位で切り替える。新世代の準備失敗は旧世代を維持し、旧世代の遅延応答を新世代へ適用しない。準備時の外部副作用はstagingするか、rollback不能として禁止・明示する。
- `--!strict`で利用できる型を提供する。`ctx: any`や`event: any`で隠れた必須能力を検証済み扱いしない。拡張独自データの不定形性は保持する。
- custom UIは開発支援版の重要機能として完成させる。ただしTUIのTheme/Component/入力ループはpresentation adapterに置き、sessionやVMの共通契約へ流し込まない。
- terminal固有UIとazparam向けUIは区別する。ANSI描画を機械的にゲームUIへ転用しない。共通の対話要求と、profile固有の高度なUI能力を分け、unsupportedを事前照会できるようにする。
- VM実行予算、メモリ上限、host呼出予算、shutdown/disposeを検証する。Luauのhost呼出制限とWasmの隔離は別々の責任である。

宣言・実装・検証結果からcapability公開情報を作る方向を取る。生成基盤の大工事を先行させず、最初は小さい台帳と整合性テストで未配線の公開を防ぐ。

## 4. 目標構造 — 高機能な製品を、選択可能な部品から組む

以下は**責任の図**であり、全てを新crateへ分割する指示ではない。

```text
composition roots
  ├─ development CLI
  ├─ azparam development host
  └─ LMPC runtime host（将来）
         │ 組合せ・権限・資源予算・実行の駆動
         ▼
  session application + agent execution core
    ・turn / tool / context / event / cancel / lifecycle
    ・sessionの状態遷移と確定契約
    ・必要なportの契約（具体的なUI/VM/OSを知らない）
         ▲ 契約を実装するadapterを外側で注入
         ├─ model: native providers または host-mediated model
         ├─ tools: native検索・FS・process / azparam操作 / runtime行動
         ├─ extensions: Luau VM + profile別API adapter
         ├─ presentation: TUI / azparam UI / headless
         └─ persistence: v3 JSONL / host保存・snapshot
```

### 分離すべき境界

| 境界 | 内側に残すもの | 外側に出すもの・契約 |
| --- | --- | --- |
| agent実行 | turn進行、tool dispatch、context、順序、中断状態 | executor/spawn、clock/deadline、ネットワーク、モデル資格情報。既存の`StreamFn`等を活用 |
| session application | 状態変更・確定・lifecycle、必要な履歴操作 | CLI設定探索、terminal、package解決、保存codec/媒体。LMPC用に別のturn実装をコピーしない |
| extension | 登録・イベント・結果の契約、所有者・世代 | Luau値、型チェッカ、ファイル探索、TUI具体型。VM側が巨大coding-agent実装へ依存しなくても組み込める状態を目指す |
| tools / effects | 型付き要求・結果、認可結果、取消状態 | native実装とhost実装。認可と実行対象の一致を最終実行地点で検査する |
| search | query/filter/match/resultの契約 | 列挙・読取・並列実行・index更新。native backendとhost/VFS backendを選ぶ |
| presentation | 表示に必要なsnapshot/event、対話request/reply | Theme/Component、描画周期、focus、keyboard、UI thread |
| persistence | append/commit/checkpoint/recoverの意味 | JSONL、hostの保存領域、必要なら別backend。既存CLIの正規storeは変更しない |

**分離の原則:** 境界を跨ぐ箇所だけを抽象化する。ファイル一件・行一件・token一個ごとの不要な動的dispatchやJSON往復を増やさない。native内部は型付きデータ・共有不変snapshot・batchを使い、直列化はLuau/プロセス/Wasmなど本当に必要な境界に限定する。

`Arc<Mutex<実装>>`を共通APIとして公開するのではなく、操作権限の狭いhandleを渡す。単一の状態所有者を設けても、長時間I/Oをその所有者の処理ループで待ち続ける設計にはしない。

### 組込み構成（目標。現在のCargo feature名ではない）

| 構成 | 組み込むもの | 必須依存にしないもの |
| --- | --- | --- |
| 開発CLI | 高機能検索・編集・process・TUI・Luau・session履歴・native provider | ゲームruntime専用機能 |
| azparam開発支援 | 同じ実行核・Luau・制作ツール・host UI/保存/model接続 | TUI、CLI起動処理、OSプロセスへの直接依存 |
| LMPC最小 | 同じ実行核・限定された観測／行動・host model・必要な状態 | TUI、ソースコード検索、shell、package manager、認証UI、全provider catalog、Luau |
| LMPC＋Luau | LMPC最小＋必要なLuau契約とVM | 開発専用の拡張UI・discovery・install。型チェックを配布前へ移す構成は、検証済みartifactと実行VM/契約の版を照合する |

Cargo featureはビルド内容の選択であり、セキュリティ境界ではない。独立profileの依存解決を検査し、workspace全体のfeature unionやリンク時dead-code除去だけで「外せた」と判定しない。

現状の実装: 実行核のOS能力とLuauがfeatureになっている。
- `pillar-ai`: `providers`（per-API adapter・HTTP transport・model registry・provider auth）が既定on。off にすると `reqwest` / `rustls` / `tokio-tungstenite` / `zstd` が依存graphから消え、host が `StreamFn` を注入するLMPC最小の形になる。
- `pillar-agent`: `harness-tools`（coding-agent向けfile/shell tool・std/tokio実行環境・session runtime・compaction・skills）、`proxy`、`session-files`（JSONL file backend）、`search`（native検索scanner）が既定on。`pillar-ai` への依存は `default-features = false` で、scaffold feature が `pillar-ai/providers` を有効化する。`--no-default-features` がLMPC最小の形（40 crates、provider catalog・terminal層なし）で、nativeでもwasm32でもコンパイルできる（`scripts/check.sh` と `dependency_profiles::the_lmpc_minimum_has_no_provider_catalog`）。
- `pillar-cli`: `luau` feature（既定on）でVM・`luaur`を外せる。
- ゲート: `crates/pillar-cli/tests/dependency_profiles.rs`が解決済みgraph（profile別）と、coreのsources（`std::process` / `std::fs` / `std::net`がgated moduleの外に無いこと）を検査する。
- `pillar-lmpc`: LMPC最小のartifact（bin `lmpc-minimal`）。host service（spawner/clock）の設置とhost modelのstand-inを持ち、`cargo check --target wasm32-unknown-unknown`と素の`#[test]`（tokio runtime無しの1 turn）でゲート。
未着手: terminal層を外す軸（azparam/LMPC向けのUI adapter分離）、実Wasm hostでの実行往復（§5-7）、feature unionに依存しない独立解決の検証。

## 5. 疎結合の合格条件

「traitがある」「crateが多い」ではなく、**外しても動き、差し替えても契約が保たれること**を検証する。

1. 実行核＋in-memory状態＋fake model＋fake toolで、TUI/Luau/OS環境なしに一turnとtool往復が動く。
   進捗: 満たした。`crates/pillar-agent/tests/host_driven_turn.rs`が`#[tokio::test]`ではなく素のtestとして、host注入のspawner（thread＋futures executor）とclockで1 turnを通す（tool往復込み、tokio runtime無し）。
2. 同じ核をnative hostとazparam用Wasm hostで駆動できる。compiledだけでなくstreaming・cancel・保存／復帰・shutdownまで実行する。
   進捗: **前提達成＋ループとprovider待ちのホスト駆動化＋headless往復**。host service注入だけで1 turnが回ること（`host_driven_turn.rs`）を機械的に確認。埋め込みhostが直接駆動するcore（loop/state/stream型）はtimer・socket・生spawnを持たず（`dependency_profiles::the_host_driven_core_is_free_of_timers_sockets_and_raw_spawning`）、providerの待ちは`pillar_ai::clock`（host注入の`SleepFn`、既定はtokio）経由に統一した（`provider_waiting_goes_through_the_host_timer`）。残るnative依存はWebSocket transportと`AbortSignal::timeout`のspawn。LMPC最小（`pillar-ai`＋`pillar-agent`）とLMPC＋Luau（＋`pillar-extensions`）は`wasm32-unknown-unknown`でコンパイルできる（`scripts/check.sh`のゲート）。ループの背景bodyはホスト注入のspawner（`pillar-agent/src/spawn.rs`、`AgentLoopConfig::spawn`）で駆動でき、リアクタ無しのホストでも動く。実行往復は未検証で、残る阻害要因はTASKSに列挙（harnessのOS境界分離、拡張の読み込みをhost供給へ、codex providerの`tokio::net`、`tokio::time`依存）。
3. Luau有／無が独立してビルド・動作する。有の場合もTUIを要求せず、実拡張からhost toolを呼べる。
   進捗: ビルド軸と起動は満たした（`scripts/check.sh`が`--no-default-features`のビルドとsmokeを実行、`dependency_profiles.rs::the_luau_feature_gates_the_vm_dependency`がfeature解決後のgraphを検査）。「TUIを要求せず実拡張からhost toolを呼べる」はVM単体＋契約で満たし、常時ゲート（headlessでの実拡張tool呼び出し）は未着手。
4. native検索backendをhost/VFS backendへ替えても、採用した検索契約が同じ。NPC構成からは検索自体を外せる。
5. 通信・ツール・UI・保存の遅延応答にsession/generation/request識別があり、取消・切替後に別sessionを変更しない。
6. import/exportとtransitive dependencyを機械検査し、最小構成へterminal・process・暗黙FS・package取得が混入したら落とす。
   進捗: dependency graph（profile別）と、coreのsourcesに対する`std::process`/`std::fs`/`std::net`の検査を実装（`dependency_profiles.rs`）。terminal層はcoding-agent/tuiの依存として残っており、外す軸は未実装。
7. native/Wasm間の同一入力・記録済みmodel/tool結果に対する意味的event traceが一致する。時刻等は注入し、LLM自身の決定性は仮定しない。
   進捗: **デモturnで達成**。`pillar-lmpc`を`cdylib`として`wasm32-unknown-unknown`へビルドし、Node（`scripts/wasm_trace.mjs`）でinstantiateして実行、nativeの`lmpc-minimal`と**event trace・transcriptが完全一致**することを`scripts/check.sh`で確認する。同じturnを`FrameHost`（thread/tokio無し・仮想時計）で回しても一致することをテストで固定。モデル接続も実証済み: `HostModelSession`（guest が request を公開 → host が reply。C ABI は `lmpc_host_*`）を追加し、native の `--host-model` と JS host（`scripts/wasm_host_model.mjs`、tool call 込み）で **trace が完全一致**することを `scripts/check.sh` で確認。panic は hook で trace buffer に残し host が読む。release の artifact サイズは 391,881 bytes（gzip 135,872）。実行中 cancel も実装（`cancel()` が agent を abort し、公開済み request を無効化して停止。native / Wasm / JS host の `--cancel` で検証）。複数 turn も実装（`HostModelSession::say()`、native と JS host が同じ scripted 応答を使い 2 turn の trace も一致）。streaming も実装（`stream_delta` が `TextDelta` を流し agent が `message_update` として中継。native/JS host の `--stream` で trace 一致を確認）。残りは実engine統合・tool progress の host 通知・session の保存復帰・release 再計測。

Wasm境界はversion付き要求／結果、opaque handle、サイズ上限、所有権・解放、cancel/deadline、再入禁止または規則を定義する。Rustの参照やArcをABIにしない。

Wasm hostの選定（azparam内のruntime、browser、WASI等）とABIは、azparam側の実環境を確認して決める。`wasm32-unknown-unknown`でcheck済みという過去記録だけでは、スレッド・時刻・Tokio・host future・Luauの停止が使える根拠にならない。

LMPCの世界状態・権限・行動適用はホストが所有する。Wasmゲストの検査を信用境界にせず、agent identity、世代付きentity handle、世界revision、実行予算をホストで再検証する。モデル資格情報をゲストへ渡さず、推論は要求／結果にできる。ゲスト停止は外部行動のrollbackを意味しない。

## 6. 性能方針 — 上限到達後も改善を止めない

最適化の順序は **正しさ・必要能力・権限保証を維持 → 待ち時間と資源消費を削減**。短い出力にして必要な検索結果を落とす、キャンセルできなくする、拡張APIを外す、といった見かけの高速化は採らない。

性能目標は合格の最低線であり、改善終了の理由ではない。各改善は同じ意味的出力・同じworkloadでbefore/afterを残す。高速化とメモリ削減が競合したら、profileごとのPareto改善と明示した予算で判断する。

### 計測するもの

| 対象 | 指標 | workload |
| --- | --- | --- |
| 起動 | cold/warmの入力受付まで、拡張typecheck/load、初回model要求前のlocal処理時間 | 拡張0/10/100、空／実設定、ネットワーク不要の起動 |
| 検索 | 最初の有用な結果／完了／取消までの時間、files・bytes/sec、read bytes、allocation、peak RSS、返却bytes/token相当量 | 1万/10万/100万ファイル。literal/regex/no-hit/大量hit、ignore、巨大行、バイナリ、contextあり、変更直後 |
| 対話・event | 入力→描画、delta→描画、queue depth/bytes、遅いconsumer下の公平性 | 長いtranscript、画像・ログ参照、burst delta、対話UI待ち |
| agent/拡張 | 純粋な一turnのoverhead、dispatch/bridge/serde時間、host往復、lock待ち、進捗とabort伝播 | 同じfake stream、拡張0/10/100、tool batch、reload100回 |
| 保存・履歴 | append ack、rewrite/recovery、list/load、live状態との一致 | 1千/1万/10万entry、並行外部変更、torn tail、書込失敗 |
| Wasm/再利用 | artifact bytes、初期化、ゲストCPU・memory、host境界copy bytes、poll/yield最大時間 | 最小／Luau有、idle/active agent 1/10/100。全員が毎frame推論する前提にしない |

測定条件にrevision、release profile、toolchain、CPU/RAM/OS、host、corpus hash、cache条件、並列度を含める。LLM・ネットワーク・ゲーム側の待ちとpillarの処理時間を分離する。既存のdebug計測値をreleaseのSLOへ流用しない。

- throughput計測はwarm-up後に反復し中央値と分散を記録する。p95/p99を掲げる対話計測は十分なサンプル数を持ち、少数回のmaxをp99と呼ばない。
- 入力応答の初期候補: p95 16ms / p99 50ms。中断受付の候補: p95 50ms。これは参照機・workloadを固定して較正する**提案値**であり、現状の達成値ではない。
- 中断受付と実処理の停止を別計測する。host操作のdeadlineと終了上限は能力別に設定し、止まらない処理をUI表示だけで「中断済み」にしない。
- 検索時間、RSS、artifact size、同時agent数の絶対上限は最初のbaselineで確定する。未測定値を性能保証にしない。
- PRでは安定した小corpusの構造的退行（件数超過、無制限buffer、二重実行、リーク）を拒否。専用性能runnerでtime/alloc/sizeを比較し、ノイズ帯を超える悪化には理由と承認を要求する。初期の警告幅は10%を候補とし、測定分散に応じて調整する。

### grep / find の強化方針

検索は開発支援版の主力能力として最適化し、Wasm最小構成へ押し込むために性能を犠牲にしない。

1. **正しさと上限を先に固定**: include/exclude、ignore、encoding、symlink、巨大file/line、context重複、件数・byte制限、中断、読取失敗の扱い。
2. **走査とI/O**: literal fast path、buffered/chunked読取、worker-local収集とbatch merge、限定範囲のcontext取得、早期中断。ファイルごとの全量確保・context用の再読取・頻繁な共有lockを計測して減らす。
3. **並列度は全体で管理**: 検索ごとにCPU数のworkerを増やさず、build・UI・他agentと予算を共有する。Wasm/host/VFSでは別executorを使えるようにする。
4. **反復検索**: ファイル一覧cache、incremental index、query cacheを順に実測評価する。editor未保存buffer、更新・rename・delete、ignore変更、監視イベント欠落を考慮し、freshnessとfallbackを定義してから導入する。
5. **検索品質と順序**: 並列先着の限定集合をsortしても集合自体は決定的にならない。exact検索と順位付き探索を分け、ranked検索はtop-kの品質、安定tie-break、同一snapshotのcursor、打切り／不完全の表示を定義する。ランキングで必要箇所を隠さない。

全tokenの再直列化、全履歴clone、無効化されないcache、表示待ちによるagent停止も同じく計測対象とする。queueは生産者・consumer・配送保証で分類し、重要eventは順序を保つ。表示deltaだけを集約可能とし、「全部bounded」への機械的置換で循環待ちを作らない。

## 7. 現状からの移行点（調査スナップショット）

以下は今回読んだmanifest・本文の範囲。全provider監査や性能測定は行っていない。LSPは無効、review graphは古いため依存関係の根拠には使用していない。

| 確認した現状 | 設計への意味 | 根拠 |
| --- | --- | --- |
| nativeのfind/grepは`ignore::WalkParallel`＋globset/regex。同期走査、file全読取、context再読取、共有match集合を使用。toolのabort判定は開始時 | 独自強化を維持し、cancel・buffer・並列予算・backend境界を改善する出発点。最速／メモリ上限達成済みとは判定しない | `crates/pillar-coding-agent/src/core/tools/search.rs` |
| `run_agent_loop`は注入されたsink/modelでawait可能、便利関数`agent_loop`は`tokio::spawn`。内部時刻はSystemTime | 全面再実装せずhost駆動入口を活かす。clock・残るspawn/timerを切り出す | `crates/pillar-agent/src/agent_loop.rs` |
| `FetchFn`を注入でき、native HTTP依存はtarget条件付き。Wasm既定は未設定エラー | transport境界は活用可能。ただし資格情報・executor・全providerの実行可能性は別検証 | `crates/pillar-ai/src/transport.rs`, `crates/pillar-ai/Cargo.toml` |
| 解決済み（2026-09-17）: `pillar-extensions`はcoding-agentにもtuiにも依存しない。共有型は`pillar-extensions-contract`（`ThemeStyle`/中立payload、`ThemeProvider`）。残る負債はLuau custom UIの同期`recv_timeout`待ち | 非同期request/replyへ移す | `crates/pillar-extensions-contract/src/lib.rs`, `crates/pillar-extensions/src/runtime.rs::context_value` |
| Luau `call_tool`のsignal/on_updateはnil。ExecHostは同期callback | 開発支援の必要上限の不足として扱う。build/asset処理の進捗・停止をbuilt-in同等にする | `crates/pillar-extensions/src/runtime.rs::call_tool`, `ExecHost` |
| CLIのEffectBrokerに認可と監査があり、execは同期process出力収集、監査はVecに保持 | 強制点の土台は維持。host化・deadline・出力上限・監査sinkの保持予算を追加する | `crates/pillar-cli/src/effects.rs` |
| crate依存allowlistの回帰テストはある。coding-agent→tui、extensions→coding-agent/tuiは現状許可 | 現在の表を固定するだけでは将来の疎結合を証明しない。profile別transitive依存と実行検証を追加する | `crates/pillar-cli/tests/dependency_direction.rs` |

これらは今回修正した不具合の一覧ではない。実装タスク化するときに再確認する。

## 8. 実施順と完了ゲート

機能トラックを主線とし、境界・計測トラックを同じ変更に併走させる。作業量の固定比率や全体書き直しは要求しない。

| 段階 | 主な仕事 | 次へ進む証拠 |
| --- | --- | --- |
| P0: 判定基盤 | 制作シナリオ／拡張集合／capability台帳の初版、release baseline、既存checkをCIへ接続。azparam実ホストの実行条件・ABI候補を確認 | 必須と対象外が区別でき、同一corpusで比較可能。既存検査が実際に落とせる |
| P1: 必要能力を埋める（主線） | 検索・編集・制作連携・長時間作業・Luau APIの不足を制作工程順に埋める。特にLuau進捗／中断・custom UI lifecycle・実制作tool接続 | §2の各工程が単体APIではなくホスト入口から完結する |
| P1に併走: 境界を固定 | OS/UI/VM依存を変更箇所から外へ寄せる。host駆動・Luau有無・Wasm往復の最小probeを維持する | §5の分離検証が順に通る。機能追加が最小構成へ不要依存を持ち込まない |
| P2: 上限到達を判定 | 必須シナリオ・拡張の成功／失敗／中断・再開と実制作評価、契約・隔離・負荷検査 | 欠けた機能を手作業で補わず成立する。未達を完了扱いしない |
| P3: 性能を主線へ | 検索I/O・コピー・serde・描画・拡張typecheck/実行・保存・Wasm初期化をプロファイル順に最適化 | 同一能力のbefore/after、退行なし、profileごとの予算達成。以後も改善継続 |
| P4: LMPC版を製品化 | 検証済みの核を選択構成し、ゲーム側の観測／行動・記憶・予算・セーブ連携を実装 | ハーネス本体のforkなしに成立。NPC間の状態／権限分離、停止、遅延応答の拒否を確認 |

P4のゲーム固有仕様をP1の前提にしない。P1で維持するWasm probeは疎結合の検査であって、NPC製品を先に作る作業ではない。性能上の重大退行やデータ破損・権限・中断の欠陥はP3まで待たず修正する。

### 直近の実装着手候補

1. Luauツールのsignal/on_updateからhost処理までを接続し、遅いbuild・待機中UIでも中断できる一本の工程を完成させる。
2. 検索の件数・byte・memory・cancel契約とrelease corpusを作り、同期全読取／context再読取／共有lockを改善する。
3. extensionの共通契約からTUI具体型を外す小さい切断と、Luau有／無のheadless構成検証を行う。
4. azparamの制作操作を、既存host APIを利用した実拡張で一工程完結させる。必要なAPIをそこから台帳へ反映する。

着手順の最終調整は、実制作で塞がっている工程と計測結果による。本書の作成は、この4件を実装済み・着手承認済みとするものではない。

## 9. 方針を機械的に守る仕組み

以下は**導入するゲートの設計**。現状の`check.sh`だけで全て成立しているわけではない。

- **依存ゲート**: `cargo metadata`の解決済みgraphをprofile別に検査。core→UI/VM/CLI/native adapterの逆依存と、最小構成へのtransitive混入を拒否する。
- **副作用ゲート**: core内の直接FS/process/network/ambient envを範囲指定のlint/AST検査で拒否。Wasm import allowlistとhost側capability検査を併用する。文字列grep一個をsandboxと呼ばない。
- **契約ゲート**: capability宣言・型・実配線の一致、Luau strict corpus、対象upstreamとのdifferential、独自強化の明示した契約テスト。
- **lifecycleゲート**: cancel/reload/session切替/保存失敗/timeout/遅いconsumerを決定的なfake clockと障害注入で検査。VM/task/listener数が反復後に戻ることを確認する。
- **製品経路ゲート**: CLI/TUI、azparam host、Wasm最小／Luau有をそれぞれ実行する。1つの入口が通っても他を通過扱いしない。
- **性能ゲート**: 小corpusのPR検査と、大corpus・実Wasm hostの定期計測を分離。性能artifactとcapability台帳を同じrevisionに紐付ける。
- **隔離ゲート**: 実ユーザーHOME/cwd/資格情報を使わない。通信遮断が必要な検査はOS/runtimeで強制する。offline flagやdead proxyだけをネットワーク隔離の証明にしない。

pi上の開発作業にも同じ考え方を適用する。禁止操作・対象外pathへの編集・未承認の外部作用は、注意書きだけでなく既存のpi-execution-guard等の実行制御へ置く。並行作業はworktree等で分離し、共有ファイルは差分を再読して統合する。

## 10. 当面行わないこと

- piの全表面の移植を自動的に必須化すること。
- CLI機能を減らしてWasm最小版を作ること、またはLMPC用にagent loopをコピーすること。
- 全crateの同時再編、巨大な共通crate／万能host interfaceの導入。
- 未計測でSQLite・永続index・全面actor化・全queue bounded化を決めること。
- 全NPCごとのOS thread、全frameごとのモデル呼出を前提にすること。
- Wasmに入れたことだけで、host権限・外部通信費用・ゲーム内作用・強制停止まで安全とすること。

**まとめ: 開発支援版は必要なところまで強くする。その能力を測定しながら高速化し、再利用のためには機能を弱めず、依存と権限を外せるようにする。**
