# 通常のGhosttyタブをTTYbirdで表示する経路

調査日: 2026-09-16。以下は初回のソース・公開API・インストール済みアプリの静的調査。実セッションの画面取得、クリップボード変更、入力送信、アプリ再起動、新しい外部投稿は行っていない。

追記: 同日の[AX実機検証と時系列チェックポイント](https://ekusiadadus.com/ja/blog/ghostty-existing-tab-preview-ax)では、専用合成タブの文字取得に成功した。非選択タブの保持済み参照も読めたが、UUIDとの対応、分割による参照無効化、Unicode範囲、出力通知には制約が残った。以下の「未検証」は初回調査時点の記述。

追記: UUID指定のVTファイル書き出しをTTYbirdへ実装し、通常・非選択・分割後のタブで取得に成功した。対象を選択するとスナップショットを表示し、`r`で再取得できる。[実装後の検証と制限](GHOSTTY-EXPORT-VALIDATION.md)を参照。以下は探索時点の記録。

## 判断

「通常のGhosttyタブは表示不可能」は広すぎる。既存Ghosttyにはアクセシビリティ経由の文字読取りとファイルへの画面書出しがある。ただし、現在の公式外部APIだけで「UUIDで選んだ任意の既存タブを、色・カーソル・可視範囲を維持し、変更通知だけで更新する」ことは確認できない。

最初の候補はmacOS Accessibilityによる文字プレビューの小さな検証。その結果を踏まえ、正確な可視画面と通知が必要な部分だけGhostty側のAPI拡張へ切り出す。tmux導入やエージェントの再起動を初手の必須条件にしない。

## 確認した版

- インストール済み: `/Applications/Ghostty.app`, 1.3.1 build 15212。
- 公式リリース一覧の最新掲載: [1.3.1](https://ghostty.org/docs/install/release-notes)、2026-03-13。
- v1.3.1の実装commit: `332b2aefc6e72d363aa93ab6ecfc86eeeeb5ed28`。
- 調査時のmain: [d4c88d8069912b653d707191388ca98e24751f12](https://github.com/ghostty-org/ghostty/commit/d4c88d8069912b653d707191388ca98e24751f12)、commit日2026-09-15。
- TTYbird: 現在の作業ツリー。`src/terminal_preview.rs`はGhostty targetを明示的に拒否し、AXアダプターは存在しない。現在の案内文はこの実装制限を示すもので、macOS全体での技術的不可能を示さない。

## 経路の比較

| 経路 | 得られるもの | 残る問題 | 判断 |
|---|---|---|---|
| Accessibility / AX | 既存Ghosttyの端末文字列 | AX権限、対象の厳密な対応付け、隠れたタブ、可視範囲、出力通知 | 最初の限定PoC候補 |
| AppleScriptだけ | UUID、タイトル、作業場所、移動・操作 | 画面の返却APIなし | 単独では不足 |
| write_screen_file | plain / VT / HTMLの一時ファイル | clipboard・PTY入力・外部アプリ起動のいずれかを伴う | 常時プレビューの既定経路にしない |
| libghostty C APIだけ | 所有しているsurfaceの文字列 | 別プロセスのGhostty surface handleを取得できない | TTYbirdへリンクするだけでは解決しない |
| ScreenCaptureKit | ウィンドウの画像ストリーム | 画面収録権限、UUID/paneとの対応、非選択タブ、画像転送・縮小負荷 | 画像ミラーが目的なら別候補 |
| Ghosttyに小さな外部APIを追加 | UUID別snapshotと変更通知を設計可能 | アプリ側の変更・配布・権限設計、再起動が必要 | 正確な常時ミラーの本命 |
| TTYbird-owned PTY | 出力通知、画面、操作 | 既存の通常タブをそのまま取り込む仕組みではない | 新規セッションでは実装済み |

## AXで既にできそうなことと、まだ証明していないこと

v1.3.1の[SurfaceView_AppKit.swift](https://github.com/ghostty-org/ghostty/blob/332b2aefc6e72d363aa93ab6ecfc86eeeeb5ed28/macos/Sources/Ghostty/Surface%20View/SurfaceView_AppKit.swift#L2203-L2295)はAX text areaを実装し、`accessibilityValue()`で`cachedScreenContents`を返す。[キャッシュ実装](https://github.com/ghostty-org/ghostty/blob/332b2aefc6e72d363aa93ab6ecfc86eeeeb5ed28/macos/Sources/Ghostty/Surface%20View/SurfaceView_AppKit.swift#L247-L317)は内部の`ghostty_surface_read_text`を呼び、要求時に500msのキャッシュを使う。これはmainだけの新機能ではない。

そのため、Accessibilityの許可を受けた外部プロセスが[AXUIElementCopyAttributeValue](https://developer.apple.com/documentation/applicationservices/1462085-axuielementcopyattributevalue)で文字列を読む経路はソースから支持される。今回、実機でAXツリーを列挙して文字列を取得したわけではない。

制約は重要:

- `GHOSTTY_POINT_SCREEN`は可視viewportだけでなく保持しているscrollbackを含む。[座標の定義](https://github.com/ghostty-org/ghostty/blob/332b2aefc6e72d363aa93ab6ecfc86eeeeb5ed28/src/terminal/point.zig#L32-L48)。`AXValue`を無条件に丸読みして「画面だけ取得」と呼んではいけない。
- 内部にはVIEWPORTを読む`cachedVisibleContents`もあるが、AXValueとvisible-character-rangeはそれを使っていない。AXVisibleCharacterRangeという名前だけで可視範囲を保証できない。
- AXのattributed string実装で確認した属性はフォントであり、端末の全色・セル・カーソル・画像を復元できるAPIではない。
- SurfaceViewに安定したUUIDを返す`accessibilityIdentifier`実装は確認できなかった。タイトル・cwdの一致だけで自動紐付けしない。
- 非選択タブやsplitの列挙・残存AX参照の挙動は実機検証が必要。画面読取りのためにタブを勝手に切り替えない。
- 1.3.1の対象ファイルに出力更新のAX通知投稿はなく、mainで確認した投稿も選択テキスト変更である。出力変更の通知とは区別する。

実装を始める場合は、まず対象を利用者が明示的に選ぶ文字プレビューに限定する。bounded rangeを要求できるか、返却対象がviewportか最近の出力かを検証し、正確なラベルを付ける。対象を一意に特定できない場合は表示を停止する。

## AppleScriptとファイル書出し

[公式AppleScript資料](https://ghostty.org/docs/features/applescript)とインストール済み`Ghostty.sdef`を確認。terminalはid/name/working directoryを公開するがscreen/content getterはない。`perform action`の戻り値はbooleanで、生成されたファイルパスではない。mainの辞書にはpid/ttyが追加されているが、画面getterではない。

[write_screen_fileの仕様](https://ghostty.org/docs/config/keybind/reference#write_screen_file)は画面を一時ファイルへ書き出す。[現行実装](https://github.com/ghostty-org/ghostty/blob/d4c88d8069912b653d707191388ca98e24751f12/src/Surface.zig#L5803-L5858)にはplain/VT/HTML出力があるが、パスはcopy/paste/openへしか渡されない。copyはclipboardを変更し、pasteは対象PTYへ文字を入力し、openは外部アプリを起動する。

clipboardを保存・復元する方式も、他アプリの同時コピーを上書きする競合をなくせない。ファイル内容がディスクへ残る点も現在のTTYbirdのメモリ内プレビューと異なる。さらにformatterはunwrap=trueであり、完全なセル配置やカーソルをそのまま保持する画面プロトコルではない。

## libghosttyが解決する部分

[ghostty_surface_read_text](https://github.com/ghostty-org/ghostty/blob/d4c88d8069912b653d707191388ca98e24751f12/include/ghostty.h#L1228-L1233)は内部surface pointerを必要とする。同じライブラリをTTYbirdへリンクしても、既存Ghosttyプロセスのpointerが得られるわけではない。さらに現行の[ヘッダー冒頭](https://github.com/ghostty-org/ghostty/blob/d4c88d8069912b653d707191388ca98e24751f12/include/ghostty.h#L1-L11)はこのsurface APIをlibghostty-internalと明記している。公開libghostty-vt APIとは区別する。

色を保持するembedding APIの[PR #12909](https://github.com/ghostty-org/ghostty/pull/12909)は存在するが、今回のAPI確認ではmerged_at=null、closed。自動close理由は投稿者がvouchedでないことだった。技術的な不可能・設計却下の証拠として扱わず、公開版の利用可能APIとしても扱わない。

[libghostty README](https://github.com/ghostty-org/ghostty/blob/d4c88d8069912b653d707191388ca98e24751f12/README.md)も、libghostty-vtを端末シーケンスの解析と状態保持のライブラリと説明する。外部Ghostty.appへのリモート接続機能とは別である。

## pub/subを実現するには

Appleには[AXObserverAddNotification](https://developer.apple.com/documentation/applicationservices/1462089-axobserveraddnotification)と[valueChanged](https://developer.apple.com/documentation/applicationservices/kaxvaluechangednotification)がある。ただし送信側が適切な通知を出さなければ受信できず、登録自体がunsupportedを返すこともある。現状で「AXを使えば出力を完全イベント駆動で取得できる」とは言えない。

提案する最小Ghostty側変更は、安定したsurface UUID、bounded viewport snapshot、変更revision、coalesced invalidation通知。これは既存API名ではなく設計提案である。TTYbirdは初回snapshotを取得し、選択中だけ購読し、切断・欠落・resize時に再同期する。入力APIやscrollbackの全取得は初期要件に含めない。

AX属性とvalueChangedの拡張はmacOS限定の小さい試作候補。通知前にGhostty側の500msキャッシュを無効化する必要がある。通知だけ追加すると、受信直後の読取りで古い内容が返る可能性がある。全タブの明確な所有・寿命・可視範囲を保証するには、Ghosttyが管理するIPC経路の方が設計しやすい。rendererの描画callbackは候補だが、cursor blink等でも起きるため内容変更revisionと同一視しない。

## 他の既存実装

cmuxの[read-screen / capture-pane](https://manaflow-ai-cmux.mintlify.app/automation/cli-reference)と`surface.read_text`は、この種の外部読取りが実装可能な証拠。[PR #219](https://github.com/manaflow-ai/cmux/pull/219)は2026-02-21にmerge済み。cmuxが管理するsurfaceへの機能であり、通常Ghostty.appの既存タブへ接続する機能ではない。cmuxへの移行を今回の必須条件とはしない。

画像ならAppleの[ScreenCaptureKit](https://developer.apple.com/documentation/screencapturekit/sccontentfilter/init(desktopindependentwindow:))でウィンドウを取得し、Ghosttyの[Kitty graphics対応](https://ghostty.org/docs/features)を利用してTUI内へ描ける構成を考えられる。これは実装可能性の推論であり、今回の実機検証ではない。任意の非選択タブを正確に識別・取得できる保証はなく、viewport/cursorの構造化データとも異なる。

## 次の検証と合格条件

1. 合成Ghostty端末だけでAX権限とtext area列挙を検証。未許可なら明示し、無断で権限要求や画面取得を始めない。
2. 同じタイトル・cwdを持つ2タブ/2splitを作り、対象の取り違えがないことを確認。非選択タブで成功する範囲を記録する。
3. 選択・スクロール・alternate screen・resizeで、取得した文字列が何に対応するか確認。scrollbackの丸読みを避けられなければ制約として止める。
4. 閉じたtabの古いAX要素を再利用せず、別tabの文字列へすり替わらないことを確認。
5. 実際の出力通知を計測。無通知なら手動更新または選択中だけの限定的な取得であると説明し、pub/subと呼ばない。
6. 色・カーソル・背景タブ・通知まで必要ならGhostty側の小さいbridgeへ進む。既存Ghosttyを差し替えても実行中プロセスには適用されないので、利用中セッションを勝手に終了・再起動しない。

## 投稿済み質問

[ユーザーのコメント](https://github.com/ghostty-org/ghostty/discussions/2353#discussioncomment-18458021)をGitHub APIで再取得。調査時点でこのコメントへの返信は0件。後続の別コメントにSuperlogicalへの推測があるが、採用・拒否・製品計画に関する公式回答ではない。新たな投稿はしていない。
