# TODO

v0.1.0 で overview・cpu・mem・procs の 4 枚ができた。残りは下の順で進める。
【人】 は持ち主の作業、【実機】 は実機でしか確かめられないもの（[TESTING.md](TESTING.md) に積む）、（要確認: #N）は `question` の issue で止まっているもの。
印は裸で書く（バッククォートの中は言及の扱い）。開発のセッションが進められる項目の数は `scripts/todo-open.sh`。

## 0. 次の機能（持ち主と決めた順）

- [x] 値のまとめ出力: `--snapshot`（JSON）と `--log-metrics`（CSV）。v0.44.0。実機の確認は TESTING.md 3i 【実機】。
- [x] 履歴の拡張（v0.45.0。実機の確認は TESTING.md 3j 【実機】）: 1 秒刻みで 5 分を基本に、15 分（5 秒平均）と 1 時間（30 秒平均）まで。システム全体の系列だけ（プロセスごとには持たない）。グラフをダブルクリックでフルサイズ、既定は 5 分、`[` `]` で 5 分 → 15 分 → 1 時間。
- [x] 通知（v0.46.0。実機の確認は TESTING.md 3k 【実機】）: ito に `ito-notify`（通知・音・webhook）を切り出し、yagura は `[notify]` と `[[alert]]`（cpu・mem・swap・gpu・temp・disk・battery）を持つ。
- [x] 通知: tsumugi の `sound.rs`・`webhook.rs` を `ito-notify` に載せ替える（tsumugi v0.104.3 で済み。トーストの Allow / Deny と点滅・バッジは tsumugi 側に残る）。
- [x] 通知: 設定の画面の「通知」のページ（v0.59.0。ito v0.30.0 の `notify_page`。実機の確認は TESTING.md 5.24 【実機】）。
- [ ] 通知: サービスの停止のアラート（`[[alert]]` に service の種類を足す）。v0.51.0 で実装済み、残りは TESTING 3k.5 の確認 【実機】
- [x] procs: 優先度・アフィニティ・開いているファイルとハンドル・ソケットの数（v0.47.0。実機の確認は TESTING.md 3l 【実機】）。
- [x] procs: 優先度を 1 段ずつ変える `}` `{`（v0.62.0）とアフィニティを選ぶ `A`（v0.64.0）。どちらもこの機械だけ。確認は TESTING.md の 3.12〜3.15【実機】。
- [ ] procs: プロセスごとのネットワーク量（送受信の速さ）。見送り: Windows は ETW（管理者）、Linux は eBPF か pcap が要り、どちらも C のライブラリや権限を抱える。
  fleet の相手のプロセスの詳細（優先度・CPU・ハンドル）も予定しない（プロトコルを広げる割に使い道が少ない）。
- [x] コンテナ（Docker / Podman）の CPU とメモリ（v0.48.0）。ランタイムの無い Linux の cgroup の分は下の containers の行。
- [x] ログインユーザー・電源プラン・ロードアベレージを overview か sys に。ロードアベレージは cpu 画面にあり、ユーザーと電源プランを sys 画面に足した（v0.50.0）。
- [ ] sys 画面のログインと電源プランを実機で確かめる（TESTING 3o）【実機】
- [x] エージェントの Prometheus 互換 `/metrics`。
  → `[agent] metrics = "127.0.0.1:9314"`（平文の HTTP、既定は無効）。hub が頼まれたときだけ `metrics.rs` で今の値をテキストにする（v0.49.0）
- [ ] `/metrics` の実機の確認 【実機】（TESTING.md 3n）。

## 0a. Linux の端末版 `yagura-tui`（Proxmox VE 向け）

- [x] `yagura-core`（egui なしの収集）を分ける（v0.73.23）。
- [x] `yagura_core::proxmox`（`pvesh` の一覧・コンテナの swap・`pct exec` の中身）と、最初の `yagura-tui`（ゲスト一覧・プロセス・中を見る）。v0.73.30。確認は TESTING.md 3p 【人】（Proxmox のホストが要る）。
- [ ] ゲストの履歴（rrddata）のグラフ、スナップショット・タスク・設定の表示。
- [ ] VM の swap（`/run/qemu-server/<id>.pid` → `/proc/<pid>/status` の VmSwap。未確認）。
- [ ] コンテナの中のアプリの版（community-scripts は `/root/.<appname>`）と GitHub の最新との比較。対応表と 1 時間 60 回の制限のキャッシュが要る。
- [x] 配布物に入れる（v0.73.32: ito の `release.yml` が `yagura-tui` を扱うようになり、`lanes.conf` の `tui_crate` / `tui_bin` で入れた。次のリリースで 8 つの成果物になる）。

## 1. 残りの画面（番号は v0.1 で固定済み）

- [x] procs: プロセスの終了（`x`、確認のオーバーレイを挟む。Windows は `TerminateProcess`、ほかは SIGTERM → SIGKILL）。
- [x] procs: 木の表示（親子）と、スコープ（All / Mine）の切り替えのキー。
- [x] procs: Windows で管理者でないと、ほかのユーザーのプロセス（SYSTEM の svchost・lsass・dwm など）の USER が `—`、MEM が `0 B` になる（uchmk/yagura#3）。タスク マネージャーは同じ権限で出す。
  sysinfo は `PROCESS_QUERY_LIMITED_INFORMATION` に落とすが、トークンもワーキング セットも読めない。`WTSEnumerateProcessesExW`（レベル 1: SID とワーキング セット、ハンドルが要らない）で欠けたものを埋める案。
  → 欠けがあるときだけ WTS を読み、PID と名前が合うものの USER（`LookupAccountSidW`、SID ごとにキャッシュ）と MEM を埋める。ワーキング セットの単位（バイトか KiB）は両方が読めたプロセスとの比で決める（v0.41.4、TESTING.md 3.11）。
- [ ] procs: 管理者でない Windows で、ほかのユーザーのプロセスの USER がまだ `—`（MEM は合う。uchmk/yagura#13）。`tasklist /v` も N/A なので、
  WTS のレベル 1 の `pUserSid` が管理者でないと空で返るのかを実機で見る 【実機】。空なら TESTING.md 3.11 を `—` を期待する形に直す（タスク マネージャーは昇格して読む）。
- [x] gpu·npu（`4`）: 使用率・VRAM・温度。Windows は PDH の `GPU Engine` カウンタ、Linux は `/sys/class/drm` と amdgpu / i915 の sysfs、NVIDIA は NVML を動的に読む（無ければ出さない）。NPU は見本の「設計」タブの限界のとおり、取れる OS だけ。
- [x] gpu·npu: Windows の dwm の専用メモリが 38.1 GiB と出た（PDH の `GPU Process Memory` の値そのまま。カードは 16 GiB、uchmk/yagura#4）。カードの総量を超える値は `—` にするか、総量で抑える。
  → 総量を超える値は `—` にする（抑えると嘘の数が出る。v0.41.4、TESTING.md 3a.14）。
- [x] gpu·npu: Windows の AMD でドライバーの版が `—`（dxdiag は 32.0.31041.1004、uchmk/yagura#4）。DXGI の `CheckInterfaceSupport` の UMD の版か、レジストリの `DriverVersion` から取る。
  → DXGI の `CheckInterfaceSupport(IDXGIDevice)` の UMD の版（dxdiag と同じ数）。どのベンダーでも出る（v0.41.4、TESTING.md 3a.15）。
- [x] procs: 右の列の開始の時刻が UTC で出る（`ui/procs.rs` の `fmt_epoch`、uchmk/yagura#4）。手元の時刻にする。
  → `util::fmt_local`（chrono の `Local`）。sys の起動の時刻は UTC のまま（v0.41.4、TESTING.md 3.12）。
- [x] svc（`6`）: Windows のサービス、Linux の systemd のユニット、macOS の launchd。読むだけから始める。
- [x] svc: 開始・停止・再起動のキー（確認を挟む。Windows は `ControlService`、Linux は `systemctl`、macOS は `launchctl kickstart` / `kill`）。権限が足りないときの言い方も。
- [x] svc: macOS の launchd の plist（`/Library/LaunchDaemons` など）を読み、止まっている・無効のジョブ、コマンドとファイルも出す。今は `launchctl list` だけ。
- [x] svc: macOS で読み込まれていないジョブ（無効のものなど）の開始。今は `launchctl kickstart` が「見つからない」で失敗してトーストに出る。`launchctl enable` と `bootstrap` まで行うか（無効を解くのは持ち主の設定を変えることになる）決める。
  → 有効なジョブは `launchctl bootstrap <ドメイン> <plist>` で読み込んでから `kickstart` する。無効のジョブは無効を解かず、`launchctl enable …` をトーストで教えるだけ（v0.7.2）。
- [x] svc: Windows のドライバー（`SERVICE_DRIVER`）を出すか、左の列の切り替えにするか決める。
  → 一緒に読み、`u`（procs の Mine や net の LISTEN と同じ鍵）でサービス ⇄ ドライバーを切り替える。左の列に今の種類を出す。ドライバーの操作の行は `sc.exe start|stop`（v0.7.3）。
- [ ] svc の実機の確認 【実機】（TESTING.md 3b）。
  3b.1・3b.5・3b.6・3b.9 は済み（uchmk/yagura#5）。3b.2 の起動と停止・3b.8・3b.14 のドライバーの停止は管理者が要り、持ち主の UAC を待つ。
- [ ] svc: Windows で WMI の `StartMode` が Unknown の 2 件が、yagura では自動か手動に入っている（uchmk/yagura#5）。どのサービスか、SCM の `QueryServiceConfig` が何を返すかを実機で見る 【実機】。
- [x] net（`7`）: インターフェースごとの送受信のグラフ、接続の一覧。Windows は IP ヘルパー、Linux は sysfs と `/proc/net`・`/proc/*/fd`。
- [x] net: macOS のソケットの一覧（`proc_pidinfo` の `PROC_PIDFDSOCKETINFO`、または `netstat -anv` のパース）と、インターフェースの種類（`SCNetworkInterface`。Bluetooth PAN も見分ける）。今はインターフェースだけ。
  → ソケットは `netstat -anvW` のパース（PID の列は見出しから数える。古い `pid` と Sonoma 以降の `process:pid` の両方）。`proc_pidinfo` は他の利用者のプロセスを root なしで覗けず、
  `lsof` は 2 秒ごとには重いので使わない。種類は `networksetup -listallhardwareports` のハードウェアポート名（Wi-Fi・Ethernet・Bluetooth PAN・Thunderbolt Bridge）で、
  1 分ごとに読み直す。SCNetworkInterface のバインディングを抱えずに済む。ポートの無いもの（utun・awdl など）は名前から推す（v0.7.4）。
- [x] net: 選んだインターフェースで接続を絞る（ローカルのアドレスがそのインターフェースのもの）か決める。今は全部を出している。
  → 選ぶだけでは絞らない（左の選択はいつもあるので、ほとんどが隠れる）。インターフェースで `Enter` を押すと絞り、どのアドレスにも付いたもの（`0.0.0.0`・`::`）は残す。選択を動かせば追い、`Esc` で外す（v0.7.5）。
- [ ] net の実機の確認 【実機】（TESTING.md 3c）。
  3c.2・3c.3・3c.7・3c.8・3c.10 は済み（uchmk/yagura#6）。3c.1 は Bluetooth が出ず、消えた `Wi-Fi 2`〜`5` が出ていたので v0.41.4 で直した。v0.41.4 でも隠れた `Wi-Fi 3`・`5` が残ったので v0.42.2 で直した（uchmk/yagura#13）。もう一度見てもらう。
- [x] net: Windows で消えた装置（`Not Present`）と隠れた Wi-Fi Direct を落とし、Bluetooth PAN はいつも残すようにした（v0.41.4）。Wi-Fi 3・5 が本当に Wi-Fi Direct だったかは
  `Get-NetAdapter -IncludeHidden | fl Name,InterfaceDescription,Status` で確かめる 【実機】。違えば隠れた装置を見分けるほかの手（`MIB_IF_ROW2` の `ConnectorPresent` と `HardwareInterface`、ifType）を探す。
  → 実機では Wi-Fi 3・5 は Wi-Fi Direct ではなく、本物のカードの隠れたインターフェース（`NCF_HIDDEN`）だった（uchmk/yagura#13）。
    ネットワークのクラスのレジストリのキーの `Characteristics` に 0x8 が立つものを `NetCfgInstanceId` で引いて落とす（30 秒キャッシュ、v0.42.2）。
- [x] net: 実機の小さな指摘（uchmk/yagura#6）。
  - `Enter` で絞った後に `j` で移ると、絞り込みは追うがトーストは前のインターフェースの名前のまま。移ったときはトーストを出し直すか、出さない。
  - 左の列でインターフェースの名前が `イーサネ…` と切れる。速さの桁を詰めるか、名前の幅を広げる。
  - 真ん中のグラフの題の速さと左の列の速さが少しずれる（読む時刻か平均の取り方の違い）。同じ値を使う。
  - `/` の絞り込みの文字が procs・svc・net で共有される。画面ごとに持つか決める（procs で打ったものが net に残ると驚く）。
  → トーストは移った先の名前で出し直す。名前の列を広げる（列の比を `[1.5, 3.2, 1.8]`）。左の列の速さは受信と送信の和なので、数の行に `rx+tx` と書く（グラフの題は片方ずつ）。
    `/` は画面ごとに持ち、打っている途中で画面を変えると打つのを終える（v0.41.5）
- [x] disk（`8`）: ドライブごとの使用量と読み書きの速さ。
  → 左がドライブ（1 分の読み書き）、真ん中がマウントされたボリューム（容量・使用率・読み書き）、右が選んだドライブのグラフ（読み・書き・ビジー）と型番・種類・バス・容量。
  Windows は PhysicalDisk のパフォーマンスカウンター（PDH）と `IOCTL_STORAGE_QUERY_PROPERTY`（管理者は要らない）、Linux は `/proc/diskstats` と `/sys/block`、ボリュームはどの OS も sysinfo（v0.8.0）。
- [x] disk: macOS のドライブ（IOKit の `IOBlockStorageDriver` の `Statistics`）。型番・SSD / HDD・バス・BSD 名と大きさ、ボリュームは `statfs` の装置から IOKit を上って結ぶ。忙しさは macOS に無いので出さない（v0.11.0）。
- [x] disk: SMART の状態と温度（Windows は `MSFT_PhysicalDisk` / `StorageReliabilityCounter`、Linux は NVMe の `hwmon` と `smartctl`）。sensors の画面と合わせて決める。
  Linux の NVMe の温度は v0.9.0 から sensors の画面に出る（hwmon の `nvme`）。残るのは SATA の温度（`drivetemp` が無いとき）と、Windows・macOS の温度、SMART の健康状態。
  → 健康状態は 1 分に 1 回、専用のスレッドで聞く（ドライブの顔ぶれが変わったときも）。Windows は WMI ではなく `IOCTL_STORAGE_PREDICT_FAILURE`（`GetDevicePowerState` で回っていないドライブは起こさない）、
  Linux は `smartctl -H -A -n standby`（root か `disk` のグループが要る。無ければ理由を出す）、macOS は `diskutil info -plist` の `SMARTStatus`。
  温度は Windows が `StorageDeviceTemperatureProperty`（SSD は 5 秒ごと、ほかは 1 分の確認で）、Linux が hwmon、無ければ smartctl の値。
  右の列に「Health」と「Temperature」の行、壊れかけのドライブは左の名前も赤。hwmon のチップに無いドライブの温度は sensors の画面にも出る（v0.12.0）。
- [ ] disk: Windows で `IOCTL_STORAGE_PREDICT_FAILURE` に答えないドライブ（実機の 3d.8 で分かる）があれば、WMI の `MSFT_PhysicalDisk` の `HealthStatus` と `StorageReliabilityCounter`（管理者）を足す。
  → NVMe は答えた（uchmk/yagura#2、Samsung 990 EVO Plus）。WMI の counter は温度・Wear だけで、通電時間とエラーは空。NVMe には足しても得るものが無い。SATA の機械で 3d.8 と 3d.17 を見てから決める。
  寿命の消費は yagura が 1%、`Get-StorageReliabilityCounter` の `Wear` が 0 だった。yagura は NVMe の health ログの Percentage Used をそのまま出している。OS が丸めているのか別の数なのかは、CrystalDiskInfo か smartctl のある機械で比べる 【実機】。
- [x] disk: macOS の NVMe のドライブの温度と SMART の中身（IOKit の `IONVMeSMARTInterface`）。
  → `NVMe SMART Capable` の付いた親の user client を `IOCreatePlugInInterfaceForService` で開き、`SMARTReadData` の 512 バイトを `disk::nvme_log` で読む。sudo は要らない（v0.14.0）。
- [x] disk: macOS の SATA のドライブの SMART の中身（`IOATASMARTInterface` の `SMARTReadData` と、温度は属性 194）。今は内蔵の SATA の Mac（古い Intel の Mac）と、SMART を通す外付けのケースだけが対象。
  → `SMART Capable` の付いた親の user client で `SMARTReturnStatus` と `SMARTReadData` を読み、属性の表は OS に依らない `disk::ata_attrs` で読む（Windows の `SMART_RCV_DRIVE_DATA` も同じ 512 バイト）。sudo は要らない（v0.15.0）。
- [x] disk: SMART の中身（使った寿命の %・代替処理済みのセクタ・電源を入れた時間）を右の列に。smartctl は `-j`、Windows は `IOCTL_STORAGE_QUERY_PROPERTY` の `StorageDeviceProtocolSpecificProperty`（NVMe の SMART ログ）。
  → 右の列の温度の下に「Life used」（80% から黄、100% で赤）・「Powered on」・「Lifetime writes」・「Remapped / Pending sectors」・「Media errors」（1 から黄）を、ドライブが答えた分だけ出す。
  Linux は `-j` ではなく今の `smartctl -H -A` の文字を読む（JSON を読むために serde_json を足さないため）。止まったドライブは前の値を残す。
  Windows は NVMe の SMART / health ログ（ページ 2）を読み、critical warning も故障の印にする。512 バイトのパースは OS に依らない `disk::nvme_log`（v0.13.0）。
- [x] disk: Windows の SATA の SMART の中身（`SMART_RCV_DRIVE_DATA` の属性。管理者が要る）。管理者でないときに何を出すかと合わせて決める。
  → 管理者なら属性を読み、そうでなければ「SMART counts: needs administrator」の 1 行を薄く出す（健康状態の行がすでに管理者が要ると言っていれば出さない）（v0.16.0）。
- [ ] disk の実機の確認 【実機】（TESTING.md 3d）。
- [x] sensors（`9`）: 温度・ファン（sysinfo の `Components`）。
  → 左がチップ（チップごとの一番熱い温度の 1 分のグラフ）、真ん中が温度とファンの一覧（今・1 分の最低と最高・高温の目安・限界）、右が選んだセンサーのグラフと目安の線。
  sysinfo の `Components` は macOS だけで使う。Linux は sysinfo がファンを読まずチップでまとめないので `/sys/class/hwmon` を自分で読み（温度が無ければ `/sys/class/thermal`）、
  Windows は sysinfo が管理者の要る WMI（`MSAcpi_ThermalZoneTemperature`）を使うので、管理者の要らない `Thermal Zone Information` のパフォーマンスカウンター（PDH）を読む。
  NVIDIA の GPU は NVML の温度とファンを足し、amdgpu のように OS が並べているものは PCI のアドレスで重ねない（v0.9.0）。
- [x] sensors: macOS のファンと、SMC のキーで読める温度の名前（`TC0P` などを「CPU proximity」に）。今は sysinfo の components の温度だけ。
  → `AppleSMC` を直に読み、名前の付いた温度とファン（最小・最大つき）を出す。Apple silicon の HID の温度にも名前（v0.17.0）
- [x] sensors: Windows のファンとコアの温度。メーカーのドライバーか、LibreHardwareMonitor のようなカーネルドライバーが要る（署名・管理者・同梱の是非を QUESTIONS で決めてから）。今は ACPI の熱ゾーンだけ。
  → Q2 の 1。HWiNFO の共有メモリと LibreHardwareMonitor（OpenHardwareMonitor）の WMI を、動いていれば読む。ドライバーは同梱しない (v0.30.0)
- [ ] sensors の実機の確認 【実機】（TESTING.md 3e）。
- [x] sys（`0`）: OS・BIOS・メモリのスロットなど、変わらない情報。
  Windows と Linux はファームウェアの SMBIOS の表を自分で読む（`smbios.rs`、純粋な関数）。Windows は `GetSystemFirmwareTable('RSMB')` で管理者が要らない。
  Linux は `/sys/firmware/dmi/tables/DMI` が root だけなので、ふだんは `/sys/class/dmi/id`（スロットなし、そう言う）、SMBIOS の無い ARM の板はデバイスツリーの `model`。
  macOS は `system_profiler -xml`（Apple シリコンはパッケージの中の 1 塊）。WMI（`Win32_PhysicalMemory`）は COM が重く遅いので使わない。
  起動時に 1 回、専用のスレッドで読む。シリアル番号は読んでも出さない（画面の写しを人に見せることがあるため）（v0.10.0）。
- [x] sys: CPU の型番・コア数・キャッシュ・ソケットを SMBIOS の type 4 と 7 から足す（今は sysinfo の CPU の名前だけ）。
  → 左の列に「Processor」の節。埋まったソケットの中央処理装置だけを読み、キャッシュはハンドルで type 7 を引く。macOS は system_profiler のチップ名とコア数（v0.18.0）
- [x] sys: GPU とドライブの一覧への近道（gpu·npu・disk の画面へ飛ぶ）を置くか決める。
  → 新しいキーは足さず、左の列に「Graphics」「Drives」の節（名前・VRAM、型番・大きさ）を置き、末尾にその画面を開く keymap のキーを出す（v0.19.0）
- [ ] sys の実機の確認 【実機】（TESTING.md 3f）。
- [x] devices（D）: USB とデバイス マネージャーに出るハードの一覧（クラス・ドライバー・状態・問題のコード・つながりの木）。
  → `collect/devices/`（Windows は SetupAPI と CfgMgr32、Linux は sysfs と pci.ids / usb.ids、macOS は IOKit の `IOUSBHostDevice` と `IOPCIDevice`）。
  画面が出ている間だけ 5 秒ごと。fleet はプロトコル 6 の `Want.devices` (v0.43.0)
- [ ] devices の実機の確認 【実機】（TESTING.md 3h）。
- [x] containers（C）: Docker・Podman のコンテナの一覧（状態・CPU・メモリ・イメージ・直近 1 分のグラフ）。
  → `collect/containers/`（エンジンの Docker 互換 API を Unix ソケット / Windows の名前付きパイプで。`docker` コマンドは使わない）。
  画面が出ている間は 3 秒ごと、出ていなければ 1 分ごと（fleet のカードの数のため）。fleet はプロトコル 7 の `Want.containers` (v0.48.0)
- [ ] containers の実機の確認 【実機】（TESTING.md 3m）。
- [ ] containers: ランタイムが無い Linux で cgroup（`/sys/fs/cgroup`）から LXC・systemd-nspawn などを拾う。
- [x] containers: 開始・停止・再起動の操作（`svcctl` と同じ確認の形で）。
  → `S`・`x`・`R`。エンジンの API に `POST /containers/{id}/{start,stop,restart}` を、一覧を読んだエンジンにだけ別のスレッドで送る。この機械だけ (v0.65.0)
- [ ] containers: fleet の相手のコンテナの開始・停止・再起動（今は見るだけ。プロトコルに操作を足し、相手の側で確かめる形が要る）。
- [x] containers: fleet のカードの警告に、再起動を繰り返すコンテナ・異常終了したコンテナの数を足す (v0.52.0)。

## 2. 構成と OS

- [x] macOS のコアの構成: `sysctl hw.perflevelN.*`（P / E コアと L2 の塊）。今は 1 論理 CPU = 1 コアで描く。
  → Apple silicon は IOKit のデバイスツリー（`cpus` の子の `cluster-type`・`cluster-id`）でクラスターごとの塊、L2 の大きさは sysctl。Intel の Mac は sysctl のコア数とスレッド数（v0.20.0）
- [x] Windows でプロセスのユーザー名が出るか確かめる 【実機】（TESTING.md 3.x）。
  → TESTING.md 3.1 で確かめた（Windows 11 x64、v0.40.1、uchmk/yagura#3）
- [ ] Windows ARM64（Snapdragon）の構成: `EfficiencyClass` が P / E を分けるか 【実機】。
  持ち主の PC は x64 で、Snapdragon の機械が無い（uchmk/yagura#10 は開いたまま。TESTING.md 1.2・2.3・3a.4）。
- [ ] AMD の GL ドライバーで落ちる件（filer の TODO と同じ `atio6axx.dll`）。filer で原因が分かったら同じ対策を入れる。今は `config.toml` の `backend = "dx12"` で避けられる。
- [x] macOS の GPU: IOKit の `IOAccelerator` の `PerformanceStatistics`（使用率・VRAM）。Pure Rust の `objc2-io-kit` で読めるか確かめる。
  → `objc2-io-kit` で読める。Apple silicon は使用率・Renderer / Tiler・使用中の統合メモリ、AMD は VRAM とセンサーも（v0.21.0）
- [x] Windows の GPU のセンサー（温度・クロック）を NVIDIA 以外でも: D3DKMT の `D3DKMTQueryAdapterInfo`（Task Manager の温度の出どころ）。
  → `gpu/kmt.rs` が LUID でアダプターを開き、温度・ファン・最初のエンジンのクロックを毎回聞く。NVML の値が優先（v0.22.0）
- [x] Linux の Intel GPU の使用率: `/proc/<pid>/fdinfo` の `drm-engine-*`（i915 / xe が出す）から、全体とプロセスごとの使用率を出す。amdgpu も同じ形でプロセスごとが取れる。
  → `gpu/fdinfo.rs` が `/dev/dri` の fd を 5 秒ごとに探し、毎回 fdinfo を読む。sysfs に使用率の無い GPU は fdinfo の合計を使う（v0.23.0）
- [x] NVML のプロセスごとの使用率（`process_utilization_stats`）。今はメモリだけ。
  → 前の回の後の標本を SM・エンコーダー・デコーダーの大きいほうで平均する。Linux だけ（Windows はカウンタが出す）（v0.24.0）
- [ ] GPU·NPU の実機の確認 【実機】（TESTING.md 3a）。

## 3. 言語

- [x] 日本語の表（`src/lang/ja.toml`）と CJK のフォント（tsumugi の `fonts.rs` の取り方）。
  → `fonts.rs` が OS のフォントをスレッドで読んで後ろに足す。全角は 2 桁で数えて切る。ヘルプの説明は `[keydesc]` で訳す（v0.25.0）
- [x] `i18n` と `common.toml` の読み込みを tsumugi の workspace の共有クレートに切り出し、filer と tsumugi から使う（tsumugi を `access:"push"` で付け直す）。
  tsumugi の設定画面の言語の欄もそれにつなぐ。
  → `tsumugi-i18n`（tsumugi v0.78.0）: 設定の置き場所・`common.toml` の `language` の読み書き・言語の決め方・表・表とソースの検査。
  tsumugi の設定画面の General → Language が `common.toml` に書く。mimamori はこのクレートで読む（v0.26.0）。
  filer はまだ画面の文字を表に通していないので、filer の i18n は別の仕事（filer の TODO に積むのは filer のセッションで）
- [x] keymap の読み込み（`config/keys.rs`・`keymap.rs`）も共有のクレートにするか、filer と見比べて決める。
  → キーの書き方（`keys.rs`）だけを `tsumugi-keys`（tsumugi v0.80.0）にした。filer の `keys.rs` とほぼ同じで、違いは `P-` だけだったため。
  `keymap.rs` は filer（yazi のセクションが多く 669 行）と mimamori（228 行）で層もコマンドも違うので各アプリに残す。
  filer が使うときは `mods.command` を `mods.ctrl` に畳んでから `from_egui` を呼ぶ（v0.27.0）

## 4. リモート

- [x] 見本の「設計」タブのとおり、ほかの機械のエージェント（同じバイナリの `--agent`）から `Msg` を受けて fleet の画面に並べる。
  通信の形（TLS と鍵の置き方）は QUESTIONS.md で決めてから。
  → Q1 の 1。エージェントは `127.0.0.1:7313` だけで待ち受け、暗号も認証も持たない（`ssh -L` か `tailscale serve` で届ける）。
  送るのは `Msg` そのものではなく、ラウンドごとの要約（CPU・メモリ・GPU・温度）を postcard で。`F` の画面にカードで並べる。見るだけ (v0.31.0)
- [x] fleet: ほかの機械のカードで `Enter` を押すと、その機械の画面（cpu・procs など）に入る。`[` / `]` で機械を切り替える。
  要約ではなく `Msg` を流す形にプロトコルを広げる（画面が出ている間だけ、procs は重いので間引く）。
  → プロトコル 2。見る側が `Want`（機械を見る・svc・net）を送り、エージェントは全員の和だけ収集して `Msg` を流す。procs は約 4 秒ごと (v0.35.0)
- [x] fleet: `[agent] allow` に書いたものだけ、ほかの機械で操作を受ける（`kill`・`service`）。エージェントの側で確かめ、確認の窓にどの機械かを出す。
  → プロトコル 3。エージェントが自分の `allow` と自分の一覧で確かめ（自分自身は終わらせない、plist は自分の読んだもの）、1 回ごとのスレッドで実行して答える。
  確認の窓の頭に機械と実行するユーザー、答えは機械の名前付きのトースト。切れたら送り直さない (v0.36.0)
- [x] fleet: sys の画面にエージェントの節（待ち受けているか・ポート・allow・つないでいる見る側の数）。
  → 手元は sys が出ている間だけ 5 秒ごとに `client::probe` で自分のポートに聞く（Hello と最初の Summary）。ほかの機械はその Hello と Summary から。
  動いていなければポート・`[agent] allow`・立て方を出す。プロトコル 4 で Hello に `port`、Summary に `viewers` (v0.40.0)
- [x] fleet: カードに電池（ノート PC）と、警告の数（温度・SMART・落ちたサービス）。
  → `collect/battery/`（Linux は sysfs、Windows は `GetSystemPowerStatus`、macOS は `AppleSmartBattery`）。警告は赤い温度・SMART が黄色のドライブ・落ちたサービスの数で、
  サービスは画面に出ていなくても 60 秒ごとに読む。警告があれば facts の行が黄色、サービスが落ちていれば赤 (v0.40.0)
- [x] fleet: `--install-service`（Windows はタスク スケジューラ、Linux は systemd の user unit、macOS は launchd の LaunchAgent）で、ログオン時にエージェントを立てる。
  → Windows はタスク スケジューラの ONLOGON が管理者を要るので、HKCU の `Run` キーにし、その場でも窓なしで立てる（`--log` で `agent.log` に書く）。
  Linux は `~/.config/systemd/user/yagura-agent.service`、macOS は `~/Library/LaunchAgents/io.github.uchmk.yagura-agent.plist`。`--uninstall-service` で止めて消す (v0.40.0)
- [x] fleet: 要約を差分にする・間隔を見る側から変える（細い回線のため）。
  → プロトコル 5。最初は丸ごとの `Summary`、その後は変わった欄だけの `Delta`（見る側への枠が詰まって落ちたら次は丸ごと）。
  `[[host]] every = 秒` で見る側が `ToAgent::Pace` を送り、その見る側への要約だけ間を空ける（0.5〜60 秒、エージェントの間隔より速くはしない） (v0.41.0)
- [x] fleet: 直接つなぐ形（Q1 の 3: Noise）。
  → `snow`（Pure Rust、`Noise_IK_25519_ChaChaPoly_BLAKE2s`）。`yagura --key` で鍵の組を作って見せ（`<設定>/yagura/key`）、見られる側は `[agent] listen` と
  `[[agent.viewer]]`（見る側の公開鍵）、見る側は `[[host]] key`（相手の公開鍵、既定のポート 7314）。知らない見る側・違う鍵は `Why::Denied`・`BadKey` で言う。
  設定の画面の Fleet に DIRECT CONNECTIONS の欄、sys の Fleet agent の節に直接の口と鍵 (v0.41.0)
- [x] fleet: カードの警告の数に、問題のあるデバイス（devices の画面の赤）を足す。devices も出ていない間は 1 分ごとに読む (v0.52.0)。

## 5. 配布

- [x] リリースの workflow（6 ターゲット: Windows / macOS / Linux × x86_64 / ARM64）。filer の `release.yml` と `release-sums.yml` に倣う。
  → tsumugi の形をそのまま（ConPTY は無し、zip と tar.gz にライセンスを同梱）。Actions タブから `tag` を渡して回す（v0.28.0）
- [x] Windows のアイコンと版の情報（`winres` は C を要らない形で）。
  → アイコンはコードで描き（`src/icon.rs`、4 つのコアのタイル）、窓のアイコンにも使う。`.res` は `build.rs` が Rust で書いて（`src/winres.rs`）リンカーに渡す。rc.exe も C も要らない（v0.29.0）
- [x] CI に Linux のスモーク（filer の `smoke` と同じく lavapipe で起動して撮る）。
  → mimamori は glow（OpenGL）なので lavapipe ではなく Mesa の llvmpipe。`scripts/smoke-linux.sh` が全画面を撮って確かめ、画像を成果物に残す（v0.29.1）

## 6. 設定の画面・テーマ・時計（uchmk の共通仕様）

- [x] 設定の画面（Ctrl+, / macOS は Cmd+,）を tsumugi と同じレイアウトで。テーマを足し、日付と時刻を tsumugi と同じ場所（ステータスバーの右端）に。
  テーマと時計は common.toml で共通にする（このファイルを変えれば uchmk のアプリが一括で変わる）。fleet の手引きの行をコピーできるように。
  → 共通仕様は tsumugi の `docs/common-spec.md`。共通のクレート `tsumugi-common`（common.toml の読み書き・見張り・時計）・`tsumugi-theme`（13 のテーマ・自分のテーマ）・
  `tsumugi-prefs`（画面の枠と部品、Language の行・CLOCK のカード・テーマのページ）を tsumugi v0.81.0 で切り出し、mimamori はその上に General・Theme・Fleet・Advanced の 4 ページ。
  色の定数をテーマを読む関数に換え、common.toml を 2 秒ごとに見て当て直す。`y` と、手引きの水色の行・設定の Copy でコピー（v0.32.0）
- [ ] 設定の画面・テーマ・時計の実機の確認 【実機】（TESTING.md 5.9〜5.18）。
- [x] tsumugi の時計の曜日が `language = "ja"` でも英語（uchmk/yagura#11 の 5.11）。tsumugi の側の直しなので、tsumugi のセッションに伝える。
  → tsumugi v0.94.3 が `ito_common::Clock` の `text`・`full` を言語つきで使い、yagura と同じ 1 文字の `(日)` になった（tsumugi の再テストは uchmk/tsumugi#23 の 15.11）
- [ ] fleet の直接つなぐ形の再テスト（TESTING.md 3g.22 の機械の名前の `addr`・3g.24・3g.25、v0.42.2 の直し）。持ち主がすぐにはできないので後で頼む 【実機】。
