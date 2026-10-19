# yagura — Claude 向けメモ

キーボードで操作するシステムモニター。Rust + egui 0.36、Windows 優先で macOS / Linux（それぞれ x86_64 / ARM64）も動かす。
姉妹プロジェクトは [kura](https://github.com/uchmk/kura)（yazi 風のファイルマネージャー）と [tsumugi](https://github.com/uchmk/tsumugi)（AI CLI のためのターミナル）。共有のクレート（`ito-*`）は [ito](https://github.com/uchmk/ito) にある（v0.32.1 で移した）。
見た目・キー・設定の置き場所は kura と揃える。画面の設計の元は HTML の見本（https://claude.ai/artifact/QhsVSAEHsUZmxbZbqyuGjj の「設計」タブ）。

## 作業ルール

- 返答は日本語。コード・コメント・コミットメッセージは英語。
- コミットと push は、そのリポジトリの検証（`scripts/verify.sh` が `ALL OK`）が通ったら、頼まれなくてもしてよい（`main` へ直接。force push・履歴の書き換えはしない。自分の変更だけを `git add` する）。
- 改行は LF（`.gitattributes` の `eol=lf`）。スクリプトで書き換えるときは改行を変えない（Python なら `newline=''`）。
- タスクは [TODO.md](TODO.md)、人への確認事項は `question` の issue（下の「確認事項」。QUESTIONS.md は Q1-Q2 の archive）。実機でしか確かめられないことは [TESTING.md](TESTING.md)（表が元）に積む（機能を足したら行を足し、`cargo run --example make-testcheck` で `TESTING-CHECKS.md` を作り直してから完了にする。印はこちらに付く）。
  - 未完了の項目の末尾の `【人】` `【実機】` `（要確認: #N）` は開発のセッションが進めない印。印は**裸で書く**（バッククォートで囲むと言及の扱いになり、
    `scripts/todo-open.sh` が数える。v0.42.0 で直した）。開発のセッションが進められる項目の数は `scripts/todo-open.sh`（`-v` で項目も）。
  - 実機の確認で見つかったもの: はっきり間違っていれば `bug`、そうでなければ `finding` の issue（フォーム `.github/ISSUE_TEMPLATE/finding.yml`）。直したら版を書いて閉じる。
  `[x]` を付けてよいのは実機のセッションと持ち主だけ。動きを変えた行は `[ ]` に戻す。

## 版・変更ログ・push

一人開発なので `main` への直接のコミット・push を許す。マージは merge コミット（rebase / squash を使わない）。`--force` は使わない。

- semver。版は `Cargo.toml` の `version` が正。PATCH = 修正・ドキュメント・小さな整理、MINOR = 機能・大きめの変更。
  既定キーや設定の互換性に関わる変更は CHANGELOG に**変更**として明記する。
- **版の繰り上げと [CHANGELOG.md](CHANGELOG.md) への追記はセット。**`version` を書き換える → `cargo build` で `Cargo.lock` も更新
  （忘れると CI の `--locked` が落ちる）→ CHANGELOG の先頭に節（Keep a Changelog、日本語、**日付は JST**: `TZ=Asia/Tokyo date +%F`）→ コードと一緒にコミット。
- コミットの件名は `vX.Y.Z: …`、最初の段落は英語で単体で通じるように書く（後でリリースノートにする）。
- **push の前は `scripts/verify.sh`**（build `--locked`・`fmt --check`・test・clippy を Linux・`x86_64-pc-windows-msvc`・`aarch64-apple-darwin` で。最後の行が `ALL OK: …` なら push してよい）。

## リリース

- `.github/workflows/release.yml` を Actions タブから `workflow_dispatch` で回し、`tag` に `vX.Y.Z` を渡す（タグが無ければ作られる。
  クラウドのセッションからタグは push できない）。`Cargo.toml` の版と違えば落ちる。成果物は 6 つ（Windows は `.zip`、macOS と Linux は `.tar.gz`、
  Windows は `yagura.exe`・`yagura.com`・ライセンスの 4 つ、ほかは実行ファイルとライセンスの 3 つのファイル）、最後に `release-sums.yml`（中身は `scripts/release-sums.sh`）が SHA-256 の表をノートに足す。
- ノートは前のタグからの `vX.Y.Z:` のコミットの最初の段落から作る。前のリリースのタグができてから次を投げる。

## ビルド・検証

- **kura / tsumugi と違い、`cargo fmt` を使ってよい**（`rustfmt.toml` で `max_width = 140`）。CI は `cargo fmt --check` を見る。
- clippy は `--all-targets -- -D warnings` で警告ゼロ。Linux では `#[cfg(windows)]` のコードがコンパイルされないので、Windows と macOS のターゲットの clippy も回す（macOS だけで使われないコードの警告も、Linux からはここでしか出ない）。
- テストは機械の設定ファイルに左右されてはいけない。一時ディレクトリは `util::test_dir("ラベル")`、設定は `settings::load(Some(dir))` に渡す。
- **画面は Xvfb で確かめる**: `scripts/xrun.sh OUTDIR --keys '<Wait:2000>2<Shot:cpu><State:a>'`。
  - `<Shot:名前>` は PNG、`<State:名前>` は画面・フォーカス・選択・列の幅などの文字を OUTDIR に書く。
  - `<Click:x,y>` と `<Drag:x1,y1-x2,y2>` の座標は窓に対する割合（0〜1）。境目を掴むときは `<State>` の `widths`（1000 分率）から位置を出す。
  - 設定は OUTDIR/config から読み（`UCHMK_CONFIG_DIR`）、列の幅は OUTDIR/state に書く（`YAGURA_STATE_HOME`）。前の回の列の幅は持ち越さない。
  - `XRUN_IDLE=10` で、最後の 10 秒の CPU 使用率を `cpu.txt` に書く。Xvfb は llvmpipe（ソフトウェア描画）なので、
    再描画 1 回が重く、1 秒ごとのサンプリングで 1 コアの 10% 前後に見える。yagura 自身（収集 0.1%・UI 0.7% ほど）とは分けて読む。
- **共通のスクリプト（`scripts/lanes.conf` の `files=` に並ぶもの）の正は ito の `scripts/lanes/`。**yagura の写しは直さない。
  ito で直して push → Cargo.toml の ito の `rev` を上げて `cargo build` → `scripts/lanes.sh sync`。`verify.sh` が `scripts/lanes.sh check` で差を見る。
- CI の `arm64`（`.github/workflows/arm64.yml`）は main への push と手動のときだけ、Windows と Linux の ARM64 のランナーで build と test を回す。
  `release.yml` はこれが緑でないと何も出さない。
- `Audit`（`.github/workflows/audit.yml`、2026-10-16 から。kura・tsumugi・ito と同じ）は `cargo audit`（RustSec の脆弱性の一覧）を
  main への push・PR・毎週月曜に回す。脆弱性で落ち、警告（保守されていないクレートなど）では落ちない。ci.yml と分けてあるのは、
  コードが変わらなくても新しい脆弱性が出れば赤くなるから。`merge-lanes.yml` は CI・Checklists・Audit の終わりで起きる。
- `Checklists`（`.github/workflows/checklists.yml`、v0.65.1 から）は `cargo run --example make-testcheck -- --check`（ito の `ito-testcheck`）で TESTING.md の表（`## 3d. …` の節の下に
  `| 3d.8 | … |`、`scripts/testcheck.toml` の `strict`）の形と、`TESTING-CHECKS.md`（生成物。行 `- [ ] **3d.8** …`、印は ` `・`x`・`~`）が元と合っているかを確かめる。TESTING.md だけの変更でも回る。行を足すときは節の中で空いている番号を使い、作り直した `TESTING-CHECKS.md` も一緒にコミットする。
  レーンの PR は行に印を付ければこれ（ジョブ `checklists`）が緑になるまでマージされず、merge-lanes.py は分担を push した後に ci.yml と checklists.yml を起こす。
- `Build`（`.github/workflows/build.yml`、v0.65.1 から。kura と同じ 6 つ）は main への push と手動で、リリースと同じ 6 つのターゲットを
  `--release` でリンクし、`yagura.com` を確かめ、実行ファイルとライセンスを成果物 `yagura-<名前>-<コミット>` に残す。テストは回さない。
- CI の `smoke` は `scripts/smoke-linux.sh`（xrun.sh で 1〜0 と F（fleet）の画面を順に撮り、panic・別の画面・真っ平らな画像で落ちる）。画像は成果物 `linux-smoke` に残る。
  画面を足したら、このスクリプトの `screens` にも足す。

## 確認事項

持ち主に決めてもらうことは `question` の issue にする（フォーム `.github/ISSUE_TEMPLATE/question.yml`。v0.42.0 から。QUESTIONS.md は archive）。

- 題は問いを 1 行で。本文は背景・止めている TODO・選択肢（番号を付け、推奨を 1 つ）。止めている TODO の項目の末尾に `（要確認: #N）` を書く。
- 投票に回してよいもの（持ち主の好みでなく、調べれば答えの出るもの）はラベル `vote` を付ける。票はコメントで
  `vote[cloud]: 2 — 理由`（クラウドのセッション）と `vote[win]: 2 — 理由`（持ち主の PC のセッション。`needs-pc` の issue で頼む）。
  取り消しは `uncount[cloud]:`。`.github/workflows/merge-lanes.yml` が数え、一致すれば `vote-decided`、割れれば `needs-owner` を付ける。
- 持ち主の `回答:` で始まるコメントは常に優先する（`answered` が付く）。反映したら TODO の印を外し、版を書いて閉じる。
- ラベルは同じワークフローが毎回そろえる（すぐ作るなら `scripts/labels.sh`）。

## Windows 実機のセッション（レーン）

tsumugi・kura と同じ仕組み（v0.58.0 から）。役割は [.claude/windows-role.md](.claude/windows-role.md)。

- タスク スケジューラの `yagura-auto-wintest` が毎時 :01 に `scripts/lane-boot.ps1`（ito の共通）を回す（tsumugi は :21、kura は :41）。
  main の `scripts/` を状態フォルダ（`%LOCALAPPDATA%\yagura-wintest`）に取り出して `auto-wintest.ps1` を実行する。作業フォルダは `C:\dev\yagura-wintest`
  （ARM64 は `C:\dev\yagura-armtest`、`-Lane arm`、タスク `yagura-auto-wintest-arm`）。
- `auto-wintest.ps1` がビルドとテストを済ませ、1 回分の塊（開いている `retest` の issue の行（x64 だけ、古い順）→ TESTING-CHECKS.md の節の順、最大 15 行。
  `scripts/wintest-queue.ps1`）を選んで Claude を起動する。道具は `scripts/wintest-kit.ps1`。塊が無い・自分の PR が開いている・ビルドが落ちたときは起動しない。
- 行の ID は TESTING.md の `3d.8` の形。`retest` の issue（題 `Re-test: vX.Y.Z`、本文は ID のチェックリスト）は、動きを変えた行を再テストに回すもの。
  動きを変えた行は印を `[ ]` に戻す。所見は `finding` の issue（`lane:win` / `lane:arm`）。
- 実機の PR（`test/win-*`・`test/arm-*`）は `Merge lanes`（`.github/workflows/merge-lanes.yml`、`scripts/merge-lanes.py`）が規則を確かめてマージする。
- UAC・サインアウト・ARM64 の機械が要る行は `needs-pc` のまま（人の作業）。レーンがやれる行は `retest` / `finding` の issue に回す。
- `scripts/check-ps1.ps1`（CI の `powershell` と `verify.sh`）がスクリプトの構文と塊の選び方を確かめる。tsumugi だけの確認は `scripts/check-ps1-app.ps1`
  （yagura のもの）。

## 持ち主の PC への依頼（needs-pc）

クラウドのセッションは、持ち主の Windows の PC でしかできないことを、ラベル `needs-pc` の Issue で PC のセッションに頼む
（雛形 `.github/ISSUE_TEMPLATE/needs-pc.yml`）。

- PC に頼むもの: 実機の確認（窓・キー・ConPTY・トースト・見た目）、Windows でだけ出る失敗の再現、リリース（`release.yml` を回す）、
  PC の環境（`%APPDATA%\uchmk` の設定・字体・タスク スケジューラ・wintest のレーン）。実装・テスト・文書・マージはクラウドのまま。
- 書くこと: やること、終わりの条件、版かコミット、頼んだセッションの題。会話の記録を読まなくても進められるように。
- PC のセッションは 30 分ごとくらいに開いている `needs-pc` を拾い、Issue に結果を書いて閉じ、頼んだセッションにもメッセージで知らせる。
  クラウドのセッションからは PC のセッションにメッセージを返せないので、PC への連絡は Issue だけで行う。
- PC のセッションはクラウドが作業しているブランチや main のコードを直接いじらない。直しが要れば担当のセッションに頼む。
- 開いている Issue と PR は GitHub Project（https://github.com/users/uchmk/projects/2）に 4 つのリポジトリ分が集まる。kura は自動で、ito・yagura・tsumugi は PC の needs-pc の見回り（毎時 :07 と :37、ラベル `lane-status` を除く）が足すので、30 分以内に載る。
  個人の Project なのでクラウドのセッションからは触れず、触る必要も無い。

## 設計の約束事

- **ディスクやプロセスに触る処理は UI スレッドで実行しない。**収集は `collect/` の 1 本のスレッドで、Source ごとに間隔を持ち、
  `Msg` を crossbeam で UI に送って `request_repaint` で起こす。間隔と一時停止は制御のチャネルで送る。
- **何もしていないときは再描画しない。**`request_repaint_after` はフレームの中で呼ぶ（`raw_input_hook` の中で頼んだものはフレームの頭で消える）。
  トーストのように時間で消えるものは、残り時間で起こす（`App::toast_left`）。
- **キーは keymap 経由**（`src/defaults/keymap.toml`、利用者の `keymap.toml` が上に重なる）。ハードコードしない。
  修飾キーの `C` は Windows と Linux で Ctrl、macOS で Cmd（macOS では Ctrl でも同じ）。`on_mac` で macOS だけ別のキーにできる。
- **画面の文字はすべて `tr("key")` / `trf("key", &[..])` を通す。**英語の表は `src/lang/en.toml`、日本語は `ja.toml`（キーを足したら両方に。
  テストがキーと `{0}` の数の食い違いを落とす）。既定の keymap の `desc` を変えたら `ja.toml` の `[keydesc]` も直す。
  幅は `util::width`（全角は 2 桁）で数える。
  言語は `config.toml` → `common.toml`（uchmk のアプリで共通。設定の画面が書く）→ OS の順で決まる。表に無いキーはテストが落とす。
- **uchmk の共通仕様**（ito の [docs/common-spec.md](https://github.com/uchmk/ito/blob/main/docs/common-spec.md)）: 設定の置き場所（`<config>/uchmk/common.toml`・`yagura/config.toml`・`themes/`）、
  テーマと時計は common.toml だけに置く（2 秒ごとに見て当て直す）、設定の画面は Ctrl+, で tsumugi と同じレイアウト（`ito-prefs`）、
  時計はステータスバーの右端。色は `config/theme.rs` の関数（`bg()`・`fg()`…）で今のテーマを読む。テストで全体のテーマを変えない（色を確かめるテストとぶつかる）。
- **列の大きさは `ito-layout`**（ito のクレート、`Cargo.toml` の `rev` で固定。ito のクレートは全部同じ `rev` にそろえる）。直すときは ito 側で直して push し、`rev` を上げる。
- **マルチ OS**: OS ごとの処理は `#[cfg(...)]` のモジュールに閉じ込める（例: `collect/topo/linux.rs`・`windows.rs`・`mac.rs`）。
  パースは純粋な関数にして、どの OS でもテストが回るようにする（`#[cfg(any(windows, test))]`）。
- **マルチアーキテクチャ**: Pure Rust のクレートを優先し、C のライブラリを抱えるものは避ける。
- 新しい画面を作るときは `Screen::built` に足し、`ui/` に描画を置き、keymap と README のキー表・画面の表・TESTING.md を更新する。

## 構成

| 場所 | 役割 |
| --- | --- |
| `src/main.rs` | eframe の起動、自前のタイトルバー（`ito-titlebar`）とベルの一覧（`ito-notices`。`App::toast` が最大 50 件を `notices` に残す）、`--keys` の受け付け、フレームごとの `pump` と描画、窓の位置の記憶（`Place`、`window.toml`） |
| `src/app.rs` | アプリの状態と `Act` の実行（画面・フォーカス・選択・絞り込み・並べ替え・列の幅・トースト）。ほかの機械に入っている間は `Away` が手元の `Model` を預かり、`sync_sources` が手元の収集と相手への `Want` を画面に合わせる |
| `src/collect/` | 収集のスレッド（cpu / mem / proc / gpu / net のインターフェース / disk / sensors と battery、svc は画面が出ている間は 5 秒・出ていなければ 60 秒ごと（fleet の警告の数のため）、net のソケットは画面が出ている間だけ、hw は起動時に 1 回だけ専用のスレッドで）。`topo/` は起動時に 1 回取るコアの構成（Linux は sysfs、Windows は Win32、macOS は IOKit のデバイスツリーと sysctl） |
| `src/collect/gpu/` | GPU・NPU。`linux.rs`（drm と accel の sysfs）と `fdinfo.rs`（`/proc/<pid>/fdinfo` の DRM のクライアントからプロセスごとの使用率とメモリ、sysfs に無い使用率）、`windows.rs`（PDH のカウンタを装置ごとにまとめる純粋な関数）と `pdh.rs`（PDH・DXGI の呼び出し）、`kmt.rs`（D3DKMT の温度・ファン・クロック）、`nvidia.rs`（NVML を実行時に読む）、`mac.rs`（IOAccelerator の読み値を装置に畳む純粋な関数）と `ioaccel.rs`（IOKit の呼び出し）。取れない値は `Why` で理由を持つ |
| `src/collect/svc/` | サービス。`linux.rs`（`systemctl` の出力のパース）、`windows.rs`（SCM の数値を状態に畳み、ドライバーを見分ける純粋な関数）と `scm.rs`（SCM の呼び出し、サービスとドライバー、設定は 1 分キャッシュ）、`mac.rs`（`launchctl list` と `print-disabled`、launchd の plist。plist はファイルの時刻でキャッシュ）。一覧が短い理由は `Note` |
| `src/collect/net/` | ネットワーク。インターフェースは sysinfo の数え値に、`linux.rs`（sysfs の種類・速度・ドライバー）と `windows.rs`（IP ヘルパーの表から人が思うアダプターを選ぶ純粋な関数）と `iphlp.rs`（IP ヘルパーの呼び出しと、隠れたアダプター（`NCF_HIDDEN`）のレジストリ、`Get-NetAdapter` が出すアダプター（WMI の `MSFT_NetAdapter`。WMI の部品は sensors の `lhm.rs` と共有の `collect/wmi.rs`））、`mac.rs`（`networksetup` のハードウェアポートで種類、`netstat -anvW` のパースでソケット）。ソケットは Linux が `/proc/net` と `/proc/*/fd`、Windows が `GetExtendedTcpTable` / `GetExtendedUdpTable` |
| `src/collect/disk/` | ディスク。ボリュームはどの OS も sysinfo。ドライブは `linux.rs`（`/proc/diskstats` と `/sys/block` のパース、ボリュームの持ち主のドライブ）、`windows.rs`（PDH のインスタンス名と `STORAGE_DEVICE_DESCRIPTOR` を読む純粋な関数）と `ioctl.rs`（PDH と `IOCTL_STORAGE_QUERY_PROPERTY`、SATA の `SMART_RCV_DRIVE_DATA` の呼び出し）、`mac.rs`（IOKit の読み値をドライブに畳む純粋な関数）と `iokit.rs`（`IOBlockStorageDriver` の `Statistics` と、ボリュームの装置から親をたどる呼び出し、NVMe の `IONVMeSMARTInterface` と ATA の `IOATASMARTInterface`）。`health.rs` は SMART の健康状態を 1 分に 1 回、専用のスレッドで聞く（Windows は `IOCTL_STORAGE_PREDICT_FAILURE` と `GetDevicePowerState`、Linux は `smartctl` の出力のパース、macOS は `diskutil info -plist`）。温度は Windows が `StorageDeviceTemperatureProperty`、Linux が hwmon、macOS が NVMe のログか ATA の属性 194。SMART の中身（寿命・時間・書き込み・エラー）は Windows の NVMe が `StorageDeviceProtocolSpecificProperty` の health ログ、macOS の NVMe が `IONVMeSMARTInterface` の同じログ（パースは OS に依らない `mod.rs` の `nvme_log`、`Check` への足し方は `Check::add_log`）、Windows と macOS の SATA が `SMART_RCV_DRIVE_DATA`（管理者だけ）と `IOATASMARTInterface` の属性の表（パースは `mod.rs` の `ata_attrs`）、Linux が smartctl の同じ出力。PDH の配列の読み方は gpu と共有の `collect/pdh.rs` |
| `src/collect/sensors/` | 温度とファン。`linux.rs`（`/sys/class/hwmon` のチップと、温度が無いときの `/sys/class/thermal`）、`windows.rs`（熱ゾーンのカウンターを畳む純粋な関数）と `zones.rs`（PDH の呼び出し）、`hwinfo.rs`（HWiNFO の共有メモリのパースと読み出し）、`lhm.rs`（LibreHardwareMonitor の WMI の行を畳む純粋な関数と WMI の呼び出し。ツールが動いていればこちらが先で、熱ゾーンは常に足す）、`mac.rs`（SMC のキーの型・値の読み方・名前と、HID の温度の名前の純粋な関数）と `smc.rs`（`AppleSMC` の呼び出し。キーは開くときに 1 回だけ並べる）、Apple silicon の HID の温度は sysinfo の components。GPU（NVML と Windows の D3DKMT）は gpu の読み値から足し、PCI のアドレスで重ねない。ドライブの温度は disk の読み値から足し、hwmon のチップ名で重ねない |
| `src/collect/hw/` | sys の画面の変わらない情報。`smbios.rs`（SMBIOS の表のパース。システム・ファームウェア・板・プロセッサーとキャッシュ・メモリの配列とスロット）、`windows.rs`（`GetSystemFirmwareTable('RSMB')` と `GetFirmwareType`）、`linux.rs`（`/sys/firmware/dmi/tables` → `/sys/class/dmi/id` → デバイスツリーの順）、`mac.rs`（`system_profiler -xml` のパース）。読めない理由は `Note` |
| `src/envreport.rs` | `yagura env`（版・OS・読んだ設定ファイル・設定の値と警告・補助ツールの有無・環境変数。kura の `envreport.rs` の写し。`--out FILE` は UTF-8 で書く） |
| `src/icon.rs` | アイコンをコードで描く（std だけ。`build.rs` も同じファイルを読む） |
| `build.rs` | Windows の `.res`（アイコンと版の情報）を ito の `ito-winres` で書き、MSVC のリンカーに渡す。rc.exe は要らない |
| `src/remote/` | fleet。`proto.rs`（postcard の枠・プロトコルの番号（9）・`Hello` と `Summary`（電池・警告の数・見る側の数）と差分の `Delta`、見る側が欲しいものの `Want` と間隔の `Pace`、操作の `Act` と答えの `Done`）、`wire.rs`（`Msg` の `&'static str` の読み戻し・procs の行の切り詰め・どの `Msg` をどの `Want` に送るかの純粋な関数）、`noise.rs`（直接つなぐ形。鍵の組と `key` のファイル、`Noise_IK` の握手、暗号の枠の `Reader` / `Writer`、`--key` と設定の画面に出す手引きと貼る行）、`agent.rs`（`--agent`。`127.0.0.1` で待ち受け、`[agent] listen` があればそこでも Noise の握手で `[[agent.viewer]]` の見る側だけを受ける。hub のスレッドが見る側の `Want` の和で収集を切り替え、`Msg` を 1 回だけ encode して合う見る側へ。変わらないものと最新の値は新しい見る側のためにキャッシュ。遅れた見る側は取りこぼす。要約は見る側ごとに差分と間隔（`Pace`）。`--log` でファイルにも書く。操作は `check` が自分の `allow` と自分の一覧で確かめ、1 回ごとのスレッドで実行して答える）、`client.rs`（`[[host]]` ごとのスレッド。つなぎ・ping・`Want` の送り直し・操作の `ToHost::Act` は 1 回だけ（つなぎ直しても送らない）・待ちを延ばしてつなぎ直す、`key` のある `[[host]]` は Noise でつなぐ（既定のポート 7314）。落ちた理由は `Why`（断られたら `Denied`・`BadKey`）。`probe` は sys の画面のために手元のエージェントに 1 回だけ聞く）、`mod.rs`（`Peer`: カードの状態と履歴、`Status`）。鍵の無い形は暗号も認証も持たず `ssh -L` か `tailscale serve` の上で、直接の形は事前に交換した公開鍵の Noise で使う |
| `src/collect/battery/` | 電池。`linux.rs`（`/sys/class/power_supply` のパース）、`windows.rs`（`GetSystemPowerStatus` の値を読み替える純粋な関数）、`mac.rs`（`AppleSmartBattery` の読み値の計算と IOKit の呼び出し）。sensors と同じ回に読む |
| `src/service.rs` | `--install-service` / `--uninstall-service`。Linux は systemd の user unit、macOS は LaunchAgent、Windows は HKCU の `Run` キー（ログオン時のタスクは管理者を要るため）。unit・plist・`reg` の引数は純粋な関数 |
| `src/bin/yagura-com.rs` | `yagura.com`、Windows のコンソールの前段。中身は ito の `ito_console::front`（v0.66.0 から、5 つのアプリで共有）で、yagura が持つのは `ANSWERS` と `TAKES_VALUE` だけ。文字で答えるコマンド（`ANSWERS`）は `yagura.exe` を走らせて待ち、ほかは窓が上がるまで待って戻る。`main.rs` の引数を足したらここの `ANSWERS` と `TAKES_VALUE` も揃える。`yagura.exe` の答えとエラーの行は `main.rs` の `out` / `err`（`ito_console`）で書く。`println!` / `eprintln!` は閉じたパイプで panic するので使わない |
| `src/kill.rs` | プロセスの終了（確認の後、1 回ごとのスレッドで。Windows は `TerminateProcess`、ほかは SIGTERM → SIGKILL） |
| `src/svcctl.rs` | サービスの開始・停止・再起動（確認の後、1 回ごとのスレッドで。Windows は SCM、Linux は `systemctl`、macOS は `launchctl`。権限不足とエラーの読み替えは純粋な関数） |
| `src/tree.rs` | procs の木の並び（親子と枝の線。親が隠れていれば一番近い見えている祖先へ、PID の使い回しは親にしない） |
| `src/model.rs` | 履歴のリングバッファ（60 秒）と最新の値 |
| `src/config/` | `keys.rs`（キーの文字列。中身は `ito-keys` の再エクスポート、`rev` は `ito-layout` と揃える）、`keymap.rs`、`cmd.rs`（コマンド文字列 → `Act`）、`settings.rs`、`edit.rs`（設定の画面から config.toml への書き込み。`[[host]]` は ito の `set_tables` で表をその場で直し、コメントと並びを残す）、`theme.rs`（今のテーマの色を読む関数と egui の `Visuals`。テーマは `ito-theme`） |
| `src/prefs.rs` | 設定の画面が読み書きするもの（common.toml・config.toml・自分のテーマ）。`ito-common` の見張りで 2 秒ごとに読み直して `Reload` で返し、書き込みと「Open」はスレッドで。自動起動の問い合わせ・設定したことの書き込み（`ask_startup`/`set_startup`）、覚えた窓の位置を忘れる（`forget_place`）、状態フォルダーを開く（`open_state`）、設定の書き出し・読み込み（`backup`。`ito_common::bundle`、yagura のフォルダーの `.toml` / `.json` だけ）、設定の初期化（`reset`。config.toml と keymap.toml を `*.bak-<秒>` に改名し、common.toml は残す）も同じ。結果は `Reload::Startup` などで返る |
| `src/i18n.rs`, `src/lang/` | `tr()` と言語の表（`en.toml`・`ja.toml`）、ヘルプの説明の `tr_desc()`。表・言語の決め方・`common.toml` の読み方・テストの検査は `ito-i18n`（ito のクレート、`rev` は `ito-layout` と揃える） |
| `src/fonts.rs` | CJK のフォント（OS のフォントをスレッドで読み、egui のフォントの後ろに足す） |
| `src/persist.rs` | `state.toml`（列の幅と、列の中のパネルの高さ）の読み書き |
| `src/stacks.rs` | 列の中にパネルを並べる `ito-layout` の木（`chain`・`grid`・`split`）と、保存した木が今の並びと同じ形かの確かめ。パネルの数で等分し、境目はドラッグとダブルクリックで動く。`App::stack` が画面と列ごとに覚え、`equalize` で忘れる |
| `--keys` の台本 / GPU の選択 | どちらも ito のクレート（`ito-keyscript`・`ito-gpu`）。`main.rs` が `Runner` を回し、`ito_gpu::options` で `backend` を当てる |
| コマンドパレット | `ito-palette`（`Ctrl+Shift+P`、`Mode::Palette`）。一覧は `App::palette_entries` が `[main]` のキーマップから作る（移動のキーは除く）。描画は `App::show_palette` |
| `src/ui/` | 描画。`overview.rs`・`cpu.rs`・`mem.rs`・`procs.rs`・`gpu.rs`・`svc.rs`・`net.rs`・`disk.rs`・`sensors.rs`・`sys.rs`・`tiles.rs`（コアのタイル）・`graph.rs`・`help.rs`・`confirm.rs`（プロセスの終了とサービスの操作の確認）・`fleet.rs`（fleet のカード。この機械と `[[host]]`、つなぎ方の手引き）・`settings.rs`（設定の画面の 7 ページ（一般・テーマ・Fleet・キー・通知・詳細・情報）。枠と部品（Window・Startup・Advanced・About のカードも）は `ito-prefs`） |
| `scripts/` | `verify.sh`（push の前）、`xrun.sh`（Xvfb で起動して撮る）、`smoke-linux.sh`（CI のスモーク）、`release-sums.sh`（リリースの SHA-256 の表） |
