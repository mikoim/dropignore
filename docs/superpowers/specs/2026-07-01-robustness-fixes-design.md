# 堅牢性バグ修正 + ドキュメント修正 設計

日付: 2026-07-01

## 背景

`dropignore` は inotify でディレクトリを監視し、ルールに一致したパスへ Dropbox の
`user.com.dropbox.ignored` 拡張属性を付与する CLI デーモンである。コードはクリーンで
clippy / テストとも通過するが、長時間稼働するデーモンとしての堅牢性、および誤った
パスへ属性を付与し得る正しさの問題が残っている。本設計はそれらのバグ修正 3 件と
ドキュメントの齟齬 1 件を対象とする。

## 対象範囲

- #1 setxattr 失敗 1 回でデーモンが停止する問題
- #2 監視中ディレクトリのリネーム/移動でレジストリが陳腐化し、誤ったパスへ属性が付く問題
- #3 inotify watch 上限（ENOSPC）到達時のエラーが分かりにくい問題
- #4 README のルール追加先パスの齟齬

### 非対象（YAGNI）

- 設定ファイルによるルール外部化
- ignore 属性を解除する unignore モード
- cookie による移動追従（移動元→移動先のパス張り替え）
- 起動時の watch 数事前チェック

## 設計

### #1 setxattr 失敗でデーモンを止めない

**現状**: `apply_dropbox_ignore`（`src/dropbox.rs`）は失敗時に内部で `error!` を出しつつ
`Err` を返す。呼び出し側の `apply_discovered_paths`（`src/app.rs:155`）および `event_loop`
内の適用箇所（`src/app.rs:105`）が `?` で伝播するため、1 件の失敗でプロセス全体が終了する。

**変更**:
- `apply_dropbox_ignore` のシグネチャ（`Result` を返す）と内部の `error!` ログは維持する。
- 呼び出し側で `?` による伝播をやめ、`Err` を握って処理を継続する。失敗の詳細は
  dropbox 側で既に `error!` 出力済みのため、呼び出し側での追加ログは不要（必要なら `debug`）。
- ディレクトリ監視の `add_watch` は従来どおり継続扱い（1 件の失敗で `apply_discovered_paths`
  全体を止めない形に揃える）。

**結果**: 個々のパスの権限エラーや一時的失敗が監視ループ全体を落とさない。

### #2 リネーム/移動でのレジストリ陳腐化（最小修正）

**現状**: `watch_mask()`（`src/watch.rs:9`）は `CREATE | MOVED_TO | DELETE_SELF | ONLYDIR`。
掃除は `event_loop`（`src/app.rs:60`）の `DELETE_SELF` 分岐のみ。監視中ディレクトリが
rename/移動されても通知されず、`WatchRegistry` に古いパスが残る。以降そのディスクリプタ宛の
イベントは古い親パスに解決され、**誤ったパスへ ignore 属性が付与され得る**。

**変更**:
- `watch_mask()` に `WatchMask::MOVE_SELF` を追加する。
- `event_loop` で `DELETE_SELF` と同様に `MOVE_SELF` でも `registry.remove_by_descriptor(&event.wd)`
  を実行する（両者を同一処理へ集約し、分岐を明快に保つ）。
- 移動先がツリー内であれば、親ディレクトリの `MOVED_TO` により再発見・再 walk され、
  新しいディスクリプタで再登録される。

**既知の許容事項**: ツリー外へ移動した場合、カーネル側の inotify watch 自体は残存し得る
（watch スロットのリーク）。ただし誤属性付与という実害は本修正で解消されるため、最小修正の
範囲としてこれは許容する。

### #3 watch 上限（ENOSPC）のエラー文言改善

**現状**: `add_watch`（`src/watch.rs:22`）の失敗 context は一般的な文言のみで、大きなツリーで
`max_user_watches` に到達した際に原因が分かりにくい。

**変更**:
- `watches().add(...)` の `Err` を検査し、`err.raw_os_error() == Some(libc::ENOSPC)` の場合のみ、
  `/proc/sys/fs/inotify/max_user_watches` の引き上げを案内する context を付与する。
- それ以外のエラーは従来どおりのメッセージを維持する。

### #4 ドキュメント修正

**現状**: `README.md` の "Extending rules" 節が「`Rule` トレイトを `src/main.rs` に実装し
`RuleEngine::new` に登録」と記載しているが、実際の実装先は `src/rules.rs`、登録は
`src/app.rs` の `RuleEngine::new` 呼び出し（`src/app.rs:22`）である。

**変更**:
- `Rule` トレイト実装先を `src/rules.rs` に修正する。
- 登録先を `src/app.rs` の `RuleEngine::new` 呼び出しへ追加、と明記する。

## テスト方針

- `WatchRegistry`（`src/watch.rs`）に対する単体テストを新規追加する:
  `insert` / `remove_by_descriptor` / `path_for` / `contains_path`。
  （`WatchDescriptor` の生成に実 inotify が必要な場合は、tempdir + 実 inotify で最小構成のテストを行う。）
- #1: 適用ループが 1 件の失敗で停止しないことを検証できる形に保つ。適用処理の継続ロジックを
  テスト可能な seam として分離する（例: 適用対象リストを渡すヘルパーを単体テスト可能にする）。
- #2 / #3: カーネル依存のため、`MOVE_SELF` を `DELETE_SELF` と同一処理へ集約し分岐を明快に保つことで
  レビューにより担保する。ENOSPC 判定は純粋関数（`raw_os_error` からメッセージを組み立てる部分）を
  切り出して単体テスト可能にする。

## 成功基準

- setxattr が 1 件失敗しても監視が継続する。
- 監視中ディレクトリのリネーム/移動後、古いディスクリプタ宛のイベントで誤ったパスに属性が付かない。
- watch 上限到達時のエラーメッセージに `max_user_watches` への言及が含まれる。
- README の "Extending rules" が実態（`src/rules.rs` / `src/app.rs`）と一致する。
- `cargo test` と `cargo clippy --all-targets` が通過する。
