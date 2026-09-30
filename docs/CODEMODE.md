# Luau Code mode

pi v0.99.0 の「モデルがプログラムを書き、その中から複数ツールを呼び、必要な結果だけ返す」という設計を選択的に取り込む。pillar の Luau VM を使い、JavaScript ランタイムや pi 全体の更新は導入しない。従来の移植元のバージョン表記はそのままとする。

Luau を有効にした CLI は `codemode` と `tool_search` を提供する。同名の拡張ツールがある場合は拡張を優先する。SDK では `pillar_extensions::codemode::create_tools(CodeModeHost)` を使い、カタログ、共通の実行経路、ツールの宣言、専用 VM 実行スレッドを注入する。SDK セッションを組み立てるホストは CLI の `tools_for_slot` / `bind_tools` も利用できる。VM と OS の結合は CLI に置き、agent / coding-agent は VM に依存しない。

Code mode だけを組み込む SDK ホストは `pillar-extensions` に `default-features = false` を指定する。VM は専用スレッド内で生成・実行・破棄され、Luaur の `send` / `async` / `typecheck` を必要としない。共有 VM を持つ拡張 runtime・bridge・loader は既定の `extension-runtime` feature に含める。複数ホストをまとめてビルドすると Cargo が依存 feature を統合するため、SDK は使わない拡張 runtime の `send` 要件を他ホストへ波及させない。

## モデルが書くコード

`codemode` は `{ "code": "..." }` を受け取る。コードは Luau の関数本体として実行し、JSON で表現できる値を `return` する。

```lua
local matches = search_tools("database", 4)
local schema = describe_tool("read_record")
local replies = tools.parallel({
    {name = "read_record", arguments = {id = "a"}},
    {name = "read_record", arguments = {id = "b"}},
})
assert(not replies[1].isError and not replies[2].isError)
local detail = tools.call("read_detail", {id = replies[1].value.id})
assert(not detail.isError)
return {name = detail.value.name, count = #replies}
```

実際のツール名と引数は探索結果の `inputSchema` に従う。`tools.parallel` は入力順で結果を返す。agent の逐次実行設定、または一つでもツールに逐次実行指定がある場合、バッチ全体を逐次実行する。

返信は `{isError, content, structuredContent, value}`。`value` は結果フック適用後の `structuredContent` があればその値、なければテキストの連結となる。JSON に見えるテキストを自動で解析しない。`AgentToolResult.is_error` により、失敗したツールも構造化データを返せる。`outputSchema` は提供者の宣言であり、この実装では出力を再検証しない。

## 実行経路と探索

親の実行コンテキストは実行開始時に登録し、完了や中断で解放する。イベントの受信が遅れていても、そのコンテキストから子を実行できる。子ツールは agent の共通処理で引数準備、スキーマ検証、before フック、実行、進捗、after フックを通る。before フックによる引数変更後にも再検証する。SDK セッションの認可と拡張イベントには `parentToolCallId` が伝わる。ホストの許可リストと除外リストは探索と実行にも適用する。

`ToolRegistration` の到達範囲:

| exposure | モデル宣言 | Code mode | `tool_search` による宣言 |
|---|---|---|---|
| Direct | アクティブな間 | アクティブな間 | 可 |
| ModelOnly | 可 | 不可 | 不可 |
| Codemode | 不可 | 可 | 不可 |
| Deferred | 探索後 | 可 | 可 |
| Hidden | 不可 | 不可 | 不可 |

`codemode` と `tool_search` 自身は ModelOnly に登録し、スクリプトからの再帰呼び出しを防ぐ。Deferred ツールはホストが `Agent::register_tool` で明示的に登録する。SDK が選ばなかったネイティブツールを自動で復活させない。

`search_tools` と `describe_tool` は宣言を増やさない。モデル向け `tool_search` は一致する Deferred ツールを次のモデルリクエストに宣言する。探索は名前・説明・namespace の単語の部分一致を順位付けし、最大 32 件を返す。

結果フックが content だけを置換した場合、構造化データを除去する。両方が必要なフックは `structuredContent` も明示的に返す。これにより、テキストの伏せ字処理を元データ経由で迂回しない。

## 隔離、中断、記録

呼び出しごとに新しい VM と専用スレッドを作る。標準の値・文字列・表・数学操作だけを公開し、`require`、OS、ファイル、shell、debug、拡張のグローバルは渡さない。副作用は公開ツール経由に限定する。Luaur の割り込みがスレッドに属するため、VM を async executor のスレッド間で移動させず、ツール I/O をホスト側で待つ。

上限は 30 秒、VM メモリ 32 MiB、10 万 safepoint、256 子呼び出し、ソース・バッチ引数・最終出力各 32 KiB。割り込みは強制 yield で実行をホストへ戻すため、スクリプトの `pcall` でも中断を握りつぶせない。ホストは専用スレッドで VM を実行し、ホストの clock と executor を用意する。VM スレッド上でツール I/O の返信を待つが、セッションや拡張のロックは保持しない。

LLM 会話には親のツール結果だけを追加する。子呼び出しは親結果の `details.nestedCalls` に ID、親 ID、名前、状態、時間を残し、セッション保存の対象となる。引数は個別 8 KiB、合計 32 KiB に制限し、省略した場合 `complete=false` にする。子の全文結果は記録しない。子使用量は完了イベントで合算するため、後続処理が中断されても完了済み分を保持する。

途中の失敗・中断は完了した副作用を取り消さない。実行途中で返信が得られなかった呼び出しは `outcome_unknown` として記録する。子が終了を要求した場合はコードを打ち切り、親にも終了要求を伝える。書き込みの自動再試行はしない。

## 検証

`cargo test --locked -p pillar-cli --test codemode` は偽モデルと実セッションを使い、複数呼び出し・並列と逐次設定・認可・型・結果加工・中断・上限・探索・保存・CLI リロードを検証する。プロバイダーへの通信や実ユーザーの資格情報は使用しない。実モデルによる Luau の生成精度と往復削減の効果は、利用環境での確認対象とする。

検証済みの変更では、agent_loop_parity と extension_safety_parity を合わせた 54 件が成功した。親の MessageEnd の受信を意図的に止め、子の実行がイベント受信に依存しないことも確認した。
