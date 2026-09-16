# 05 — 検証方針

[開発方針](../DEVELOPMENT-STRATEGY.md) の必要機能上限・高性能化・疎結合を証明する。採用したpi互換面にはupstream比較を使い、独自強化は自身の契約・制作工程・性能で検証する。

本書は検証の設計であり、列挙した全層・CI・corpusが既に存在するという主張ではない。

## 検証層と証明範囲

| 層 | 証明するもの | 証明しないもの |
| --- | --- | --- |
| Unit / 移植unit | 純粋処理、境界条件、移植した期待挙動 | 実upstreamとの一致、hostへの配線 |
| Differential | 固定したupstreamと同じ入力に対する採用面の一致 | 独自強化の品質、全機能の完成 |
| Contract | tool/event/session/host境界のschema・意味・失敗規則 | 実ホストでのlifecycle |
| Extension | Luauの型・登録・dispatch・host往復・UI回答・cancel/reload | nativeだけの成功によるWasm互換性 |
| Conformance | 使用するluaur機能とpillar VM設定の適合 | host APIの認可や正しさ |
| 製品経路 | CLI/TUI・azparam host・実Wasm hostごとの制作工程 | 別の入口・構成の成功 |
| 障害・負荷 | 保存失敗、取消、遅いconsumer、世代交換、上限、資源回収 | 無条件の外部副作用rollback |
| 性能 | 同一能力・workloadでの時間・memory・copy・artifact size | 出力を削っただけの高速化 |

現状のRustテストは主に`crates/*/tests/`と各module内にある。`*_parity.rs`という名前だけでdifferentialと呼ばない。新しい専用directoryの有無も、検証実施の証拠にはならない。

## 採用面のupstream比較

参照revisionは [06-upstream-sync.md](06-upstream-sync.md) に従う。元テスト名・入力・生成元revisionを追跡し、skipには理由を残す。採用していない機能のテストを自動的に必須化しない。

- session: 同じ記録済み対話からJSONLを比較。時刻・IDを正規化し、木構造やentry順序は保持する。
- CLI: 提供するflag・エラー・終了コードを比較。pillar固有の名義・path・追加機能は明示した期待値を使う。
- tool: 同じ入力のcontent/detailsと打切り・失敗条件を比較する。
- provider: fake transportでrequest body・streaming順序・usage・retry/cancelを比較する。
- extension: 採用したイベントの順序・payload・変換・block/modifyを比較する。

fixtureはofflineで再生でき、出所を明示する。独自検索の順位など、意図的に異なるものを無理にbyte一致させない。代わりに独自契約の正確性・品質を検査する。

## Luauと制作工程

[開発方針](../DEVELOPMENT-STRATEGY.md) の代表拡張集合を`--!strict`で検査し、同じ拡張を実際のloader→runner→session→host経路で実行する。

必須の失敗系:

- reasonなしblock、拒否hookの例外、不正な返却、未提供capability。
- 複数拡張の同名登録、setup失敗、reload失敗、古い世代からの応答。
- 長時間toolの進捗・取消、UI待ち中のshutdown、host timeout。
- 保存失敗・中間破損・torn tail・外部writer、保存状態とlive状態の一致。

fake modelの決定的工程と実制作の品質評価を分ける。拡張APIが型にあるだけ、ツールが登録されたのみ、画面に成功表示が出ただけでは完了にしない。

## 疎結合・Wasm・性能のゲート

- 現在の依存表は`crates/pillar-cli/tests/dependency_direction.rs`で検査する。目標の疎結合にはprofile別transitive依存、import allowlist、Luau有無、native/Wasmの実動作を追加検査する。
- 同じ実行核をTUI/Luau/OSなしでも駆動する。host clock/model/tool結果を注入し、取消・復帰・終了まで比較する。
- PRの小corpusと定期の大corpusを分離する。参照機・release条件・cache・並列度を固定し、速度だけでなく結果品質とpeak memoryを記録する。
- 隔離が必要な検査は実HOME・資格情報を使用せず、通信もOS/runtimeで制限する。dead proxyだけをネットワーク禁止の証明にしない。

`scripts/check.sh`は現時点のbuild/lint/test/smoke入口。CI接続や上記追加ゲートの完了状況は [TASKS.md](../TASKS.md) と実行結果で確認する。通常turnの検証回数とcheckpointの範囲は [RULES.md](../RULES.md) に従う。

## 新機能の完了条件

実装変更には、該当する契約、実ホスト経路、失敗・中断、性能負荷の検査を同時に付ける。意図的差分には差分を固定するテストを置く。能力を検証済みとして公開するのは、これらの証拠が同じrevisionに揃ってからとする。
