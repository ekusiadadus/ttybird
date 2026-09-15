# TTYbird

散らばったAIエージェントを見つけ、元の端末へ戻るRust製TUIです。
いつもの起動方法を保ったまま、親子ツリー、検索、Ghostty/tmuxへの移動、
**libghostty-vt**による読み取り専用tmuxプレビューを追加します。
通常の一覧表示に常駐daemonやhooksは不要です。

**アルファ版です。** PIDの存在とモデルの作業中を区別します。
推定された状態と不明な状態を明示し、Ghosttyの初回紐付けは利用者が選択します。

[English / 詳しい操作](README.md) · [設計](docs/ARCHITECTURE.md) ·
[プロバイダー対応](docs/PROVIDERS.md) · [検証範囲](docs/VALIDATION.md)

![親子ツリー・詳細・プレビューの操作](docs/walkthrough.gif)

実際のTUI描画と合成データによるデモです。実セッションの収録や、実際の
Ghosttyペインへのフォーカス移動を示すものではありません。

## 起動

[Releases](https://github.com/ekusiadadus/ttybird/releases)から対象OSのアーカイブを取得し、展開します。
実行時にRust・Zig・Ghostty.appは不要です。`ps`とmacOSでは`lsof`、
各連携にはtmux/SSHが必要です。

```sh
./ttybird --local
```

セッションを選び **Enter**。紐付け済みtmuxペインへ移動するか、macOSでは
Ghosttyペインの選択画面を開きます。最初に対象ペインを選択すると、プロセスの
PID・起動時刻が一致する間は保存した対応付けを使います。
Ghostty連携にはGhostty 1.3以降のAppleScript機能が必要です。
macOSがAutomation権限を求めることがあります。

Nixでの固定ビルドと開発環境は[Nix](docs/NIX.md)を参照してください。
ソースビルドにはRust 1.90以上と **Zig 0.15.2** が必要です。

```sh
git clone https://github.com/ekusiadadus/ttybird.git
cd ttybird
cargo install --path . --locked
ttybird --local
```

## 主なキー

| キー | 操作 |
|---|---|
| ↑ / ↓、j / k | セッションを選択 |
| Space、← / → | 親子ツリーの折りたたみ・展開・移動 |
| Enter | 対応端末へ移動、またはGhosttyペインを選択 |
| p | ローカルtmuxのプレビュー。PageUp/PageDownで表示範囲を移動 |
| d / / | 詳細 / 検索 |
| a | 観測できた入力待ちに絞る |
| b / h | バックグラウンドプロセス / 履歴を表示 |
| r / ? | 更新 / ヘルプ |
| q / Ctrl-C | 終了し、端末設定を復元 |

スクリプトには`--plain`、`--json`、`--watch --json`を使えます。
`ttybird doctor`で実行環境、`ttybird providers`で対応能力を確認できます。

## 対応範囲と制約

- Codex/Claudeの状態はログの限定サンプルなどから取得します。他のCLIは
  原則プロセス情報のみで、作業状態は不明です。入力待ちの直接観測は現在、
  任意のClaude hooksに限られます。[対応表](docs/PROVIDERS.md)
- 親と子は同じPID・TTYを共有することがあります。端末移動はホスト端末へ
  戻る操作で、Codex内部のサブエージェント画面切替ではありません。
- `log only`は履歴で、既定では非表示です。ログが開かれているだけでは
  子が作業中とは判断しません。[判定モデル](docs/LIVENESS.md)
- プレビューはローカルtmuxの可視画面を約2秒ごとに取得します。
  libghostty-vtで解析し、端末へエスケープ列を再送しません。
  取得内容はメモリ内のみで、入力送信・承認・ログ保存は行いません。
- libghostty-vtはGhostty.appの画面取得APIではありません。
  Ghostty単独やSSH先のプレビューは未対応です。Ghosttyへの移動には
  AppleScriptを使います。

SSHの登録、手動紐付け、任意hooks、開発コマンドは[英語README](README.md)を参照してください。
[全テストの監査](docs/TEST-AUDIT.md)には、削除・統合の判断と残した保証を記録しています。
不具合報告にはOS・端末・TTYbirdの版と匿名化した再現手順を添えてください。
実際の会話ログや認証情報を公開しないでください。ライセンスはMITです。
